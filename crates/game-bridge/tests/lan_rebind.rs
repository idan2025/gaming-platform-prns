//! Mode 3: a game that listens on the Wi-Fi's address, not the room's, is
//! still joinable through the room (`lan_rebind.rs`).
//!
//! What Need for Speed: Underground 2 does under Proton: its discovery socket
//! is on every address, so the race shows up, and its game socket is on the
//! machine's Wi-Fi address alone, so a join to the room address was refused.
//! Here the host's namespace gets a second address standing in for the Wi-Fi,
//! the stand-in game binds its TCP and UDP ports there and only there, and a
//! member in another namespace joins it through real adapters at the host's
//! room address — and sees every answer come from that room address.
//!
//! Same harness as `lan_adapter.rs`: re-runs itself under `unshare -rn`, no
//! root needed, skips where unprivileged user namespaces are off.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use game_bridge::lan::RoomSubnet;
use game_bridge::lan_adapter::{self, AdapterConfig, TunDevice};
use game_bridge::lan_filter::{LanPolicy, LanPort, LanProto};
use game_bridge::lan_session::{LanHostArgs, LanMemberArgs, LanSession};
use game_bridge::profile::GameProfile;
use game_bridge::{lan_pump, lan_rebind};

mod common;

const INSIDE: &str = "GAME_BRIDGE_LAN_REBIND_TEST_INSIDE";
/// The stand-in game's ports: declared, so the room carries them.
const GAME_TCP: u16 = 9900;
const GAME_UDP: u16 = 3658;
/// A port the game did not declare, listened on at the same Wi-Fi address.
const PRIVATE_TCP: u16 = 6001;
/// The host's stand-in Wi-Fi address.
const WIFI: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);

#[test]
fn a_game_listening_on_the_wifi_address_is_joined_at_the_room_address() {
    if std::env::var_os(INSIDE).is_some() {
        return; // The inner run is `inside_a_user_namespace`.
    }
    let probe = std::process::Command::new("unshare").args(["-rn", "true"]).output();
    if !probe.as_ref().is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: unprivileged user namespaces are not available here ({probe:?})");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new("unshare")
        .arg("-rn")
        .arg(exe)
        .args([
            "--exact",
            "inside_a_user_namespace",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(INSIDE, "1")
        .output()
        .expect("unshare runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the inner run failed\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    assert!(stdout.contains("1 passed"), "the inner run did not run\n{stdout}");
}

fn profile() -> GameProfile {
    let mut p = GameProfile::sven_coop();
    p.id = "lan-rebind-test".to_string();
    p.app_name = "lan-rebind-test".to_string();
    p.query = None;
    p
}

/// A network namespace of its own with a room adapter for `address`; returns
/// the adapter and the namespace, so later work can enter it ([`in_ns`]).
fn namespace_with_adapter(address: Ipv4Addr, subnet: RoomSubnet) -> (OwnedFd, OwnedFd) {
    std::thread::spawn(move || {
        // SAFETY: moves only this thread into a new network namespace.
        assert_eq!(
            unsafe { libc::unshare(libc::CLONE_NEWNET) },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        let config = AdapterConfig { name: "gbl0".to_string(), address, subnet };
        let fd = lan_adapter::open_configured(&config).expect("the adapter is created");
        let ns = std::fs::File::open("/proc/thread-self/ns/net").expect("this namespace");
        (fd, OwnedFd::from(ns))
    })
    .join()
    .unwrap()
}

/// Run `f` on a thread inside namespace `ns`.
fn in_ns<T: Send + 'static>(ns: &OwnedFd, f: impl FnOnce() -> T + Send + 'static) -> T {
    let ns = ns.try_clone().unwrap();
    std::thread::spawn(move || {
        // SAFETY: moves only this thread, into a namespace this process made.
        assert_eq!(unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) }, 0);
        f()
    })
    .join()
    .unwrap()
}

/// Give the namespace's loopback a second, ordinary address, as a machine's
/// Wi-Fi has: `lo:7`, so `127.0.0.1` stays.
fn add_wifi_address() {
    // SAFETY: plain ioctls on a socket this function owns, with an ifreq
    // naming an alias of `lo`.
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        assert!(sock >= 0);
        for (request, addr) in
            [(libc::SIOCSIFADDR, WIFI), (libc::SIOCSIFNETMASK, Ipv4Addr::BROADCAST)]
        {
            let mut req: libc::ifreq = std::mem::zeroed();
            for (d, s) in req.ifr_name.iter_mut().zip(b"lo:7\0") {
                *d = *s as libc::c_char;
            }
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: 0,
                sin_addr: libc::in_addr { s_addr: u32::from(addr).to_be() },
                sin_zero: [0; 8],
            };
            req.ifr_ifru.ifru_addr = std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin);
            assert_eq!(
                libc::ioctl(sock, request, &mut req),
                0,
                "{}",
                std::io::Error::last_os_error()
            );
        }
        libc::close(sock);
    }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[test]
#[ignore = "runs inside `unshare -rn`, started by a_game_listening_on_the_wifi_address_is_joined_at_the_room_address"]
fn inside_a_user_namespace() {
    if std::env::var_os(INSIDE).is_none() {
        return;
    }
    lan_adapter::bring_up("lo").expect("lo comes up");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let dir = common::scratch_dir("lan-rebind");
        let mesh_port = common::free_tcp_port();

        let mut host_args = LanHostArgs::new(profile());
        host_args.identity = dir.join("host.identity");
        host_args.tcp = Some(format!("0.0.0.0:{mesh_port}"));
        host_args.announce_interval = 1;
        let host = Arc::new(LanSession::host(host_args).await.unwrap());

        let mut member_args = LanMemberArgs::new(profile());
        member_args.identity = dir.join("member.identity");
        member_args.tcp = Some(format!("127.0.0.1:{mesh_port}"));
        member_args.room_hash =
            Some(host.room_hash().unwrap().as_bytes().iter().map(|b| format!("{b:02x}")).collect());
        let member = Arc::new(LanSession::join(member_args).await.unwrap());

        wait_until("the member is seated", || member.own_address().is_some()).await;
        let subnet = host.subnet();
        let (h_addr, m_addr) = (host.own_address().unwrap(), member.own_address().unwrap());

        let policy = LanPolicy {
            ports: vec![
                LanPort { proto: LanProto::Tcp, port: GAME_TCP },
                LanPort { proto: LanProto::Udp, port: GAME_UDP },
            ],
            any: false,
        };
        let (host_fd, host_ns) = namespace_with_adapter(h_addr, subnet);
        let (member_fd, member_ns) = namespace_with_adapter(m_addr, subnet);

        // The host's game, the way Wine binds it: the Wi-Fi address only.
        let (game_tcp, game_udp, private) = in_ns(&host_ns, || {
            lan_adapter::bring_up("lo").unwrap();
            add_wifi_address();
            let tcp = TcpListener::bind((WIFI, GAME_TCP)).unwrap();
            let udp = UdpSocket::bind((WIFI, GAME_UDP)).unwrap();
            let private = TcpListener::bind((WIFI, PRIVATE_TCP)).unwrap();
            (tcp, udp, private)
        });
        for (fd, ns, session) in [
            (host_fd, host_ns.try_clone().unwrap(), host.clone()),
            (member_fd, member_ns.try_clone().unwrap(), member.clone()),
        ] {
            let device = Arc::new(TunDevice::from_fd(fd).unwrap());
            // The pump runs out here; the game and its addresses are in the
            // adapter's namespace, so that is where it must look.
            let survey = move |room: Ipv4Addr, ports: &[LanPort]| {
                let ports = ports.to_vec();
                in_ns(&ns, move || lan_rebind::survey(room, &ports))
            };
            tokio::spawn(lan_pump::pump_surveying(device, session, policy.clone(), survey));
        }
        wait_until("the host's pump finds the game on the Wi-Fi address", || {
            host.rebound().len() == 2
        })
        .await;
        assert!(host.rebound().iter().all(|r| r.address == WIFI), "{:?}", host.rebound());
        assert!(member.rebound().is_empty(), "nothing to translate on the member");

        // The game: answer one TCP join and one UDP datagram, and say who
        // they came from.
        let game = std::thread::spawn(move || {
            let (mut conn, from_tcp) = game_tcp.accept().unwrap();
            let mut buf = [0u8; 64];
            let n = conn.read(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"join");
            conn.write_all(b"welcome").unwrap();
            let (n, from_udp) = game_udp.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"ping");
            game_udp.send_to(b"pong", from_udp).unwrap();
            (from_tcp, from_udp)
        });

        // The member joins the room address, as every game client does.
        let (welcome, tcp_peer, pong_from) = in_ns(&member_ns, move || {
            let mut conn = TcpStream::connect_timeout(
                &SocketAddr::from((h_addr, GAME_TCP)),
                Duration::from_secs(10),
            )
            .expect("the join to the room address is answered, not refused");
            let tcp_peer = conn.peer_addr().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            conn.write_all(b"join").unwrap();
            let mut buf = [0u8; 64];
            let n = conn.read(&mut buf).unwrap();
            let welcome = buf[..n].to_vec();

            let udp = UdpSocket::bind((m_addr, 0)).unwrap();
            udp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut pong = None;
            for _ in 0..5 {
                udp.send_to(b"ping", (h_addr, GAME_UDP)).unwrap();
                if let Ok((n, from)) = udp.recv_from(&mut buf) {
                    assert_eq!(&buf[..n], b"pong");
                    pong = Some(from);
                    break;
                }
            }
            (welcome, tcp_peer, pong.expect("the UDP answer crosses back"))
        });
        assert_eq!(welcome, b"welcome");
        assert_eq!(tcp_peer, SocketAddr::from((h_addr, GAME_TCP)));
        assert_eq!(
            pong_from,
            SocketAddr::from((h_addr, GAME_UDP)),
            "the answer came from the host's room address, never its Wi-Fi's"
        );
        let (from_tcp, from_udp) = game.join().unwrap();
        assert_eq!(from_tcp.ip(), m_addr, "the game sees the member's room address");
        assert_eq!(from_udp.ip(), m_addr);

        // The translation is for the game's ports only: a listener on the
        // same Wi-Fi address, on a port the game did not declare, stays out of
        // the room.
        let reached = in_ns(&member_ns, move || {
            TcpStream::connect_timeout(
                &SocketAddr::from((h_addr, PRIVATE_TCP)),
                Duration::from_secs(3),
            )
            .is_ok()
        });
        assert!(!reached, "a member reached an undeclared port on the host's Wi-Fi address");
        drop(private);
    });
}
