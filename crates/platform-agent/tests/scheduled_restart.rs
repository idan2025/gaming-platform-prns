//! Daily restart windows (`ROADMAP.md` item 7, `schedule.rs`), against a real
//! Docker daemon. The stand-in server answers no query, so the game cannot
//! say whether anyone is playing, and echoes its stdin into its log, so the
//! warning shows there. Skips itself without Docker. Ports 27230–27234 are
//! its own.

use std::time::Duration;

use platform_agent::agent::{Agent, MoveTiming};
use platform_agent::config::AgentConfig;
use platform_agent::instance::InstanceSpec;
use platform_agent::schedule::RestartSchedule;

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
    let dir = std::env::temp_dir().join(format!("gpp-echo-sched-{}", std::process::id()));
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

fn started_at(id: &str) -> String {
    std::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.StartedAt}}", &format!("gpp-{id}")])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// A window open now: this hour to the next.
fn open_now(with_players: bool) -> RestartSchedule {
    let hour =
        (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
            % 86_400
            / 3600) as u8;
    RestartSchedule { start_utc: hour, end_utc: (hour + 1) % 24, with_players }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_restarts_in_its_window_only_when_allowed_and_only_once() {
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
start = 27230
end = 27234

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
    agent.set_move_timing(MoveTiming {
        settle: Duration::from_secs(1),
        warn: Duration::from_secs(1),
    });
    let id = format!("sched{}", std::process::id());
    let _cleanup = Container(id.clone());
    agent
        .create(InstanceSpec {
            instance_id: id.clone(),
            game_id: "sven-coop".into(),
            name: "schedule test".into(),
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
    let first = started_at(&id);

    // Waiting for empty: the game cannot be asked, and silence is not empty.
    agent.set_restart_schedule(&id, Some(open_now(false))).await.unwrap();
    agent.scheduled_restarts().await;
    assert_eq!(started_at(&id), first, "never restarted on silence");
    let status = agent.restart_schedules().await.unwrap();
    let waiting = status[&id].waiting.clone().expect("it says why");
    assert!(waiting.contains("cannot be asked"), "{waiting}");

    // Allowed with players on: warned, then restarted, once.
    agent.set_restart_schedule(&id, Some(open_now(true))).await.unwrap();
    agent.scheduled_restarts().await;
    let second = started_at(&id);
    assert_ne!(second, first, "restarted in its window");
    let log = agent.logs(&id, 50).await.unwrap();
    assert!(log.contains("got: say This server restarts"), "the players were told: {log:?}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    agent.scheduled_restarts().await;
    assert_eq!(started_at(&id), second, "once a window");
    assert!(agent.restart_schedules().await.unwrap()[&id].stored.last_unix.is_some());

    // A stopped server is never started by its schedule.
    agent.set_restart_schedule(&id, Some(open_now(true))).await.unwrap();
    let dir_file = dir.path().join(format!("instances/{id}/restart-schedule.json"));
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&dir_file).unwrap()).unwrap();
    stored["last_unix"] = serde_json::Value::Null;
    std::fs::write(&dir_file, serde_json::to_vec(&stored).unwrap()).unwrap();
    agent.stop(&id).await.unwrap();
    agent.scheduled_restarts().await;
    assert_eq!(
        agent.list().await.unwrap().iter().find(|i| i.instance_id == id).unwrap().state,
        platform_agent::instance::InstanceState::Stopped,
        "a Stop is never undone"
    );

    agent.set_restart_schedule(&id, None).await.unwrap();
    assert!(!agent.restart_schedules().await.unwrap().contains_key(&id));
}
