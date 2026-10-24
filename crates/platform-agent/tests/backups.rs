//! Backups of a server's writable files (`ROADMAP.md` item 7, `backups.rs`),
//! against a real Docker daemon. Skips itself without Docker. Ports
//! 27235–27249 are its own, five per test.

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
async fn a_backup_brings_a_servers_files_back_and_only_while_it_is_stopped() {
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let Some(agent) = agent_in(dir.path(), 27235).await else { return };
    let id = format!("backup{}", std::process::id());
    let _cleanup = Container(id.clone());
    agent.create(spec(&id)).await.expect("create");

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

/// Pushes `last_unix` back, so the next pass finds the schedule due.
fn make_due(root: &std::path::Path, id: &str) {
    let path = root.join(format!("instances/{id}/{}", platform_agent::backups::SCHEDULE_FILE));
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    v["last_unix"] = serde_json::json!(v["last_unix"].as_i64().unwrap() - 2 * 3600);
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
}

/// A schedule backs up a running server, deletes only the backups it made
/// beyond `keep`, leaves a stopped server alone, and a removed server's
/// backups stay until the operator deletes them.
#[tokio::test(flavor = "multi_thread")]
async fn a_schedule_prunes_only_its_own_backups_and_a_removed_servers_stay() {
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let Some(agent) = agent_in(dir.path(), 27240).await else { return };
    let id = format!("bsched{}", std::process::id());
    let _cleanup = Container(id.clone());
    agent.create(spec(&id)).await.expect("create");

    let by_hand = agent.create_backup(&id).await.unwrap();
    let schedule = platform_agent::backups::BackupSchedule { every_hours: 1, keep: 1 };
    agent.set_backup_schedule(&id, Some(schedule)).await.unwrap();
    assert!(agent.set_backup_schedule("nobody", Some(schedule)).await.is_err());

    agent.scheduled_backups().await;
    let first = agent.backup_schedule(&id).await.unwrap().unwrap().made;
    assert_eq!(first.len(), 1, "due at once");
    agent.scheduled_backups().await;
    assert_eq!(agent.backups(&id).await.unwrap().len(), 2, "not due again within the hour");

    make_due(dir.path(), &id);
    agent.scheduled_backups().await;
    let names: Vec<String> = agent.backups(&id).await.unwrap().into_iter().map(|b| b.name).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&by_hand.name), "a backup made by hand is never pruned");
    assert!(!names.contains(&first[0]), "the schedule's oldest went");

    agent.stop(&id).await.unwrap();
    make_due(dir.path(), &id);
    let before = agent.backups(&id).await.unwrap();
    agent.scheduled_backups().await;
    assert_eq!(agent.backups(&id).await.unwrap(), before, "a stopped server is not backed up");

    assert!(agent.delete_removed_backups(&id).await.is_err(), "not while the server exists");
    agent.remove(&id).await.unwrap();
    let sets = agent.all_backups().await.unwrap();
    let set = sets.iter().find(|s| s.instance_id == id).expect("a removed server's backups stay");
    assert!(!set.server_exists);
    assert_eq!(set.backups.len(), 2);

    agent.delete_backup(&id, &by_hand.name).await.expect("one at a time, server gone");
    assert_eq!(agent.delete_removed_backups(&id).await.unwrap(), 1);
    assert!(agent.all_backups().await.unwrap().iter().all(|s| s.instance_id != id));
    assert!(!dir.path().join(format!("backups/{id}")).exists());
    assert!(agent.delete_removed_backups("../x").await.is_err());
}

/// A removed server's identity is its address, and nothing can use it again,
/// so it goes with the server. The folder (its world, its backups) stays for
/// the operator to delete. A move left the old worker holding the secret of a
/// server that was live somewhere else.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_servers_identity_goes_and_its_folder_stays() {
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let Some(agent) = agent_in(dir.path(), 27246).await else { return };
    let id = format!("bident{}", std::process::id());
    let _cleanup = Container(id.clone());
    agent.create(spec(&id)).await.expect("create");
    agent.stop(&id).await.unwrap();
    agent.set_identity(&id, &[7u8; 64]).await.expect("an identity to lose");
    let identity = platform_agent::mesh::identity_path(dir.path(), &id);
    assert!(identity.exists());

    agent.remove(&id).await.unwrap();
    assert!(!identity.exists(), "the removed server's address was left on disk");
    assert!(identity.parent().unwrap().exists(), "the folder is the operator's to delete");
}

/// A backup leaves the node as a file and comes back in as one, over the
/// HTTP API, and an upload that is not a backup is refused and leaves
/// nothing behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_backup_exported_as_a_file_imports_and_restores_and_a_stranger_file_does_not() {
    if !ensure_image() {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let Some(agent) = agent_in(dir.path(), 27245).await else { return };
    let agent = std::sync::Arc::new(agent);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = platform_agent::api::router(agent.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let http = reqwest::Client::new();
    let id = format!("bexport{}", std::process::id());
    let _cleanup = Container(id.clone());
    agent.create(spec(&id)).await.expect("create");
    let writable = dir.path().join(format!("instances/{id}/writable"));
    let slot =
        std::fs::read_dir(&writable).unwrap().flatten().next().expect("a writable slot").path();
    std::fs::write(slot.join("save.dat"), b"original").unwrap();
    let made = agent.create_backup(&id).await.unwrap();

    let base = format!("http://{addr}/instances/{id}/backups");
    let file = http.get(format!("{base}/{}/file", made.name)).send().await.unwrap();
    assert_eq!(file.status(), 200);
    assert!(file
        .headers()
        .get("content-disposition")
        .is_some_and(|v| v.to_str().unwrap().contains(&made.name)));
    let bytes = file.bytes().await.unwrap();
    assert_eq!(bytes.len() as u64, made.bytes);
    for hostile in ["..%2F..%2Fagent.toml", "x.tar.gz"] {
        let r = http.get(format!("{base}/{hostile}/file")).send().await.unwrap();
        assert_eq!(r.status(), 404, "{hostile}");
    }

    let upload = |body: Vec<u8>| http.post(format!("{base}/import")).body(body).send();
    let imported = upload(bytes.to_vec()).await.unwrap();
    assert_eq!(imported.status(), 200);
    let imported: serde_json::Value = imported.json().await.unwrap();
    let imported_name = imported["name"].as_str().unwrap().to_string();
    assert_ne!(imported_name, made.name, "an import gets a name of its own");

    // Not an archive, and an archive with something beside `writable`.
    let mut stranger = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut stranger, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        tar.append_dir_all("writable", &writable).unwrap();
        tar.append_dir_all("elsewhere", &writable).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    for body in [b"not a backup".to_vec(), stranger] {
        let r = upload(body).await.unwrap();
        assert_eq!(r.status(), 400);
    }
    let bdir = dir.path().join(format!("backups/{id}"));
    let left: Vec<String> = std::fs::read_dir(&bdir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left.len(), 2, "only the two real backups, no upload left: {left:?}");
    let nobody = http
        .post(format!("http://{addr}/instances/nobody/backups/import"))
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(nobody.status(), 409);

    agent.stop(&id).await.unwrap();
    std::fs::write(slot.join("save.dat"), b"changed").unwrap();
    agent.restore_backup(&id, &imported_name).await.expect("an imported backup restores");
    assert_eq!(std::fs::read(slot.join("save.dat")).unwrap(), b"original");
}
