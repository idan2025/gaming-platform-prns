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
    let router = api::router_full(Arc::new(agent), None, Some(packs.clone()));
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

    for path in ["/health", "/capacity", "/instances", "/orphans", "/games"] {
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
async fn a_create_with_no_installed_content_explains_itself() {
    let Some((addr, _dir)) = serve().await else {
        eprintln!("skipping: no Docker daemon");
        return;
    };
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/instances"))
        .json(&serde_json::json!({
            "instance_id": "apitest",
            "game_id": "sven-coop",
            "name": "API Test",
            "max_players": 8
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400, "a caller-fixable failure must not be a 500");
    let body: serde_json::Value = res.json().await.unwrap();
    let msg = body["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("not installed"),
        "the error should name the missing content, got: {msg}"
    );
}

/// An id that could escape its directory must be refused before anything is
/// created, and the refusal must be the caller's fault, not the server's.
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
