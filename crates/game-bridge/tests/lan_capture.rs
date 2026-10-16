//! Linux: what the room adapter's routes cannot catch, caught
//! (`lan_adapter/linux_capture.rs`).
//!
//! In a network namespace of its own: the room adapter, and a veth standing
//! in for the Wi-Fi with an address and a default route. Each kind of game
//! traffic that leaves by the Wi-Fi whatever the room's routes say is sent
//! *before* the capture starts — and must not reach the room, or this test
//! proves nothing — and again after, when it must:
//!
//! - a limited broadcast from a socket bound to the Wi-Fi's address;
//! - a broadcast to the Wi-Fi's own network from a socket on `0.0.0.0`;
//! - a unicast to a room member from a socket pinned to the Wi-Fi the way
//!   Wine pins one (`SO_BINDTODEVICE`);
//!
//! and the other way, a packet the pump marked for a pinned socket reaches it
//! — and an unmarked one does not. The survey must also find the pinned
//! socket for what it is.
//!
//! **No root needed**: it re-runs itself under `unshare -rn`, like
//! `tests/lan_adapter.rs`, and skips where user namespaces are off.

#![cfg(target_os = "linux")]

use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use game_bridge::lan::RoomSubnet;
use game_bridge::lan_adapter::{self, AdapterConfig, Capture};
use game_bridge::lan_filter::{LanPort, LanProto};
use game_bridge::lan_rebind;

const INSIDE: &str = "GAME_BRIDGE_LAN_CAPTURE_TEST_INSIDE";
const ROOM: Ipv4Addr = Ipv4Addr::new(198, 19, 0, 5);
const PEER: Ipv4Addr = Ipv4Addr::new(198, 19, 0, 7);
const WIFI: Ipv4Addr = Ipv4Addr::new(10, 9, 0, 1);
const WIFI_BROADCAST: Ipv4Addr = Ipv4Addr::new(10, 9, 0, 255);

#[test]
fn what_leaves_by_the_wifi_is_brought_to_the_room() {
    if std::env::var_os(INSIDE).is_some() {
        return;
    }
    let probe = std::process::Command::new("unshare").args(["-rn", "true"]).output();
    if !probe.as_ref().is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: unprivileged user namespaces are not available here ({probe:?})");
        return;
    }
    if !std::process::Command::new("ip").arg("-V").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: no `ip` to make the stand-in Wi-Fi with");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new("unshare")
        .arg("-rn")
        .arg(exe)
        .args(["--exact", "inside_a_user_namespace", "--ignored", "--nocapture", "--test-threads=1"])
        .env(INSIDE, "1")
        .output()
        .expect("unshare runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the inner run failed\n--- stdout\n{stdout}\n--- stderr\n{stderr}");
    assert!(stdout.contains("1 passed"), "the inner run did not run\n{stdout}");
}

fn ip(args: &str) {
    let out = std::process::Command::new("ip").args(args.split(' ')).output().unwrap();
    assert!(out.status.success(), "ip {args}: {}", String::from_utf8_lossy(&out.stderr));
}

fn tc_rules(dev: &str, hook: &str) -> String {
    let out = std::process::Command::new("tc")
        .args(["filter", "show", "dev", dev, hook])
        .output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

/// The next UDP packet to `port` the room adapter hands the pump, within
/// `wait`.
fn read_room(fd: &OwnedFd, port: u16, wait: Duration) -> Option<Vec<u8>> {
    let until = Instant::now() + wait;
    let mut buf = [0u8; 2048];
    while Instant::now() < until {
        // SAFETY: reading into a buffer we own; the descriptor is non-blocking.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let p = &buf[..n as usize];
        if p.len() >= 28 && p[9] == 17 && u16::from_be_bytes([p[22], p[23]]) == port {
            return Some(p.to_vec());
        }
    }
    None
}

fn endpoints(p: &[u8]) -> (Ipv4Addr, Ipv4Addr) {
    game_bridge::lan::ipv4_endpoints(p).unwrap()
}

fn pinned_socket(port: u16) -> UdpSocket {
    // SAFETY: plain socket calls on a descriptor owned by the UdpSocket.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        assert!(fd >= 0);
        let dev = b"veth0";
        assert_eq!(
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_BINDTODEVICE, dev.as_ptr().cast(), 5),
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        let sin = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr { s_addr: 0 },
            sin_zero: [0; 8],
        };
        assert_eq!(
            libc::bind(
                fd,
                (&sin as *const libc::sockaddr_in).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
            ),
            0
        );
        use std::os::fd::FromRawFd;
        UdpSocket::from_raw_fd(fd)
    }
}

/// An IPv4 UDP packet with no UDP checksum, its header checksum right.
fn udp(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16, tos: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0u8; 28];
    p[0] = 0x45;
    p[1] = tos;
    p[2..4].copy_from_slice(&((28 + payload.len()) as u16).to_be_bytes());
    p[8] = 64;
    p[9] = 17;
    p[12..16].copy_from_slice(&src.octets());
    p[16..20].copy_from_slice(&dst.octets());
    let mut acc: u32 = 0;
    for c in p[..20].chunks(2) {
        acc += u32::from(u16::from_be_bytes([c[0], c[1]]));
    }
    while acc >> 16 != 0 {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    p[10..12].copy_from_slice(&(!(acc as u16)).to_be_bytes());
    p[20..22].copy_from_slice(&sport.to_be_bytes());
    p[22..24].copy_from_slice(&dport.to_be_bytes());
    p[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    p.extend_from_slice(payload);
    p
}

fn write_room(fd: &OwnedFd, packet: &[u8]) {
    // SAFETY: writing a buffer we own, of its own length.
    let n = unsafe { libc::write(fd.as_raw_fd(), packet.as_ptr().cast(), packet.len()) };
    assert_eq!(n as usize, packet.len(), "{}", std::io::Error::last_os_error());
}

#[test]
#[ignore = "runs only inside the user namespace the outer test makes"]
fn inside_a_user_namespace() {
    ip("link set lo up");
    ip("link add veth0 type veth peer name veth1");
    ip("link set veth1 up");
    ip("link set veth0 up");
    ip("addr add 10.9.0.1/24 dev veth0");
    // A gateway that needs no answer: the neighbour is fixed.
    ip("neigh add 10.9.0.254 lladdr 02:00:00:00:00:01 dev veth0 nud permanent");
    ip("route add default via 10.9.0.254 dev veth0");

    let subnet = RoomSubnet::new(Ipv4Addr::new(198, 19, 0, 0), 16).unwrap();
    let config = AdapterConfig { name: "gbl0".into(), address: ROOM, subnet };
    let room = lan_adapter::open_configured(&config).expect("the room adapter");
    let short = Duration::from_millis(400);
    let long = Duration::from_secs(3);

    let bound = UdpSocket::bind((WIFI, 0)).unwrap();
    bound.set_broadcast(true).unwrap();
    let anywhere = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    anywhere.set_broadcast(true).unwrap();
    let pinned = pinned_socket(0);
    pinned.set_broadcast(true).unwrap();

    let send_all = |port_base: u16| {
        bound.send_to(b"limited", (Ipv4Addr::BROADCAST, port_base)).unwrap();
        anywhere.send_to(b"directed", (WIFI_BROADCAST, port_base + 1)).unwrap();
        pinned.send_to(b"unicast", (PEER, port_base + 2)).unwrap();
    };

    // Before: all three leave by the "Wi-Fi", and the room sees none.
    send_all(41000);
    for port in [41000, 41001, 41002] {
        assert!(
            read_room(&room, port, short).is_none(),
            "port {port} reached the room without the capture: this test would prove nothing"
        );
    }

    let capture = Capture::start("gbl0").expect("the capture starts");
    assert!(tc_rules("veth0", "egress").contains("mirred"), "{}", tc_rules("veth0", "egress"));

    send_all(42000);
    let p = read_room(&room, 42000, long).expect("the bound socket's limited broadcast");
    assert_eq!(endpoints(&p), (WIFI, Ipv4Addr::BROADCAST));
    let p = read_room(&room, 42001, long).expect("the Wi-Fi network's own broadcast");
    assert_eq!(endpoints(&p), (WIFI, WIFI_BROADCAST));
    let p = read_room(&room, 42002, long).expect("the pinned socket's unicast to a member");
    assert_eq!(endpoints(&p), (WIFI, PEER));

    // The way in: a packet the pump marked for the pinned socket reaches it,
    // arriving as if on the Wi-Fi; one without the mark stays on the room
    // adapter, where a pinned socket never sees it.
    let game = pinned_socket(43000);
    game.set_read_timeout(Some(long)).unwrap();
    let mut buf = [0u8; 64];
    write_room(&room, &udp(PEER, WIFI, 5000, 43000, 0, b"unmarked"));
    game.set_read_timeout(Some(short)).unwrap();
    assert!(game.recv_from(&mut buf).is_err(), "an unmarked packet must not be handed over");
    game.set_read_timeout(Some(long)).unwrap();
    let tos = lan_rebind::PINNED_DSCP << 2;
    write_room(&room, &udp(PEER, WIFI, 5000, 43000, tos, b"marked"));
    let (n, from) = game.recv_from(&mut buf).expect("the marked packet reaches the pinned socket");
    assert_eq!(&buf[..n], b"marked");
    assert_eq!(from, (PEER, 5000).into());

    // The survey sees the pinned socket for what it is, and where.
    let port = LanPort { proto: LanProto::Udp, port: 43000 };
    let s = lan_rebind::survey_on(ROOM, &[port], Some("gbl0"));
    assert_eq!(s.rebound, [lan_rebind::Rebound { port, address: WIFI, pinned: true }]);
    assert!(s.broadcasts.contains(&WIFI_BROADCAST), "{:?}", s.broadcasts);
    // A socket on every address, unpinned, needs nothing.
    let open = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    let port = LanPort { proto: LanProto::Udp, port: open.local_addr().unwrap().port() };
    assert!(lan_rebind::survey_on(ROOM, &[port], Some("gbl0")).rebound.is_empty());
    // One bound to the Wi-Fi's address alone is translated, but not pinned.
    let only = UdpSocket::bind((WIFI, 0)).unwrap();
    let port = LanPort { proto: LanProto::Udp, port: only.local_addr().unwrap().port() };
    assert_eq!(
        lan_rebind::survey_on(ROOM, &[port], Some("gbl0")).rebound,
        [lan_rebind::Rebound { port, address: WIFI, pinned: false }]
    );

    // Gone with the room, and nothing of another program's touched.
    drop(capture);
    assert!(!tc_rules("veth0", "egress").contains("mirred"), "{}", tc_rules("veth0", "egress"));
    drop(room);
}
