//! A game datagram must not buy a proof packet on the return leg.
//!
//! The server's destination is declared `ProofStrategy::ProveNone`
//! (`relay.rs`). `ProofStrategy` gates link *data* only — the
//! link-establishment proof is unconditional
//! (`prns-core/src/routing/links/establish/mod.rs:354`) and the §3.4 detail
//! probe rides the request context — so the only thing it removes is a signed
//! proof for every game datagram the server receives. Measured before this
//! test existed: `ProveAll` put exactly one 117.7 B packet on the wire per
//! inbound datagram, a 1:1 shadow of the upstream game traffic travelling
//! against the downstream game updates. `ProveNone` puts none.
//!
//! Reverting the strategy would fail nothing else in the suite: every other
//! test asserts that bytes *arrive*, and they still would. This one asserts
//! what does not.
//!
//! The stand-in game **never replies**, so on the mesh TCP interface the
//! server->client direction carries nothing but link keepalives and whatever
//! proofs the responder owes. Counting bytes there isolates them.
//!
//! Needs `ss` (iproute2) to read the socket's counters, and skips itself where
//! there is none — the same bargain the agent's Docker tests make.

use std::process::Command;
use std::time::Duration;

use game_bridge::config::{ClientArgs, ServerArgs};
use game_bridge::profile::GameProfile;
use game_bridge::BridgeSession;
use tokio::net::UdpSocket;

mod common;

/// One-way game traffic to push through the bridge. Large enough that a 1:1
/// proof shadow cannot hide in the keepalives, small enough to stay quick.
const DATAGRAMS: usize = 200;

async fn spawn_silent_udp_port(sock: UdpSocket) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        while sock.recv_from(&mut buf).await.is_ok() {}
    });
}

fn have_ss() -> bool {
    Command::new("ss").arg("-V").output().map(|o| o.status.success()).unwrap_or(false)
}

/// `(bytes_sent, data_segs_out, bytes_received)` for the established socket
/// whose *source* port is the mesh port: the server's accepted end of the link.
fn server_side_counters(mesh_port: u16) -> Option<(u64, u64, u64)> {
    let out = Command::new("ss")
        .args(["-tni", "state", "established", &format!("sport = :{mesh_port}")])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| -> Option<u64> {
        text.split_whitespace().find_map(|t| t.strip_prefix(name)?.parse::<u64>().ok())
    };
    Some((field("bytes_sent:")?, field("data_segs_out:")?, field("bytes_received:")?))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_game_datagram_does_not_buy_a_proof_packet_back() {
    if !have_ss() {
        eprintln!("skipping: no `ss` to read socket counters with");
        return;
    }

    let mesh_port = common::free_tcp_port();
    let listen_port = common::free_tcp_port();
    let dir = common::scratch_dir("proof-cost");

    let game = UdpSocket::bind("127.0.0.1:0").await.expect("game port binds");
    let game_port = game.local_addr().unwrap().port();
    spawn_silent_udp_port(game).await;

    let mut profile = GameProfile::sven_coop();
    profile.id = "proof-cost-test".to_string();
    profile.app_name = "proof-cost-test".to_string();
    profile.default_port = game_port;
    profile.query = None;

    let mut server_args = ServerArgs::new(profile.clone());
    server_args.identity = dir.join("server.identity");
    server_args.tcp = Some(format!("0.0.0.0:{mesh_port}"));
    server_args.announce_interval = 1;
    server_args.game_port = game_port;
    let _server = BridgeSession::start_server(server_args).await.expect("server starts");

    let mut client_args = ClientArgs::new(profile);
    client_args.identity = dir.join("client.identity");
    client_args.tcp = Some(format!("127.0.0.1:{mesh_port}"));
    client_args.listen_port = listen_port;
    let _client = BridgeSession::start_client(client_args).await.expect("client starts");

    // The link comes up on the first datagram, but only once an announce has
    // been heard. Wait for the server's own end of the mesh socket to start
    // carrying the client's traffic — readiness cannot be read from what the
    // server sends, because passing this test means it sends nothing.
    let local = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut ready = None;
    for _ in 0..80 {
        let _ = local.send_to(b"warmup", ("127.0.0.1", listen_port)).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Some(counters) = server_side_counters(mesh_port) {
            if counters.2 > 0 {
                ready = Some(counters);
                break;
            }
        }
    }
    let before = ready.expect("the mesh link carried the client's warmup traffic");

    let payload = vec![0x5au8; 200];
    for _ in 0..DATAGRAMS {
        let _ = local.send_to(&payload, ("127.0.0.1", listen_port)).await;
        tokio::time::sleep(Duration::from_millis(4)).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;

    let after = server_side_counters(mesh_port).expect("the mesh socket is still up");
    let arrived = after.2 - before.2;
    let segments_back = after.1 - before.1;

    assert!(
        arrived > 0,
        "the game datagrams never reached the server, so the return leg proves nothing"
    );
    // `ProveAll` sends exactly one proof per inbound datagram. Keepalives run
    // on the link's RTT and contribute a handful of segments at most, so a
    // tenth of the datagram count separates the two outcomes with room to
    // spare: measured, 1 segment against 200.
    assert!(
        segments_back < DATAGRAMS as u64 / 10,
        "the server answered {DATAGRAMS} one-way game datagrams with {segments_back} \
         segments; a proof per datagram is a packet against the game's own downstream \
         traffic that nothing ever reads"
    );
}
