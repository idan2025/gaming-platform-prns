//! Kick, ban and the node's ban record (`MINECRAFT.md` §4), against a real
//! Docker daemon. The stand-in server echoes every console line it reads, so
//! what reached the game is in its log. Skips itself without Docker. Ports
//! 27230–27234 are its own.

use std::time::Duration;

use platform_agent::agent::Agent;
use platform_agent::config::AgentConfig;
use platform_agent::instance::InstanceSpec;

const ECHO: &str = "gpp-test-echo:latest";
const ECHO_DOCKERFILE: &str =
    "FROM busybox\nCMD [\"sh\", \"-c\", \"trap 'exit 0' TERM; while read -r l; do echo \\\"got: $l\\\"; done\"]\n";

fn ensure_image() -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", ECHO])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(present, Ok(s) if s.success()) {
        return true;
    }
    let dir = std::env::temp_dir().join(format!("gpp-echo-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err()
        || std::fs::write(dir.join("Dockerfile"), ECHO_DOCKERFILE).is_err()
    {
        return false;
    }
    let built = std::process::Command::new("docker")
        .args(["build", "-q", "-t", ECHO, "."])
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
            .output();
    }
}

/// Lines of the log that are exactly `line`.
fn count(log: &str, line: &str) -> usize {
    log.lines().filter(|l| l.trim_end() == line).count()
}

async fn wait_for(agent: &Agent, id: &str, line: &str, after: usize) -> String {
    let mut log = String::new();
    for _ in 0..80 {
        log = agent.logs(id, 200).await.unwrap();
        if count(&log, line) > after {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    log
}

#[tokio::test(flavor = "multi_thread")]
async fn a_steam_ban_reaches_the_game_survives_a_restart_and_nothing_else_gets_through() {
    use game_bridge::console::PlayerAction;
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for sub in ["maps/soundcache", "logs", "scripts"] {
        std::fs::create_dir_all(dir.path().join("content/sven-coop/test/svencoop").join(sub)).unwrap();
    }
    let toml = format!(
        r#"
data_root = "{}"
max_instances = 2
api_bind = "127.0.0.1:0"

[port_range]
start = 27230
end = 27234

[games.sven-coop]
image = "{ECHO}"
content_root = "/game"
content_version = "test"
"#,
        dir.path().display()
    );
    let Ok(agent) = Agent::new(AgentConfig::parse(&toml).unwrap(), vec![game_bridge::GamePack::sven_coop()]).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let id = format!("moderation{}", std::process::id());
    let _cleanup = Container(id.clone());
    agent
        .create(InstanceSpec {
            instance_id: id.clone(),
            game_id: "sven-coop".into(),
            name: "moderation test".into(),
            max_players: 8,
            port: None,
            extra_ports: Default::default(),
            bots: None,
            map: None,
            restart_on_crash: false,
            owner: None,
            options: Default::default(),
        })
        .await
        .expect("create");

    let sent = agent.moderate(&id, PlayerAction::Ban, "STEAM_0:1:42", Some("cheating")).await.expect("ban");
    assert_eq!(sent, ["banid 0 STEAM_0:1:42 kick"]);
    let log = wait_for(&agent, &id, "got: banid 0 STEAM_0:1:42 kick", 0).await;
    assert!(log.contains("got: banid 0 STEAM_0:1:42 kick"), "{log:?}");
    let view = agent.moderation_view(&id).await.unwrap();
    assert_eq!(view["ban_list"], "node-record");
    assert_eq!(view["banned"][0]["target"], "STEAM_0:1:42");
    assert_eq!(view["banned"][0]["reason"], "cheating");

    for (action, target, reason) in [
        (PlayerAction::Kick, "Bob\nquit", None),
        (PlayerAction::Kick, "Bob", Some("bye; quit")),
        (PlayerAction::Ban, "Bob", None),
        (PlayerAction::Op, "STEAM_0:1:42", None),
    ] {
        assert!(agent.moderate(&id, action, target, reason).await.is_err(), "{action:?} {target:?} {reason:?}");
    }
    agent.moderate(&id, PlayerAction::Kick, "Big Bob", None).await.expect("kick by name");

    // The game cannot keep the ban, so the node sends it again after a start.
    agent.restart(&id).await.expect("restart");
    let log = wait_for(&agent, &id, "got: banid 0 STEAM_0:1:42", 0).await;
    assert_eq!(count(&log, "got: banid 0 STEAM_0:1:42"), 1, "the ban was sent again after the restart: {log:?}");
    assert!(!log.contains("got: quit"), "{log:?}");

    // A stopped server's record still changes, and it reaches the game later.
    agent.stop(&id).await.expect("stop");
    agent.moderate(&id, PlayerAction::Unban, "STEAM_0:1:42", None).await.expect("unban while stopped");
    assert!(agent.moderation_view(&id).await.unwrap()["banned"].as_array().unwrap().is_empty());
    assert!(agent.moderate(&id, PlayerAction::Kick, "Bob", None).await.is_err(), "a kick needs a running server");
}
