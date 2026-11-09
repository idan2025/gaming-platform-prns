//! The macOS room adapter (`PLAN.md` §14.5).
//!
//! **A `feth` pair, read and written through BPF.** `feth` is macOS's fake
//! Ethernet: two ends, each a normal broadcast-capable interface, wired to each
//! other. One end (`feth0`) carries the room's address and the routes; the
//! helper holds the other (`feth1`) open through BPF, reads the frames the
//! machine sends into the room and writes the room's frames back. It is a thin
//! Ethernet shim around the same IPv4 packets the Linux and Windows adapters
//! carry: the pump above it is shared and never sees a frame.
//!
//! **Why not a `utun`, which is the obvious macOS tunnel?** A `utun` is a
//! point-to-point interface, and the kernel refuses a limited broadcast on
//! one: `sendto(255.255.255.255)` fails with `EADDRNOTAVAIL`, however it is
//! routed or bound (measured on macOS 15: a host route, a gateway route, a
//! socket bound to the room address and `IP_BOUND_IF` all fail; only a
//! *directed* broadcast to the subnet works). Old LAN games find each other
//! with exactly that limited broadcast, so a `utun` room would be a room those
//! games cannot see. A `feth` end has the BROADCAST flag, and its limited
//! broadcast arrives here as a frame to `ff:ff:ff:ff:ff:ff`. The `utun` stays
//! as a fallback for a macOS where a `feth` cannot be made: unicast and
//! directed broadcast work, the limited broadcast does not, and the helper says
//! so.
//!
//! **What goes through the shim**
//! - *Out of the machine*: an IPv4 frame is unwrapped to its packet (trimmed
//!   to the IP total length: Ethernet pads); IGMP is dropped; an ARP request
//!   for any room address that is not this machine's own is answered with one
//!   fixed MAC (proxy ARP for the subnet), so the kernel will send the unicast
//!   at all. Everything else (IPv6, LLDP, …) is ignored: a room carries IPv4.
//! - *Into the machine*: a room packet is wrapped in a frame, to the broadcast
//!   MAC when its destination is a broadcast and to the `feth0` MAC otherwise.
//!
//! **Lifetime.** The helper holds the interfaces and destroys them when the
//! room ends — whether the launcher quit, crashed or was killed, because the
//! helper sees the relay drop (and also on `SIGTERM`/`SIGINT`/`SIGHUP`). That is
//! the same guarantee as Windows and Linux for everything but one case: a
//! `feth` is not tied to a descriptor the way a `utun` or a TUN device is, so a
//! helper that is itself `SIGKILL`ed leaves its pair behind. Every helper
//! therefore reaps what an earlier one left before it makes its own: any `feth`
//! carrying an address in the room range (`198.18.0.0/15`, which belongs to
//! this project and to nothing else) is destroyed first. The left-over pair is
//! inert — no route to anywhere but the room range — until then.
//!
//! **Elevation.** Only root can make either, so, as on Windows, the launcher
//! never elevates itself: it asks macOS's own administrator prompt
//! (`osascript … with administrator privileges`) to start `lan-helper serve`,
//! which connects *out* to the launcher with a one-time token
//! (`lan_relay.rs`).
//!
//! **The helper runs `/sbin/ifconfig` and `/sbin/route`**, by absolute path and
//! only as root, with arguments built from a validated room address and subnet
//! and interface names it read back from the system. Linux avoids spawning
//! because a file capability is not inherited by a child; root through the
//! admin prompt has no such limit, and the BSD ioctls for an alias are not in
//! `libc`.
//!
//! **No default route and no IPv6.** The adapter gets its subnet and the
//! limited-broadcast host route that makes old games find each other, and goes
//! with the interface. The kernel also gives a `feth`/`utun` an IPv6 link-local
//! address; the shim ignores IPv6 frames, so it stays inert.

use std::collections::VecDeque;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::Mutex;

use tokio::io::unix::AsyncFd;

use super::{invalid, AdapterConfig, AdapterSetup};
use crate::lan::{RoomSubnet, LAN_MTU};
use crate::lan_pump::PacketDevice;
use crate::lan_relay::{self, RelayToken, RemoteDevice};

const IFCONFIG: &str = "/sbin/ifconfig";
const ROUTE: &str = "/sbin/route";

/// Whether this process is root, which making either interface needs.
pub fn is_elevated() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

// ---------------------------------------------------------------------------
// Running the system tools
// ---------------------------------------------------------------------------

/// Run one of the two system tools by absolute path and return its stdout.
/// `args` are built only from a validated room address and subnet and
/// interface names the system gave back.
fn run(program: &str, args: &[String]) -> io::Result<String> {
    let out = std::process::Command::new(program).args(args).output()?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let said = String::from_utf8_lossy(&out.stderr);
    // A route that is already there is not a failure (a reconfigured adapter).
    if said.contains("File exists") {
        return Ok(String::new());
    }
    Err(io::Error::other(format!("{program} {}: {}", args.join(" "), said.trim())))
}

fn ifconfig(args: &[&str]) -> io::Result<String> {
    run(IFCONFIG, &args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

/// The dotted netmask of a prefix length.
fn netmask(prefix_len: u8) -> Ipv4Addr {
    let bits = if prefix_len == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix_len.min(32))) };
    Ipv4Addr::from(bits)
}

/// A `feth<N>` or `utun<N>` name, nothing else: only names the system itself
/// returned are ever passed back to it.
fn is_clone_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

// ---------------------------------------------------------------------------
// Ethernet frames
// ---------------------------------------------------------------------------

/// The MAC the "other side of the room" answers with, for every room address
/// that is not this machine's: locally administered (`02:…`), never a real one.
const PEER_MAC: [u8; 6] = [0x02, 0x67, 0x62, 0x6c, 0x00, 0x01];
const BROADCAST_MAC: [u8; 6] = [0xff; 6];
const ETH_IPV4: [u8; 2] = [0x08, 0x00];
const ETH_ARP: [u8; 2] = [0x08, 0x06];
const ETH_HEADER: usize = 14;

/// A frame carrying `packet` to the machine: to the broadcast MAC when the
/// packet is addressed to a broadcast (`subnet`'s, the limited one, or
/// multicast), else to `own_mac`.
fn wrap_ipv4(packet: &[u8], own_mac: [u8; 6], subnet: &RoomSubnet) -> Option<Vec<u8>> {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return None;
    }
    let dst = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
    let mac = if subnet.is_broadcast(dst) { BROADCAST_MAC } else { own_mac };
    let mut frame = Vec::with_capacity(ETH_HEADER + packet.len());
    frame.extend_from_slice(&mac);
    frame.extend_from_slice(&PEER_MAC);
    frame.extend_from_slice(&ETH_IPV4);
    frame.extend_from_slice(packet);
    Some(frame)
}

/// What the shim makes of one frame the machine sent into the room.
#[derive(Debug, PartialEq, Eq)]
enum Outgoing {
    /// An IPv4 packet for the room.
    Packet(Vec<u8>),
    /// An ARP reply to put back on the wire.
    Reply(Vec<u8>),
    /// Nothing for a room: IPv6, IGMP, an ARP for this machine, a runt…
    Ignore,
}

fn read_frame(frame: &[u8], own_address: Ipv4Addr, subnet: &RoomSubnet) -> Outgoing {
    if frame.len() < ETH_HEADER {
        return Outgoing::Ignore;
    }
    let ethertype = [frame[12], frame[13]];
    let body = &frame[ETH_HEADER..];
    if ethertype == ETH_IPV4 {
        if body.len() < 20 || body[0] >> 4 != 4 {
            return Outgoing::Ignore;
        }
        // IGMP is the kernel's own group chatter, not a game's.
        if body[9] == 2 {
            return Outgoing::Ignore;
        }
        // Ethernet pads a short frame; the packet is its own length.
        let total = u16::from_be_bytes([body[2], body[3]]) as usize;
        if total < 20 || total > body.len() {
            return Outgoing::Ignore;
        }
        return Outgoing::Packet(body[..total].to_vec());
    }
    if ethertype == ETH_ARP {
        return match arp_reply(frame, own_address, subnet) {
            Some(r) => Outgoing::Reply(r),
            None => Outgoing::Ignore,
        };
    }
    Outgoing::Ignore
}

/// The proxy-ARP answer to a request for a room address that is not ours.
fn arp_reply(frame: &[u8], own_address: Ipv4Addr, subnet: &RoomSubnet) -> Option<Vec<u8>> {
    let a = frame.get(ETH_HEADER..ETH_HEADER + 28)?;
    // Ethernet, IPv4, 6- and 4-byte addresses, a request.
    if a[..8] != [0, 1, 8, 0, 6, 4, 0, 1] {
        return None;
    }
    let sha = &a[8..14];
    let spa = Ipv4Addr::new(a[14], a[15], a[16], a[17]);
    let tpa = Ipv4Addr::new(a[24], a[25], a[26], a[27]);
    // A probe (sender 0.0.0.0) or a request for our own address is not ours to
    // answer, nor is anything outside the room.
    if spa.is_unspecified()
        || tpa == own_address
        || !subnet.contains(tpa)
        || tpa == subnet.broadcast()
    {
        return None;
    }
    let mut out = Vec::with_capacity(ETH_HEADER + 28);
    out.extend_from_slice(sha); // to the asker
    out.extend_from_slice(&PEER_MAC);
    out.extend_from_slice(&ETH_ARP);
    out.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]); // reply
    out.extend_from_slice(&PEER_MAC); // sender hardware: the room
    out.extend_from_slice(&tpa.octets()); // sender protocol: the address asked for
    out.extend_from_slice(sha); // target hardware: the asker
    out.extend_from_slice(&spa.octets()); // target protocol: the asker
    Some(out)
}

// ---------------------------------------------------------------------------
// BPF
// ---------------------------------------------------------------------------

// `<net/bpf.h>`, the same on Intel and Apple Silicon.
const BIOCSBLEN: libc::c_ulong = 0xc004_4266;
const BIOCSETIF: libc::c_ulong = 0x8020_426c;
const BIOCIMMEDIATE: libc::c_ulong = 0x8004_4270;
const BIOCSHDRCMPLT: libc::c_ulong = 0x8004_4275;
const BIOCSSEESENT: libc::c_ulong = 0x8004_4277;
/// Offsets in `struct bpf_hdr`.
const BPF_CAPLEN: usize = 8;
const BPF_HDRLEN: usize = 16;
const BPF_BUFFER: u32 = 256 * 1024;

fn ioctl_u32(fd: &OwnedFd, request: libc::c_ulong, value: &mut u32) -> io::Result<()> {
    // SAFETY: the request takes a pointer to a u32 that lives through the call.
    if unsafe { libc::ioctl(fd.as_raw_fd(), request, value as *mut u32) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Open a free `/dev/bpfN`, attach it to `interface` and make it deliver each
/// frame at once, keep our own source MAC, and not echo what it writes.
fn open_bpf(interface: &str) -> io::Result<OwnedFd> {
    let mut last = io::Error::from(io::ErrorKind::NotFound);
    for i in 0..256 {
        let path = std::ffi::CString::new(format!("/dev/bpf{i}")).expect("no NUL");
        // SAFETY: a plain open; the descriptor is owned at once.
        let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw < 0 {
            last = io::Error::last_os_error();
            if last.raw_os_error() == Some(libc::EBUSY) {
                continue;
            }
            if last.kind() == io::ErrorKind::NotFound {
                break;
            }
            continue;
        }
        // SAFETY: `raw` is a descriptor we just opened and nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut len = BPF_BUFFER;
        ioctl_u32(&fd, BIOCSBLEN, &mut len)?;
        // struct ifreq: a 16-byte name, then 16 bytes the request ignores.
        let mut ifr = [0u8; 32];
        if interface.len() >= 16 {
            return Err(invalid(format!("{interface:?} is not an interface name")));
        }
        ifr[..interface.len()].copy_from_slice(interface.as_bytes());
        // SAFETY: BIOCSETIF reads a 32-byte ifreq that lives through the call.
        if unsafe { libc::ioctl(fd.as_raw_fd(), BIOCSETIF, ifr.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        ioctl_u32(&fd, BIOCIMMEDIATE, &mut 1)?;
        ioctl_u32(&fd, BIOCSHDRCMPLT, &mut 1)?;
        ioctl_u32(&fd, BIOCSSEESENT, &mut 0)?;
        // SAFETY: plain fcntl calls on a descriptor we own.
        unsafe {
            let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        return Ok(fd);
    }
    Err(io::Error::other(format!("no free /dev/bpf device: {last}")))
}

/// Split one `read` of a BPF device into its frames.
fn bpf_frames(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + 18 <= buf.len() {
        let caplen =
            u32::from_ne_bytes(buf[off + BPF_CAPLEN..off + BPF_CAPLEN + 4].try_into().unwrap())
                as usize;
        let hdrlen =
            u16::from_ne_bytes(buf[off + BPF_HDRLEN..off + BPF_HDRLEN + 2].try_into().unwrap())
                as usize;
        let start = off + hdrlen;
        if hdrlen < 18 || start + caplen > buf.len() {
            break;
        }
        out.push(&buf[start..start + caplen]);
        // BPF_WORDALIGN: each record starts on a 4-byte boundary.
        off += (hdrlen + caplen + 3) & !3;
    }
    out
}

// ---------------------------------------------------------------------------
// The feth pair
// ---------------------------------------------------------------------------

/// The pair of interfaces, destroyed when this drops.
struct FethPair {
    /// The end that carries the room's address.
    near: String,
    /// The end the helper holds through BPF.
    far: String,
}

impl Drop for FethPair {
    fn drop(&mut self) {
        // Destroying either end takes the route and the address with it.
        for name in [&self.near, &self.far] {
            let _ = ifconfig(&[name.as_str(), "destroy"]);
        }
    }
}

/// Destroy any `feth` pair an earlier helper left behind, recognised by an
/// address in the room range (`198.18.0.0/15`, this project's alone).
fn reap_stale() {
    let Ok(list) = ifconfig(&["-l"]) else { return };
    for name in list.split_whitespace().filter(|n| is_clone_name(n, "feth")) {
        let Ok(info) = ifconfig(&[name]) else { continue };
        let ours = info.lines().any(|l| {
            l.trim_start()
                .strip_prefix("inet ")
                .and_then(|r| r.split_whitespace().next())
                .and_then(|a| a.parse::<Ipv4Addr>().ok())
                .is_some_and(|a| a.octets()[0] == 198 && (a.octets()[1] & 0xfe) == 18)
        });
        if !ours {
            continue;
        }
        let peer = info
            .lines()
            .find_map(|l| l.trim_start().strip_prefix("peer: "))
            .map(|p| p.trim().to_string());
        let _ = ifconfig(&[name, "destroy"]);
        if let Some(p) = peer.filter(|p| is_clone_name(p, "feth")) {
            let _ = ifconfig(&[p.as_str(), "destroy"]);
        }
    }
}

/// `ifconfig feth create` prints the name it made.
fn create_feth() -> io::Result<String> {
    let name = ifconfig(&["feth", "create"])?;
    if is_clone_name(&name, "feth") {
        Ok(name)
    } else {
        Err(io::Error::other(format!("ifconfig made {name:?}, not a feth interface")))
    }
}

/// The MAC `ifconfig` reports for `name`.
fn ether_of(name: &str) -> io::Result<[u8; 6]> {
    let info = ifconfig(&[name])?;
    let mac = info
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("ether "))
        .and_then(|r| r.split_whitespace().next())
        .ok_or_else(|| io::Error::other(format!("{name} has no ether address")))?;
    let mut out = [0u8; 6];
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        return Err(io::Error::other(format!("{mac:?} is not a MAC address")));
    }
    for (b, p) in out.iter_mut().zip(parts) {
        *b = u8::from_str_radix(p, 16).map_err(|_| io::Error::other("bad MAC digit"))?;
    }
    Ok(out)
}

/// The room's adapter as a `feth` pair.
pub struct Feth {
    bpf: AsyncFd<OwnedFd>,
    near_mac: [u8; 6],
    address: Ipv4Addr,
    subnet: RoomSubnet,
    /// Packets already unwrapped from a read, not yet handed to the pump.
    queue: Mutex<VecDeque<Vec<u8>>>,
    near: String,
    /// Destroys the interfaces last: after the descriptor is closed.
    _pair: FethPair,
}

impl Feth {
    /// Make the pair, give it the room's address and the limited-broadcast
    /// route, and attach BPF. Needs root. Inside a tokio runtime.
    pub fn create(config: &AdapterConfig) -> io::Result<Self> {
        config.validate()?;
        reap_stale();
        let near = create_feth()?;
        let far = match create_feth() {
            Ok(f) => f,
            Err(e) => {
                let _ = ifconfig(&[near.as_str(), "destroy"]);
                return Err(e);
            }
        };
        // From here the pair destroys itself if anything below fails.
        let pair = FethPair { near: near.clone(), far: far.clone() };
        let mask = netmask(config.subnet.prefix_len).to_string();
        let address = config.address.to_string();
        let mtu = LAN_MTU.to_string();
        ifconfig(&[near.as_str(), "peer", far.as_str()])?;
        ifconfig(&[
            near.as_str(),
            "inet",
            address.as_str(),
            "netmask",
            mask.as_str(),
            "mtu",
            mtu.as_str(),
            "up",
        ])?;
        ifconfig(&[far.as_str(), "mtu", mtu.as_str(), "up"])?;
        run(
            ROUTE,
            &[
                "-n".to_string(),
                "add".to_string(),
                "-host".to_string(),
                Ipv4Addr::BROADCAST.to_string(),
                "-interface".to_string(),
                near.clone(),
            ],
        )?;
        let near_mac = ether_of(&near)?;
        let bpf = open_bpf(&far)?;
        // Read events only: a BPF device cannot be registered for write
        // readiness (kqueue answers EINVAL), and a write to it never blocks.
        // SAFETY: an `OwnedFd` is open, owned by the `AsyncFd` from here, and
        // closed only when it is dropped.
        let bpf = unsafe { AsyncFd::register_with_interest(bpf, tokio::io::Interest::READABLE) }
            .map_err(|e| e.into_parts().1)?;
        Ok(Self {
            bpf,
            near_mac,
            address: config.address,
            subnet: config.subnet,
            queue: Mutex::new(VecDeque::new()),
            near,
            _pair: pair,
        })
    }

    /// The interface that carries the room's address (`feth0`).
    pub fn interface(&self) -> &str {
        &self.near
    }

    fn write_frame(&self, frame: &[u8]) -> io::Result<()> {
        // SAFETY: writing from a buffer we own, of its own length.
        let n = unsafe {
            libc::write(self.bpf.get_ref().as_raw_fd(), frame.as_ptr().cast(), frame.len())
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl PacketDevice for Feth {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut scratch = vec![0u8; BPF_BUFFER as usize];
        loop {
            if let Some(p) = self.queue.lock().expect("queue").pop_front() {
                let n = p.len().min(buf.len());
                buf[..n].copy_from_slice(&p[..n]);
                return Ok(n);
            }
            let mut guard = self.bpf.readable().await?;
            let read = guard.try_io(|fd| {
                // SAFETY: reading into a buffer we own, of the length given.
                let n = unsafe {
                    libc::read(fd.as_raw_fd(), scratch.as_mut_ptr().cast(), scratch.len())
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            let n = match read {
                Ok(r) => r?,
                Err(_would_block) => continue,
            };
            let mut queue = self.queue.lock().expect("queue");
            for frame in bpf_frames(&scratch[..n]) {
                match read_frame(frame, self.address, &self.subnet) {
                    Outgoing::Packet(p) => queue.push_back(p),
                    Outgoing::Reply(r) => {
                        // Not worth stopping the room for: the machine asks again.
                        let _ = self.write_frame(&r);
                    }
                    Outgoing::Ignore => {}
                }
            }
        }
    }

    async fn send(&self, packet: &[u8]) -> io::Result<()> {
        let Some(frame) = wrap_ipv4(packet, self.near_mac, &self.subnet) else {
            return Ok(());
        };
        // A BPF device takes a frame at once; a full queue is a dropped frame,
        // which an Ethernet wire may do too.
        match self.write_frame(&frame) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// The utun fallback
// ---------------------------------------------------------------------------

/// The kernel control a `utun` is made through.
const UTUN_CONTROL: &str = "com.apple.net.utun_control";
/// `getsockopt` option that returns the interface's name (`<net/if_utun.h>`).
const UTUN_OPT_IFNAME: libc::c_int = 2;
/// The address-family word in front of every packet, `AF_INET` in network order.
const AF_INET_WORD: [u8; 4] = (libc::AF_INET as u32).to_be_bytes();

/// A `utun`, for a macOS where a `feth` cannot be made. A `utun` goes with its
/// descriptor, so it cannot be left behind; it cannot carry a limited
/// broadcast either (see the module docs).
pub struct Utun {
    fd: AsyncFd<OwnedFd>,
    name: String,
}

impl Utun {
    /// Make a `utun`, give it the room's address and routes, and keep it open.
    pub fn create(config: &AdapterConfig) -> io::Result<Self> {
        config.validate()?;
        let (fd, name) = open_utun()?;
        for (program, args) in utun_commands(&name, config) {
            run(program, &args)?;
        }
        // SAFETY: an `OwnedFd` is open, owned by the `AsyncFd` from here.
        let fd = unsafe { AsyncFd::register(fd) }.map_err(|e| e.into_parts().1)?;
        Ok(Self { fd, name })
    }

    pub fn interface(&self) -> &str {
        &self.name
    }
}

impl PacketDevice for Utun {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            let result = guard.try_io(|fd| {
                let mut head = [0u8; 4];
                let iov = [
                    libc::iovec { iov_base: head.as_mut_ptr().cast(), iov_len: head.len() },
                    libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() },
                ];
                // SAFETY: two iovecs over buffers we own, for the lengths given.
                let n = unsafe { libc::readv(fd.as_raw_fd(), iov.as_ptr(), 2) };
                if n < 0 {
                    return Err(io::Error::last_os_error());
                }
                let n = n as usize;
                if n < head.len() || head != AF_INET_WORD {
                    return Ok(0); // not IPv4: nothing for a room
                }
                Ok(n - head.len())
            });
            match result {
                Ok(Ok(0)) => continue,
                Ok(other) => return other,
                Err(_would_block) => continue,
            }
        }
    }

    async fn send(&self, packet: &[u8]) -> io::Result<()> {
        loop {
            let mut guard = self.fd.writable().await?;
            let result = guard.try_io(|fd| {
                let iov = [
                    libc::iovec { iov_base: AF_INET_WORD.as_ptr() as *mut _, iov_len: 4 },
                    libc::iovec { iov_base: packet.as_ptr() as *mut _, iov_len: packet.len() },
                ];
                // SAFETY: two iovecs over buffers that outlive the call; the
                // kernel only reads from them.
                let n = unsafe { libc::writev(fd.as_raw_fd(), iov.as_ptr(), 2) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
            match result {
                Ok(r) => return r,
                Err(_would_block) => continue,
            }
        }
    }
}

/// Open a fresh `utun` through the kernel control and read back its name.
fn open_utun() -> io::Result<(OwnedFd, String)> {
    // SAFETY: plain socket creation; the descriptor is owned at once.
    let raw = unsafe { libc::socket(libc::PF_SYSTEM, libc::SOCK_DGRAM, libc::SYSPROTO_CONTROL) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a descriptor we just made and nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    // SAFETY: a zeroed ctl_info is valid (plain integers and a byte array).
    let mut info: libc::ctl_info = unsafe { std::mem::zeroed() };
    for (dst, src) in info.ctl_name.iter_mut().zip(UTUN_CONTROL.bytes()) {
        *dst = src as libc::c_char;
    }
    // SAFETY: CTLIOCGINFO fills the ctl_info we pass, which outlives the call.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::CTLIOCGINFO, &mut info) } < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: a zeroed sockaddr_ctl is valid; every field the kernel reads is
    // set below. Unit 0 asks for the next free `utunN`.
    let mut addr: libc::sockaddr_ctl = unsafe { std::mem::zeroed() };
    addr.sc_len = std::mem::size_of::<libc::sockaddr_ctl>() as u8;
    addr.sc_family = libc::AF_SYSTEM as u8;
    addr.ss_sysaddr = libc::AF_SYS_CONTROL as u16;
    addr.sc_id = info.ctl_id;
    addr.sc_unit = 0;
    // SAFETY: connect reads `sc_len` bytes from a sockaddr_ctl that lives
    // through the call.
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_ctl).cast(),
            std::mem::size_of::<libc::sockaddr_ctl>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut name = [0u8; libc::IFNAMSIZ];
    let mut len = name.len() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `name`.
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SYSPROTO_CONTROL,
            UTUN_OPT_IFNAME,
            name.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let end = name.iter().position(|&b| b == 0).unwrap_or(len as usize);
    let name = String::from_utf8_lossy(&name[..end]).into_owned();
    if !is_clone_name(&name, "utun") {
        return Err(invalid(format!("the kernel named the interface {name:?}, not utunN")));
    }

    // SAFETY: plain fcntl calls on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    Ok((fd, name))
}

/// The arguments that give a `utun` the room's address and subnet. Pure, so a
/// test can read them. No limited-broadcast route: a `utun` cannot carry one.
pub(crate) fn utun_commands(
    interface: &str,
    config: &AdapterConfig,
) -> Vec<(&'static str, Vec<String>)> {
    let address = config.address.to_string();
    let mask = netmask(config.subnet.prefix_len).to_string();
    vec![
        // A point-to-point interface needs a peer; the member's own address as
        // the peer adds only a host route to itself.
        (
            IFCONFIG,
            vec![
                interface.into(),
                "inet".into(),
                address.clone(),
                address,
                "netmask".into(),
                mask.clone(),
                "mtu".into(),
                LAN_MTU.to_string(),
                "up".into(),
            ],
        ),
        (
            ROUTE,
            vec![
                "-n".into(),
                "add".into(),
                "-net".into(),
                config.subnet.prefix.to_string(),
                "-netmask".into(),
                mask,
                "-interface".into(),
                interface.into(),
            ],
        ),
    ]
}

// ---------------------------------------------------------------------------
// One adapter, whichever was possible
// ---------------------------------------------------------------------------

/// The room's adapter on this Mac: a `feth` pair, or a `utun` where that could
/// not be made.
pub enum Device {
    Feth(Feth),
    Utun(Utun),
}

impl Device {
    /// Needs root. Inside a tokio runtime.
    pub fn create(config: &AdapterConfig) -> io::Result<Self> {
        match Feth::create(config) {
            Ok(f) => Ok(Self::Feth(f)),
            Err(e) => {
                eprintln!(
                    "lan-helper: a feth interface could not be made ({e}); using a utun, which \
                     carries unicast and the subnet's broadcast but not a limited broadcast to \
                     255.255.255.255 (older LAN games will not find each other)"
                );
                Ok(Self::Utun(Utun::create(config)?))
            }
        }
    }

    pub fn interface(&self) -> &str {
        match self {
            Self::Feth(f) => f.interface(),
            Self::Utun(u) => u.interface(),
        }
    }
}

impl PacketDevice for Device {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Feth(d) => d.recv(buf).await,
            Self::Utun(d) => d.recv(buf).await,
        }
    }

    async fn send(&self, packet: &[u8]) -> io::Result<()> {
        match self {
            Self::Feth(d) => d.send(packet).await,
            Self::Utun(d) => d.send(packet).await,
        }
    }
}

// ---------------------------------------------------------------------------
// The helper, and the launcher's side of it
// ---------------------------------------------------------------------------

/// Resolves on `SIGTERM`, `SIGINT` or `SIGHUP`: the helper then still tears the
/// pair down, instead of dying with it in place.
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
        _ = hup.recv() => {}
    }
}

/// `lan-helper serve`: make the adapter, connect out to the launcher that
/// started this, and relay until it goes away. The interfaces are destroyed
/// when this returns, however the launcher ended.
pub async fn serve(
    config: &AdapterConfig,
    launcher: std::net::SocketAddr,
    token: &RelayToken,
) -> io::Result<()> {
    let device = Device::create(config)?;
    let stream = lan_relay::connect_to_launcher(launcher, token).await?;
    tokio::select! {
        result = lan_relay::relay(&device, stream) => result,
        _ = shutdown_signal() => Ok(()),
    }
}

/// The adapter as the launcher holds it.
pub enum MacosAdapter {
    /// This process made it itself (it is already root): a test, or a
    /// launcher run with `sudo`.
    Direct(Device),
    /// `lan-helper serve` holds it and relays.
    Relayed(RemoteDevice),
}

impl PacketDevice for MacosAdapter {
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

/// A shell word for `s`: single-quoted, so a path with a space survives. The
/// helper's path is the bundle's own, but one with a quote, a backslash or a
/// control character is refused rather than escaped.
fn shell_word(s: &str) -> io::Result<String> {
    if s.chars().any(|c| matches!(c, '\'' | '"' | '\\') || c.is_control()) {
        return Err(invalid(format!("{s:?} cannot be passed through the administrator prompt")));
    }
    Ok(format!("'{s}'"))
}

/// The AppleScript that runs `command` as an administrator. `command` has no
/// double quote or backslash (`shell_word`'s rule; the arguments are names,
/// addresses, numbers and hex).
fn admin_script(command: &str) -> String {
    format!("do shell script \"{command}\" with administrator privileges")
}

fn command_line(helper: &Path, args: &[String]) -> io::Result<String> {
    let mut words = vec![shell_word(&helper.to_string_lossy())?];
    for a in args {
        words.push(shell_word(a)?);
    }
    Ok(words.join(" "))
}

/// Start the helper as root: directly when this process already is, else
/// through macOS's own administrator prompt — the one moment a person is
/// asked, and the only elevated code is the helper. The helper is left running
/// in the background (`&`): it lives for the whole room and exits by itself
/// when the relay drops.
fn start_helper(helper: &Path, args: &[String]) -> io::Result<()> {
    if is_elevated() {
        let mut child = std::process::Command::new(helper).args(args).spawn()?;
        // Reaped on its own thread: the helper lives for the whole room.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        return Ok(());
    }
    let command = format!("{} >/dev/null 2>&1 &", command_line(helper, args)?);
    let out = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(admin_script(&command))
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    Err(admin_error(&out.stderr))
}

/// `osascript`'s error, with a declined prompt (error -128) said plainly.
fn admin_error(stderr: &[u8]) -> io::Error {
    let said = String::from_utf8_lossy(stderr);
    if said.contains("-128") || said.contains("User canceled") {
        io::Error::new(io::ErrorKind::PermissionDenied, "the administrator prompt was declined")
    } else {
        io::Error::other(format!("the administrator prompt failed: {}", said.trim()))
    }
}

/// Run the helper once as root, through the system prompt, and wait for it:
/// the launcher's "Fix". `in_place` is Linux's distinction; here the helper
/// always runs where it is.
pub fn run_helper_elevated(helper: &Path, args: &[String], _in_place: bool) -> io::Result<String> {
    if is_elevated() {
        let out = std::process::Command::new(helper).args(args).output()?;
        return if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
        };
    }
    let out = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(admin_script(&command_line(helper, args)?))
        .output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(admin_error(&out.stderr))
    }
}

pub(super) async fn open(
    setup: &AdapterSetup,
    config: &AdapterConfig,
) -> anyhow::Result<MacosAdapter> {
    match setup {
        AdapterSetup::InProcess => {
            if !is_elevated() {
                anyhow::bail!("making a room adapter in this process needs root");
            }
            Ok(MacosAdapter::Direct(Device::create(config)?))
        }
        AdapterSetup::Helper { path, portable: _ } => {
            let helper = path.clone();
            let remote =
                super::relayed(config, Vec::new(), move |args| start_helper(&helper, &args))
                    .await?;
            Ok(MacosAdapter::Relayed(remote))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AdapterConfig {
        AdapterConfig::from_cidr("gbl0", "198.19.3.4/16").unwrap()
    }

    fn subnet() -> RoomSubnet {
        config().subnet
    }

    #[test]
    fn a_prefix_length_is_a_dotted_mask() {
        assert_eq!(netmask(16), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(netmask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(netmask(15), Ipv4Addr::new(255, 254, 0, 0));
        assert_eq!(netmask(0), Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn only_names_the_system_returned_are_passed_back() {
        assert!(is_clone_name("feth0", "feth"));
        assert!(is_clone_name("feth12", "feth"));
        assert!(!is_clone_name("feth", "feth"));
        assert!(!is_clone_name("feth0; rm", "feth"));
        assert!(!is_clone_name("en0", "feth"));
        assert!(!is_clone_name("utun1", "feth"));
        assert!(is_clone_name("utun7", "utun"));
    }

    fn udp(dst: Ipv4Addr) -> Vec<u8> {
        let mut p = vec![0u8; 20 + 8 + 4];
        let len = p.len() as u16;
        p[0] = 0x45;
        p[2..4].copy_from_slice(&len.to_be_bytes());
        p[9] = 17;
        p[12..16].copy_from_slice(&[198, 19, 3, 4]);
        p[16..20].copy_from_slice(&dst.octets());
        p
    }

    /// A room packet reaches the machine in a frame: unicast to the
    /// interface's own MAC, every kind of broadcast to the broadcast MAC.
    #[test]
    fn a_room_packet_is_framed_to_the_machine_and_a_broadcast_to_everyone() {
        let own = [0x66, 0x65, 0x74, 0x68, 0, 0];
        let uni = wrap_ipv4(&udp(Ipv4Addr::new(198, 19, 3, 4)), own, &subnet()).unwrap();
        assert_eq!(&uni[..6], &own);
        assert_eq!(&uni[6..12], &PEER_MAC);
        assert_eq!(&uni[12..14], &ETH_IPV4);
        assert_eq!(uni.len(), 14 + 32);
        for dst in [
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(198, 19, 255, 255),
            Ipv4Addr::new(239, 255, 255, 250),
        ] {
            let f = wrap_ipv4(&udp(dst), own, &subnet()).unwrap();
            assert_eq!(&f[..6], &BROADCAST_MAC, "{dst}");
        }
        assert!(wrap_ipv4(&[0x60, 0, 0], own, &subnet()).is_none(), "not IPv4");
        assert!(wrap_ipv4(&[], own, &subnet()).is_none());
    }

    #[test]
    fn a_frame_from_the_machine_is_unwrapped_to_its_packet_and_padding_goes() {
        let packet = udp(Ipv4Addr::BROADCAST);
        let mut frame = vec![0xff; 6];
        frame.extend_from_slice(&[0x66, 0x65, 0x74, 0x68, 0, 0]);
        frame.extend_from_slice(&ETH_IPV4);
        frame.extend_from_slice(&packet);
        frame.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // Ethernet padding
        let own = Ipv4Addr::new(198, 19, 3, 4);
        assert_eq!(read_frame(&frame, own, &subnet()), Outgoing::Packet(packet));
        // A length that lies is dropped, never read past.
        let mut bad = frame.clone();
        bad[ETH_HEADER + 2..ETH_HEADER + 4].copy_from_slice(&9000u16.to_be_bytes());
        assert_eq!(read_frame(&bad, own, &subnet()), Outgoing::Ignore);
        // IGMP is the kernel's chatter, IPv6 is not a room's.
        let mut igmp = frame.clone();
        igmp[ETH_HEADER + 9] = 2;
        assert_eq!(read_frame(&igmp, own, &subnet()), Outgoing::Ignore);
        let mut v6 = frame.clone();
        v6[12..14].copy_from_slice(&[0x86, 0xdd]);
        assert_eq!(read_frame(&v6, own, &subnet()), Outgoing::Ignore);
        assert_eq!(read_frame(&[0u8; 5], own, &subnet()), Outgoing::Ignore);
    }

    fn arp_request(sha: [u8; 6], spa: [u8; 4], tpa: [u8; 4]) -> Vec<u8> {
        let mut f = vec![0xff; 6];
        f.extend_from_slice(&sha);
        f.extend_from_slice(&ETH_ARP);
        f.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
        f.extend_from_slice(&sha);
        f.extend_from_slice(&spa);
        f.extend_from_slice(&[0; 6]);
        f.extend_from_slice(&tpa);
        f
    }

    /// The kernel will not send a unicast until something answers its ARP, and
    /// the whole room is "something": every room address but its own.
    #[test]
    fn the_room_answers_arp_for_every_member_but_never_for_this_machine() {
        let own = Ipv4Addr::new(198, 19, 3, 4);
        let mac = [0x66, 0x65, 0x74, 0x68, 0, 0];
        let ask = arp_request(mac, [198, 19, 3, 4], [198, 19, 0, 9]);
        let Outgoing::Reply(r) = read_frame(&ask, own, &subnet()) else { panic!("no reply") };
        assert_eq!(&r[..6], &mac, "to the asker");
        assert_eq!(&r[6..12], &PEER_MAC);
        assert_eq!(&r[12..14], &ETH_ARP);
        assert_eq!(&r[14..22], &[0, 1, 8, 0, 6, 4, 0, 2], "a reply");
        assert_eq!(&r[22..28], &PEER_MAC, "the room's hardware address…");
        assert_eq!(&r[28..32], &[198, 19, 0, 9], "…for the address asked about");
        assert_eq!(&r[32..38], &mac);
        assert_eq!(&r[38..42], &[198, 19, 3, 4]);
        for (spa, tpa) in [
            ([198, 19, 3, 4], [198, 19, 3, 4]),  // this machine's own address
            ([0, 0, 0, 0], [198, 19, 0, 9]),     // an address probe
            ([198, 19, 3, 4], [192, 168, 1, 1]), // outside the room
            ([198, 19, 3, 4], [198, 19, 255, 255]), // the broadcast is nobody's
        ] {
            let a = arp_request(mac, spa, tpa);
            assert_eq!(read_frame(&a, own, &subnet()), Outgoing::Ignore, "{spa:?} asks {tpa:?}");
        }
    }

    #[test]
    fn a_bpf_read_is_split_into_its_frames() {
        // Two records, each a 18-byte header padded to 20, the frame padded to 4.
        fn record(frame: &[u8]) -> Vec<u8> {
            let mut r = vec![0u8; 20];
            r[8..12].copy_from_slice(&(frame.len() as u32).to_ne_bytes());
            r[12..16].copy_from_slice(&(frame.len() as u32).to_ne_bytes());
            r[16..18].copy_from_slice(&20u16.to_ne_bytes());
            r.extend_from_slice(frame);
            while !r.len().is_multiple_of(4) {
                r.push(0);
            }
            r
        }
        let mut buf = record(&[1, 2, 3, 4, 5]);
        buf.extend(record(&[9, 8, 7]));
        assert_eq!(bpf_frames(&buf), vec![&[1u8, 2, 3, 4, 5][..], &[9, 8, 7][..]]);
        // A record that claims more than was read is dropped, not read past.
        let mut short = record(&[1, 2, 3, 4, 5]);
        short.truncate(23);
        assert!(bpf_frames(&short).is_empty());
    }

    #[test]
    fn a_utun_gets_the_room_subnet_and_nothing_else() {
        let cmds = utun_commands("utun7", &config());
        assert_eq!(cmds.len(), 2);
        assert!(cmds.iter().all(|(p, _)| p.starts_with('/')), "absolute paths only");
        let joined: Vec<String> =
            cmds.iter().map(|(p, a)| format!("{p} {}", a.join(" "))).collect();
        assert_eq!(
            joined[0],
            "/sbin/ifconfig utun7 inet 198.19.3.4 198.19.3.4 netmask 255.255.0.0 mtu 1400 up"
        );
        assert_eq!(
            joined[1],
            "/sbin/route -n add -net 198.19.0.0 -netmask 255.255.0.0 -interface utun7"
        );
        assert!(!joined.iter().any(|c| c.contains("default") || c.contains("0.0.0.0/0")));
    }

    #[test]
    fn the_prompt_command_refuses_what_a_script_could_not_carry() {
        let ok = command_line(
            Path::new("/Applications/Lanthorn Hosted.app/Contents/MacOS/lan-helper"),
            &["serve".into(), "gbl0".into(), "198.19.3.4/16".into()],
        )
        .unwrap();
        assert_eq!(
            ok,
            "'/Applications/Lanthorn Hosted.app/Contents/MacOS/lan-helper' 'serve' 'gbl0' \
             '198.19.3.4/16'"
        );
        for bad in ["/tmp/x'y", "/tmp/x\"y", "/tmp/x\\y", "/tmp/x\ny"] {
            assert!(command_line(Path::new(bad), &[]).is_err(), "{bad:?}");
        }
        assert_eq!(
            admin_script("'/a b/h' 'serve'"),
            "do shell script \"'/a b/h' 'serve'\" with administrator privileges"
        );
    }

    #[test]
    fn a_declined_prompt_is_said_plainly() {
        let e = admin_error(b"execution error: User canceled. (-128)");
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(admin_error(b"something else").to_string().contains("something else"));
    }
}
