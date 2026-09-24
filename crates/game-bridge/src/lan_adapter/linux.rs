//! The Linux room adapter: a TUN device (`PLAN.md` §14, steps 2 and 5b).
//!
//! A layer-3 TUN device carrying the room's subnet and nothing else, made
//! **non-persistent**: it exists exactly as long as the descriptor that made it
//! is open. The elevated `lan-helper serve` holds that descriptor and relays
//! packets to the unprivileged launcher (`lan_relay.rs`), the same shape as on
//! Windows — so the adapter, its address and its routes cannot outlive the
//! launcher, however it ends: a quit, a crash, `kill -9`. The helper sees its
//! relay drop, exits, and the kernel takes the device away with its last
//! descriptor.
//!
//! The helper gets its privilege one of two ways (`open`):
//!
//! - **Granted once**: `cap_net_admin` on the helper file, which an installed
//!   launcher can ask for (`pkexec setcap`). Then no prompt per room.
//! - **Per room**: nothing granted — the portable case, and the AppImage's,
//!   whose `nosuid`, owner-only FUSE mount can neither hold a capability nor be
//!   read by root. The launcher copies the helper into the session's runtime
//!   directory (`/run/user/<uid>`, memory-backed and cleared at logout), runs
//!   the copy through `pkexec`, and deletes it as soon as it has connected
//!   back. Nothing is installed and nothing is left.
//!
//! Everything is an ioctl, deliberately: a helper granted `cap_net_admin` by
//! file capability does not pass it to a child it spawns, so shelling out to
//! `ip` would work under `sudo` and fail under `setcap`.
//!
//! **The limited-broadcast route is what makes old games find each other.**
//! Without it a broadcast to `255.255.255.255` leaves by the default route —
//! the real LAN — which is the Hamachi "adapter metric" failure on Linux. It
//! also means that while a room is up, *every* program's limited broadcasts go
//! to the room; the route goes with the device.

use std::ffi::CString;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use tokio::io::unix::AsyncFd;

use super::{invalid, AdapterConfig, AdapterSetup};
use crate::lan::LAN_MTU;
use crate::lan_pump::PacketDevice;
use crate::lan_relay::{self, RelayToken, RemoteDevice};

/// Create and configure a non-persistent adapter and keep it open. Needs
/// `CAP_NET_ADMIN`: this is what `lan-helper serve` does, and what a process
/// that already holds the capability — a test in a user namespace — does
/// itself. The device goes when the descriptor does.
pub fn open_configured(config: &AdapterConfig) -> io::Result<OwnedFd> {
    config.validate()?;
    let fd = open_tun(&config.name)?;
    configure(config)?;
    Ok(fd)
}

/// Bring an interface up. Exposed for `lo` in a fresh network namespace.
pub fn bring_up(name: &str) -> io::Result<()> {
    let sock = control_socket()?;
    let mut req = ifreq(name)?;
    ioctl_ifreq(&sock, libc::SIOCGIFFLAGS, &mut req)?;
    // SAFETY: SIOCGIFFLAGS filled the flags member of the union.
    unsafe {
        req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
    }
    ioctl_ifreq(&sock, libc::SIOCSIFFLAGS, &mut req)
}

/// A room adapter opened for reading and writing IPv4 packets.
pub struct TunDevice {
    fd: AsyncFd<OwnedFd>,
}

impl TunDevice {
    /// Wrap a descriptor from [`open_configured`]. Must be called inside a
    /// tokio runtime.
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        Ok(Self { fd: AsyncFd::new(fd)? })
    }

    /// The next packet the machine sent into the room.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            match guard.try_io(|fd| {
                // SAFETY: reading into a buffer we own, of the length given.
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    /// Hand one packet from the room to the machine.
    pub async fn send(&self, packet: &[u8]) -> io::Result<()> {
        loop {
            let mut guard = self.fd.writable().await?;
            match guard.try_io(|fd| {
                // SAFETY: writing from a buffer we own, of its own length.
                let n =
                    unsafe { libc::write(fd.as_raw_fd(), packet.as_ptr().cast(), packet.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }
}

impl crate::lan_pump::PacketDevice for TunDevice {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        TunDevice::recv(self, buf).await
    }

    async fn send(&self, packet: &[u8]) -> io::Result<()> {
        TunDevice::send(self, packet).await
    }
}

// ---------------------------------------------------------------------------
// ioctls
// ---------------------------------------------------------------------------

fn open_tun(name: &str) -> io::Result<OwnedFd> {
    let path = CString::new("/dev/net/tun").expect("no interior nul");
    // SAFETY: a valid C string and plain flags.
    let raw =
        unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` returned a descriptor nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut req = ifreq(name)?;
    req.ifr_ifru.ifru_flags = (libc::IFF_TUN | libc::IFF_NO_PI) as libc::c_short;
    // SAFETY: TUNSETIFF takes a pointer to an ifreq, which `req` is.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNSETIFF, &mut req) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn control_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket creation.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `socket` returned a descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn ifreq(name: &str) -> io::Result<libc::ifreq> {
    if name.len() >= libc::IFNAMSIZ || name.bytes().any(|b| b == 0) {
        return Err(invalid(format!("{name:?} is not an interface name")));
    }
    // SAFETY: ifreq is plain data; all-zero is a valid value.
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(name.bytes()) {
        *dst = src as libc::c_char;
    }
    Ok(req)
}

fn ioctl_ifreq(sock: &OwnedFd, request: libc::Ioctl, req: &mut libc::ifreq) -> io::Result<()> {
    // SAFETY: every SIOC[GS]IF* request here takes a pointer to an ifreq.
    if unsafe { libc::ioctl(sock.as_raw_fd(), request, req as *mut libc::ifreq) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn sockaddr(addr: Ipv4Addr) -> libc::sockaddr {
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr { s_addr: u32::from(addr).to_be() },
        sin_zero: [0; 8],
    };
    // SAFETY: sockaddr_in and sockaddr are the same size, and the kernel reads
    // this one as the AF_INET it says it is.
    unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin) }
}

fn set_addr(sock: &OwnedFd, name: &str, request: libc::Ioctl, addr: Ipv4Addr) -> io::Result<()> {
    let mut req = ifreq(name)?;
    req.ifr_ifru.ifru_addr = sockaddr(addr);
    ioctl_ifreq(sock, request, &mut req)
}

fn configure(config: &AdapterConfig) -> io::Result<()> {
    let sock = control_socket()?;
    let name = &config.name;
    let netmask = Ipv4Addr::from(u32::MAX << (32 - config.subnet.prefix_len as u32));
    set_addr(&sock, name, libc::SIOCSIFADDR, config.address)?;
    set_addr(&sock, name, libc::SIOCSIFNETMASK, netmask)?;
    set_addr(&sock, name, libc::SIOCSIFBRDADDR, config.subnet.broadcast())?;
    let mut req = ifreq(name)?;
    req.ifr_ifru.ifru_mtu = LAN_MTU as libc::c_int;
    ioctl_ifreq(&sock, libc::SIOCSIFMTU, &mut req)?;
    disable_ipv6(name);
    bring_up(name)?;
    add_limited_broadcast_route(&sock, name)
}

/// The room is IPv4 (`PLAN.md` §14.1). Left on, the kernel gives the adapter a
/// link-local IPv6 address and sends router and neighbour solicitations into
/// it, which the pump would read only to drop. Best effort: a kernel without
/// IPv6 has no such file, and that is fine.
fn disable_ipv6(name: &str) {
    let path = format!("/proc/sys/net/ipv6/conf/{name}/disable_ipv6");
    let _ = std::fs::write(path, b"1");
}

/// The kernel's `struct rtentry` (`include/uapi/linux/route.h`), which the
/// libc crate does not define for glibc targets.
#[repr(C)]
struct RtEntry {
    rt_pad1: libc::c_ulong,
    rt_dst: libc::sockaddr,
    rt_gateway: libc::sockaddr,
    rt_genmask: libc::sockaddr,
    rt_flags: libc::c_ushort,
    rt_pad2: libc::c_short,
    rt_pad3: libc::c_ulong,
    rt_pad4: *mut libc::c_void,
    rt_metric: libc::c_short,
    rt_dev: *mut libc::c_char,
    rt_mtu: libc::c_ulong,
    rt_window: libc::c_ulong,
    rt_irtt: libc::c_ushort,
}

const RTF_UP: libc::c_ushort = 0x0001;
const RTF_HOST: libc::c_ushort = 0x0004;

fn add_limited_broadcast_route(sock: &OwnedFd, name: &str) -> io::Result<()> {
    let dev =
        CString::new(name).map_err(|_| invalid(format!("{name:?} is not an interface name")))?;
    let mut route = RtEntry {
        rt_pad1: 0,
        rt_dst: sockaddr(Ipv4Addr::BROADCAST),
        rt_gateway: sockaddr(Ipv4Addr::UNSPECIFIED),
        rt_genmask: sockaddr(Ipv4Addr::BROADCAST),
        rt_flags: RTF_UP | RTF_HOST,
        rt_pad2: 0,
        rt_pad3: 0,
        rt_pad4: std::ptr::null_mut(),
        rt_metric: 0,
        rt_dev: dev.as_ptr() as *mut libc::c_char,
        rt_mtu: 0,
        rt_window: 0,
        rt_irtt: 0,
    };
    // SAFETY: SIOCADDRT takes a pointer to an rtentry; `dev` outlives the call.
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCADDRT, &mut route as *mut RtEntry) } < 0 {
        let err = io::Error::last_os_error();
        // Reconfiguring an adapter that already has it is not a failure.
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(err);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Running a room on an adapter
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The helper, and the launcher's side of it
// ---------------------------------------------------------------------------

/// `lan-helper serve`: make the adapter, connect out to the launcher that
/// started this, and relay until it goes away. The adapter goes with this
/// process's descriptor, however the launcher ended.
pub async fn serve(
    config: &AdapterConfig,
    launcher: std::net::SocketAddr,
    token: &RelayToken,
) -> io::Result<()> {
    let device = TunDevice::from_fd(open_configured(config)?)?;
    let stream = lan_relay::connect_to_launcher(launcher, token).await?;
    lan_relay::relay(&device, stream).await
}

/// The adapter as the launcher holds it.
pub enum LinuxAdapter {
    /// This process made it itself (it already holds the capability).
    Direct(TunDevice),
    /// `lan-helper serve` holds it and relays.
    Relayed(RemoteDevice),
}

impl PacketDevice for LinuxAdapter {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Direct(d) => d.recv(buf).await,
            Self::Relayed(d) => d.recv(buf).await,
        }
    }

    async fn send(&self, packet: &[u8]) -> io::Result<()> {
        match self {
            Self::Direct(d) => d.send(packet).await,
            Self::Relayed(d) => d.send(packet).await,
        }
    }
}

/// Whether the helper at `path` already holds its capability.
fn granted(path: &std::path::Path) -> bool {
    std::process::Command::new(path)
        .arg("check")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The program that asks a person for their password and runs one command as
/// root. `pkexec` is polkit's, and every desktop that can show a password
/// prompt has it.
fn elevator() -> Option<std::path::PathBuf> {
    // A debug build may be pointed at a stand-in, so a test can drive the
    // per-room path without a person at a polkit prompt. Compiled out of a
    // release: a release asks polkit, and only polkit.
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var_os(ELEVATOR_ENV) {
        return Some(p.into());
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|d| d.join("pkexec")).find(|p| p.is_file())
}

/// The environment variable a debug build reads a stand-in elevator from.
pub const ELEVATOR_ENV: &str = "GAME_BRIDGE_ELEVATOR";

/// A copy of the helper for one elevated run, deleted when dropped.
///
/// It lives in the session's runtime directory — memory-backed, cleared at
/// logout — under an unguessable name, created exclusively with mode 0700, so
/// nothing of it survives even a machine that loses power mid-room. It is
/// dropped as soon as the helper has connected back: a running program does
/// not need its file.
pub struct HelperCopy(std::path::PathBuf);

impl HelperCopy {
    pub fn stage(helper: &std::path::Path) -> io::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .filter(|d| d.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let bytes = std::fs::read(helper)?;
        let mut nonce = [0u8; 8];
        getrandom::getrandom(&mut nonce).map_err(|e| io::Error::other(e.to_string()))?;
        let path = dir.join(format!("gpp-lan-helper-{}", hex::encode(nonce)));
        let mut file =
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o700).open(&path)?;
        let copy = Self(path);
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(copy)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for HelperCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Start the helper with `args`: directly when it already holds its
/// capability, else as a staged copy through the elevator. Returns the copy,
/// if one was made, for the caller to drop once the helper has connected.
fn start_helper(path: &std::path::Path, args: Vec<String>) -> io::Result<Option<HelperCopy>> {
    let forced = cfg!(debug_assertions) && std::env::var_os(ELEVATOR_ENV).is_some();
    let (program, copy, prefix) = if granted(path) && !forced {
        (path.to_path_buf(), None, None)
    } else {
        let elevator = elevator().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no pkexec to ask for your password. Install polkit, or grant the helper its \
                     permission once: sudo setcap cap_net_admin+ep {}",
                    path.display()
                ),
            )
        })?;
        let copy = HelperCopy::stage(path)?;
        (elevator, Some(copy.path().to_path_buf()), Some(copy))
    };
    let mut cmd = std::process::Command::new(&program);
    if let Some(copy) = &copy {
        cmd.arg(copy);
    }
    let mut child = cmd.args(&args).stdin(std::process::Stdio::null()).spawn()?;
    // Reaped on its own thread: the helper lives for the whole room, and an
    // unwaited child would stay a zombie until the launcher exits.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(prefix)
}

/// Bring the adapter up for `config` and open it.
pub(super) async fn open(
    setup: &AdapterSetup,
    config: &AdapterConfig,
) -> anyhow::Result<LinuxAdapter> {
    match setup {
        AdapterSetup::InProcess => {
            let c = config.clone();
            let fd = tokio::task::spawn_blocking(move || open_configured(&c)).await??;
            Ok(LinuxAdapter::Direct(TunDevice::from_fd(fd)?))
        }
        AdapterSetup::Helper { path, .. } => {
            let path = path.clone();
            let remote =
                super::relayed(config, Vec::new(), move |args| start_helper(&path, args)).await?;
            Ok(LinuxAdapter::Relayed(remote))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtentry_matches_the_kernels_size() {
        // x86_64 and aarch64: 120 bytes (include/uapi/linux/route.h).
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<RtEntry>(), 120);
    }
}
