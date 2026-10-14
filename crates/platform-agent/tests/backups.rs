//! Backups of a server's writable files (`ROADMAP.md` item 7, `backups.rs`),
//! against a real Docker daemon. Skips itself without Docker. Ports
//! 27235–27239 are its own.

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
    let dir = std::env::temp_dir().join(format!("gpp-echo-backup-{}", std::process::id()));
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

#[tokio::test(flavor = "multi_thread")]
async fn a_backup_brings_a_servers_files_back_and_only_while_it_is_stopped() {
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
start = 27235
end = 27239

[games.sven-coop]
image = "{GAME}"
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
    let id = format!("backup{}", std::process::id());
    let _cleanup = Container(id.clone());
    agent
        .create(InstanceSpec {
            instance_id: id.clone(),
            game_id: "sven-coop".into(),
            name: "backup test".into(),
            max_players: 8,
            port: None,
            extra_ports: Default::default(),
            bots: None,
            map: None,
            restart_on_crash: false,
            owner: None,
        })
        .await
        .expect("create");

    // A file the game wrote into one of its writable paths.
    let writable = dir.path().join(format!("instances/{id}/writable"));
    let slot =
        std::fs::read_dir(&writable).unwrap().flatten().next().expect("a writable slot").path();
    std::fs::write(slot.join("save.dat"), b"before").unwrap();

    let made = agent.create_backup(&id).await.expect("a backup of a running server");
    assert_eq!(agent.backups(&id).await.unwrap().len(), 1);
    std::fs::write(slot.join("save.dat"), b"after").unwrap();

    let refused = agent.restore_backup(&id, &made.name).await;
    assert!(refused.is_err(), "never under a running game");
    assert_eq!(std::fs::read(slot.join("save.dat")).unwrap(), b"after");

    agent.stop(&id).await.unwrap();
    for hostile in ["../../agent.toml", "x.tar.gz", "20261014-051002.tar.gz"] {
        assert!(agent.restore_backup(&id, hostile).await.is_err(), "{hostile:?}");
        assert!(agent.delete_backup(&id, hostile).await.is_err(), "{hostile:?}");
    }
    agent.restore_backup(&id, &made.name).await.expect("restore while stopped");
    assert_eq!(std::fs::read(slot.join("save.dat")).unwrap(), b"before");

    // The server starts again on the restored files: its mounts all have a
    // source.
    agent.restart(&id).await.expect("starts on the restored files");
    let running = agent.list().await.unwrap().into_iter().find(|i| i.instance_id == id).unwrap();
    assert_eq!(running.state, platform_agent::instance::InstanceState::Running);

    agent.delete_backup(&id, &made.name).await.unwrap();
    assert!(agent.backups(&id).await.unwrap().is_empty());
    assert!(agent.backups("nobody").await.is_err());
}
