//! `StreamRelay` end to end: a TCP game, bridged over Reticulum, on localhost.
//!
//! `DESIGN.md` §2.1 and `PLAN.md` §8. The datagram relay has
//! `browse_discovery.rs` for its baseline claim; this is the equivalent for the
//! stream path, and it exercises the three things a stream needs that a
//! datagram does not:
//!
//!  - **bytes arrive in order and unsplit** — a reply is asserted against a
//!    payload larger than one channel chunk, so it is reassembled from several;
//!  - **a close means something** — the game client half-closes, the stand-in
//!    game server reads EOF and answers, and the client still receives that
//!    answer;
//!  - **one connection is one link** — two simultaneous connections do not see
//!    each other's bytes.
//!
//! Topology, all on loopback:
//!
//!   game client → bridge client → Reticulum (TCP interface) → bridge server → game server
//!
//! No index, no account, no internet — the same baseline the browse test pins.

use std::time::Duration;

use game_bridge::config::{ClientArgs, ServerArgs};
use game_bridge::profile::{GameProfile, GameTransport};
use game_bridge::BridgeSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

mod common;

/// A stand-in game server: greets, echoes what it is sent with a marker, and
/// answers a half-close with a farewell before hanging up.
///
/// Deliberately not an echo alone. An echo would pass even if the relay never
/// propagated the client's close, and that is the failure mode a stream bridge
/// has and a datagram bridge cannot.
async fn spawn_stand_in_game(listener: TcpListener) {
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                if sock.write_all(b"HELLO\n").await.is_err() {
                    return;
                }
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) => {
                            // The client hung up its writing half. If the relay
                            // dropped that close instead of forwarding it, this
                            // never runs and the test times out waiting.
                            let _ = sock.write_all(b"BYE\n").await;
                            let _ = sock.shutdown().await;
                            return;
                        }
                        Ok(n) => {
                            let mut reply = Vec::with_capacity(n + 6);
                            reply.extend_from_slice(b"ECHO:");
                            reply.extend_from_slice(&buf[..n]);
                            if sock.write_all(&reply).await.is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
            });
        }
    });
}

fn tcp_profile() -> GameProfile {
    let mut profile = GameProfile::sven_coop();
    // A distinct app name: this is a different game on the wire, and reusing
    // `sven-coop` would put a TCP server on the destination deployed UDP peers
    // announce under (`PLAN.md` §5).
    profile.id = "stream-test".to_string();
    profile.app_name = "stream-test".to_string();
    profile.display_name = "Stream Test".to_string();
    profile.transport = GameTransport::Tcp;
    profile.min_link_class = 2;
    profile.query = None;
    profile
}

/// Read until `needle` appears or the deadline passes.
async fn read_until(sock: &mut TcpStream, needle: &[u8], timeout: Duration) -> Vec<u8> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 8192];
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, sock.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                seen.extend_from_slice(&buf[..n]);
                if seen.windows(needle.len()).any(|w| w == needle) {
                    return seen;
                }
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
    seen
}

/// Connect to the bridge client, retrying: the bridge needs a moment to hear
/// the server's announce, and a connection accepted before it has one is
/// closed rather than queued.
async fn connect_through_bridge(port: u16, timeout: Duration) -> Option<TcpStream> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(mut sock) = TcpStream::connect(("127.0.0.1", port)).await {
            let greeting = read_until(&mut sock, b"HELLO", Duration::from_secs(5)).await;
            if greeting.starts_with(b"HELLO") {
                return Some(sock);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

struct Bridged {
    _server: BridgeSession,
    _client: BridgeSession,
    listen_port: u16,
}

async fn start_bridge(tag: &str) -> Bridged {
    let mesh_port = common::free_tcp_port();
    let game_port = common::free_tcp_port();
    let listen_port = common::free_tcp_port();
    let dir = common::scratch_dir(tag);

    let game = TcpListener::bind(("127.0.0.1", game_port))
        .await
        .expect("stand-in game binds");
    spawn_stand_in_game(game).await;

    let mut server_args = ServerArgs::new(tcp_profile());
    server_args.identity = dir.join("server.identity");
    server_args.tcp = Some(format!("0.0.0.0:{mesh_port}"));
    server_args.announce_interval = 1;
    server_args.game_port = game_port;
    let server = BridgeSession::start_server(server_args)
        .await
        .expect("bridge server starts");

    let mut client_args = ClientArgs::new(tcp_profile());
    client_args.identity = dir.join("client.identity");
    client_args.tcp = Some(format!("127.0.0.1:{mesh_port}"));
    client_args.listen_port = listen_port;
    let client = BridgeSession::start_client(client_args)
        .await
        .expect("bridge client starts");

    Bridged { _server: server, _client: client, listen_port }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tcp_game_is_bridged_over_reticulum_with_no_index() {
    let bridged = start_bridge("stream").await;
    let mut sock = connect_through_bridge(bridged.listen_port, Duration::from_secs(60))
        .await
        .expect("the bridge client accepted a connection and the game greeted us");

    // Larger than one channel chunk, so the reply comes back in several and the
    // relay has to deliver them in order and unsplit.
    let payload = vec![b'x'; 12_000];
    sock.write_all(&payload).await.expect("write to the bridge");

    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut buf = vec![0u8; 16 * 1024];
    while seen.len() < payload.len() + 5 && tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, sock.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => seen.extend_from_slice(&buf[..n]),
            Ok(Err(e)) => panic!("read from the bridge failed: {e}"),
            Err(_) => break,
        }
    }

    assert!(
        seen.starts_with(b"ECHO:"),
        "expected the game's reply, saw {} byte(s)",
        seen.len()
    );
    assert_eq!(
        seen.len(),
        payload.len() + 5,
        "the stream lost or duplicated bytes across chunk boundaries"
    );
    assert!(
        seen[5..].iter().all(|b| *b == b'x'),
        "the stream was reassembled out of order or corrupted"
    );
}

/// The property a datagram relay never has to get right: a close in one
/// direction has to reach the far end, and must not tear down the other
/// direction before the answer comes back.
#[tokio::test(flavor = "multi_thread")]
async fn a_half_close_reaches_the_game_and_the_answer_still_returns() {
    let bridged = start_bridge("stream-close").await;
    let mut sock = connect_through_bridge(bridged.listen_port, Duration::from_secs(60))
        .await
        .expect("the bridge client accepted a connection and the game greeted us");

    sock.write_all(b"last\n").await.expect("write to the bridge");
    let echoed = read_until(&mut sock, b"ECHO:last", Duration::from_secs(60)).await;
    assert!(
        echoed.windows(9).any(|w| w == b"ECHO:last"),
        "never saw the echo before half-closing"
    );

    sock.shutdown().await.expect("half-close the writing side");
    let farewell = read_until(&mut sock, b"BYE", Duration::from_secs(60)).await;
    assert!(
        farewell.windows(3).any(|w| w == b"BYE"),
        "the game never saw the client's close, or the answer never came back"
    );
}

/// One connection is one link. Two at once must not braid together — the
/// datagram relay keys links by source address, and a stream relay that
/// borrowed that idea would put both connections on one link.
#[tokio::test(flavor = "multi_thread")]
async fn two_connections_do_not_share_a_stream() {
    let bridged = start_bridge("stream-two").await;
    let mut first = connect_through_bridge(bridged.listen_port, Duration::from_secs(60))
        .await
        .expect("first connection");
    let mut second = connect_through_bridge(bridged.listen_port, Duration::from_secs(60))
        .await
        .expect("second connection");

    first.write_all(b"first\n").await.expect("write on the first");
    second.write_all(b"second\n").await.expect("write on the second");

    let a = read_until(&mut first, b"ECHO:first", Duration::from_secs(60)).await;
    let b = read_until(&mut second, b"ECHO:second", Duration::from_secs(60)).await;

    assert!(a.windows(10).any(|w| w == b"ECHO:first"), "first connection lost its reply");
    assert!(!a.windows(11).any(|w| w == b"ECHO:second"), "the first connection saw the second's bytes");
    assert!(b.windows(11).any(|w| w == b"ECHO:second"), "second connection lost its reply");
    assert!(!b.windows(10).any(|w| w == b"ECHO:first"), "the second connection saw the first's bytes");
}
