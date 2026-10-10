//! Mode 3 on Windows: a game listening on the machine's Ethernet address, not
//! the room's, is still joined at the room address (`lan_rebind.rs`).
//!
//! The same claim as `lan_rebind.rs`, on the system that needs more for it:
//! Windows is strong-host, so the pump's translation reaches the game only
//! because the room adapter is weak-host (`lan_adapter/windows.rs`), and the
//! game's answer leaves through the room adapter only for the same reason.
//! A plain Windows TCP listener and UDP socket bound to the runner's own
//! Ethernet address stand in for the game; the room's other member speaks raw
//! IPv4, as in `lan_wintun.rs`, and must get the answers from the host's room
//! address.
//!
//! Needs an administrator and `wintun.dll` (CI's `lan-windows` job). Without
//! them it skips.

#![cfg(windows)]

use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use game_bridge::lan_adapter::{is_elevated, run_room_on_adapter, AdapterSetup, WINTUN_DLL_ENV};
use game_bridge::lan_filter::{LanPolicy, LanPort, LanProto};
use game_bridge::lan_session::{LanHostArgs, LanMemberArgs, LanSession};
use game_bridge::profile::GameProfile;

mod common;

const GAME_TCP: u16 = 9900;
const GAME_UDP: u16 = 3658;
const MEMBER_PORT: u16 = 51000;

fn profile() -> GameProfile {
    let mut p = GameProfile::sven_coop();
    p.id = "lan-rebind-windows-test".to_string();
    p.app_name = "lan-rebind-windows-test".to_string();
    p.query = None;
    p
}

fn sum(bytes: &[u8]) -> u16 {
    let mut acc: u32 = 0;
    for c in bytes.chunks(2) {
        acc += u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]));
    }
    while acc >> 16 != 0 {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    !(acc as u16)
}

/// An IPv4 packet carrying `transport` (whose checksum, at `csum_at`, is
/// filled in over the pseudo-header).
fn packet(
    src: Ipv4Addr,
    dst: Ipv4Addr,
    protocol: u8,
    mut transport: Vec<u8>,
    csum_at: usize,
) -> Vec<u8> {
    let mut covered = src.octets().to_vec();
    covered.extend_from_slice(&dst.octets());
    covered.extend_from_slice(&[0, protocol]);
    covered.extend_from_slice(&(transport.len() as u16).to_be_bytes());
    covered.extend_from_slice(&transport);
    let s = sum(&covered);
    transport[csum_at..csum_at + 2].copy_from_slice(&s.to_be_bytes());
    let mut p = vec![0u8; 20];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&((20 + transport.len()) as u16).to_be_bytes());
    p[8] = 64;
    p[9] = protocol;
    p[12..16].copy_from_slice(&src.octets());
    p[16..20].copy_from_slice(&dst.octets());
    let s = sum(&p);
    p[10..12].copy_from_slice(&s.to_be_bytes());
    p.extend_from_slice(&transport);
    p
}

fn tcp_syn(src: SocketAddr, dst: SocketAddr) -> Vec<u8> {
    let (SocketAddr::V4(src), SocketAddr::V4(dst)) = (src, dst) else { unreachable!() };
    let mut t = vec![0u8; 20];
    t[0..2].copy_from_slice(&src.port().to_be_bytes());
    t[2..4].copy_from_slice(&dst.port().to_be_bytes());
    t[4..8].copy_from_slice(&0x1000_0000u32.to_be_bytes());
    t[12] = 0x50;
    t[13] = 0x02; // SYN
    t[14..16].copy_from_slice(&64240u16.to_be_bytes());
    packet(*src.ip(), *dst.ip(), 6, t, 16)
}

fn udp(src: SocketAddr, dst: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let (SocketAddr::V4(src), SocketAddr::V4(dst)) = (src, dst) else { unreachable!() };
    let mut t = vec![0u8; 8];
    t[0..2].copy_from_slice(&src.port().to_be_bytes());
    t[2..4].copy_from_slice(&dst.port().to_be_bytes());
    t[4..6].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    t.extend_from_slice(payload);
    packet(*src.ip(), *dst.ip(), 17, t, 6)
}

/// `(protocol, source, destination, TCP flags or 0, payload)`.
fn parse(p: &[u8]) -> Option<(u8, SocketAddr, SocketAddr, u8, &[u8])> {
    let ihl = (p.first()? & 0x0f) as usize * 4;
    if p.len() < ihl + 8 {
        return None;
    }
    let ip = |at: usize| Ipv4Addr::new(p[at], p[at + 1], p[at + 2], p[at + 3]);
    let port = |at: usize| u16::from_be_bytes([p[at], p[at + 1]]);
    let (flags, payload) = match p[9] {
        6 if p.len() >= ihl + 20 => (p[ihl + 13], &p[ihl + (p[ihl + 12] >> 4) as usize * 4..]),
        17 => (0, &p[ihl + 8..]),
        _ => return None,
    };
    Some((p[9], (ip(12), port(ihl)).into(), (ip(16), port(ihl + 2)).into(), flags, payload))
}

async fn wait_until(what: &str, within: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn holds(addr: Ipv4Addr) -> bool {
    UdpSocket::bind((addr, 0)).is_ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_game_on_the_ethernet_address_is_joined_at_the_room_address_on_windows() {
    if std::env::var_os(WINTUN_DLL_ENV).is_none() {
        eprintln!("skipping: set {WINTUN_DLL_ENV} to wintun.dll (CI's lan-windows job does)");
        return;
    }
    assert!(is_elevated(), "the Wintun test needs an administrator");

    let dir = common::scratch_dir("lan-rebind-windows");
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

    let policy = LanPolicy {
        ports: vec![
            LanPort { proto: LanProto::Tcp, port: GAME_TCP },
            LanPort { proto: LanProto::Udp, port: GAME_UDP },
        ],
        any: false,
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
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

    // The game, bound the way Wine binds one — or a native game with a fixed
    // address set: the runner's own Ethernet address, not the room's.
    let ethernet = game_bridge::lan_rebind::local_addresses()
        .into_iter()
        .find(|a| !a.is_loopback() && !a.is_link_local() && *a != h_addr && a.octets()[0] != 198)
        .expect("the runner has an Ethernet address");
    let game_tcp = TcpListener::bind((ethernet, GAME_TCP)).unwrap();
    let game_udp = UdpSocket::bind((ethernet, GAME_UDP)).unwrap();
    game_udp.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    println!("game on {ethernet}, room address {h_addr}");

    wait_until("the pump finds the game on the Ethernet address", Duration::from_secs(30), || {
        host.rebound().len() == 2
    })
    .await;
    assert!(host.rebound().iter().all(|r| r.address == ethernet), "{:?}", host.rebound());

    // A member's join to the room address: a SYN in, a SYN-ACK back from the
    // room address. Without weak host on the room adapter Windows drops the
    // translated SYN, or sends the answer out of the Ethernet adapter.
    let syn = tcp_syn((m_addr, MEMBER_PORT).into(), (h_addr, GAME_TCP).into());
    let mut syn_ack = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    'join: while Instant::now() < deadline {
        member.send(syn.clone()).unwrap();
        let window = Instant::now() + Duration::from_millis(500);
        while let Ok(Some(p)) =
            tokio::time::timeout(window.saturating_duration_since(Instant::now()), member.recv())
                .await
        {
            if let Some((6, src, dst, flags, _)) = parse(&p) {
                if dst == SocketAddr::from((m_addr, MEMBER_PORT)) {
                    syn_ack = Some((src, flags));
                    break 'join;
                }
            }
        }
    }
    let (src, flags) = syn_ack.expect("the join to the room address was never answered");
    assert_eq!(src, SocketAddr::from((h_addr, GAME_TCP)), "answered from the room address");
    assert_eq!(flags & 0x12, 0x12, "a SYN-ACK, not a reset (flags {flags:#04x})");

    // UDP, both ways.
    let ping = udp((m_addr, MEMBER_PORT).into(), (h_addr, GAME_UDP).into(), b"ping");
    let mut from = None;
    let mut buf = [0u8; 64];
    let deadline = Instant::now() + Duration::from_secs(20);
    while from.is_none() && Instant::now() < deadline {
        member.send(ping.clone()).unwrap();
        if let Ok((n, f)) = game_udp.recv_from(&mut buf) {
            assert_eq!(&buf[..n], b"ping");
            from = Some(f);
        }
    }
    let from = from.expect("the datagram to the room address reached the game");
    assert_eq!(from, SocketAddr::from((m_addr, MEMBER_PORT)), "from the member's room address");
    game_udp.send_to(b"pong", from).unwrap();
    let mut pong = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while pong.is_none() && Instant::now() < deadline {
        let Ok(Some(p)) = tokio::time::timeout(Duration::from_secs(1), member.recv()).await else {
            continue;
        };
        if let Some((17, src, _, _, b"pong")) = parse(&p) {
            pong = Some(src);
        }
    }
    assert_eq!(
        pong,
        Some(SocketAddr::from((h_addr, GAME_UDP))),
        "the answer left through the room, from the room address"
    );

    // Weak host must not let the room reach the Ethernet address by naming
    // it: the pump only delivers what is addressed to this member.
    member.send(udp((m_addr, MEMBER_PORT).into(), (ethernet, GAME_UDP).into(), b"sneak")).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    while let Ok((n, _)) = game_udp.recv_from(&mut buf) {
        assert_ne!(&buf[..n], b"sneak", "a room packet addressed to the Ethernet address got in");
    }

    drop(game_tcp);
    stop_tx.send(()).unwrap();
    runner.await.unwrap().unwrap();
}
