//! Blocker B, executed: one destination carrying several of a server's ports.
//!
//! `GAMES.md` §3. A Source-engine server is a UDP game port *and* a TCP RCON
//! port *and* optionally a second UDP port for SourceTV, while a Reticulum
//! destination fronts one. Extra UDP ports ride framing generation 2's channel
//! ids (`framing.rs`); an extra TCP port rides its own stream id pair
//! (`stream.rs`), because framing's channel bits are a datagram concern and a
//! stream never passes through `frame`.
//!
//! Both mechanisms are exercised here over a real loopback mesh, on one
//! destination, at the same time.

use std::time::Duration;

use game_bridge::config::{ClientArgs, ServerArgs};
use game_bridge::framing::{self, FRAMING_V1, FRAMING_V2};
use game_bridge::profile::{GamePort, GameProfile, GameTransport};
use game_bridge::BridgeSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

mod common;

/// A stand-in UDP port that answers `<tag>:<what it was sent>`.
async fn spawn_udp_port(sock: UdpSocket, tag: &'static str) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            let Ok((n, src)) = sock.recv_from(&mut buf).await else {
                return;
            };
            let mut reply = tag.as_bytes().to_vec();
            reply.extend_from_slice(b":");
            reply.extend_from_slice(&buf[..n]);
            if sock.send_to(&reply, src).await.is_err() {
                return;
            }
        }
    });
}

/// A stand-in RCON: TCP, answers `rcon:<line>`, and speaks only when spoken to
/// — which is why an extra TCP port is connected lazily.
async fn spawn_rcon(listener: TcpListener) {
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            let mut reply = b"rcon:".to_vec();
                            reply.extend_from_slice(&buf[..n]);
                            if sock.write_all(&reply).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
}

async fn ask_udp(sock: &UdpSocket, port: u16, what: &[u8], timeout: Duration) -> Option<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 8192];
    while tokio::time::Instant::now() < deadline {
        if sock.send_to(what, ("127.0.0.1", port)).await.is_err() {
            return None;
        }
        if let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(750), sock.recv_from(&mut buf)).await
        {
            return Some(buf[..n].to_vec());
        }
    }
    None
}

/// TF2's shape: UDP game, TCP RCON, second UDP port for SourceTV.
fn source_profile(game_port: u16, rcon_port: u16, tv_port: u16) -> GameProfile {
    let mut profile = GameProfile::sven_coop();
    profile.id = "multi-port-test".to_string();
    profile.app_name = "multi-port-test".to_string();
    profile.display_name = "Multi Port Test".to_string();
    profile.default_port = game_port;
    profile.transport = GameTransport::Udp;
    profile.min_link_class = 2;
    profile.query = None;
    profile.extra_ports = vec![
        GamePort {
            channel: 1,
            name: "rcon".to_string(),
            port: rcon_port,
            transport: GameTransport::Tcp,
        },
        GamePort {
            channel: 2,
            name: "tv".to_string(),
            port: tv_port,
            transport: GameTransport::Udp,
        },
    ];
    profile
}

/// A multi-port game advertises generation 2, which is what permits a peer to
/// send it a non-zero channel at all. A single-port game must keep advertising
/// generation 1: announcing a capability it never exercises would put a number
/// on the wire that means less than it says.
#[test]
fn only_a_multi_port_game_advertises_framing_v2() {
    assert_eq!(GameProfile::sven_coop().protocol_version(), FRAMING_V1);
    assert_eq!(source_profile(1, 2, 3).protocol_version(), FRAMING_V2);
    assert!(!framing::supports_channels(GameProfile::sven_coop().protocol_version()));
}

#[tokio::test(flavor = "multi_thread")]
async fn one_destination_carries_a_game_an_rcon_and_a_second_udp_port() {
    let mesh_port = common::free_tcp_port();
    let listen_port = common::free_tcp_port();
    let dir = common::scratch_dir("multi-port");

    // The three ports a Source server would expose, all stand-ins.
    let game = UdpSocket::bind("127.0.0.1:0").await.expect("game port binds");
    let game_port = game.local_addr().unwrap().port();
    spawn_udp_port(game, "game").await;

    let tv = UdpSocket::bind("127.0.0.1:0").await.expect("tv port binds");
    let tv_port = tv.local_addr().unwrap().port();
    spawn_udp_port(tv, "tv").await;

    let rcon = TcpListener::bind("127.0.0.1:0").await.expect("rcon port binds");
    let rcon_port = rcon.local_addr().unwrap().port();
    spawn_rcon(rcon).await;

    let profile = source_profile(game_port, rcon_port, tv_port);

    let mut server_args = ServerArgs::new(profile.clone());
    server_args.identity = dir.join("server.identity");
    server_args.tcp = Some(format!("0.0.0.0:{mesh_port}"));
    server_args.announce_interval = 1;
    server_args.game_port = game_port;
    let _server = BridgeSession::start_server(server_args)
        .await
        .expect("bridge server starts");

    let mut client_args = ClientArgs::new(profile);
    client_args.identity = dir.join("client.identity");
    client_args.tcp = Some(format!("127.0.0.1:{mesh_port}"));
    client_args.listen_port = listen_port;
    let _client = BridgeSession::start_client(client_args)
        .await
        .expect("bridge client starts");

    // Channel 0: the game's own port, byte-identical to what a v1 peer speaks.
    let local = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let answer = ask_udp(&local, listen_port, b"ping", Duration::from_secs(60))
        .await
        .expect("the game port answered");
    assert_eq!(answer, b"game:ping");

    // Channel 2: a second UDP port, on the same destination and the same link
    // family, distinguished only by framing generation 2's channel bits.
    let tv_local = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let answer = ask_udp(&tv_local, listen_port + 2, b"ping", Duration::from_secs(60))
        .await
        .expect("the second UDP port answered");
    assert_eq!(
        answer, b"tv:ping",
        "a second UDP port must not be answered by the game port"
    );

    // Channel 1: TCP, on its own stream ids rather than framing bits.
    let mut sock = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while tokio::time::Instant::now() < deadline && sock.is_none() {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", listen_port + 1)).await {
            if s.write_all(b"status").await.is_ok() {
                let mut buf = vec![0u8; 1024];
                if let Ok(Ok(n)) =
                    tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await
                {
                    if n > 0 {
                        assert_eq!(&buf[..n], b"rcon:status");
                        sock = Some(s);
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(sock.is_some(), "the RCON port never answered through the bridge");

    // And the game port still works afterwards: the three share a destination,
    // not a queue.
    let answer = ask_udp(&local, listen_port, b"again", Duration::from_secs(30))
        .await
        .expect("the game port still answers");
    assert_eq!(answer, b"game:again");
}

/// The `PLAN.md` §5 gate on the wire: a client whose pack declares extra ports,
/// pointed at a server that announces generation 1, must keep its extra ports
/// off the link entirely. A deployed peer does not reject a channel id — it
/// masks only `FLAG_FINAL` and mis-reassembles the rest — so "the server would
/// have dropped it" is not a defence. Channel 0 has to keep working throughout.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_with_extra_ports_sends_none_of_them_to_a_v1_server() {
    let mesh_port = common::free_tcp_port();
    let listen_port = common::free_tcp_port();
    let dir = common::scratch_dir("multi-port-v1-peer");

    let game = UdpSocket::bind("127.0.0.1:0").await.expect("game port binds");
    let game_port = game.local_addr().unwrap().port();
    spawn_udp_port(game, "game").await;

    // The server runs the same game, single-port: no extra ports, so it
    // announces generation 1 exactly like a deployed v0.1.10 peer.
    let mut server_profile = source_profile(game_port, 0, 0);
    server_profile.extra_ports = Vec::new();
    assert_eq!(server_profile.protocol_version(), FRAMING_V1);

    let mut server_args = ServerArgs::new(server_profile);
    server_args.identity = dir.join("server.identity");
    server_args.tcp = Some(format!("0.0.0.0:{mesh_port}"));
    server_args.announce_interval = 1;
    server_args.game_port = game_port;
    let _server = BridgeSession::start_server(server_args)
        .await
        .expect("bridge server starts");

    // The client's own pack says three ports. Its own capability is not the
    // question; the peer's is.
    let mut client_args = ClientArgs::new(source_profile(game_port, 1, 2));
    client_args.identity = dir.join("client.identity");
    client_args.tcp = Some(format!("127.0.0.1:{mesh_port}"));
    client_args.listen_port = listen_port;
    let _client = BridgeSession::start_client(client_args)
        .await
        .expect("bridge client starts");

    // Channel 0 works, which also proves the client discovered the server and
    // read its announce before the next assertion runs.
    let local = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let answer = ask_udp(&local, listen_port, b"ping", Duration::from_secs(60))
        .await
        .expect("the game port answered");
    assert_eq!(answer, b"game:ping");

    // Channel 2 must go nowhere at all.
    let tv_local = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    assert!(
        ask_udp(&tv_local, listen_port + 2, b"ping", Duration::from_secs(5))
            .await
            .is_none(),
        "an extra port must not reach a server that never advertised framing v2"
    );

    // And the game port is unharmed by the attempt.
    let answer = ask_udp(&local, listen_port, b"again", Duration::from_secs(30))
        .await
        .expect("the game port still answers");
    assert_eq!(answer, b"game:again");
}
