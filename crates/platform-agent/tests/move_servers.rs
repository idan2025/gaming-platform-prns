//! Moving servers onto a new copy of their game (`ROADMAP.md` §12.4), against
//! a real Docker daemon.
//!
//! The stand-in server runs until it is stopped, unless its content copy holds
//! a file named `broken`, in which case it exits 3 at once: a game that does
//! not come up on the new copy. Two copies of Sven Co-op, `old` and `new`; a
//! server is created by an agent configured for `old`, and moved by a second
//! agent configured for `new` — containers outlive the agent, which is how a
//! node meets a server on the old copy after an update. Skips itself without
//! Docker. Ports 27210–27224 are its own.

use std::sync::Arc;
use std::time::Duration;

use platform_agent::agent::{Agent, MoveOutcome, MoveTiming};
use platform_agent::config::AgentConfig;
use platform_agent::instance::{InstanceSpec, InstanceState};

const GAME: &str = "gpp-test-mover:latest";
const GAME_DOCKERFILE: &str = "FROM busybox\nCMD [\"sh\", \"-c\", \"[ -e /game/broken ] && exit 3; trap 'exit 0' TERM; while true; do sleep 1; done\"]\n";

fn ensure_image() -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", GAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(present, Ok(s) if s.success()) {
        return true;
    }
    let dir = std::env::temp_dir().join(format!("gpp-mover-{}", std::process::id()));
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

/// Removes the test's containers however it ends.
struct Containers(Vec<String>);

impl Drop for Containers {
    fn drop(&mut self) {
        for id in &self.0 {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &format!("gpp-{id}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

fn copy(dir: &std::path::Path, version: &str) {
    let content = dir.join(format!("content/sven-coop/{version}/svencoop"));
    for sub in ["maps/soundcache", "logs", "scripts"] {
        std::fs::create_dir_all(content.join(sub)).unwrap();
    }
}

async fn agent(dir: &std::path::Path, version: &str, ports: (u16, u16)) -> Option<Arc<Agent>> {
    let toml = format!(
        r#"
data_root = "{}"
max_instances = 4
api_bind = "127.0.0.1:0"

[port_range]
start = {}
end = {}

[games.sven-coop]
image = "{GAME}"
content_root = "/game"
content_version = "{version}"
"#,
        dir.display(),
        ports.0,
        ports.1
    );
    let config = AgentConfig::parse(&toml).ok()?;
    let agent = Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await.ok()?;
    agent.set_move_timing(MoveTiming {
        settle: Duration::from_secs(3),
        warn: Duration::from_secs(1),
    });
    Some(Arc::new(agent))
}

fn spec(id: &str) -> InstanceSpec {
    InstanceSpec {
        instance_id: id.to_string(),
        game_id: "sven-coop".to_string(),
        name: "move test".to_string(),
        max_players: 8,
        port: None,
        extra_ports: Default::default(),
        map: Some("stadium4".to_string()),
        bots: None,
        restart_on_crash: false,
        owner: Some("abcd".to_string()),
        options: Default::default(),
    }
}

/// What Docker says the container mounts as the game's files.
fn mounted_copy(id: &str) -> String {
    let out = std::process::Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{range .Mounts}}{{if eq .Destination \"/game\"}}{{.Source}}{{end}}{{end}}",
            &format!("gpp-{id}"),
        ])
        .output()
        .expect("docker inspect");
    let source = String::from_utf8_lossy(&out.stdout).trim().to_string();
    source.rsplit('/').next().unwrap_or_default().to_string()
}

fn env_of(id: &str) -> String {
    let out = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{json .Config.Env}}", &format!("gpp-{id}")])
        .output()
        .expect("docker inspect");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A server on the old copy is recreated on the new one with the same id, so
/// the same folder and mesh identity, the same ports, name, map and owner.
#[tokio::test(flavor = "multi_thread")]
async fn a_moved_server_keeps_its_id_ports_folder_and_settings() {
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    copy(dir.path(), "old");
    copy(dir.path(), "new");
    let id = format!("mover{}", std::process::id());
    let _cleanup = Containers(vec![id.clone()]);
    let Some(before) = agent(dir.path(), "old", (27210, 27214)).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let created = before.create(spec(&id)).await.expect("create on the old copy");
    assert_eq!(mounted_copy(&id), "old");
    // The mesh identity's file stands for the whole instance folder: a move
    // that deleted the folder would give the server a new address.
    let identity = platform_agent::mesh::identity_path(dir.path(), &id);
    std::fs::create_dir_all(identity.parent().unwrap()).unwrap();
    std::fs::write(&identity, b"the same server").unwrap();
    drop(before);

    let after = agent(dir.path(), "new", (27210, 27214)).await.unwrap();
    let outcome = after.move_instance(&id, false).await.expect("the move");
    assert_eq!(outcome, MoveOutcome::Moved { from: "old".into(), to: "new".into() });
    assert_eq!(mounted_copy(&id), "new");
    let listed = after.list().await.unwrap();
    let moved = listed.iter().find(|i| i.instance_id == id).expect("same id");
    assert_eq!(moved.state, InstanceState::Running);
    assert_eq!(moved.port, created.port, "same port");
    assert_eq!(moved.owner.as_deref(), Some("abcd"));
    assert_eq!(moved.name, "move test");
    assert!(env_of(&id).contains("GPP_MAP=stadium4"), "same map");
    assert_eq!(std::fs::read(&identity).unwrap(), b"the same server", "same folder, same identity");
    assert!(dir.path().join("content/sven-coop/old").is_dir(), "a move deletes nothing");

    let again = after.move_instance(&id, false).await;
    assert!(again.is_err(), "already on the new copy: {again:?}");
}

/// A server that does not stay up on the new copy is put back on the old one,
/// and the old copy cannot be deleted while that is happening.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_fails_on_the_new_copy_is_put_back_and_its_copy_is_kept() {
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    copy(dir.path(), "old");
    copy(dir.path(), "new");
    std::fs::write(dir.path().join("content/sven-coop/new/broken"), b"").unwrap();
    let id = format!("rollback{}", std::process::id());
    let _cleanup = Containers(vec![id.clone()]);
    let Some(before) = agent(dir.path(), "old", (27215, 27219)).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let created = before.create(spec(&id)).await.expect("create on the old copy");
    drop(before);

    let after = agent(dir.path(), "new", (27215, 27219)).await.unwrap();
    let mover = {
        let after = after.clone();
        let id = id.clone();
        tokio::spawn(async move { after.move_instance(&id, false).await })
    };
    // While the move runs, neither copy can be deleted, though for a moment no
    // container mounts the old one.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let deleted = after.delete_content("sven-coop", "old").await;
    assert!(deleted.is_err(), "the old copy was deletable mid-move: {deleted:?}");

    let outcome = mover.await.unwrap().expect("a rollback is an outcome, not an error");
    let MoveOutcome::RolledBack { why, .. } = outcome else {
        panic!("expected a rollback, got {outcome:?}")
    };
    assert!(why.contains("exit 3"), "{why}");
    assert_eq!(mounted_copy(&id), "old", "back on the old copy");
    let listed = after.list().await.unwrap();
    let back = listed.iter().find(|i| i.instance_id == id).unwrap();
    assert_eq!(back.state, InstanceState::Running);
    assert_eq!(back.port, created.port);
    let record = after.moves().into_iter().find(|r| r.instance_id == id).unwrap();
    assert!(
        matches!(record.state, platform_agent::agent::MoveState::RolledBack { .. }),
        "{record:?}"
    );

    // And the pass does not try the same copy again.
    after
        .set_update_policy(
            "sven-coop",
            serde_json::from_value(serde_json::json!({ "move_servers": "when_empty" })).unwrap(),
        )
        .await
        .unwrap();
    after.move_servers_pass().await;
    assert_eq!(mounted_copy(&id), "old");
}

/// "When empty" never moves a server it cannot ask how many are playing: the
/// stand-in answers no query, so it stays, and the page says why.
#[tokio::test(flavor = "multi_thread")]
async fn when_empty_never_moves_a_server_that_cannot_be_asked() {
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    copy(dir.path(), "old");
    copy(dir.path(), "new");
    let id = format!("silent{}", std::process::id());
    let _cleanup = Containers(vec![id.clone()]);
    let Some(before) = agent(dir.path(), "old", (27220, 27224)).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    before.create(spec(&id)).await.expect("create on the old copy");
    drop(before);

    let after = agent(dir.path(), "new", (27220, 27224)).await.unwrap();
    after.move_servers_pass().await;
    assert!(after.moves().is_empty(), "never is the default: nothing is even considered");

    after
        .set_update_policy(
            "sven-coop",
            serde_json::from_value(serde_json::json!({ "move_servers": "when_empty" })).unwrap(),
        )
        .await
        .unwrap();
    after.move_servers_pass().await;
    assert_eq!(mounted_copy(&id), "old", "silence is not zero");
    let record = after.moves().into_iter().find(|r| r.instance_id == id).expect("a waiting record");
    let platform_agent::agent::MoveState::Waiting { why } = &record.state else {
        panic!("{record:?}")
    };
    assert!(why.contains("cannot be asked"), "{why}");
}
