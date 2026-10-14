//! Update checks and the disk reserve (`ROADMAP.md` §12.1, §12.2), against a
//! real Docker daemon.
//!
//! Skips itself without Docker, and builds its stand-in steamcmd images from
//! busybox rather than pulling anything. Its own file, so the reserve test is
//! not run beside tests that write or free megabytes on the same disk: it
//! measures free space, and its tests take turns (`ONE_AT_A_TIME`). Ports
//! 27190–27194 are its own.

use std::sync::Arc;
use std::time::Duration;

use game_bridge::content::PackContent;
use platform_agent::agent::Agent;
use platform_agent::api;
use platform_agent::config::AgentConfig;
use serde_json::{json, Value};

/// Stands in for steamcmd. Asked `+app_info_print`, it prints an app_info
/// document in Steam's shape (`branches` inside `depots`) at public build
/// 300, with a beta branch on another build; asked to install, it writes
/// Steam's manifest at build 300.
const CHECK_STEAMCMD: &str = "gpp-test-steamcmd-check:latest";
const CHECK_DOCKERFILE: &str = r#"FROM busybox
RUN printf '#!/bin/sh\nfor a in "$@"; do if [ "$a" = +app_info_print ]; then printf "noise\\n\\"276060\\"\\n{\\n\\t\\"depots\\"\\n\\t{\\n\\t\\t\\"branches\\"\\n\\t\\t{\\n\\t\\t\\t\\"beta\\"\\n\\t\\t\\t{\\n\\t\\t\\t\\t\\"buildid\\"\\t\\t\\"999\\"\\n\\t\\t\\t}\\n\\t\\t\\t\\"public\\"\\n\\t\\t\\t{\\n\\t\\t\\t\\t\\"buildid\\"\\t\\t\\"300\\"\\n\\t\\t\\t}\\n\\t\\t}\\n\\t}\\n}\\n"; exit 0; fi; done\nwhile [ "$1" != "+force_install_dir" ]; do shift || exit 3; done\nshift\nmkdir -p "$1/steamapps"\nprintf "\\"AppState\\"\\n{\\n\\t\\"buildid\\"\\t\\t\\"300\\"\\n}\\n" > "$1/steamapps/appmanifest_276060.acf"\n' > /fake && chmod +x /fake
ENTRYPOINT ["/fake"]
"#;

/// Stands in for a long steamcmd download: a megabyte every half second for
/// twenty seconds, so a test can let the disk cross the reserve part-way.
const BIG_STEAMCMD: &str = "gpp-test-steamcmd-big:latest";
const BIG_DOCKERFILE: &str = r#"FROM busybox
RUN printf '#!/bin/sh\nwhile [ "$1" != "+force_install_dir" ]; do shift || exit 3; done\nshift\ni=0\nwhile [ $i -lt 40 ]; do i=$((i+1)); dd if=/dev/zero of="$1/part$i" bs=1024 count=1024 2>/dev/null; sleep 0.5; done\n' > /fake && chmod +x /fake
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
        "gpp-update-image-{}-{}",
        std::process::id(),
        tag.replace([':', '/'], "-")
    ));
    if std::fs::create_dir_all(&dir).is_err()
        || std::fs::write(dir.join("Dockerfile"), dockerfile).is_err()
    {
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

/// These tests run one at a time: the reserve tests measure free space, and
/// "nothing left behind" looks at every task container for the app, so a
/// download running in a neighbouring test would read as a leftover.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn task_containers() -> String {
    let out = std::process::Command::new("docker")
        .args(["ps", "-a", "--filter", "name=gpp-content-276060-", "--format", "{{.Names}}"])
        .output()
        .expect("docker ps");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// An agent with Sven Co-op (steamcmd) installed at `content/sven-coop/test`,
/// its manifest at `build`, and a second game whose files a person installs
/// (`manual`), which has nothing to check.
async fn serve(
    dir: &std::path::Path,
    steamcmd: &str,
    dockerfile: &str,
    build: Option<&str>,
) -> Option<std::net::SocketAddr> {
    if !ensure_image(steamcmd, dockerfile) {
        return None;
    }
    let content = dir.join("content/sven-coop/test/svencoop");
    for sub in ["maps/soundcache", "logs"] {
        std::fs::create_dir_all(content.join(sub)).ok()?;
    }
    if let Some(build) = build {
        let apps = dir.join("content/sven-coop/test/steamapps");
        std::fs::create_dir_all(&apps).ok()?;
        std::fs::write(
            apps.join("appmanifest_276060.acf"),
            format!("\"AppState\"\n{{\n\t\"buildid\"\t\t\"{build}\"\n}}\n"),
        )
        .ok()?;
    }
    let toml = format!(
        r#"
data_root = "{}"
max_instances = 4
api_bind = "127.0.0.1:0"
allow_content_fetch = true
steamcmd_image = "{steamcmd}"

[port_range]
start = 27190
end = 27194

[games.sven-coop]
image = "busybox"
content_root = "/game"
content_version = "test"
"#,
        dir.display()
    );
    let config = AgentConfig::parse(&toml).ok()?;
    let mut handmade = game_bridge::GamePack::sven_coop();
    handmade.id = "handmade".into();
    handmade.content = PackContent::Manual { note: None };
    let agent = Arc::new(
        Agent::new(config, vec![game_bridge::GamePack::sven_coop(), handmade]).await.ok()?,
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let addr = listener.local_addr().ok()?;
    let router = api::router(agent);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Some(addr)
}

async fn get(addr: std::net::SocketAddr, path: &str) -> Value {
    reqwest::get(format!("http://{addr}{path}")).await.unwrap().json().await.unwrap()
}

async fn send(
    method: reqwest::Method,
    addr: std::net::SocketAddr,
    path: &str,
    body: Value,
) -> (u16, Value) {
    let r = reqwest::Client::new()
        .request(method, format!("http://{addr}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

async fn wait_install(addr: std::net::SocketAddr) -> Value {
    let mut st = Value::Null;
    for _ in 0..200 {
        st = get(addr, "/content/sven-coop").await;
        let state = &st["status"]["state"];
        if state != "running" && state != "queued" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    st
}

fn staging_is_empty(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir.join("staging")).map(|rd| rd.count() == 0).unwrap_or(true)
}

/// Off by default; a check reads the public branch (not beta), compares it
/// with the copy's own manifest, and "skip this build" hides that build only.
/// A game with no source to ask is never checked.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_finds_the_public_build_and_a_skip_hides_only_that_build() {
    let _one = ONE_AT_A_TIME.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let Some(addr) = serve(dir.path(), CHECK_STEAMCMD, CHECK_DOCKERFILE, Some("100")).await else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    let before = get(addr, "/updates").await;
    assert_eq!(
        before["settings"]["check_every_hours"],
        Value::Null,
        "checks must be off by default: {before}"
    );
    let games = before["games"].as_array().unwrap();
    assert_eq!(games.len(), 1, "only the steamcmd game is listed: {before}");
    assert_eq!(games[0]["available"], Value::Null, "never checked is not up to date: {before}");

    let (status, notice) =
        send(reqwest::Method::POST, addr, "/updates/sven-coop/check", json!({})).await;
    assert_eq!(status, 200, "{notice}");
    assert_eq!(notice["latest_build"], "300", "the public branch, not beta: {notice}");
    assert_eq!(notice["installed_build"], "100");
    assert_eq!(notice["available"], true);
    assert_eq!(notice["snoozed"], false);

    let (status, after) = send(
        reqwest::Method::POST,
        addr,
        "/updates/sven-coop/snooze",
        json!({ "skip_build": true }),
    )
    .await;
    assert_eq!(status, 200, "{after}");
    assert_eq!(after["games"][0]["snoozed"], true, "{after}");
    let (_, shown) =
        send(reqwest::Method::DELETE, addr, "/updates/sven-coop/snooze", json!({})).await;
    assert_eq!(shown["games"][0]["snoozed"], false, "{shown}");

    // The copy catches up: up to date, from the manifest on disk.
    std::fs::write(
        dir.path().join("content/sven-coop/test/steamapps/appmanifest_276060.acf"),
        "\"AppState\"\n{\n\t\"buildid\"\t\t\"300\"\n}\n",
    )
    .unwrap();
    assert_eq!(get(addr, "/updates").await["games"][0]["available"], false);

    let (status, body) =
        send(reqwest::Method::POST, addr, "/updates/handmade/check", json!({})).await;
    assert_eq!(status, 409, "a manual game has nothing to check: {body}");
    assert!(body["error"].as_str().unwrap().contains("steamcmd"), "{body}");
}

/// An update that would take the disk below the reserve never starts: nothing
/// is copied, nothing is deleted to make room, and the refusal names the
/// unused copy that would.
#[tokio::test(flavor = "multi_thread")]
async fn an_update_that_does_not_fit_above_the_reserve_never_starts_and_deletes_nothing() {
    let _one = ONE_AT_A_TIME.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("content/sven-coop/old");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("big.bin"), vec![0u8; 64 * 1024]).unwrap();
    let Some(addr) = serve(dir.path(), CHECK_STEAMCMD, CHECK_DOCKERFILE, Some("100")).await else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    let free = get(addr, "/updates").await["storage"]["free_bytes"]
        .as_u64()
        .expect("free space on a Linux node");
    // A reserve just above what is free: nothing at all fits.
    let (status, body) = send(
        reqwest::Method::PUT,
        addr,
        "/updates/settings",
        json!({ "reserve_bytes": free + (1 << 30) }),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    reqwest::Client::new()
        .post(format!("http://{addr}/content/sven-coop"))
        .json(&json!({ "update": true, "remove_old": true }))
        .send()
        .await
        .unwrap();
    let done = wait_install(addr).await;
    assert_eq!(done["status"]["state"], "failed", "{done}");
    let error = done["status"]["error"].as_str().unwrap();
    assert!(error.starts_with("not enough room"), "{error}");
    assert!(error.contains("sven-coop old"), "the unused copy is named: {error}");
    let versions: Vec<_> = std::fs::read_dir(dir.path().join("content/sven-coop"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    assert_eq!(versions.len(), 2, "no new copy, and nothing deleted to make room: {versions:?}");
    assert!(old.join("big.bin").is_file());
    assert!(staging_is_empty(dir.path()));
}

/// A download that brings the disk down to the reserve is stopped the way a
/// cancel stops it: its container removed, nothing installed, nothing left in
/// staging, and the reason on the page instead of "cancelled".
#[tokio::test(flavor = "multi_thread")]
async fn a_download_that_reaches_the_reserve_is_stopped_and_leaves_nothing() {
    let _one = ONE_AT_A_TIME.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let Some(addr) = serve(dir.path(), BIG_STEAMCMD, BIG_DOCKERFILE, None).await else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    // Install into a version that is not there yet.
    std::fs::remove_dir_all(dir.path().join("content/sven-coop/test")).unwrap();
    let free = get(addr, "/updates").await["storage"]["free_bytes"]
        .as_u64()
        .expect("free space on a Linux node");
    // Six megabytes above the reserve: room to start, not to finish 40.
    send(
        reqwest::Method::PUT,
        addr,
        "/updates/settings",
        json!({ "reserve_bytes": free - 6 * 1024 * 1024 }),
    )
    .await;

    let started: Value = reqwest::Client::new()
        .post(format!("http://{addr}/content/sven-coop"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(started["started"], true, "{started}");
    let done = wait_install(addr).await;
    assert_eq!(done["status"]["state"], "failed", "{done}");
    let error = done["status"]["error"].as_str().unwrap();
    assert!(error.starts_with("stopped: the disk reached"), "{error}");
    assert!(!dir.path().join("content/sven-coop/test").exists(), "a stopped download installed");
    assert!(staging_is_empty(dir.path()), "staging left behind");
    assert_eq!(task_containers(), "", "the task container outlived the stop");
}

/// "Tell me" (the default) downloads nothing; "download it" fetches the found
/// build beside the old copy through the same path as Update, and deletes
/// nothing. A game with no source to ask cannot be set to download.
#[tokio::test(flavor = "multi_thread")]
async fn a_found_build_downloads_only_when_the_game_says_so_and_keeps_the_old_copy() {
    let _one = ONE_AT_A_TIME.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let Some(addr) = serve(dir.path(), CHECK_STEAMCMD, CHECK_DOCKERFILE, Some("100")).await else {
        eprintln!("skipping: no Docker daemon, or no test image");
        return;
    };
    let (status, notice) =
        send(reqwest::Method::POST, addr, "/updates/sven-coop/check", json!({})).await;
    assert_eq!(status, 200, "{notice}");
    assert_eq!(notice["available"], true, "{notice}");
    assert_eq!(notice["on_new_build"], "notify", "telling is the default: {notice}");
    let idle = get(addr, "/content/sven-coop").await;
    assert_eq!(idle["status"]["state"], "idle", "a notice alone downloads nothing: {idle}");

    let (status, err) = send(
        reqwest::Method::PUT,
        addr,
        "/updates/handmade/policy",
        json!({"on_new_build": "download"}),
    )
    .await;
    assert_eq!(status, 409, "a manual game has nothing to download: {err}");

    let (status, body) = send(
        reqwest::Method::PUT,
        addr,
        "/updates/sven-coop/policy",
        json!({"on_new_build": "download"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let done = wait_install(addr).await;
    assert_eq!(done["status"]["state"], "done", "the found build downloads by itself: {done}");
    assert_eq!(done["status"]["update"]["to_build"], "300", "{done}");
    assert_eq!(done["status"]["update"]["old"], "kept", "nothing automatic deletes: {done}");
    assert!(
        dir.path().join("content/sven-coop/test/steamapps").is_dir(),
        "the old copy is still there"
    );

    let after = get(addr, "/updates").await;
    let game = &after["games"][0];
    assert_eq!(game["installed_build"], "300", "new servers use the new copy: {after}");
    assert_eq!(game["available"], false, "{after}");
    assert_eq!(game["on_new_build"], "download", "{after}");
    assert!(task_containers().is_empty(), "no task container is left behind");
}
