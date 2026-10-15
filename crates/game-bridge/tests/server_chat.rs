//! Server chat end to end (`ROADMAP.md` §6.1, `server_chat.rs`), on loopback.
//!
//!   launcher (client session) → Reticulum (TCP interface) → bridge server
//!
//! Each test is one rule that, removed, would not fail anything else: chat is
//! asked only of a server that announced it, a chat link never becomes a game
//! connection, a link that never said `Hello` hears nothing, and who said a
//! line is the identity its link proved.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use game_bridge::config::{ClientArgs, ServerArgs};
use game_bridge::profile::{GameProfile, GameTransport};
use game_bridge::server_chat::{self, ChatClient};
use game_bridge::BridgeSession;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

mod common;

fn profile(tag: &str, transport: GameTransport) -> GameProfile {
    let mut p = GameProfile::sven_coop();
    // Its own app name: never the destination deployed Sven peers use.
    p.id = format!("chat-{tag}");
    p.app_name = format!("chat-{tag}");
    p.display_name = "Chat Test".to_string();
    p.transport = transport;
    p.query = None;
    p
}

struct Mesh {
    server: BridgeSession,
    mesh_port: u16,
    profile: GameProfile,
    dir: std::path::PathBuf,
}

async fn server(tag: &str, transport: GameTransport, chat: bool, game_port: u16) -> Mesh {
    let mesh_port = common::free_tcp_port();
    let dir = common::scratch_dir(tag);
    let profile = profile(tag, transport);
    let mut args = ServerArgs::new(profile.clone());
    args.identity = dir.join("server.identity");
    args.tcp = Some(format!("0.0.0.0:{mesh_port}"));
    args.announce_interval = 1;
    args.game_port = game_port;
    args.identify_timeout_secs = 2;
    args.chat = chat;
    let server = BridgeSession::start_server(args).await.expect("server starts");
    Mesh { server, mesh_port, profile, dir }
}

async fn client(mesh: &Mesh, who: &str) -> BridgeSession {
    let mut args = ClientArgs::new(mesh.profile.clone());
    args.identity = mesh.dir.join(format!("{who}.identity"));
    args.tcp = Some(format!("127.0.0.1:{}", mesh.mesh_port));
    args.listen_port = common::free_tcp_port();
    BridgeSession::start_client(args).await.expect("client starts")
}

async fn join(session: &BridgeSession, mesh: &Mesh, name: &str) -> ChatClient {
    let hash = mesh.server.own_hash().unwrap();
    let mut last = None;
    for _ in 0..4 {
        match session.open_chat(hash, name).await {
            Ok(c) => return c,
            Err(e) => last = Some(e),
        }
    }
    panic!("could not join chat: {:?}", last.unwrap());
}

/// Poll until `f` holds or the deadline passes.
async fn eventually(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    f()
}

/// A server that did not announce chat is never asked for it: no link, no
/// stream byte, and a plain answer for the player.
#[tokio::test(flavor = "multi_thread")]
async fn chat_is_never_offered_to_a_server_that_did_not_announce_it() {
    let mesh = server("off", GameTransport::Udp, false, common::free_tcp_port()).await;
    let c = client(&mesh, "a").await;
    let e = c.open_chat(mesh.server.own_hash().unwrap(), "Ada").await.err().expect("refused");
    assert!(e.to_string().contains("no chat"), "{e}");
    assert_eq!(mesh.server.chat().unwrap().members(), 0);
}

/// Two players and the host talk; each line comes from the identity its link
/// proved, everyone is named, and turning chat off ends it for everyone.
#[tokio::test(flavor = "multi_thread")]
async fn players_and_the_host_chat_through_a_server() {
    let mesh = server("talk", GameTransport::Udp, true, common::free_tcp_port()).await;
    let a = client(&mesh, "a").await;
    let b = client(&mesh, "b").await;
    let ca = join(&a, &mesh, "Ada").await;
    let cb = join(&b, &mesh, "Bo").await;
    let hub = mesh.server.chat().unwrap().clone();
    assert!(eventually(10, || hub.names().len() == 3).await, "{:?}", hub.names());

    ca.say("hello from Ada").unwrap();
    assert!(eventually(10, || cb.lines_since(0).iter().any(|l| l.text == "hello from Ada")).await);
    let line = cb.lines_since(0).into_iter().find(|l| l.text == "hello from Ada").unwrap();
    let ada = hub.names().into_iter().find(|(_, n)| n == "Ada").unwrap().0;
    assert_eq!(line.from, ada, "stamped by the server with Ada's identity");
    assert!(cb.names().iter().any(|(id, n)| *id == ada && n == "Ada"));

    hub.host_say("restarting in five minutes").unwrap();
    assert!(
        eventually(10, || ca.lines_since(0).iter().any(|l| l.from == hub.host_identity())).await
    );
    assert!(hub.lines_since(0).iter().any(|l| l.text == "hello from Ada"));

    // Chat links are not players.
    assert!(mesh.server.connected_clients().await.is_empty());

    mesh.server.set_chat(false);
    assert!(eventually(10, || !ca.is_open() && !cb.is_open()).await, "off closes every chat link");
}

/// A TCP game's server connects to the game when a link identifies. A chat
/// link identifies too, and must never become a game connection — on an open
/// server and an allowlisted one.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_link_never_starts_a_game_connection() {
    let game = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let game_port = game.local_addr().unwrap().port();
    let accepts = Arc::new(AtomicUsize::new(0));
    let counted = accepts.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = game.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut b = [0u8; 64];
                while sock.read(&mut b).await.is_ok_and(|n| n > 0) {}
            });
        }
    });
    let mesh = server("tcp", GameTransport::Tcp, true, game_port).await;
    let a = client(&mesh, "a").await;
    let ca = join(&a, &mesh, "Ada").await;
    let hub = mesh.server.chat().unwrap().clone();
    assert!(eventually(10, || hub.names().len() == 2).await);
    // Past the identify timeout, when an open server starts a held link.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(accepts.load(Ordering::SeqCst), 0, "a chat link opened a game connection");
    assert!(ca.is_open());
}

/// A link that never said `Hello` — a game link, an older client — never
/// receives a byte of chat, however much is said.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_that_never_said_hello_never_receives_chat() {
    let mesh = server("quiet", GameTransport::Udp, true, common::free_tcp_port()).await;
    let a = client(&mesh, "a").await;
    let ca = join(&a, &mesh, "Ada").await;

    let other = client(&mesh, "old").await;
    let hash = mesh.server.own_hash().unwrap();
    let link = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(l) = other.handle().establish_link(hash).await {
                return l;
            }
            let _ = other.handle().request_path(hash).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("a plain link comes up");
    let (to_server, to_client) = server_chat::stream_ids();
    let (mut reader, _writer) = other.handle().byte_stream(link, to_client, to_server).await;

    for i in 0..3 {
        ca.say(&format!("line {i}")).unwrap();
    }
    mesh.server.chat().unwrap().host_say("anyone there").unwrap();
    let mut buf = [0u8; 64];
    let got = tokio::time::timeout(Duration::from_secs(4), reader.read(&mut buf)).await;
    assert!(got.is_err(), "a link that never said Hello received chat: {got:?}");
}
