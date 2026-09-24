//! Mode 3, step 4: the Windows adapter, end to end (`PLAN.md` §14.3).
//!
//! The room's host puts the room on a real Wintun adapter the way the launcher
//! will: `run_room_on_adapter` starts the real `lan-helper serve`, which creates
//! the adapter as an administrator and relays it back over the token-checked
//! loopback link (`lan_relay.rs`). A plain Windows UDP socket then does what an
//! old LAN game does — broadcasts to `255.255.255.255` — and the room's other
//! member, which has no adapter and reads the room directly, must receive it.
//!
//! That arrival is the metric fix, proven: a GitHub runner has an Ethernet
//! adapter, and without the room adapter's interface metric and host route a
//! limited broadcast leaves by Ethernet and the room never sees it.
//!
//! Windows has no network namespaces, so the other member cannot have an
//! adapter of its own on the same machine — packets between two local
//! addresses never touch an adapter. It speaks to the room in raw IPv4, which
//! is what an adapter would hand it anyway.
//!
//! Needs an administrator and `wintun.dll`: CI's `lan-windows` job fetches it,
//! digest-checked, and sets `GAME_BRIDGE_WINTUN_DLL`. Without that it skips.

#![cfg(windows)]

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use game_bridge::lan_adapter::{is_elevated, run_room_on_adapter, AdapterSetup, WINTUN_DLL_ENV};
use game_bridge::lan_filter::{LanPolicy, LanPort, LanProto};
use game_bridge::lan_session::{LanHostArgs, LanMemberArgs, LanSession};
use game_bridge::profile::GameProfile;

mod common;

const GAME_PORT: u16 = 27960;
const PRIVATE_PORT: u16 = 6000;

fn profile() -> GameProfile {
    let mut p = GameProfile::sven_coop();
    p.id = "lan-wintun-test".to_string();
    p.app_name = "lan-wintun-test".to_string();
    p.query = None;
    p
}

/// An IPv4/UDP packet as an adapter would carry it. The UDP checksum is left
/// zero, which IPv4 defines as "none".
fn udp_packet(src: SocketAddr, dst: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let (SocketAddr::V4(src), SocketAddr::V4(dst)) = (src, dst) else { unreachable!() };
    let total = 20 + 8 + payload.len();
    let mut p = vec![0u8; total];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    p[8] = 64;
    p[9] = 17;
    p[12..16].copy_from_slice(&src.ip().octets());
    p[16..20].copy_from_slice(&dst.ip().octets());
    let sum = p[..20].chunks(2).map(|w| u16::from_be_bytes([w[0], w[1]]) as u32).sum::<u32>();
    let sum = (sum & 0xffff) + (sum >> 16);
    let sum = !((sum & 0xffff) + (sum >> 16)) as u16;
    p[10..12].copy_from_slice(&sum.to_be_bytes());
    p[20..22].copy_from_slice(&src.port().to_be_bytes());
    p[22..24].copy_from_slice(&dst.port().to_be_bytes());
    p[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    p[28..].copy_from_slice(payload);
    p
}

/// `(source, destination, payload)` of an IPv4/UDP packet.
fn parse_udp(p: &[u8]) -> Option<(SocketAddr, SocketAddr, &[u8])> {
    let ihl = (p.first()? & 0x0f) as usize * 4;
    if p.len() < ihl + 8 || p[9] != 17 {
        return None;
    }
    let ip = |at: usize| Ipv4Addr::new(p[at], p[at + 1], p[at + 2], p[at + 3]);
    let port = |at: usize| u16::from_be_bytes([p[at], p[at + 1]]);
    Some((
        SocketAddr::from((ip(12), port(ihl))),
        SocketAddr::from((ip(16), port(ihl + 2))),
        &p[ihl + 8..],
    ))
}

async fn wait_until(what: &str, within: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Whether Wintun's driver package is in Windows' driver store.
///
/// *Installed*, not *loaded*: Windows unloads the driver the moment no Wintun
/// adapter exists, while the package stays in the store — which is exactly
/// what portable mode must remove. Asking whether it was loaded (the first
/// version of this check) read "gone" after every room, portable or not.
fn wintun_installed() -> bool {
    let out =
        std::process::Command::new("pnputil").arg("/enum-drivers").output().expect("pnputil runs");
    String::from_utf8_lossy(&out.stdout).to_ascii_lowercase().contains("wintun.inf")
}

/// Wait until the Wintun driver package is installed (or not).
async fn wait_driver(want_installed: bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let installed = wintun_installed();
        if installed == want_installed {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the Wintun driver installed is {installed}, wanted {want_installed}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Whether this machine holds `addr` — i.e. whether the adapter carries it.
fn holds(addr: Ipv4Addr) -> bool {
    UdpSocket::bind((addr, 0)).is_ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_windows_broadcast_leaves_through_the_room_adapter_and_its_reply_comes_back() {
    if std::env::var_os(WINTUN_DLL_ENV).is_none() {
        eprintln!("skipping: set {WINTUN_DLL_ENV} to wintun.dll (CI's lan-windows job does)");
        return;
    }
    assert!(is_elevated(), "the Wintun test needs an administrator");

    let dir = common::scratch_dir("lan-wintun");
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
    let member = LanSession::join(member_args).await.unwrap();
    wait_until("the member is seated", Duration::from_secs(60), || member.own_address().is_some())
        .await;
    let (h_addr, m_addr) = (host.own_address().unwrap(), member.own_address().unwrap());

    // The room on a real adapter, through the real helper.
    let policy =
        LanPolicy { ports: vec![LanPort { proto: LanProto::Udp, port: GAME_PORT }], any: false };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let started = Instant::now();
    let runner = tokio::spawn(run_room_on_adapter(
        host.clone(),
        policy,
        "gbl0".to_string(),
        AdapterSetup::Helper { path: env!("CARGO_BIN_EXE_lan-helper").into(), portable: false },
        async move {
            let _ = stop_rx.await;
        },
    ));
    wait_until("the adapter holds the host's room address", Duration::from_secs(60), || {
        holds(h_addr)
    })
    .await;
    let adapter_up_after = started.elapsed();

    let game = UdpSocket::bind(("0.0.0.0", GAME_PORT)).unwrap();
    game.set_broadcast(true).unwrap();
    game.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let private = UdpSocket::bind(("0.0.0.0", PRIVATE_PORT)).unwrap();
    private.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

    // An old game looking for a server. Retried: a fresh adapter's address
    // may still be settling when the first one goes out.
    let mut heard = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    'search: while Instant::now() < deadline {
        game.send_to(b"anyone hosting?", (Ipv4Addr::BROADCAST, GAME_PORT)).unwrap();
        let window = Instant::now() + Duration::from_millis(500);
        while let Ok(Some(p)) =
            tokio::time::timeout(window.saturating_duration_since(Instant::now()), member.recv())
                .await
        {
            if let Some((src, dst, b"anyone hosting?")) = parse_udp(&p) {
                heard = Some((src, dst));
                break 'search;
            }
        }
    }
    let (src, dst) = heard.expect(
        "a limited broadcast from Windows never reached the room: it left by another adapter (the metric fix)",
    );
    assert_eq!(src, SocketAddr::from((h_addr, GAME_PORT)), "it came from the host's room address");
    assert_eq!(dst.ip(), std::net::IpAddr::V4(Ipv4Addr::BROADCAST));

    // The reply, from the other member, reaches the Windows socket.
    let reply = udp_packet((m_addr, GAME_PORT).into(), (h_addr, GAME_PORT).into(), b"me!");
    // Windows may also loop the game's own broadcasts back to its socket, so
    // read past anything that is not the reply.
    game.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let mut got = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    'reply: while Instant::now() < deadline {
        member.send(reply.clone()).unwrap();
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = game.recv_from(&mut buf) {
            if &buf[..n] == b"me!" {
                got = Some(from);
                break 'reply;
            }
        }
    }
    assert_eq!(
        got,
        Some(SocketAddr::from((m_addr, GAME_PORT))),
        "the reply reached the game's socket"
    );

    // A port the game never declared stays shut, even with a listener on it.
    member
        .send(udp_packet((m_addr, GAME_PORT).into(), (h_addr, PRIVATE_PORT).into(), b"knock"))
        .unwrap();
    let mut buf = [0u8; 64];
    assert!(
        private.recv_from(&mut buf).is_err(),
        "a member reached a port the game did not declare"
    );

    // Leaving takes the adapter away: the helper sees the link drop.
    stop_tx.send(()).unwrap();
    runner.await.unwrap().unwrap();
    wait_until("the adapter is gone", Duration::from_secs(30), || !holds(h_addr)).await;

    // An installed launcher keeps the driver: the next room starts faster, and
    // that is the trade it makes. This is also what makes the portable check
    // below mean something — the driver is there to be removed.
    wait_driver(true).await;

    // Portable mode leaves nothing: the helper removes the driver again once
    // its adapter is gone.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let runner = tokio::spawn(run_room_on_adapter(
        host.clone(),
        LanPolicy { ports: vec![LanPort { proto: LanProto::Udp, port: GAME_PORT }], any: false },
        "gbl0".to_string(),
        AdapterSetup::Helper { path: env!("CARGO_BIN_EXE_lan-helper").into(), portable: true },
        async move {
            let _ = stop_rx.await;
        },
    ));
    wait_until("the portable adapter comes up", Duration::from_secs(60), || holds(h_addr)).await;
    stop_tx.send(()).unwrap();
    runner.await.unwrap().unwrap();
    wait_until("the portable adapter is gone", Duration::from_secs(30), || !holds(h_addr)).await;
    wait_driver(false).await;

    eprintln!("lan_wintun: the adapter was up {adapter_up_after:?} after the runner started");
}
