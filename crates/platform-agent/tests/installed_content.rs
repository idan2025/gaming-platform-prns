//! The Installed page's routes, against a real Docker daemon (`ROADMAP.md` §11).
//!
//! Skips itself without Docker, like the other integration tests here, and
//! builds its stand-in images from busybox rather than pulling anything. Its
//! port range (27180–27184) is its own: tests that run at once share one
//! daemon.

use std::sync::Arc;
use std::time::Duration;

use platform_agent::agent::Agent;
use platform_agent::api;
use platform_agent::config::AgentConfig;
use platform_agent::instance::InstanceSpec;
use serde_json::Value;

const SLEEPER: &str = "gpp-test-sleeper:latest";
/// Stands in for steamcmd, slowly: about a megabyte every half second into
/// whatever `+force_install_dir` named, twelve times, so a test can watch the
/// bytes grow and cancel part-way.
const SLOW_STEAMCMD: &str = "gpp-test-steamcmd-slow:latest";
const SLOW_DOCKERFILE: &str = r#"FROM busybox
RUN printf '#!/bin/sh\nwhile [ "$1" != "+force_install_dir" ]; do shift || exit 3; done\nshift\nmkdir -p "$1/svencoop/maps/soundcache" "$1/svencoop/logs"\nfor i in 1 2 3 4 5 6 7 8 9 10 11 12; do dd if=/dev/zero of="$1/part$i" bs=1024 count=1024 2>/dev/null; sleep 0.5; done\n' > /fake && chmod +x /fake
ENTRYPOINT ["/fake"]
"#;

fn ensure_image(tag: &str, dockerfile: &str) -> bool {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(present, Ok(s) if s.success()) {
        return true;
    }
    let dir = std::env::temp_dir().join(format!(
        "gpp-installed-image-{}-{}",
        std::process::id(),
        tag.replace([':', '/'], "-")
    ));
    if std::fs::create_dir_all(&dir).is_err() || std::fs::write(dir.join("Dockerfile"), dockerfile).is_err() {
        return false;
    }
    let built = std::process::Command::new("docker")
        .args(["build", "-q", "-t", tag, "."])
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

/// Removes its containers when dropped, so a test that fails part-way —
/// an assert panicking inside it — still leaves no server on the daemon. A
/// cleanup after the test body is skipped by exactly that panic, and once
/// left a stopped server on a real node's page.
struct Containers(Vec<String>);

impl Drop for Containers {
    fn drop(&mut self) {
        for id in &self.0 {
            docker_rm(&format!("gpp-{id}"));
        }
    }
}

fn task_containers() -> String {
    let out = std::process::Command::new("docker")
        .args(["ps", "-a", "--filter", "name=gpp-content-276060-", "--format", "{{.Names}}"])
        .output()
        .expect("docker ps");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The task containers once Docker's list has caught up with a removal. The
/// list lags a remove by a moment (seen on the ARM runner), so an empty answer
/// is waited for, up to 5 s; a container that is really left behind still
/// fails.
async fn task_containers_settled() -> String {
    let mut left = task_containers();
    for _ in 0..50 {
        if left.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        left = task_containers();
    }
    left
}

/// An agent whose Sven Co-op runtime points at `version`, with an installed
/// copy at `content/sven-coop/test` carrying the pack's writable mountpoints.
async fn serve(
    dir: &std::path::Path,
    version: &str,
) -> Option<(Arc<Agent>, std::net::SocketAddr)> {
    serve_with(dir, version, SLOW_STEAMCMD, SLOW_DOCKERFILE, (27180, 27184)).await
}

async fn serve_with(
    dir: &std::path::Path,
    version: &str,
    steamcmd: &str,
    steamcmd_dockerfile: &str,
    ports: (u16, u16),
) -> Option<(Arc<Agent>, std::net::SocketAddr)> {
    if !ensure_image(SLEEPER, "FROM busybox\nCMD [\"sleep\", \"3600\"]\n")
        || !ensure_image(steamcmd, steamcmd_dockerfile)
    {
        return None;
    }
    let content = dir.join("content/sven-coop/test/svencoop");
    for sub in ["maps/soundcache", "logs"] {
        std::fs::create_dir_all(content.join(sub)).ok()?;
    }
    std::fs::write(content.join("game.bin"), vec![0u8; 4096]).ok()?;
    let toml = format!(
        r#"
data_root = "{}"
max_instances = 8
api_bind = "127.0.0.1:0"
allow_content_fetch = true
steamcmd_image = "{steamcmd}"

[port_range]
start = {}
end = {}

[games.sven-coop]
image = "{SLEEPER}"
content_root = "/game"
content_version = "{version}"
"#,
        dir.display(),
        ports.0,
        ports.1
    );
    let config = AgentConfig::parse(&toml).ok()?;
    let agent = Arc::new(Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await.ok()?);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let addr = listener.local_addr().ok()?;
    let router = api::router(agent.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Some((agent, addr))
}

fn spec(id: &str) -> InstanceSpec {
    InstanceSpec {
        instance_id: id.to_string(),
        game_id: "sven-coop".to_string(),
        name: "installed test".to_string(),
        max_players: 8,
        port: None,
        extra_ports: Default::default(),
        map: None,
        bots: None,
        restart_on_crash: false,
        owner: None,
    }
}

async fn get(addr: std::net::SocketAddr, path: &str) -> Value {
    reqwest::get(format!("http://{addr}{path}")).await.unwrap().json().await.unwrap()
}

async fn delete(addr: std::net::SocketAddr, path: &str) -> (u16, Value) {
    let r = reqwest::Client::new().delete(format!("http://{addr}{path}")).send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// **A copy a container mounts is never deleted, running or stopped.** A
/// stopped server restarts from the same mount, so "nothing is running on
/// it" is not "nothing uses it".
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_a_stopped_server_mounts_is_not_deleted_until_the_server_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let Some((agent, addr)) = serve(dir.path(), "test").await else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    let id = format!("installed{}", std::process::id());
    docker_rm(&format!("gpp-{id}"));
    let _cleanup = Containers(vec![id.clone()]);

    let outcome = async {
        agent.create(spec(&id)).await?;
        agent.stop(&id).await?;

        let listed = get(addr, "/content").await;
        let row = &listed["copies"][0];
        assert_eq!(row["game_id"], "sven-coop");
        assert_eq!(row["version"], "test");
        assert_eq!(row["configured"], true);
        assert_eq!(row["unused"], false);
        assert_eq!(row["used_by"], serde_json::json!([id]), "a stopped server still uses its copy");
        assert!(row["bytes"].as_u64().unwrap() >= 4096, "{row}");

        let (status, body) = delete(addr, "/content/sven-coop/test").await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["servers"], serde_json::json!([id]), "the page needs the servers to name");
        assert!(dir.path().join("content/sven-coop/test").is_dir(), "refused, yet deleted");

        agent.remove(&id).await?;
        let (status, body) = delete(addr, "/content/sven-coop/test").await;
        assert_eq!(status, 200, "{body}");
        assert!(body["bytes"].as_u64().unwrap() >= 4096, "{body}");
        assert!(!dir.path().join("content/sven-coop/test").exists());
        assert_eq!(get(addr, "/content").await["copies"], serde_json::json!([]));
        anyhow::Ok(())
    }
    .await;
    outcome.unwrap();
}

/// A copy nothing names and nothing mounts is the case the page exists for:
/// it is marked unused and listed first.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_nothing_names_or_mounts_is_unused_and_listed_first() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("content/sven-coop/old")).unwrap();
    let Some((_agent, addr)) = serve(dir.path(), "test").await else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    let listed = get(addr, "/content").await;
    let rows = listed["copies"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{listed}");
    assert_eq!(rows[0]["version"], "old");
    assert_eq!(rows[0]["unused"], true);
    assert_eq!(rows[1]["version"], "test");
    assert_eq!(rows[1]["unused"], false);
}

/// The path is built from validated ids, never taken from the request. Each of
/// these is refused before anything touches the disk.
#[tokio::test(flavor = "multi_thread")]
async fn a_hostile_game_or_version_is_refused_never_turned_into_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let Some((_agent, addr)) = serve(dir.path(), "test").await else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    // Something a `..` could reach, to prove it was not reached.
    let bait = dir.path().join("content/bait");
    std::fs::create_dir_all(&bait).unwrap();
    let long = "a".repeat(200);
    for path in [
        "/content/sven-coop/%2E%2E".to_string(),
        "/content/%2E%2E/bait".to_string(),
        "/content/sven-coop/..%2F..%2Fbait".to_string(),
        "/content/sven-coop/%20".to_string(),
        format!("/content/sven-coop/{long}"),
    ] {
        let (status, body) = delete(addr, &path).await;
        assert!(status == 400 || status == 404, "{path} answered {status}: {body}");
    }
    assert!(bait.is_dir(), "a hostile id reached the disk");
    assert!(dir.path().join("content/sven-coop/test").is_dir());
}

/// Progress is bytes on disk, and grows; no total is invented; a delete is
/// refused while the game downloads; a cancel leaves nothing behind and
/// removes the task's container; and the next install still works.
#[tokio::test(flavor = "multi_thread")]
async fn a_download_shows_growing_bytes_and_a_cancel_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let Some((_agent, addr)) = serve(dir.path(), "fresh").await else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    let client = reqwest::Client::new();
    let started: Value = client
        .post(format!("http://{addr}/content/sven-coop"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(started["started"], true, "{started}");

    // Bytes grow, measured on disk.
    let mut readings = Vec::new();
    for _ in 0..40 {
        let st = get(addr, "/content/sven-coop").await;
        let status = &st["status"];
        assert_eq!(status["state"], "running", "{st}");
        assert!(status.get("total_bytes").is_none(), "an invented total: {st}");
        readings.push(status["bytes"].as_u64().unwrap());
        if readings.iter().filter(|b| **b > 0).count() >= 2 && readings.last() > readings.first() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(readings.last().unwrap() > &0, "bytes never moved: {readings:?}");

    // The page's one read lists the download beside the copies.
    let listed = get(addr, "/content").await;
    assert_eq!(listed["downloads"][0]["game"], "sven-coop", "{listed}");
    assert_eq!(listed["downloads"][0]["status"]["state"], "running", "{listed}");

    // No delete of any copy of a game that is downloading.
    let (status, body) = delete(addr, "/content/sven-coop/test").await;
    assert_eq!(status, 409, "{body}");
    assert!(dir.path().join("content/sven-coop/test").is_dir());

    let (status, body) = delete(addr, "/content/sven-coop").await;
    assert_eq!(status, 200, "{body}");
    let mut ended = Value::Null;
    for _ in 0..80 {
        ended = get(addr, "/content/sven-coop").await;
        if ended["status"]["state"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(ended["status"]["state"], "failed", "{ended}");
    assert_eq!(ended["status"]["error"], "cancelled", "{ended}");
    assert!(!dir.path().join("content/sven-coop/fresh").exists(), "a cancelled download installed");
    let staging: Vec<_> = std::fs::read_dir(dir.path().join("staging"))
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(staging.is_empty(), "staging left behind: {staging:?}");
    assert_eq!(task_containers_settled().await, "", "the task container outlived its cancel");

    // A cancel is not a wedge: the next install runs to the end.
    let (status, _) = delete(addr, "/content/sven-coop").await;
    assert_eq!(status, 404, "nothing is downloading, so nothing to cancel");
    client.post(format!("http://{addr}/content/sven-coop")).send().await.unwrap();
    let mut done = Value::Null;
    for _ in 0..120 {
        done = get(addr, "/content/sven-coop").await;
        if done["status"]["state"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(done["status"]["state"], "done", "{done}");
    assert!(dir.path().join("content/sven-coop/fresh/part12").is_file());
}

/// Stands in for steamcmd's update: refuses `validate` (it would put back the
/// files an operator changed, such as YaPB's `liblist.gam`), adds a file, and
/// writes Steam's manifest at build 200.
const UPDATE_STEAMCMD: &str = "gpp-test-steamcmd-update:latest";
const UPDATE_DOCKERFILE: &str = r#"FROM busybox
RUN printf '#!/bin/sh\nfor a in "$@"; do [ "$a" = validate ] && { echo "validate on an update"; exit 9; }; done\nwhile [ "$1" != "+force_install_dir" ]; do shift || exit 3; done\nshift\nmkdir -p "$1/steamapps"\necho updated > "$1/updated.txt"\nprintf "\\"AppState\\"\\n{\\n\\t\\"buildid\\"\\t\\t\\"200\\"\\n}\\n" > "$1/steamapps/appmanifest_276060.acf"\n' > /fake && chmod +x /fake
ENTRYPOINT ["/fake"]
"#;

fn seed_manifest(dir: &std::path::Path, build: &str) {
    let apps = dir.join("content/sven-coop/test/steamapps");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(
        apps.join("appmanifest_276060.acf"),
        format!("\"AppState\"\n{{\n\t\"buildid\"\t\t\"{build}\"\n}}\n"),
    )
    .unwrap();
}

async fn wait_install(addr: std::net::SocketAddr) -> Value {
    let mut st = Value::Null;
    for _ in 0..120 {
        st = get(addr, "/content/sven-coop").await;
        if st["status"]["state"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    st
}

/// An update makes a new version from the old copy (keeping what the operator
/// added), points new servers at it, and with "remove old" deletes the old
/// copy only once the stopped server still mounting it is removed.
#[tokio::test(flavor = "multi_thread")]
async fn an_update_moves_new_servers_and_removes_the_old_copy_once_unused() {
    let dir = tempfile::tempdir().unwrap();
    let Some((agent, addr)) =
        serve_with(dir.path(), "test", UPDATE_STEAMCMD, UPDATE_DOCKERFILE, (27185, 27189)).await
    else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    seed_manifest(dir.path(), "100");
    let added = dir.path().join("content/sven-coop/test/svencoop/addons/yapb.so");
    std::fs::create_dir_all(added.parent().unwrap()).unwrap();
    std::fs::write(&added, b"operator's own").unwrap();
    let pid = std::process::id();
    let (old_srv, new_srv) = (format!("updold{pid}"), format!("updnew{pid}"));
    for id in [&old_srv, &new_srv] {
        docker_rm(&format!("gpp-{id}"));
    }
    let _cleanup = Containers(vec![old_srv.clone(), new_srv.clone()]);

    let outcome = async {
        agent.create(spec(&old_srv)).await?;
        agent.stop(&old_srv).await?;

        let r = reqwest::Client::new()
            .post(format!("http://{addr}/content/sven-coop"))
            .json(&serde_json::json!({ "update": true, "remove_old": true }))
            .send()
            .await?;
        assert!(r.status().is_success());
        let done = wait_install(addr).await;
        let report = &done["status"]["update"];
        assert_eq!(done["status"]["state"], "done", "{done}");
        let to = report["to_version"].as_str().expect("a new version").to_string();
        assert!(to.starts_with("test-"), "{report}");
        assert_eq!(report["from_build"], "100");
        assert_eq!(report["to_build"], "200");
        assert_eq!(report["old"], "waiting", "a stopped server still holds the old copy: {report}");
        assert_eq!(report["old_used_by"], serde_json::json!([old_srv]));

        let new_dir = dir.path().join("content/sven-coop").join(&to);
        assert!(new_dir.join("updated.txt").is_file());
        assert_eq!(std::fs::read(new_dir.join("svencoop/addons/yapb.so"))?, b"operator's own");
        assert!(dir.path().join("content/sven-coop/test").is_dir(), "deleted while mounted");

        // New servers land on the new copy; the old server keeps its own.
        agent.create(spec(&new_srv)).await?;
        let listed = get(addr, "/content").await;
        let row = |v: &str| listed["copies"].as_array().unwrap().iter().find(|c| c["version"] == v).cloned().unwrap();
        assert_eq!(row(&to)["configured"], true, "{listed}");
        assert_eq!(row(&to)["used_by"], serde_json::json!([new_srv]));
        assert_eq!(row("test")["configured"], false);

        // The last server on the old copy goes, and the copy goes with it.
        agent.remove(&old_srv).await?;
        assert!(!dir.path().join("content/sven-coop/test").exists(), "the old copy outlived its last server");
        assert!(new_dir.is_dir());
        anyhow::Ok(())
    }
    .await;
    outcome.unwrap();
}

/// Nothing newer: the copy steamcmd checked is discarded, so no second
/// identical copy is left, and new servers stay where they were.
#[tokio::test(flavor = "multi_thread")]
async fn an_update_with_nothing_newer_leaves_no_second_copy() {
    let dir = tempfile::tempdir().unwrap();
    let Some((_agent, addr)) =
        serve_with(dir.path(), "test", UPDATE_STEAMCMD, UPDATE_DOCKERFILE, (27185, 27189)).await
    else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    seed_manifest(dir.path(), "200");
    reqwest::Client::new()
        .post(format!("http://{addr}/content/sven-coop"))
        .json(&serde_json::json!({ "update": true, "remove_old": true }))
        .send()
        .await
        .unwrap();
    let done = wait_install(addr).await;
    assert_eq!(done["status"]["state"], "done", "{done}");
    assert_eq!(done["status"]["update"]["to_version"], Value::Null, "{done}");
    let versions: Vec<_> = std::fs::read_dir(dir.path().join("content/sven-coop")).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
    assert_eq!(versions, vec![std::ffi::OsString::from("test")]);
    let staging: Vec<_> = std::fs::read_dir(dir.path().join("staging"))
        .map(|rd| rd.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    assert!(staging.is_empty());
}
