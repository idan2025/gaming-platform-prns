//! The operator's console line (`ROADMAP.md` item 7), against a real Docker
//! daemon. The stand-in server echoes every line it reads on stdin into its
//! log, which is where a game's answer shows. Skips itself without Docker.
//! Ports 27225–27229 are its own.

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

#[tokio::test(flavor = "multi_thread")]
async fn an_operators_line_reaches_the_console_and_a_second_line_never_does() {
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for sub in ["maps/soundcache", "logs", "scripts"] {
        std::fs::create_dir_all(dir.path().join("content/sven-coop/test/svencoop").join(sub))
            .unwrap();
    }
    let toml = format!(
        r#"
data_root = "{}"
max_instances = 2
api_bind = "127.0.0.1:0"

[port_range]
start = 27225
end = 27229

[games.sven-coop]
image = "{ECHO}"
content_root = "/game"
content_version = "test"
"#,
        dir.path().display()
    );
    let Ok(agent) =
        Agent::new(AgentConfig::parse(&toml).unwrap(), vec![game_bridge::GamePack::sven_coop()])
            .await
    else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let id = format!("console{}", std::process::id());
    let _cleanup = Container(id.clone());
    agent
        .create(InstanceSpec {
            instance_id: id.clone(),
            game_id: "sven-coop".into(),
            name: "console test".into(),
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

    agent.send_console(&id, "status; echo chained").await.expect("one line is sent");
    let refused = agent.send_console(&id, "status\nquit").await;
    assert!(refused.is_err(), "a second line must be refused");
    assert!(agent.send_console("nobody", "status").await.is_err());

    let mut log = String::new();
    for _ in 0..40 {
        log = agent.logs(&id, 50).await.unwrap();
        if log.contains("got: status; echo chained") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(log.contains("got: status; echo chained"), "the line reached the console: {log:?}");
    assert!(!log.contains("got: quit"), "nothing of the refused line arrived: {log:?}");
}
