//! Integration tests against a real Docker daemon.
//!
//! These skip themselves when Docker is unavailable rather than failing, so the
//! suite still runs on a machine without it. When Docker *is* available they
//! exercise the property that matters most in this crate: **the agent must
//! never touch a container it did not create.**
//!
//! The machine this was written on was already running unrelated containers, and
//! that is the normal case for a node someone volunteers. It is exactly why the
//! guard reads an inspected label rather than trusting a name prefix.

use std::collections::BTreeMap;

use platform_agent::config::{GameRuntime, MANAGED_LABEL};
use platform_agent::docker::DockerRuntime;
use platform_agent::instance::{InstanceSpec, InstanceState};
use platform_agent::store::Mount;

const TEST_IMAGE: &str = "gpp-test-sleeper:latest";

async fn runtime_or_skip() -> Option<DockerRuntime> {
    let rt = DockerRuntime::connect().ok()?;
    rt.ping().await.ok()?;
    Some(rt)
}

/// A tiny local image that stays up, built from the busybox already on the host
/// so the test needs no network.
fn ensure_test_image() -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", TEST_IMAGE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(present, Ok(s) if s.success()) {
        return true;
    }
    let dir = std::env::temp_dir().join(format!("gpp-test-image-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    if std::fs::write(dir.join("Dockerfile"), "FROM busybox\nCMD [\"sleep\", \"3600\"]\n").is_err() {
        return false;
    }
    let built = std::process::Command::new("docker")
        .args(["build", "-q", "-t", TEST_IMAGE, "."])
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

fn test_runtime() -> GameRuntime {
    GameRuntime {
        image: TEST_IMAGE.to_string(),
        content_root: "/game".into(),
        content_version: "test".to_string(),
        memory_limit_bytes: Some(64 * 1024 * 1024),
        cpus: Some(0.5),
        env: BTreeMap::new(),
    }
}

fn spec(id: &str) -> InstanceSpec {
    InstanceSpec {
        instance_id: id.to_string(),
        game_id: "sven-coop".to_string(),
        name: "Test".to_string(),
        max_players: 8,
        port: None,
    }
}

/// The guard, tested for real: a container carrying this agent's *name prefix*
/// but not its label must be refused, and left completely alone. A name prefix
/// is a convention anyone can collide with; the label is the only thing this
/// agent actually sets.
#[tokio::test(flavor = "multi_thread")]
async fn refuses_to_touch_a_container_it_did_not_create() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let id = format!("guardtest{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let created = std::process::Command::new("docker")
        .args(["create", "--name", &name, "busybox", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(created, Ok(s) if s.success()) {
        eprintln!("skipping: could not create the fixture container");
        return;
    }

    let stop = rt.stop(&id).await;
    let remove = rt.remove(&id).await;
    let survived = matches!(
        std::process::Command::new("docker")
            .args(["inspect", &name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
        Ok(s) if s.success()
    );
    let managed = rt.list_managed().await.unwrap();
    docker_rm(&name);

    assert!(stop.is_err(), "the agent stopped a container it does not manage");
    assert!(remove.is_err(), "the agent removed a container it does not manage");
    assert!(survived, "the unmanaged container should have been left alone entirely");
    assert!(
        !managed.iter().any(|c| c.instance_id == id),
        "an unlabelled container turned up in the managed list"
    );
}

/// Whatever else is running on this host must be invisible to the agent.
#[tokio::test(flavor = "multi_thread")]
async fn other_peoples_containers_are_not_listed() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let Ok(all) = std::process::Command::new("docker").args(["ps", "-aq"]).output() else {
        return;
    };
    let total = String::from_utf8_lossy(&all.stdout).lines().count();
    let managed = rt.list_managed().await.unwrap();
    assert!(managed.len() <= total, "the agent claims more containers than exist");
    for c in &managed {
        assert!(!c.instance_id.is_empty(), "a managed container with no instance label");
    }
}

/// The full lifecycle against a real daemon: create with mounts and a port
/// binding, see it running, stop it, remove it.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_can_be_created_started_stopped_and_removed() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        eprintln!("skipping: could not build the test image");
        return;
    }

    let id = format!("life{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let dir = std::env::temp_dir().join(format!("gpp-life-{}", std::process::id()));
    let content = dir.join("content");
    let writable = dir.join("writable");
    // The mountpoint must exist inside the read-only content copy: a writable
    // bind nested in a read-only bind cannot have its mountpoint created by the
    // container runtime. A real game install already has these directories.
    std::fs::create_dir_all(content.join("logs")).unwrap();
    std::fs::create_dir_all(&writable).unwrap();

    let mounts = vec![
        Mount { host_path: content.clone(), container_path: "/game".into(), read_only: true },
        Mount { host_path: writable.clone(), container_path: "/game/logs".into(), read_only: false },
    ];

    let outcome = async {
        rt.create_and_start(&spec(&id), &test_runtime(), &mounts, 27199).await?;
        assert_eq!(rt.state_of(&id).await?, InstanceState::Running, "the instance did not come up");

        let managed = rt.list_managed().await?;
        assert!(
            managed.iter().any(|c| c.instance_id == id),
            "the agent's own container is missing from its inventory"
        );

        rt.stop(&id).await?;
        assert_eq!(rt.state_of(&id).await?, InstanceState::Stopped);
        rt.remove(&id).await?;
        assert_eq!(rt.state_of(&id).await?, InstanceState::Missing);
        Ok::<(), anyhow::Error>(())
    }
    .await;

    docker_rm(&name);
    let _ = std::fs::remove_dir_all(&dir);
    outcome.expect("the instance lifecycle should complete");
}

/// The label is what the guard reads, so it had better be on the container the
/// agent itself creates — otherwise the agent would refuse to manage its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_created_container_carries_the_managed_label() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        return;
    }
    let id = format!("label{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let created = rt.create_and_start(&spec(&id), &test_runtime(), &[], 27198).await;
    let labels = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{json .Config.Labels}}", &name])
        .output();
    docker_rm(&name);

    created.expect("the container should have been created");
    let text = String::from_utf8_lossy(&labels.expect("docker inspect should run").stdout).to_string();
    assert!(
        text.contains(MANAGED_LABEL),
        "the managed label is missing, so the guard would refuse to manage our own container: {text}"
    );
}

/// The read-only content mount must actually be read-only in the container. If
/// it were writable, one instance could corrupt the copy every other instance
/// on the node is running from.
#[tokio::test(flavor = "multi_thread")]
async fn the_shared_content_mount_is_read_only_in_the_container() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        return;
    }
    let id = format!("ro{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let dir = std::env::temp_dir().join(format!("gpp-ro-{}", std::process::id()));
    let content = dir.join("content");
    std::fs::create_dir_all(&content).unwrap();

    let mounts = vec![Mount {
        host_path: content.clone(),
        container_path: "/game".into(),
        read_only: true,
    }];

    let created = rt.create_and_start(&spec(&id), &test_runtime(), &mounts, 27197).await;
    let write_attempt = std::process::Command::new("docker")
        .args(["exec", &name, "sh", "-c", "touch /game/scribble"])
        .output();
    docker_rm(&name);
    let host_file_appeared = content.join("scribble").exists();
    let _ = std::fs::remove_dir_all(&dir);

    created.expect("the container should have been created");
    let out = write_attempt.expect("docker exec should run");
    assert!(
        !out.status.success(),
        "a write to the shared content mount succeeded; every instance on this node shares it"
    );
    assert!(!host_file_appeared, "the write reached the host's shared content directory");
}

/// The bug an integration test found and a unit test could not: a writable bind
/// nested inside a read-only bind fails to start unless its mountpoint already
/// exists in the read-only source. runc reports it as
/// `mkdirat ... read-only file system`, which says nothing about game content —
/// so the agent has to check for it and say something that does.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_mountpoint_in_shared_content_fails_to_start() {
    let Some(rt) = runtime_or_skip().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    if !ensure_test_image() {
        return;
    }
    let id = format!("nomount{}", std::process::id());
    let name = format!("gpp-{id}");
    docker_rm(&name);

    let dir = std::env::temp_dir().join(format!("gpp-nomount-{}", std::process::id()));
    let content = dir.join("content");
    let writable = dir.join("writable");
    // Deliberately do NOT create content/logs.
    std::fs::create_dir_all(&content).unwrap();
    std::fs::create_dir_all(&writable).unwrap();

    let mounts = vec![
        Mount { host_path: content.clone(), container_path: "/game".into(), read_only: true },
        Mount { host_path: writable.clone(), container_path: "/game/logs".into(), read_only: false },
    ];
    let result = rt.create_and_start(&spec(&id), &test_runtime(), &mounts, 27196).await;
    docker_rm(&name);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        result.is_err(),
        "a writable mount nested in a read-only one started despite a missing mountpoint; \
         if the runtime has changed, `missing_content_dirs` may no longer be needed"
    );
}

/// The check that turns that opaque runtime failure into a sentence naming the
/// path a game install is missing.
#[test]
fn missing_content_dirs_names_what_is_absent() {
    use platform_agent::store::{ContentRef, StoreLayout};

    let dir = std::env::temp_dir().join(format!("gpp-missing-{}", std::process::id()));
    let content = dir.join("content").join("sven-coop").join("1.0");
    std::fs::create_dir_all(content.join("svencoop/maps")).unwrap();

    let layout = StoreLayout::new(&dir);
    let plan = layout
        .plan_instance(
            "inst1",
            &ContentRef { game_id: "sven-coop".into(), version: "1.0".into() },
            std::path::Path::new("/game"),
            &["svencoop/maps".to_string(), "svencoop/logs".to_string()],
        )
        .unwrap();

    let missing = StoreLayout::missing_content_dirs(&plan);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(missing.len(), 1, "only the absent one should be reported");
    assert!(missing[0].ends_with("svencoop/logs"));
}
