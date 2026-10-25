//! The local API, over a real socket.
//!
//! Skips itself without Docker, because `Agent::new` pings the daemon on the way
//! up — an agent that could not talk to Docker would be lying about being ready.

use std::sync::Arc;

use platform_agent::agent::Agent;
use platform_agent::api;
use platform_agent::config::AgentConfig;

fn config_toml(data_root: &std::path::Path) -> String {
    format!(
        r#"
data_root = "{}"
max_instances = 2
api_bind = "127.0.0.1:0"

[port_range]
start = 27150
end = 27159

[games.sven-coop]
image = "gpp-test-sleeper:latest"
content_root = "/game"
content_version = "test"
"#,
        data_root.display()
    )
}

/// Bring the API up on an ephemeral loopback port and hand back its address.
async fn serve() -> Option<(std::net::SocketAddr, tempfile::TempDir)> {
    let dir = tempfile::tempdir().ok()?;
    let config = AgentConfig::parse(&config_toml(dir.path())).ok()?;
    let agent = Agent::new(config, vec![game_bridge::GamePack::sven_coop()])
        .await
        .ok()?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, api::router(Arc::new(agent))).await;
    });
    Some((addr, dir))
}

/// With a real pack directory, so the import route has somewhere to write.
async fn serve_with_packs(
) -> Option<(std::net::SocketAddr, std::path::PathBuf, tempfile::TempDir)> {
    let dir = tempfile::tempdir().ok()?;
    let packs = dir.path().join("packs");
    std::fs::create_dir_all(&packs).ok()?;
    let config = AgentConfig::parse(&config_toml(dir.path())).ok()?;
    let agent = Agent::new(config, vec![game_bridge::GamePack::sven_coop()])
        .await
        .ok()?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let addr = listener.local_addr().ok()?;
    let router = api::router_full(Arc::new(agent), None, Some(packs.clone()), None);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Some((addr, packs, dir))
}

/// The same, but with a token, so the auth layer is the real one.
async fn serve_with_token(token: &str) -> Option<(std::net::SocketAddr, tempfile::TempDir)> {
    let dir = tempfile::tempdir().ok()?;
    let config = AgentConfig::parse(&config_toml(dir.path())).ok()?;
    let agent = Agent::new(config, vec![game_bridge::GamePack::sven_coop()])
        .await
        .ok()?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let addr = listener.local_addr().ok()?;
    let router = api::router_with_token(Arc::new(agent), Some(token.to_string()));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Some((addr, dir))
}

/// **The load-bearing auth test.** Every route creates or destroys containers,
/// so once a token exists it has to gate all of them — a single route that
/// forgot the layer is a route that runs containers for anyone who can reach
/// the port.
///
/// Loopback is not an exemption. These requests all come from loopback, and
/// they are still refused, because a browser on the operator's machine is a
/// loopback client and so is every other program on it.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_token_every_route_is_refused() {
    let token = "t".repeat(32);
    let Some((addr, _dir)) = serve_with_token(&token).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let client = reqwest::Client::new();

    for path in [
        "/health", "/capacity", "/instances", "/orphans", "/games", "/usage", "/storage",
        "/orphans/summary", "/instances/x/logs", "/instances/x/players", "/content",
        "/content/sven-coop", "/updates", "/restart-schedules", "/instances/x/backups", "/backups", "/instances/x/backup-schedule", "/instances/x/backups/20261014-051002.tar.gz/file",
        "/controller/status", "/controller/instances",
    ] {
        let status = client
            .get(format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 401, "{path} answered without a token");
    }

    // A wrong token is no better than none.
    let status = client
        .get(format!("http://{addr}/health"))
        .bearer_auth("x".repeat(32))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 401, "a wrong token was accepted");

    // And a destructive route is not somehow more open than a read.
    let status = client
        .delete(format!("http://{addr}/instances/whatever"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 401, "delete answered without a token");

    // Deleting a game's files, and cancelling its download.
    for path in ["/content/sven-coop/test", "/content/sven-coop", "/updates/sven-coop/snooze"] {
        let status = client
            .delete(format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 401, "DELETE {path} answered without a token");
    }

    // Nor are the update routes: a check runs a container, a setting changes
    // what downloads.
    for (method, path) in [
        (reqwest::Method::POST, "/updates/sven-coop/check"),
        (reqwest::Method::POST, "/updates/sven-coop/snooze"),
        (reqwest::Method::PUT, "/updates/settings"),
        (reqwest::Method::PUT, "/updates/sven-coop/policy"),
        (reqwest::Method::POST, "/instances/anything/move"),
        (reqwest::Method::DELETE, "/packs/sven-coop"),
        (reqwest::Method::GET, "/packs/repository?url=https://example.org/"),
        (reqwest::Method::POST, "/content/sven-coop/test/use"),
        (reqwest::Method::POST, "/instances/anything/console"),
        (reqwest::Method::POST, "/instances/anything/moderation"),
        (reqwest::Method::GET, "/instances/anything/moderation"),
        (reqwest::Method::POST, "/instances/anything/world"),
        (reqwest::Method::GET, "/resources/check?game=sven-coop"),
        (reqwest::Method::GET, "/minecraft/versions?server=paper"),
        (reqwest::Method::PUT, "/instances/anything/restart-schedule"),
        (reqwest::Method::POST, "/instances/anything/backups"),
        (reqwest::Method::POST, "/instances/anything/backups/20261014-051002.tar.gz/restore"),
        (reqwest::Method::DELETE, "/instances/anything/backups/20261014-051002.tar.gz"),
        (reqwest::Method::PUT, "/instances/anything/backup-schedule"),
        (reqwest::Method::DELETE, "/backups/anything"),
        (reqwest::Method::POST, "/instances/anything/backups/import"),
        (reqwest::Method::GET, "/instances/anything/chat"),
        (reqwest::Method::POST, "/instances/anything/chat"),
        (reqwest::Method::PUT, "/instances/anything/chat"),
        (reqwest::Method::POST, "/instances/anything/chat/mute"),
        (reqwest::Method::GET, "/packs/official"),
        (reqwest::Method::PUT, "/packs/official"),
        (reqwest::Method::POST, "/packs/official/sync"),
    ] {
        let status = client
            .request(method.clone(), format!("http://{addr}{path}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 401, "{method} {path} answered without a token");
    }

    // Nor are the routes that delete files or change what restarts.
    for path in ["/orphans/clean", "/instances/x/restart-on-crash"] {
        let status = client
            .post(format!("http://{addr}{path}"))
            .json(&serde_json::json!({ "on": true }))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 401, "{path} answered without a token");
    }

    // The right token works, or the test above proves only that the API is
    // broken.
    let status = client
        .get(format!("http://{addr}/health"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 200);
}

/// The UI must load without a token — a login page you need the token to reach
/// is a login page nobody can use. It carries no secret and no data about this
/// node; everything it then asks for goes through the auth layer.
#[tokio::test(flavor = "multi_thread")]
async fn the_web_ui_loads_without_a_token_but_the_api_still_does_not() {
    let token = "t".repeat(32);
    let Some((addr, _dir)) = serve_with_token(&token).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let client = reqwest::Client::new();

    for (path, needle) in [
        ("/", "<html"),
        ("/app.js", "Bearer"),
        ("/style.css", ":root"),
    ] {
        let resp = client.get(format!("http://{addr}{path}")).send().await.unwrap();
        assert_eq!(resp.status(), 200, "{path} did not serve");
        let body = resp.text().await.unwrap();
        assert!(body.contains(needle), "{path} served something unexpected");
    }

    // And serving the UI did not open the API.
    let status = client
        .get(format!("http://{addr}/instances"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 401);
}

/// A website the operator opens can make their browser send a request to a
/// loopback node: a POST with no body needs no preflight, and DNS rebinding
/// gives a site's own name the address `127.0.0.1`. A node with no token has
/// nothing else in the way, so a request the browser marks as another site's,
/// an `Origin` that is not the node, and a `Host` that is not a loopback name
/// are refused before any route runs. A program that sends none of these is
/// unaffected, and the page itself still works.
#[tokio::test(flavor = "multi_thread")]
async fn another_site_cannot_drive_a_loopback_node_through_a_browser() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let client = reqwest::Client::new();
    let clean = format!("http://{addr}/orphans/clean");
    let port = addr.port();

    let refused = [
        client.post(&clean).header("sec-fetch-site", "cross-site"),
        client.post(&clean).header("sec-fetch-site", "same-site"),
        client.post(&clean).header("origin", "http://evil.example"),
        client.post(&clean).header("origin", "null"),
        client.post(&clean).header("host", format!("evil.example:{port}")),
    ];
    for request in refused {
        assert_eq!(request.send().await.unwrap().status(), 403);
    }

    for request in [
        client.get(format!("http://{addr}/instances")),
        client.get(format!("http://{addr}/instances")).header("sec-fetch-site", "same-origin"),
        client.get(format!("http://{addr}/instances")).header("origin", format!("http://{addr}")),
        client.get(format!("http://localhost:{port}/instances")),
    ] {
        assert_eq!(request.send().await.unwrap().status(), 200);
    }

    // The page says what it may load and that nothing may frame it.
    let page = client.get(format!("http://{addr}/")).send().await.unwrap();
    let csp = page.headers()["content-security-policy"].to_str().unwrap().to_string();
    assert!(csp.contains("frame-ancestors 'none'") && csp.contains("script-src 'self'"), "{csp}");
    assert_eq!(page.headers()["x-frame-options"], "DENY");
    assert_eq!(page.headers()["x-content-type-options"], "nosniff");
}

/// Importing a pack must take effect on a *running* node — "add a game" that
/// needed a restart would not be one click. The reload goes through the
/// operator's trust policy, so this also proves the import did not bypass it.
#[tokio::test(flavor = "multi_thread")]
async fn an_imported_pack_becomes_runnable_without_a_restart() {
    let Some((addr, dir, _tmp)) = serve_with_packs().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let client = reqwest::Client::new();

    let before: Vec<serde_json::Value> = client
        .get(format!("http://{addr}/games"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!before.iter().any(|g| g["id"] == "imported-game"));

    let pack = r#"
schema_version = 1
id = "imported-game"
display_name = "Imported Game"
app_name = "imported-game"
default_port = 27015
transport = "udp"
min_link_class = 1
query = "a2s"
"#;
    let resp = client
        .post(format!("http://{addr}/packs"))
        .json(&serde_json::json!({ "toml": pack }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["imported"]["id"], "imported-game");
    assert_eq!(body["loaded"], serde_json::json!(true));
    // No [games.imported-game] runtime, so it is installed and not runnable —
    // and it says so rather than failing later at start.
    assert_eq!(body["imported"]["runnable"], serde_json::json!(false));

    assert!(dir.join("imported-game.toml").exists());

    let after: Vec<serde_json::Value> = client
        .get(format!("http://{addr}/games"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let found = after
        .iter()
        .find(|g| g["id"] == "imported-game")
        .expect("the imported pack is live without a restart");
    assert_eq!(found["runnable"], serde_json::json!(false));
    assert!(found["reason"].as_str().unwrap().contains("games.imported-game"));
    // Its tier is read from its file, so an operator sees what they added.
    assert_eq!(found["trust"], "unsigned local");
    assert!(found["signer"].is_null());

    // A second import of the same id is refused rather than replacing a pack
    // whose game may be running.
    let again = client
        .post(format!("http://{addr}/packs"))
        .json(&serde_json::json!({ "toml": pack }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 400);
}

/// The web UI needs to know what it may offer. A pack with no runtime is listed
/// as not runnable rather than hidden, because "where did my game go" is the
/// next question and the answer is one line of the operator's own config.
#[tokio::test(flavor = "multi_thread")]
async fn games_lists_packs_and_says_which_can_actually_run() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/games"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let games = body.as_array().expect("an array");
    let sven = games
        .iter()
        .find(|g| g["id"] == "sven-coop")
        .expect("the pack this node was given");
    assert_eq!(sven["runnable"], serde_json::json!(true));
    assert_eq!(sven["default_port"], serde_json::json!(27015));
    assert!(sven["reason"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn health_reports_the_nodes_limits() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["max_instances"], serde_json::json!(2));
    assert_eq!(body["port_range"]["start"], serde_json::json!(27150));
    // The build answering, which the web UI shows in a corner. Compared
    // against the crate's own version rather than a literal, so a release bump
    // does not have to remember this test.
    assert_eq!(body["version"], serde_json::json!(env!("CARGO_PKG_VERSION")));
}

/// A game whose pack declares no bots is refused by name, before anything is
/// typed at a server. The refusal matters more than it looks: Counter-Strike
/// 1.6 *has* the Z-Bot code in its server library and runs it only as Condition
/// Zero, so a node that tried anyway would send `bot_quota` into a server that
/// silently ignores it and report success.
#[tokio::test(flavor = "multi_thread")]
async fn a_game_without_bots_is_told_so_rather_than_asked_for_them() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/instances/nothing-here/bots"))
        .json(&serde_json::json!({ "count": 4 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body: serde_json::Value = res.json().await.unwrap();
    let msg = body["error"].as_str().unwrap_or_default().to_string();
    assert!(msg.contains("nothing-here"), "{msg}");
}

/// The UI decides whether to show a bots control from this flag, so it has to
/// be present and honest for every game the node lists.
#[tokio::test(flavor = "multi_thread")]
async fn games_say_whether_they_have_bots() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/games"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let games = body.as_array().expect("a list of games");
    assert!(!games.is_empty());
    for game in games {
        assert!(game["bots"].is_boolean(), "{game}");
    }
}

/// The artwork id is a field the UI reads by name; Sven Co-op's pack names
/// the Steam app players own.
#[tokio::test(flavor = "multi_thread")]
async fn games_carry_the_steam_app_players_own() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/games"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sven = body.as_array().unwrap().iter().find(|g| g["id"] == "sven-coop").expect("sven");
    assert_eq!(sven["steam_app_id"], 225840, "{sven}");
    assert!(sven["art_url"].as_str().unwrap_or("").contains("/225840/"), "{sven}");
}

/// Announce now on a LAN-only node is a clear refusal, not a silent success
/// that announced nothing.
#[tokio::test(flavor = "multi_thread")]
async fn announcing_on_a_lan_only_node_says_why_it_cannot() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/mesh/announce"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap_or("").contains("LAN-only"), "{body}");
}

/// A node with no `[uplink]` block has no mesh node to configure, so the
/// interfaces routes answer 501 with the reason rather than 404 or a panic. The
/// route is wired and reachable; it just has nothing to act on. (The interface
/// logic itself is unit-tested in `interfaces.rs`; this pins the API wiring.)
#[tokio::test(flavor = "multi_thread")]
async fn interfaces_without_an_uplink_report_not_implemented() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let resp = reqwest::get(format!("http://{addr}/interfaces")).await.unwrap();
    assert_eq!(resp.status().as_u16(), 501, "no uplink must be a clear 501");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap_or("").contains("uplink"),
        "the error should explain the missing uplink, got {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn listing_instances_on_a_fresh_node_is_an_empty_list() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let body: serde_json::Value = reqwest::get(format!("http://{addr}/instances"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body.is_array(), "instances must be a list, got {body}");
}

/// A create that cannot work must fail with the *reason*, as a 400. An operator
/// reading "500" learns nothing; the missing content directory is the whole
/// answer to "why will my server not start".
#[tokio::test(flavor = "multi_thread")]
/// Pressing Start on a game whose files are not here yet begins the download
/// instead of refusing. "Install it first" is a step the machine can take on
/// its own, and making a person take it is making them learn the difference
/// between a pack and an install in order to do the obvious thing.
///
/// It answers 202, not 200: the download cannot happen inside this request —
/// 2.7 GB takes longer than a browser will hold a connection — so the honest
/// answer is "accepted, and here is what is happening".
async fn starting_a_server_with_no_content_begins_the_download() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/instances"))
        .json(&serde_json::json!({
            "instance_id": "sven-1",
            "game_id": "sven-coop",
            "name": "Test",
            "max_players": 8
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["installing"], "sven-coop");
    // The sentence is for a person to read, so it has to say what happens next.
    assert!(body["message"].as_str().unwrap().contains("start once it finishes"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hostile_instance_id_is_refused() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    for bad in ["../escape", "..", ".hidden", "Upper"] {
        let res = reqwest::Client::new()
            .post(format!("http://{addr}/instances"))
            .json(&serde_json::json!({
                "instance_id": bad,
                "game_id": "sven-coop",
                "name": "Nope",
                "max_players": 8
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400, "instance_id {bad:?} should have been refused");
    }
}

/// A game with an `[eula]` refuses a spec without the owner's acceptance, and
/// says so: the missing copy must not turn the refusal into a download.
#[tokio::test(flavor = "multi_thread")]
async fn a_spec_without_the_eula_is_refused_and_starts_no_download() {
    let Some(dir) = tempfile::tempdir().ok() else { return };
    let toml = config_toml(dir.path()).replace("max_instances = 2", "max_instances = 2\nallow_content_fetch = true").replace(
        "[games.sven-coop]",
        "[games.minecraft-java]\nimage = \"gpp-test-sleeper:latest\"\ncontent_root = \"/game\"\ncontent_version = \"paper-26.3\"\n\n[games.sven-coop]",
    );
    let pack = game_bridge::GamePack::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/minecraft-java.toml")).unwrap();
    let Ok(agent) = Agent::new(AgentConfig::parse(&toml).unwrap(), vec![pack]).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let agent = Arc::new(agent);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = api::router(agent.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/instances"))
        .json(&serde_json::json!({
            "instance_id": "mc1", "game_id": "minecraft-java", "name": "No EULA", "max_players": 20
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body = res.text().await.unwrap();
    assert!(body.contains("End User License Agreement"), "{body}");
    let status: serde_json::Value = reqwest::get(format!("http://{addr}/content/minecraft-java")).await.unwrap().json().await.unwrap();
    assert_eq!(status["status"]["state"], "idle", "no download was started: {status}");
}

/// Stopping something that is not there is the caller's problem, and must not
/// take the agent down or touch anything else.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_an_unknown_instance_is_a_clean_error() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/instances/doesnotexist/stop"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    // And the agent is still alive afterwards.
    let health = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert!(health.status().is_success());
}


/// Cleaning up leftovers deletes old instance directories that have no
/// container, and nothing else: a directory touched in the last ten minutes
/// may belong to a create that has not made its container yet.
#[tokio::test(flavor = "multi_thread")]
async fn cleaning_leftovers_removes_only_old_directories_without_a_container() {
    let Some((addr, dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let instances = dir.path().join("instances");
    let old = instances.join("long-gone");
    let fresh = instances.join("being-created");
    std::fs::create_dir_all(old.join("writable")).unwrap();
    std::fs::write(old.join("writable/save.dat"), vec![0u8; 4096]).unwrap();
    std::fs::create_dir_all(&fresh).unwrap();
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::open(&old).unwrap().set_modified(hour_ago).unwrap();

    let client = reqwest::Client::new();
    let summary: serde_json::Value =
        client.get(format!("http://{addr}/orphans/summary")).send().await.unwrap().json().await.unwrap();
    assert_eq!(summary["count"], 1, "{summary}");
    assert_eq!(summary["skipped_recent"], 1, "{summary}");
    assert!(summary["bytes"].as_u64().unwrap() >= 4096, "{summary}");

    let done: serde_json::Value =
        client.post(format!("http://{addr}/orphans/clean")).send().await.unwrap().json().await.unwrap();
    assert_eq!(done["count"], 1, "{done}");
    assert!(!old.exists(), "the leftover was not removed");
    assert!(fresh.exists(), "a directory a create may still need was removed");
}

/// Restart-on-crash is only for a server this node has.
#[tokio::test(flavor = "multi_thread")]
async fn restart_on_crash_for_an_unknown_server_is_refused() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let status = reqwest::Client::new()
        .post(format!("http://{addr}/instances/nobody-here/restart-on-crash"))
        .json(&serde_json::json!({ "on": true }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 404);
}

/// A pack goes, with its file, and comes off `/games` without a restart. A
/// server of its game, even a stopped one, holds it: it could not be started
/// again without the pack.
#[tokio::test(flavor = "multi_thread")]
async fn a_pack_is_removed_only_when_no_server_of_its_game_exists() {
    let Some((addr, dir, _tmp)) = serve_with_packs().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let client = reqwest::Client::new();
    let pack = r#"
schema_version = 1
id = "removable-game"
display_name = "Removable Game"
app_name = "removable-game"
default_port = 27015
transport = "udp"
min_link_class = 1
query = "a2s"
"#;
    let resp = client.post(format!("http://{addr}/packs")).json(&serde_json::json!({ "toml": pack })).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // A stopped server of that game, as the agent would label one.
    let name = format!("gpp-removable{}", std::process::id());
    let created = std::process::Command::new("docker")
        .args(["create", "--name", &name])
        .args(["--label", &format!("{}=1", platform_agent::config::MANAGED_LABEL)])
        .args(["--label", &format!("{}=removable{}", platform_agent::config::INSTANCE_LABEL, std::process::id())])
        .args(["--label", &format!("{}=removable-game", platform_agent::config::GAME_LABEL)])
        .args(["busybox", "true"])
        .output()
        .unwrap();
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let held = client.delete(format!("http://{addr}/packs/removable-game")).send().await.unwrap();
    let status = held.status();
    let _ = std::process::Command::new("docker").args(["rm", "-f", &name]).output();
    assert_eq!(status, 409, "{}", held.text().await.unwrap());
    assert!(dir.join("removable-game.toml").exists(), "a refused removal removes nothing");

    let resp = client.delete(format!("http://{addr}/packs/removable-game")).send().await.unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    assert!(!dir.join("removable-game.toml").exists());
    let games: Vec<serde_json::Value> =
        client.get(format!("http://{addr}/games")).send().await.unwrap().json().await.unwrap();
    assert!(!games.iter().any(|g| g["id"] == "removable-game"), "gone without a restart");

    let again = client.delete(format!("http://{addr}/packs/removable-game")).send().await.unwrap();
    assert_eq!(again.status(), 404);
    let hostile = client.delete(format!("http://{addr}/packs/..%2Fagent")).send().await.unwrap();
    assert_eq!(hostile.status(), 404, "an id names no path");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_game_says_where_its_files_go_and_which_folders_they_need() {
    let dir = tempfile::tempdir().unwrap();
    let config = AgentConfig::parse(&config_toml(dir.path())).unwrap();
    let mut pack = game_bridge::GamePack::sven_coop();
    pack.content = game_bridge::content::PackContent::Manual { note: Some("from your own Steam library".into()) };
    let Ok(agent) = Agent::new(config, vec![pack.clone()]).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, api::router(Arc::new(agent))).await;
    });
    let content = || async {
        let body: serde_json::Value =
            reqwest::get(format!("http://{addr}/games")).await.unwrap().json().await.unwrap();
        body.as_array().unwrap().iter().find(|g| g["id"] == "sven-coop").unwrap()["content"].clone()
    };

    let c = content().await;
    let want = dir.path().join("content/sven-coop/test");
    assert_eq!(c["driver"], "manual");
    assert_eq!(c["installed"], false);
    assert_eq!(c["install_dir"], want.display().to_string(), "the exact folder, before any Start fails");
    assert_eq!(c["must_contain"], serde_json::json!(pack.writable_paths));
    assert_eq!(c["note"], "from your own Steam library");

    // Put there without one of the folders a server mounts over: named.
    std::fs::create_dir_all(want.join(&pack.writable_paths[0])).unwrap();
    let c = content().await;
    assert_eq!(c["installed"], true);
    assert!(c["install_dir"].is_null());
    assert_eq!(c["missing"], serde_json::json!(pack.writable_paths[1..]));
    for p in &pack.writable_paths {
        std::fs::create_dir_all(want.join(p)).unwrap();
    }
    assert_eq!(content().await["missing"], serde_json::json!([]));
}

/// Placement reads the disk out of the capacity report, under the reserve the
/// node itself keeps, so the two numbers must be the ones `/updates` shows.
/// Report only one of them and an index would have nothing to compare it to.
#[tokio::test(flavor = "multi_thread")]
async fn capacity_reports_free_disk_and_the_reserve_it_is_held_to() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let client = reqwest::Client::new();
    let cap: serde_json::Value =
        client.get(format!("http://{addr}/capacity")).send().await.unwrap().json().await.unwrap();
    let updates: serde_json::Value =
        client.get(format!("http://{addr}/updates")).send().await.unwrap().json().await.unwrap();

    let free = cap["disk_free_bytes"].as_u64().expect("free disk is reported");
    assert!(free > 0, "{cap}");
    assert_eq!(
        cap["disk_reserve_bytes"], updates["reserve_bytes"],
        "the capacity report and the updates page must name the same reserve"
    );
}

/// The page signs in with an account instead of the token. The first account
/// needs the token and only while there is none; a sign-in gives a session that
/// opens every route and sign-out ends; a wrong name and a wrong password read
/// alike; and the token keeps working, but changes no account.
#[tokio::test(flavor = "multi_thread")]
async fn the_page_signs_in_with_an_account_made_once_with_the_token() {
    let token = "t".repeat(32);
    let Some((addr, dir)) = serve_with_token(&token).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let c = reqwest::Client::new();
    let url = |p: &str| format!("http://{addr}{p}");
    let post = |p: &str, body: serde_json::Value| c.post(url(p)).json(&body);

    let state: serde_json::Value = c.get(url("/auth/state")).send().await.unwrap().json().await.unwrap();
    assert_eq!(state["has_accounts"], false);
    assert_eq!(state["token_required"], true);

    let wrong = post("/auth/setup", serde_json::json!({"token": "nope", "username": "op", "password": "correct horse battery"}));
    assert_eq!(wrong.send().await.unwrap().status(), 401);
    let made = post("/auth/setup", serde_json::json!({"token": token, "username": "op", "password": "correct horse battery"}));
    assert_eq!(made.send().await.unwrap().status(), 200);
    let again = post("/auth/setup", serde_json::json!({"token": token, "username": "two", "password": "another long password"}));
    assert_eq!(again.send().await.unwrap().status(), 409, "the first account is made once");

    let no_name = post("/auth/login", serde_json::json!({"username": "ghost", "password": "correct horse battery"}));
    let no_name: serde_json::Value = no_name.send().await.unwrap().json().await.unwrap();
    let bad_pass = post("/auth/login", serde_json::json!({"username": "op", "password": "wrong password!!"}));
    let bad_pass: serde_json::Value = bad_pass.send().await.unwrap().json().await.unwrap();
    assert_eq!(no_name["error"], bad_pass["error"], "a missing name must read like a wrong password");

    let login = post("/auth/login", serde_json::json!({"username": "op", "password": "correct horse battery"}));
    let login: serde_json::Value = login.send().await.unwrap().json().await.unwrap();
    let session = login["session"].as_str().expect("a session").to_string();
    let with = |s: &str, p: &str| c.get(url(p)).bearer_auth(s.to_string());
    assert_eq!(with(&session, "/instances").send().await.unwrap().status(), 200);
    let me: serde_json::Value = with(&session, "/auth/me").send().await.unwrap().json().await.unwrap();
    assert_eq!((me["via"].as_str(), me["username"].as_str()), (Some("account"), Some("op")));

    // The token still opens everything, but it is no account to change.
    assert_eq!(with(&token, "/instances").send().await.unwrap().status(), 200);
    let by_token = c.post(url("/auth/password")).bearer_auth(&token)
        .json(&serde_json::json!({"current": "correct horse battery", "new": "a new long password"}));
    assert_eq!(by_token.send().await.unwrap().status(), 403);

    assert_eq!(c.post(url("/auth/logout")).bearer_auth(&session).send().await.unwrap().status(), 204);
    assert_eq!(with(&session, "/instances").send().await.unwrap().status(), 401, "sign-out ended it");

    let file = std::fs::read_to_string(dir.path().join("web-accounts.json")).unwrap();
    assert!(!file.contains("correct horse"), "a password reached the file in clear");
}

/// A user runs servers and nothing else; asks to become an admin; an admin
/// promotes them, and their very next request has the admin's reach, with no
/// new sign-in. The node enforces it, not the page.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_runs_servers_only_until_an_admin_makes_them_one() {
    let token = "t".repeat(32);
    let Some((addr, _dir)) = serve_with_token(&token).await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let c = reqwest::Client::new();
    let url = |p: &str| format!("http://{addr}{p}");
    let setup = c.post(url("/auth/setup")).json(&serde_json::json!({"token": token, "username": "op", "password": "correct horse battery"}));
    assert_eq!(setup.send().await.unwrap().status(), 200);
    let login = |user: &'static str, pass: &'static str| {
        let c = c.clone();
        let u = url("/auth/login");
        async move {
            let r: serde_json::Value = c.post(u).json(&serde_json::json!({"username": user, "password": pass})).send().await.unwrap().json().await.unwrap();
            r["session"].as_str().unwrap().to_string()
        }
    };
    let admin = login("op", "correct horse battery").await;
    let added = c.post(url("/auth/users")).bearer_auth(&admin).json(&serde_json::json!({"username": "guest", "password": "another long password"}));
    let added: serde_json::Value = added.send().await.unwrap().json().await.unwrap();
    assert_eq!(added["role"], "user", "a new account is a user unless asked otherwise");

    let guest = login("guest", "another long password").await;
    let status = |s: &str, m: reqwest::Method, p: &str| c.request(m, url(p)).bearer_auth(s.to_string()).send();
    assert_eq!(status(&guest, reqwest::Method::GET, "/instances").await.unwrap().status(), 200, "a user sees servers");
    for (m, p) in [
        (reqwest::Method::POST, "/orphans/clean"),
        (reqwest::Method::GET, "/instances/x/identity"),
        (reqwest::Method::POST, "/packs"),
        (reqwest::Method::GET, "/controller/admin/region-requests"),
        (reqwest::Method::POST, "/auth/users/guest/role"),
        (reqwest::Method::GET, "/auth/users"),
    ] {
        assert_eq!(status(&guest, m.clone(), p).await.unwrap().status(), 403, "{m} {p} is an admin's");
    }

    let asked = c.post(url("/auth/admin-request")).bearer_auth(&guest).send().await.unwrap();
    assert_eq!(asked.status(), 200);
    let users: serde_json::Value = c.get(url("/auth/users")).bearer_auth(&admin).send().await.unwrap().json().await.unwrap();
    let g = users.as_array().unwrap().iter().find(|u| u["username"] == "guest").unwrap();
    assert!(g["admin_requested_unix"].is_u64(), "the admin sees the request");

    let promote = c.post(url("/auth/users/guest/role")).bearer_auth(&admin).json(&serde_json::json!({"role": "admin"}));
    assert_eq!(promote.send().await.unwrap().status(), 200);
    assert_ne!(status(&guest, reqwest::Method::POST, "/orphans/clean").await.unwrap().status(), 403, "the same session now reaches it");
    let demote_last = c.post(url("/auth/users/op/role")).bearer_auth(&admin).json(&serde_json::json!({"role": "user"}));
    assert_eq!(demote_last.send().await.unwrap().status(), 200, "op may step down now that guest is an admin");
    let me: serde_json::Value = c.get(url("/auth/me")).bearer_auth(&admin).send().await.unwrap().json().await.unwrap();
    assert_eq!(me["role"], "user");
}

#[test]
fn a_route_nobody_listed_is_an_admins() {
    use axum::http::Method;
    assert!(platform_agent::api::user_may(&Method::POST, "/instances/srv/stop"));
    assert!(platform_agent::api::user_may(&Method::POST, "/controller/instances/srv/relocate"));
    assert!(!platform_agent::api::user_may(&Method::POST, "/some/route/added/later"));
    assert!(!platform_agent::api::user_may(&Method::PUT, "/instances/srv/identity"));
    assert!(!platform_agent::api::user_may(&Method::POST, "/controller/admin-requests"));
    // Reports carry reporters' contacts; acting on anyone's server is an
    // admin's; a user reads their own notices and may ask for another look.
    assert!(!platform_agent::api::user_may(&Method::GET, "/reports"));
    assert!(!platform_agent::api::user_may(&Method::POST, "/reports/0123456789abcdef/close"));
    assert!(!platform_agent::api::user_may(&Method::POST, "/controller/admin/instances/srv/remove"));
    assert!(!platform_agent::api::user_may(&Method::GET, "/controller/admin/instances"));
    assert!(platform_agent::api::user_may(&Method::GET, "/controller/me/notices"));
    assert!(platform_agent::api::user_may(&Method::POST, "/controller/me/notices/0123456789abcdef/review"));
    assert!(platform_agent::api::user_may(&Method::POST, "/auth/delete-me"));
}
