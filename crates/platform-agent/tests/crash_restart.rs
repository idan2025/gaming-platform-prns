//! Restart-on-crash, against a real Docker daemon.
//!
//! The stand-in server segfaults (exit 139) two seconds after it starts, the
//! way a game server that crashes does. A server that asked to be restarted is
//! started again by the agent's crash watch, and one that did not ask is left
//! stopped with its exit on record. Skips itself without Docker.

use std::sync::Arc;
use std::time::Duration;

use platform_agent::agent::Agent;
use platform_agent::config::AgentConfig;
use platform_agent::instance::{InstanceSpec, InstanceState};

const CRASHER: &str = "gpp-test-crasher:latest";
const PORTS_A: (u16, u16) = (27170, 27174);
const PORTS_B: (u16, u16) = (27175, 27179);

fn ensure_crasher() -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", CRASHER])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(present, Ok(s) if s.success()) {
        return true;
    }
    let dir = std::env::temp_dir().join(format!("gpp-crasher-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err()
        || std::fs::write(dir.join("Dockerfile"), "FROM busybox\nCMD [\"sh\", \"-c\", \"sleep 2; exit 139\"]\n").is_err()
    {
        return false;
    }
    let built = std::process::Command::new("docker")
        .args(["build", "-q", "-t", CRASHER, "."])
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_dir_all(&dir);
    matches!(built, Ok(s) if s.success())
}

fn docker_rm(name: &str) {
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn started_at(name: &str) -> String {
    std::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.StartedAt}}", name])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn spec(id: &str, restart_on_crash: bool) -> InstanceSpec {
    InstanceSpec {
        instance_id: id.to_string(),
        game_id: "sven-coop".to_string(),
        name: "crash test".to_string(),
        max_players: 8,
        port: None,
        extra_ports: Default::default(),
        map: None,
        bots: None,
        restart_on_crash,
        owner: None,
        options: Default::default(),
    }
}

/// `ports` keeps tests that run at once out of each other's way: they share one
/// Docker daemon, so two agents with one range would hand out the same port.
async fn agent_or_skip(dir: &std::path::Path, ports: (u16, u16)) -> Option<Arc<Agent>> {
    if !ensure_crasher() {
        return None;
    }
    // The content copy carries the writable mountpoints the Sven pack declares.
    let content = dir.join("content/sven-coop/test/svencoop");
    for sub in ["maps/soundcache", "logs", "scripts"] {
        std::fs::create_dir_all(content.join(sub)).ok()?;
    }
    let toml = format!(
        r#"
data_root = "{}"
max_instances = 4
api_bind = "127.0.0.1:0"

[port_range]
start = {}
end = {}

[games.sven-coop]
image = "{CRASHER}"
content_root = "/game"
content_version = "test"
"#,
        dir.display(),
        ports.0,
        ports.1
    );
    let config = AgentConfig::parse(&toml).ok()?;
    Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await.ok().map(Arc::new)
}

async fn wait_stopped(agent: &Agent, id: &str) -> bool {
    for _ in 0..60 {
        if agent.list().await.unwrap().iter().any(|i| i.instance_id == id && i.state == InstanceState::Stopped) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_server_that_asked_is_started_again_and_one_that_did_not_is_left() {
    let dir = tempfile::tempdir().unwrap();
    let Some(agent) = agent_or_skip(dir.path(), PORTS_A).await else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    let pid = std::process::id();
    let (on, off) = (format!("crashon{pid}"), format!("crashoff{pid}"));
    for id in [&on, &off] {
        docker_rm(&format!("gpp-{id}"));
    }

    let outcome = async {
        // The first pass only marks what is already stopped as seen.
        agent.check_crashes().await;
        agent.create(spec(&on, true)).await?;
        agent.create(spec(&off, false)).await?;
        assert!(wait_stopped(&agent, &on).await && wait_stopped(&agent, &off).await, "the stand-in never crashed");

        let listed = agent.list_detailed().await?;
        let exit = listed.iter().find(|i| i.instance_id == off).and_then(|i| i.last_exit.clone());
        let exit = exit.expect("a stopped server reports how it ended");
        assert_eq!(exit.code, 139);
        assert!(exit.is_crash());

        let before = started_at(&format!("gpp-{on}"));
        agent.check_crashes().await;
        let after = started_at(&format!("gpp-{on}"));
        assert_ne!(before, after, "the server that asked was not started again");
        let listed = agent.list_detailed().await?;
        let on_row = listed.iter().find(|i| i.instance_id == on).unwrap();
        assert_eq!(on_row.restart_on_crash, Some(true));
        assert_eq!(on_row.crash_restarts, Some(1));
        let off_row = listed.iter().find(|i| i.instance_id == off).unwrap();
        assert_eq!(off_row.restart_on_crash, Some(false));
        assert_eq!(off_row.state, InstanceState::Stopped, "a server that did not ask was started");

        // The same exit is never acted on twice: a second look restarts nothing.
        let again = started_at(&format!("gpp-{on}"));
        agent.check_crashes().await;
        assert_eq!(again, started_at(&format!("gpp-{on}")), "one crash was restarted twice");
        Ok::<(), anyhow::Error>(())
    }
    .await;

    for id in [&on, &off] {
        docker_rm(&format!("gpp-{id}"));
    }
    outcome.unwrap();
}

/// A crash loop ends: after the hourly allowance the server stays stopped.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_keeps_crashing_is_left_stopped_after_the_allowance() {
    let dir = tempfile::tempdir().unwrap();
    let Some(agent) = agent_or_skip(dir.path(), PORTS_B).await else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    let id = format!("crashloop{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let outcome = async {
        agent.check_crashes().await;
        agent.create(spec(&id, true)).await?;
        for round in 0..=platform_agent::agent::MAX_CRASH_RESTARTS_PER_HOUR {
            assert!(wait_stopped(&agent, &id).await, "round {round}: never crashed");
            agent.check_crashes().await;
        }
        let row = agent.list_detailed().await?.into_iter().find(|i| i.instance_id == id).unwrap();
        assert_eq!(row.crash_restarts, Some(platform_agent::agent::MAX_CRASH_RESTARTS_PER_HOUR as u32));
        assert!(wait_stopped(&agent, &id).await);
        let before = started_at(&name);
        tokio::time::sleep(Duration::from_millis(500)).await;
        agent.check_crashes().await;
        assert_eq!(before, started_at(&name), "restarted past the allowance");
        Ok::<(), anyhow::Error>(())
    }
    .await;
    docker_rm(&name);
    outcome.unwrap();
}
