//! Mode 3, virtual LAN: the Linux adapter (`PLAN.md` §14, step 2).
//!
//! A layer-3 TUN device carrying the room's subnet and nothing else. The split
//! that keeps the launcher unprivileged (`PLAN.md` §14.1):
//!
//! - **[`create`] and [`destroy`] need `CAP_NET_ADMIN`**, and are all the
//!   `lan-helper` binary does. `create` makes the device *persistent* and
//!   *owned* by the user, then configures it.
//! - **[`TunDevice::attach`] does not.** A persistent TUN device owned by a
//!   user can be opened by that user with no capability at all, so the
//!   launcher — which runs the room — opens it itself and never elevates.
//!
//! Everything is an ioctl, deliberately: a helper granted `cap_net_admin` by
//! file capability does not pass it to a child it spawns, so shelling out to
//! `ip` would work under `sudo` and fail under `setcap`.
//!
//! # Rules a later change could quietly break
//!
//! - **Only `gbl*` names, only the room range.** The helper is privileged and
//!   its arguments are the caller's. [`AdapterConfig::validate`] refuses any
//!   other device name — so it cannot be pointed at `eth0` — and any subnet
//!   outside `198.18.0.0/15` (`lan::RoomSubnet::new`), so it cannot route a
//!   real LAN into a room.
//! - **No default route.** The device gets its subnet and one host route for
//!   the limited broadcast `255.255.255.255`, and nothing else.
//! - **The limited-broadcast route is what makes old games find each other.**
//!   Without it a broadcast to `255.255.255.255` leaves by the default route —
//!   the real LAN — which is the Hamachi "adapter metric" failure on Linux. It
//!   also means that while a room is up, *every* program's limited broadcasts
//!   go to the room; [`destroy`] takes it away with the device.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use tokio::io::unix::AsyncFd;

use crate::lan::{RoomSubnet, LAN_MTU};

/// Every adapter this platform makes is named with this prefix.
pub const ADAPTER_PREFIX: &str = "gbl";

/// What the adapter is given once its member is seated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterConfig {
    pub name: String,
    pub address: Ipv4Addr,
    pub subnet: RoomSubnet,
}

impl AdapterConfig {
    /// Refuse anything the helper must not be talked into.
    pub fn validate(&self) -> io::Result<()> {
        validate_name(&self.name)?;
        if RoomSubnet::new(self.subnet.prefix, self.subnet.prefix_len) != Some(self.subnet) {
            return Err(invalid(format!(
                "{}/{} is not a room subnet (rooms live in 198.18.0.0/15)",
                self.subnet.prefix, self.subnet.prefix_len
            )));
        }
        if !self.subnet.contains(self.address) || self.address == self.subnet.broadcast() {
            return Err(invalid(format!("{} is not a member address in its subnet", self.address)));
        }
        Ok(())
    }
}

/// An adapter name: `gbl` and then letters, digits or `-`, within the
/// kernel's 15 bytes.
pub fn validate_name(name: &str) -> io::Result<()> {
    let ok = name.starts_with(ADAPTER_PREFIX)
        && name.len() < libc::IFNAMSIZ
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "{name:?} is not a room adapter name ({ADAPTER_PREFIX}*, at most 15 bytes)"
        )))
    }
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Create the adapter, persistent and owned by `owner`, and configure it.
/// Needs `CAP_NET_ADMIN`. Running it again on an existing adapter reconfigures
/// it, which is how a member whose address changed is moved.
pub fn create(config: &AdapterConfig, owner: u32) -> io::Result<()> {
    config.validate()?;
    let fd = open_tun(&config.name)?;
    tun_ioctl(&fd, libc::TUNSETPERSIST, 1)?;
    tun_ioctl(&fd, libc::TUNSETOWNER, owner as libc::c_ulong)?;
    configure(config)
}

/// Remove an adapter [`create`] made. Needs `CAP_NET_ADMIN`.
pub fn destroy(name: &str) -> io::Result<()> {
    validate_name(name)?;
    let fd = open_tun(name)?;
    tun_ioctl(&fd, libc::TUNSETPERSIST, 0)
}

/// Create and configure a non-persistent adapter and keep it open: for a
/// process that already holds `CAP_NET_ADMIN` and needs no helper, like a test
/// inside a user namespace. The device goes when the descriptor does.
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
    /// Open an adapter [`create`] made for this user. Needs no capability.
    /// Must be called inside a tokio runtime.
    pub fn attach(name: &str) -> io::Result<Self> {
        validate_name(name)?;
        Self::from_fd(open_tun(name)?)
    }

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

fn tun_ioctl(fd: &OwnedFd, request: libc::Ioctl, arg: libc::c_ulong) -> io::Result<()> {
    // SAFETY: TUNSETPERSIST and TUNSETOWNER take their argument by value.
    if unsafe { libc::ioctl(fd.as_raw_fd(), request, arg) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(name: &str, prefix: Ipv4Addr, len: u8, address: Ipv4Addr) -> AdapterConfig {
        AdapterConfig {
            name: name.to_string(),
            address,
            subnet: RoomSubnet { prefix, prefix_len: len },
        }
    }

    #[test]
    fn the_helper_cannot_be_pointed_at_a_real_interface() {
        for name in
            ["eth0", "lo", "wlan0", "tailscale0", "gbl0; rm", "gbl0/../x", "gbl-this-is-too-long"]
        {
            assert!(validate_name(name).is_err(), "{name}");
        }
        assert!(validate_name("gbl0").is_ok());
        assert!(validate_name("gbl-room-1").is_ok());
    }

    #[test]
    fn the_helper_cannot_route_a_real_lan() {
        let lan = config("gbl0", Ipv4Addr::new(192, 168, 1, 0), 24, Ipv4Addr::new(192, 168, 1, 7));
        assert!(lan.validate().is_err());
        // A prefix that is not masked is not a subnet `RoomSubnet::new` made.
        let unmasked =
            config("gbl0", Ipv4Addr::new(198, 19, 3, 4), 16, Ipv4Addr::new(198, 19, 3, 4));
        assert!(unmasked.validate().is_err());
        let ok = config("gbl0", Ipv4Addr::new(198, 19, 0, 0), 16, Ipv4Addr::new(198, 19, 3, 4));
        assert!(ok.validate().is_ok());
        let outside =
            config("gbl0", Ipv4Addr::new(198, 19, 0, 0), 16, Ipv4Addr::new(198, 18, 3, 4));
        assert!(outside.validate().is_err(), "an address outside its own subnet");
        let bcast =
            config("gbl0", Ipv4Addr::new(198, 19, 0, 0), 16, Ipv4Addr::new(198, 19, 255, 255));
        assert!(bcast.validate().is_err(), "the broadcast address is nobody's");
    }

    #[test]
    fn rtentry_matches_the_kernels_size() {
        // x86_64 and aarch64: 120 bytes (include/uapi/linux/route.h).
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<RtEntry>(), 120);
    }
}
