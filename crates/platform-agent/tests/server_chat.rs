//! Chat on a node's server (`ROADMAP.md` §6.1), against a real Docker daemon.
//!
//! A node's server offers no chat until its operator turns it on (the user's
//! call: a node serves strangers, and someone must moderate). Then a player's
//! client joins it over the mesh, and the operator reads, speaks and mutes
//! through the loopback API's methods. Skips itself without Docker. Ports
//! 27230–27234 are its own.

use std::sync::Arc;
use std::time::Duration;

use game_bridge::config::ClientArgs;
use game_bridge::profile::GameProfile;
use game_bridge::BridgeSession;
use platform_agent::agent::Agent;
use platform_agent::config::AgentConfig;
use platform_agent::instance::InstanceSpec;

const GAME: &str = "gpp-test-chat:latest";
const GAME_DOCKERFILE: &str =
    "FROM busybox\nCMD [\"sh\", \"-c\", \"trap 'exit 0' TERM; while true; do sleep 1; done\"]\n";

fn ensure_image() -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", GAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(present, Ok(s) if s.success()) {
        return true;
    }
    let dir = std::env::temp_dir().join(format!("gpp-chat-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err()
        || std::fs::write(dir.join("Dockerfile"), GAME_DOCKERFILE).is_err()
    {
        return false;
    }
    let built = std::process::Command::new("docker")
        .args(["build", "-q", "-t", GAME, "."])
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_dir_all(&dir);
    matches!(built, Ok(s) if s.success())
}

struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &format!("gpp-{}", self.0)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn eventually(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    f()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nodes_server_chats_only_once_its_operator_turns_it_on() {
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let content = dir.path().join("content/sven-coop/test/svencoop");
    for sub in ["maps/soundcache", "logs", "scripts"] {
        std::fs::create_dir_all(content.join(sub)).unwrap();
    }
    let mesh_port = free_port();
    let toml = format!(
        r#"
data_root = "{}"
max_instances = 2
api_bind = "127.0.0.1:0"

[port_range]
start = 27230
end = 27234

[mesh]
tcp = "0.0.0.0:{mesh_port}"
announce_interval = 1
shared_instance_port = {}

[games.sven-coop]
image = "{GAME}"
content_root = "/game"
content_version = "test"
"#,
        dir.path().display(),
        free_port(),
    );
    let config = AgentConfig::parse(&toml).unwrap();
    let agent =
        Arc::new(Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await.unwrap());
    let id = format!("chat{}", std::process::id());
    let _cleanup = Container(id.clone());
    agent
        .create(InstanceSpec {
            instance_id: id.clone(),
            game_id: "sven-coop".to_string(),
            name: "chat test".to_string(),
            max_players: 8,
            port: None,
            extra_ports: Default::default(),
            map: None,
            bots: None,
            restart_on_crash: false,
            owner: None,
            options: Default::default(),
        })
        .await
        .unwrap();
    let view = agent.chat(&id, 0).await.unwrap();
    assert!(!view.on, "off on a node until the operator turns it on");
    assert!(view.live, "the server is on the mesh");
    let destination = agent.mesh().status_of(&id).await.unwrap().destination.unwrap();
    let hash =
        personal_rns::prelude::DestinationHash::from_slice(&hex::decode(&destination).unwrap())
            .unwrap();

    let mut args = ClientArgs::new(GameProfile::sven_coop());
    args.identity = dir.path().join("player.identity");
    args.tcp = Some(format!("127.0.0.1:{mesh_port}"));
    args.listen_port = free_port();
    let player = BridgeSession::start_client(args).await.unwrap();
    let e = player.open_chat(hash, "Ada").await.err().expect("no chat yet");
    assert!(e.to_string().contains("no chat"), "{e}");

    agent.set_chat(&id, true).await.unwrap();
    assert!(agent.chat(&id, 0).await.unwrap().on);
    // The next announce says so; the client asks again until it hears it.
    let mut chat = None;
    for _ in 0..20 {
        if let Ok(c) = player.open_chat(hash, "Ada").await {
            chat = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let chat = chat.expect("the player joins once chat is on");
    let agent2 = agent.clone();
    let id2 = id.clone();
    let mut seen = false;
    for _ in 0..40 {
        if agent2.chat(&id2, 0).await.unwrap().members.iter().any(|m| m.name == "Ada") {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(seen, "the operator sees who is in the chat");

    agent.chat_say(&id, "welcome to the node").await.unwrap();
    assert!(
        eventually(10, || chat.lines_since(0).iter().any(|l| l.text == "welcome to the node"))
            .await
    );
    chat.say("thanks").unwrap();
    let mut heard = false;
    for _ in 0..40 {
        if agent
            .chat(&id, 0)
            .await
            .unwrap()
            .lines
            .iter()
            .any(|l| l.text == "thanks" && l.name.as_deref() == Some("Ada"))
        {
            heard = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(heard, "the operator reads what the player said, by name");

    let ada =
        agent.chat(&id, 0).await.unwrap().members.into_iter().find(|m| m.name == "Ada").unwrap();
    agent.chat_mute(&id, &ada.identity, true).await.unwrap();
    assert!(agent.chat(&id, 0).await.unwrap().members.iter().any(|m| m.name == "Ada" && m.muted));

    agent.set_chat(&id, false).await.unwrap();
    assert!(eventually(10, || !chat.is_open()).await, "off ends the chat");
    assert!(
        std::fs::read_to_string(dir.path().join("instances").join(&id).join("chat.json"))
            .unwrap()
            .contains("false"),
        "the choice is kept for the next start"
    );
    agent.remove(&id).await.ok();
}
