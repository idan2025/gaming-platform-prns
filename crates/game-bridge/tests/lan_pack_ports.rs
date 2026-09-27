//! Mode 3: every shipped `[lan]` pack's ports cross a room, and the room check
//! tells a working room from a broken one (`PLAN.md` §14.3, step 7).
//!
//! `tests/lan_openttd.rs` plays one game for real. Most of the LAN back
//! catalogue is commercial Windows software CI cannot run, so a pack for one is
//! a list of ports somebody captured — and this is what proves the room
//! carries *that list*, in the shape an old LAN game uses it:
//!
//! - a broadcast **from** a declared UDP port **to** `255.255.255.255` on it,
//!   heard by every other member with the sender's room address as its source
//!   (Need for Speed: Underground 2's search is exactly this, 9999 → 9999);
//! - an unsolicited unicast to that port, both ways (NFSU2's 3658/3659);
//! - a TCP connection to every declared TCP port, from every member to every
//!   other one — including host → client, which NFSU2's host does to each
//!   client on 3282–3285;
//! - an undeclared TCP port still shut.
//!
//! Then the room check (`lan_check.rs`) runs from every member and must pass
//! with the game's own sockets bound to its ports — and must not hand those
//! sockets its probe — and it must name two faults it exists to find: a
//! limited broadcast the machine sent out of another network, and a member
//! whose adapter is not up.
//!
//! It finds its packs by property (a `[lan]` block), never by id, so a new
//! LAN pack is covered the moment it lands.
//!
//! **No root needed**: like `lan_adapter.rs`, it re-runs itself under
//! `unshare -rn` and skips where unprivileged user namespaces are off.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use game_bridge::lan_adapter::{self, AdapterConfig, TunDevice};
use game_bridge::lan_check;
use game_bridge::lan_filter::LanProto;
use game_bridge::lan_pump;
use game_bridge::lan_session::{LanHostArgs, LanMemberArgs, LanSession};
use game_bridge::pack::GamePack;
use tokio::net::UdpSocket;

mod common;

const INSIDE: &str = "GAME_BRIDGE_LAN_PACK_PORTS_TEST_INSIDE";
/// Declared by no pack; something listens there on every member.
const PRIVATE_TCP: u16 = 6001;

#[test]
fn every_lan_packs_ports_cross_a_room_and_the_room_check_finds_faults() {
    if std::env::var_os(INSIDE).is_some() {
        return;
    }
    let probe = std::process::Command::new("unshare").args(["-rn", "true"]).output();
    if !probe.as_ref().is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: unprivileged user namespaces are not available here ({probe:?})");
        return;
    }
    let out = std::process::Command::new("unshare")
        .arg("-rn")
        .arg(std::env::current_exe().unwrap())
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

/// A network namespace with one room adapter in it. Work is done inside it by
/// [`Ns::run`], on a thread that joins it — sockets made there stay there.
struct Ns {
    name: &'static str,
    address: Ipv4Addr,
    netns: std::fs::File,
}

impl Ns {
    fn new(
        name: &'static str,
        address: Ipv4Addr,
        subnet: game_bridge::lan::RoomSubnet,
    ) -> (Self, OwnedFd) {
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
            let netns = std::fs::File::open("/proc/thread-self/ns/net").unwrap();
            (Self { name, address, netns }, fd)
        })
        .join()
        .unwrap()
    }

    fn run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> T {
        let fd = self.netns.try_clone().unwrap();
        std::thread::spawn(move || {
            // SAFETY: moves only this fresh thread into the namespace.
            assert_eq!(unsafe { libc::setns(fd.as_raw_fd(), libc::CLONE_NEWNET) }, 0);
            f()
        })
        .join()
        .unwrap()
    }

    fn udp(&self, port: u16) -> UdpSocket {
        let s = self.run(move || {
            let s = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).unwrap();
            s.set_broadcast(true).unwrap();
            s.set_nonblocking(true).unwrap();
            s
        });
        UdpSocket::from_std(s).unwrap()
    }

    fn tcp_listener(&self, port: u16) -> TcpListener {
        self.run(move || TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).unwrap())
    }

    fn ip(&self, args: &'static [&'static str]) {
        let out = self
            .run(move || std::process::Command::new("ip").args(args).output().expect("ip runs"));
        assert!(out.status.success(), "ip {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

async fn recv(sock: &UdpSocket, within: Duration) -> Option<(Vec<u8>, SocketAddr)> {
    let mut buf = vec![0u8; 2048];
    let (n, from) = tokio::time::timeout(within, sock.recv_from(&mut buf)).await.ok()?.ok()?;
    Some((buf[..n].to_vec(), from))
}

/// The next datagram on `sock` that did not come from `own` — Linux loops a
/// limited broadcast back to the sender's own sockets.
async fn recv_from_others(
    sock: &UdpSocket,
    own: Ipv4Addr,
    within: Duration,
) -> Option<(Vec<u8>, SocketAddr)> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let got = recv(sock, left).await?;
        if got.1.ip() != std::net::IpAddr::V4(own) {
            return Some(got);
        }
    }
}

async fn drain(sock: &UdpSocket) {
    while recv(sock, Duration::from_millis(50)).await.is_some() {}
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn hex(hash: prns_core::wire::DestinationHash) -> String {
    hash.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
#[ignore = "runs inside `unshare -rn`, started by every_lan_packs_ports_cross_a_room_and_the_room_check_finds_faults"]
fn inside_a_user_namespace() {
    if std::env::var_os(INSIDE).is_none() {
        return;
    }
    lan_adapter::bring_up("lo").expect("lo comes up");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
    let packs: Vec<GamePack> = GamePack::load_dir(&dir)
        .expect("the shipped pack directory reads")
        .packs
        .into_iter()
        .filter(|p| p.lan.is_some())
        .collect();
    assert!(packs.len() >= 2, "every shipped [lan] pack is found by property");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    for pack in packs {
        eprintln!("--- {}", pack.id);
        rt.block_on(one_pack(pack));
    }
}

async fn one_pack(pack: GamePack) {
    let policy = pack.lan.as_ref().unwrap().policy();
    let profile = pack.to_profile().unwrap();
    let scratch = common::scratch_dir(&format!("lan-pack-ports-{}", pack.id));
    let mesh_port = common::free_tcp_port();

    let mut host_args = LanHostArgs::new(profile.clone());
    host_args.identity = scratch.join("host.identity");
    host_args.tcp = Some(format!("0.0.0.0:{mesh_port}"));
    host_args.announce_interval = 1;
    let host = Arc::new(LanSession::host(host_args).await.unwrap());
    let room = hex(host.room_hash().unwrap());
    let join = |who: &str| {
        let mut a = LanMemberArgs::new(profile.clone());
        a.identity = scratch.join(format!("{who}.identity"));
        a.tcp = Some(format!("127.0.0.1:{mesh_port}"));
        a.room_hash = Some(room.clone());
        a
    };
    let m1 = Arc::new(LanSession::join(join("m1")).await.unwrap());
    let m2 = Arc::new(LanSession::join(join("m2")).await.unwrap());
    wait_until("both members are seated and know each other", || {
        [&m1, &m2].iter().all(|m| m.view().members.len() == 3)
    })
    .await;

    let subnet = host.subnet();
    let mut nss = Vec::new();
    for (name, session) in [("host", &host), ("m1", &m1), ("m2", &m2)] {
        let (ns, fd) = Ns::new(name, session.own_address().unwrap(), subnet);
        let device = Arc::new(TunDevice::from_fd(fd).unwrap());
        tokio::spawn(lan_pump::pump(device, session.clone(), policy.clone()));
        nss.push(ns);
    }
    let sessions = [host.clone(), m1.clone(), m2.clone()];

    let udp_ports: Vec<u16> =
        policy.ports.iter().filter(|p| p.proto == LanProto::Udp).map(|p| p.port).collect();
    let tcp_ports: Vec<u16> =
        policy.ports.iter().filter(|p| p.proto == LanProto::Tcp).map(|p| p.port).collect();

    // --- UDP: the game's search, and its answer, on every declared port.
    for &port in &udp_ports {
        let socks: Vec<UdpSocket> = nss.iter().map(|ns| ns.udp(port)).collect();
        for s in 0..nss.len() {
            let sender = &nss[s];
            let payload = vec![0x5a; 384]; // NFSU2's search is 384 bytes
            let mut heard = vec![false; nss.len()];
            heard[s] = true;
            for _ in 0..10 {
                socks[s].send_to(&payload, (Ipv4Addr::BROADCAST, port)).await.unwrap();
                for r in 0..nss.len() {
                    if heard[r] {
                        continue;
                    }
                    if let Some((bytes, from)) =
                        recv_from_others(&socks[r], nss[r].address, Duration::from_millis(300))
                            .await
                    {
                        assert_eq!(
                            bytes, payload,
                            "{}: {} heard a broadcast intact",
                            pack.id, nss[r].name
                        );
                        assert_eq!(
                            from,
                            SocketAddr::from((sender.address, port)),
                            "{}: {} saw the broadcast from {}'s room address and port",
                            pack.id,
                            nss[r].name,
                            sender.name
                        );
                        heard[r] = true;
                    }
                }
                if heard.iter().all(|h| *h) {
                    break;
                }
            }
            assert!(
                heard.iter().all(|h| *h),
                "{}: UDP {port} broadcast from {} reached {heard:?}",
                pack.id,
                sender.name
            );
            // Every hearer answers the address it heard, from the same port.
            for r in (0..nss.len()).filter(|&r| r != s) {
                socks[r].send_to(b"answer", (sender.address, port)).await.unwrap();
                let (bytes, from) =
                    recv_from_others(&socks[s], sender.address, Duration::from_secs(5))
                        .await
                        .unwrap_or_else(|| {
                            panic!(
                                "{}: {}'s UDP {port} answer reached {}",
                                pack.id, nss[r].name, sender.name
                            )
                        });
                assert_eq!(
                    (bytes.as_slice(), from),
                    (&b"answer"[..], SocketAddr::from((nss[r].address, port)))
                );
            }
            for s in &socks {
                drain(s).await;
            }
        }
    }

    // --- TCP: every declared port, from every member to every other one.
    for &port in &tcp_ports {
        let listeners: Vec<TcpListener> = nss.iter().map(|ns| ns.tcp_listener(port)).collect();
        for (to, listener) in listeners.into_iter().enumerate() {
            let to_addr = nss[to].address;
            let expected = nss.len() - 1;
            let accept = std::thread::spawn(move || {
                let mut seen = Vec::new();
                for _ in 0..expected {
                    let (mut conn, peer) = listener.accept().unwrap();
                    let mut buf = [0u8; 5];
                    conn.read_exact(&mut buf).unwrap();
                    conn.write_all(&buf).unwrap();
                    seen.push(peer.ip());
                }
                seen
            });
            for from in (0..nss.len()).filter(|&f| f != to) {
                let (from_name, to_name, id) = (nss[from].name, nss[to].name, pack.id.clone());
                nss[from].run(move || {
                    let target = SocketAddr::V4(SocketAddrV4::new(to_addr, port));
                    let mut c = TcpStream::connect_timeout(&target, Duration::from_secs(10))
                        .unwrap_or_else(|e| {
                            panic!("{id}: {from_name} → {to_name} TCP {port}: {e}")
                        });
                    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                    c.write_all(b"hello").unwrap();
                    let mut buf = [0u8; 5];
                    c.read_exact(&mut buf).unwrap();
                    assert_eq!(&buf, b"hello");
                });
            }
            let seen = accept.join().unwrap();
            assert_eq!(seen.len(), expected, "{}: TCP {port} on {}", pack.id, nss[to].name);
        }
    }

    // --- An undeclared port stays shut, even with a listener behind it.
    let _private = nss[1].tcp_listener(PRIVATE_TCP);
    let m1_addr = nss[1].address;
    let reached = nss[0].run(move || {
        TcpStream::connect_timeout(
            &SocketAddr::from((m1_addr, PRIVATE_TCP)),
            Duration::from_secs(2),
        )
        .is_ok()
    });
    assert!(!reached, "{}: the host reached an undeclared TCP port on a member", pack.id);

    // --- The room check passes from every member, with the game's sockets
    // bound to its own ports, and never hands them its probe.
    let port = lan_check::probe_port(&policy).expect("a [lan] pack with UDP has a probe port");
    let games: Vec<UdpSocket> = nss.iter().map(|ns| ns.udp(port)).collect();
    for (i, ns) in nss.iter().enumerate() {
        let checker = ns.udp(0);
        let report =
            lan_check::check_room_with(&sessions[i], port, checker, Duration::from_secs(3))
                .await
                .unwrap();
        assert!(
            report.ok(),
            "{}: the check from {} failed: {report:?}\n{}",
            pack.id,
            ns.name,
            report.findings().join("\n")
        );
        assert_eq!(report.members.len(), 2);
        for (j, game) in games.iter().enumerate().filter(|(j, _)| *j != i) {
            assert!(
                recv_from_others(game, nss[j].address, Duration::from_millis(200)).await.is_none(),
                "{}: the game on {} was handed a room check's probe",
                pack.id,
                nss[j].name
            );
        }
        drain(&games[i]).await; // its own limited broadcast, looped back
    }

    // --- Fault 1: this machine sends 255.255.255.255 out of another network —
    // Windows' metric problem, made here by pointing the route at `lo`.
    nss[2].ip(&["route", "del", "255.255.255.255", "dev", "gbl0"]);
    nss[2].ip(&["link", "set", "lo", "up"]);
    nss[2].ip(&["route", "add", "255.255.255.255", "dev", "lo"]);
    let report =
        lan_check::check_room_with(&m2, port, nss[2].udp(0), Duration::from_secs(2)).await.unwrap();
    assert!(!report.ok());
    assert!(!report.limited_broadcast_left, "{report:?}");
    assert!(report.subnet_broadcast_left, "the subnet broadcast still takes the room: {report:?}");
    assert!(
        report.members.iter().all(|m| m.answered_unicast && !m.heard_limited_broadcast),
        "{report:?}"
    );
    let findings = report.findings().join("\n");
    assert!(findings.contains("another network"), "{findings}");
    assert!(!findings.contains("answered nothing"), "the members are not at fault:\n{findings}");

    // --- Fault 2: a member is seated but its adapter never came up.
    let m3 = Arc::new(LanSession::join(join("m3")).await.unwrap());
    wait_until("m1 sees the fourth member", || m1.view().members.len() == 4).await;
    let m3_addr = m3.own_address().unwrap();
    let report =
        lan_check::check_room_with(&m1, port, nss[1].udp(0), Duration::from_secs(2)).await.unwrap();
    assert!(!report.ok());
    let silent: Vec<_> = report.members.iter().filter(|m| !m.ok()).map(|m| m.address).collect();
    assert_eq!(silent, vec![m3_addr], "only the member without an adapter fails: {report:?}");
    let findings = report.findings().join("\n");
    assert!(findings.contains(&format!("{m3_addr} answered nothing")), "{findings}");
    eprintln!("{}: every port crossed; the check passed and named both faults", pack.id);
}
