//! A server's own files are held to its game's `writable_limit_bytes`: over
//! it, the server is stopped and refuses to start until it is back under,
//! and nothing is deleted. Needs Docker; skips without it.

use platform_agent::agent::Agent;
use platform_agent::config::AgentConfig;
use platform_agent::instance::InstanceSpec;

const GAME: &str = "gpp-test-echo:latest";
const GAME_DOCKERFILE: &str =
    "FROM busybox\nCMD [\"sh\", \"-c\", \"trap 'exit 0' TERM; while read -r l; do echo \\\"got: $l\\\"; done\"]\n";

fn ensure_image() -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", GAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(present, Ok(s) if s.success()) {
        return true;
    }
    let dir = std::env::temp_dir().join(format!("gpp-echo-disklimit-{}", std::process::id()));
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
            .output();
    }
}

async fn agent_in(root: &std::path::Path, first_port: u16) -> Option<Agent> {
    for sub in ["maps/soundcache", "logs", "scripts"] {
        std::fs::create_dir_all(root.join("content/sven-coop/test/svencoop").join(sub)).unwrap();
    }
    let toml = format!(
        r#"
data_root = "{}"
max_instances = 8
api_bind = "127.0.0.1:0"

[port_range]
start = {first_port}
end = {}

[games.sven-coop]
image = "{GAME}"
content_root = "/game"
content_version = "test"
writable_limit_bytes = 1024
"#,
        root.display(),
        first_port + 4
    );
    match Agent::new(AgentConfig::parse(&toml).unwrap(), vec![game_bridge::GamePack::sven_coop()])
        .await
    {
        Ok(agent) => Some(agent),
        Err(_) => {
            eprintln!("skipping: no Docker daemon");
            None
        }
    }
}

fn spec(id: &str) -> InstanceSpec {
    InstanceSpec {
        instance_id: id.to_string(),
        game_id: "sven-coop".into(),
        name: "backup test".into(),
        max_players: 8,
        port: None,
        extra_ports: Default::default(),
        bots: None,
        map: None,
        restart_on_crash: false,
        owner: None,
        options: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_over_its_disk_limit_is_stopped_and_stays_stopped_until_it_is_under() {
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let Some(agent) = agent_in(dir.path(), 27255).await else { return };
    let id = format!("disklimit{}", std::process::id());
    let _cleanup = Container(id.clone());
    agent.create(spec(&id)).await.expect("create");

    // Under the limit: nothing happens.
    assert!(agent.check_disk_limits().await.is_empty());

    // The game writes past it.
    let writable = dir.path().join(format!("instances/{id}/writable"));
    let slot = std::fs::read_dir(&writable).unwrap().flatten().next().expect("a writable slot").path();
    std::fs::write(slot.join("big.dat"), vec![0u8; 4096]).unwrap();
    assert_eq!(agent.check_disk_limits().await, vec![id.clone()], "a server over its limit was not stopped");
    assert_eq!(state(&agent, &id).await, platform_agent::instance::InstanceState::Stopped);

    // It does not come back while it is over, and its files are all there.
    let refused = agent.start(&id).await.expect_err("started while over its disk limit");
    assert!(format!("{refused:#}").contains("writable_limit_bytes"), "{refused:#}");
    assert!(slot.join("big.dat").exists(), "the limit deleted a file");

    // Back under, it starts.
    std::fs::remove_file(slot.join("big.dat")).unwrap();
    agent.start(&id).await.expect("start once under the limit");
    assert_eq!(state(&agent, &id).await, platform_agent::instance::InstanceState::Running);
}

async fn state(agent: &Agent, id: &str) -> platform_agent::instance::InstanceState {
    agent.list().await.unwrap().into_iter().find(|i| i.instance_id == id).unwrap().state
}
