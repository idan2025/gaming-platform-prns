//! Mode 3, step 2: two real adapters, two network namespaces, one room
//! (`PLAN.md` §14.3).
//!
//! The room's host and one member each get a TUN adapter inside their own
//! network namespace, so a packet between them cannot take a shortcut through
//! a shared routing table — it has to leave one namespace's adapter, cross the
//! room, and come out of the other's. What crosses is ordinary UDP from
//! ordinary sockets: a limited broadcast to `255.255.255.255`, the kind an old
//! LAN game sends to find a server, and the unicast reply it gets back.
//!
//! **No root needed.** The test re-runs itself under `unshare -rn`, inside a
//! user namespace where it holds `CAP_NET_ADMIN` over network namespaces it
//! creates. Where unprivileged user namespaces are disabled (some distros,
//! and Ubuntu's AppArmor default), it says so and skips.

#![cfg(target_os = "linux")]

use std::net::{Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Duration;

use game_bridge::lan::RoomSubnet;
use game_bridge::lan_adapter::{self, AdapterConfig, TunDevice};
use game_bridge::lan_filter::{LanPolicy, LanPort, LanProto};
use game_bridge::lan_pump;
use game_bridge::lan_session::{LanHostArgs, LanMemberArgs, LanSession};
use game_bridge::profile::GameProfile;
use tokio::net::UdpSocket;

mod common;

const INSIDE: &str = "GAME_BRIDGE_LAN_ADAPTER_TEST_INSIDE";
/// The stand-in game's port: declared, so the room carries it.
const GAME_PORT: u16 = 27960;
/// A port the game did not declare, with a listener on it.
const PRIVATE_PORT: u16 = 6000;

#[test]
fn a_broadcast_and_its_reply_cross_two_real_adapters() {
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
    p.id = "lan-adapter-test".to_string();
    p.app_name = "lan-adapter-test".to_string();
    p.query = None;
    p
}

/// In a fresh network namespace of its own: an adapter for `address`, and
/// sockets on the game's port and on a port the game did not declare. Returned
/// to the caller, they stay in that namespace whichever thread uses them.
fn namespace_with_adapter(
    address: Ipv4Addr,
    subnet: RoomSubnet,
) -> (OwnedFd, StdUdpSocket, StdUdpSocket) {
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
        let game = StdUdpSocket::bind(("0.0.0.0", GAME_PORT)).unwrap();
        game.set_broadcast(true).unwrap();
        game.set_nonblocking(true).unwrap();
        let private = StdUdpSocket::bind(("0.0.0.0", PRIVATE_PORT)).unwrap();
        private.set_nonblocking(true).unwrap();
        (fd, game, private)
    })
    .join()
    .unwrap()
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn recv(sock: &UdpSocket, within: Duration) -> Option<(Vec<u8>, SocketAddr)> {
    let mut buf = vec![0u8; 2048];
    let (n, from) = tokio::time::timeout(within, sock.recv_from(&mut buf)).await.ok()?.ok()?;
    Some((buf[..n].to_vec(), from))
}

#[test]
#[ignore = "runs inside `unshare -rn`, started by a_broadcast_and_its_reply_cross_two_real_adapters"]
fn inside_a_user_namespace() {
    if std::env::var_os(INSIDE).is_none() {
        return;
    }
    // The mesh between host and member runs over loopback in this namespace,
    // which starts with `lo` down.
    lan_adapter::bring_up("lo").expect("lo comes up");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let dir = common::scratch_dir("lan-adapter");
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
            ports: vec![LanPort { proto: LanProto::Udp, port: GAME_PORT }],
            any: false,
        };
        let mut sockets = Vec::new();
        for (session, addr) in [(host.clone(), h_addr), (member.clone(), m_addr)] {
            let (fd, game, private) = namespace_with_adapter(addr, subnet);
            let device = Arc::new(TunDevice::from_fd(fd).unwrap());
            tokio::spawn(lan_pump::pump(device, session, policy.clone()));
            sockets
                .push((UdpSocket::from_std(game).unwrap(), UdpSocket::from_std(private).unwrap()));
        }
        let (host_game, _host_private) = &sockets[0];
        let (member_game, member_private) = &sockets[1];

        // An old LAN game looking for a server: a limited broadcast from the
        // member's namespace, heard by a plain socket in the host's.
        let mut heard = None;
        for _ in 0..20 {
            member_game
                .send_to(b"anyone hosting?", (Ipv4Addr::BROADCAST, GAME_PORT))
                .await
                .unwrap();
            if let Some(got) = recv(host_game, Duration::from_millis(500)).await {
                heard = Some(got);
                break;
            }
        }
        let (bytes, from) = heard.expect("the host's namespace heard the member's broadcast");
        assert_eq!(bytes, b"anyone hosting?");
        assert_eq!(
            from,
            SocketAddr::from((m_addr, GAME_PORT)),
            "and saw it come from the member's room address"
        );

        // The server answers the address it heard from.
        host_game.send_to(b"me!", from).await.unwrap();
        // Linux loops a limited broadcast back to the sender's own sockets, so
        // the member's own question may be waiting ahead of the answer.
        let (bytes, from) = loop {
            let got =
                recv(member_game, Duration::from_secs(5)).await.expect("the reply crosses back");
            if got.1.ip() != std::net::IpAddr::V4(m_addr) {
                break got;
            }
        };
        assert_eq!(bytes, b"me!");
        assert_eq!(from, SocketAddr::from((h_addr, GAME_PORT)));

        // A port the game never declared is not reachable, even though
        // something on the member's machine is listening there.
        host_game.send_to(b"knock knock", (m_addr, PRIVATE_PORT)).await.unwrap();
        assert!(
            recv(member_private, Duration::from_secs(2)).await.is_none(),
            "a member reached a port the game did not declare"
        );
    });
}
