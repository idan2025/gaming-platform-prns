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
