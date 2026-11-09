//! A restarted agent knows its servers by name, against a real Docker daemon.
//!
//! The agent keeps the spec it was asked for in memory only, so before this a
//! restarted agent listed (and announced) every server by its instance id. The
//! name is on the container (`GPP_SERVER_NAME`); the list reads it back.
//! Skips itself without Docker.

use std::sync::Arc;

use platform_agent::agent::Agent;
use platform_agent::config::AgentConfig;
use platform_agent::instance::InstanceSpec;

const IMAGE: &str = "gpp-test-sleeper:latest";
const PORTS: (u16, u16) = (27250, 27254);

fn ensure_image() -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", IMAGE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(present, Ok(s) if s.success()) {
        return true;
    }
    let dir = std::env::temp_dir().join(format!("gpp-sleeper-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err()
        || std::fs::write(dir.join("Dockerfile"), "FROM busybox\nCMD [\"sleep\", \"600\"]\n").is_err()
    {
        return false;
    }
    let built = std::process::Command::new("docker")
        .args(["build", "-q", "-t", IMAGE, "."])
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

async fn agent_or_skip(dir: &std::path::Path) -> Option<Arc<Agent>> {
    if !ensure_image() {
        return None;
    }
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
image = "{IMAGE}"
content_root = "/game"
content_version = "test"
"#,
        dir.display(),
        PORTS.0,
        PORTS.1
    );
    let config = AgentConfig::parse(&toml).ok()?;
    Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await.ok().map(Arc::new)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_agent_still_knows_each_servers_name() {
    let dir = tempfile::tempdir().unwrap();
    let Some(agent) = agent_or_skip(dir.path()).await else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    let id = format!("named{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let outcome = async {
        agent
            .create(InstanceSpec {
                instance_id: id.clone(),
                game_id: "sven-coop".to_string(),
                name: "Friday night".to_string(),
                max_players: 8,
                port: None,
                extra_ports: Default::default(),
                map: None,
                bots: None,
                restart_on_crash: false,
                owner: None,
                options: Default::default(),
            })
            .await?;
        drop(agent);
        let again = agent_or_skip(dir.path()).await.ok_or_else(|| anyhow::anyhow!("no second agent"))?;
        let row = again.list().await?.into_iter().find(|i| i.instance_id == id);
        let listed = row.map(|r| r.name).unwrap_or_default();
        anyhow::ensure!(listed == "Friday night", "a restarted agent named the server {listed:?}");
        Ok::<(), anyhow::Error>(())
    }
    .await;
    docker_rm(&name);
    outcome.unwrap();
}
