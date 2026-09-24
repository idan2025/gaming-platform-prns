//! Mode 3, step 1: a LAN room over a real mesh (`PLAN.md` §14.3).
//!
//! A host and members on a loopback TCP interface, with no virtual adapter:
//! the packets are handed to the session directly, the way step 2's adapter
//! will hand them over. What is pinned here is the room itself — who is
//! seated at which address, that a broadcast reaches everyone else and a
//! unicast reaches one member, that leaving is noticed, and that an allowlist
//! refuses rather than seats.

use std::net::Ipv4Addr;
use std::time::Duration;

use game_bridge::lan::{Refusal, TRANSPORT_MODE_LAN};
use game_bridge::lan_session::{LanHostArgs, LanMemberArgs, LanSession};
use game_bridge::profile::GameProfile;
use prns_core::identity::in_memory::InMemoryNodeIdentity;
use prns_core::identity::IdentitySigner;

mod common;

const PATIENCE: Duration = Duration::from_secs(60);

fn profile() -> GameProfile {
    let mut p = GameProfile::sven_coop();
    p.id = "lan-room-test".to_string();
    p.app_name = "lan-room-test".to_string();
    p.display_name = "LAN Room Test".to_string();
    p.query = None;
    p
}

/// A minimal IPv4 packet: the room reads the header and nothing else.
fn packet(src: Ipv4Addr, dst: Ipv4Addr, body: &[u8]) -> Vec<u8> {
    let mut p = vec![0u8; 20];
    p[0] = 0x45;
    p[12..16].copy_from_slice(&src.octets());
    p[16..20].copy_from_slice(&dst.octets());
    p.extend_from_slice(body);
    p
}

fn body(p: &[u8]) -> &[u8] {
    &p[20..]
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The next packet carrying `want`, skipping anything else the room delivered.
async fn recv_body(s: &LanSession, want: &[u8]) -> Option<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let p = tokio::time::timeout(left, s.recv()).await.ok()??;
        if body(&p) == want {
            return Some(p);
        }
    }
}

/// Nothing carrying `unwanted` arrives within a short window.
async fn never_receives(s: &LanSession, unwanted: &[u8]) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, s.recv()).await {
            Ok(Some(p)) if body(&p) == unwanted => return false,
            Ok(Some(_)) => continue,
            _ => return true,
        }
    }
}

async fn open_room(tag: &str, allowlist: Vec<String>) -> (LanSession, u16, std::path::PathBuf) {
    let mesh_port = common::free_tcp_port();
    let dir = common::scratch_dir(tag);
    let mut args = LanHostArgs::new(profile());
    args.identity = dir.join("host.identity");
    args.tcp = Some(format!("0.0.0.0:{mesh_port}"));
    args.announce_interval = 1;
    args.name = Some("Test room".to_string());
    args.allowlist = allowlist;
    let host = LanSession::host(args).await.expect("the room opens");
    (host, mesh_port, dir)
}

async fn join(
    dir: &std::path::Path,
    name: &str,
    mesh_port: u16,
    room: Option<String>,
) -> LanSession {
    let mut args = LanMemberArgs::new(profile());
    args.identity = dir.join(format!("{name}.identity"));
    args.tcp = Some(format!("127.0.0.1:{mesh_port}"));
    args.room_hash = room;
    LanSession::join(args).await.expect("the member starts")
}

fn hex(h: prns_core::wire::DestinationHash) -> String {
    h.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_room_seats_its_members_and_carries_broadcast_and_unicast() {
    let (host, mesh_port, dir) = open_room("lan-room", Vec::new()).await;
    let room = host.room_hash().expect("a host knows its own room");

    // One member names the room; the other finds it by its announce, which is
    // also what a browser would list.
    let a = join(&dir, "a", mesh_port, Some(hex(room))).await;
    let b = join(&dir, "b", mesh_port, None).await;

    wait_until("everyone sees three members", || {
        [&host, &a, &b].iter().all(|s| s.view().members.len() == 3 && s.own_address().is_some())
    })
    .await;

    let (h_addr, a_addr, b_addr) =
        (host.own_address().unwrap(), a.own_address().unwrap(), b.own_address().unwrap());
    assert!(h_addr != a_addr && a_addr != b_addr && h_addr != b_addr, "addresses are distinct");
    let subnet = host.view().subnet;
    for addr in [h_addr, a_addr, b_addr] {
        assert!(subnet.contains(addr), "{addr} is in the room's subnet");
    }
    let mut tables: Vec<_> = [&host, &a, &b]
        .iter()
        .map(|s| {
            let mut m: Vec<_> =
                s.view().members.iter().map(|m| (*m.identity.as_bytes(), m.address)).collect();
            m.sort();
            m
        })
        .collect();
    tables.dedup();
    assert_eq!(tables.len(), 1, "host and members hold the same table");

    let listed = b.discovered().await;
    let row = listed
        .iter()
        .find(|r| r.destination_hash == room)
        .and_then(|r| r.record().cloned())
        .expect("the room is listed with a §3.3 record");
    assert_eq!(row.flags.transport_mode, TRANSPORT_MODE_LAN);
    assert_eq!(row.game_id, "lan-room-test");

    // A broadcast from a member reaches the host and the other member, and is
    // not echoed back to its sender.
    a.send(packet(a_addr, Ipv4Addr::BROADCAST, b"beacon-from-a")).unwrap();
    assert!(recv_body(&b, b"beacon-from-a").await.is_some(), "the other member hears it");
    assert!(recv_body(&host, b"beacon-from-a").await.is_some(), "the host hears it");
    assert!(never_receives(&a, b"beacon-from-a").await, "the sender does not hear itself");

    // The host is a member of its own room: its broadcast reaches both.
    host.send(packet(h_addr, subnet.broadcast(), b"beacon-from-host")).unwrap();
    assert!(recv_body(&a, b"beacon-from-host").await.is_some());
    assert!(recv_body(&b, b"beacon-from-host").await.is_some());

    // A unicast reaches its addressee and nobody else.
    a.send(packet(a_addr, b_addr, b"just-for-b")).unwrap();
    let got = recv_body(&b, b"just-for-b").await.expect("the addressee receives it");
    assert_eq!(&got[12..16], &a_addr.octets(), "and can see who sent it");
    assert!(never_receives(&host, b"just-for-b").await, "the host does not deliver it to itself");

    // A member cannot speak as another: the header says B, the sender is A.
    a.send(packet(b_addr, Ipv4Addr::BROADCAST, b"a-pretending-to-be-b")).unwrap();
    assert!(never_receives(&host, b"a-pretending-to-be-b").await);

    // Leaving is noticed by everyone who stays.
    let mut b = b;
    b.stop().await;
    wait_until("the member that left is dropped", || {
        host.view().members.len() == 2 && a.view().members.len() == 2
    })
    .await;
    assert!(a.view().members.iter().all(|m| m.address != b_addr));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_identity_off_the_allowlist_is_refused_and_never_seated() {
    let dir = common::scratch_dir("lan-room-allowlist-ids");
    let allowed_path = dir.join("allowed.identity");
    let secret = personal_rns::load_or_create_identity_secret(&allowed_path).unwrap();
    let allowed: String = InMemoryNodeIdentity::from_secret_key_bytes(&secret)
        .identity_hash()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let (host, mesh_port, room_dir) = open_room("lan-room-allowlist", vec![allowed]).await;
    let room = hex(host.room_hash().unwrap());
    std::fs::copy(&allowed_path, room_dir.join("allowed.identity")).unwrap();

    let ok = join(&room_dir, "allowed", mesh_port, Some(room.clone())).await;
    let stranger = join(&room_dir, "stranger", mesh_port, Some(room)).await;

    wait_until("the allowed member is seated", || ok.own_address().is_some()).await;
    wait_until("the stranger is told no", || stranger.view().refused.is_some()).await;
    assert_eq!(stranger.view().refused, Some(Refusal::NotAllowed));
    assert!(stranger.own_address().is_none());
    assert_eq!(host.view().members.len(), 2, "the host and the allowed member, nobody else");
}
