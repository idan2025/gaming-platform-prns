//! The node's ops log over its API: the numbers and the events are an admin's
//! to read, and a sign-in that fails is in the log without its password.
//!
//! Skips itself without Docker, like the other API tests: `Agent::new` pings
//! the daemon on the way up.

use std::sync::Arc;

use axum::http::Method;
use platform_agent::agent::Agent;
use platform_agent::api;
use platform_agent::config::AgentConfig;

const TOKEN: &str = "ops-dashboard-test-token-0123456789";

fn config_toml(data_root: &std::path::Path) -> String {
    format!(
        r#"
data_root = "{}"
max_instances = 2
api_bind = "127.0.0.1:0"

[port_range]
start = 27200
end = 27209

[games.sven-coop]
image = "gpp-test-sleeper:latest"
content_root = "/game"
content_version = "test"
"#,
        data_root.display()
    )
}

#[test]
fn the_node_numbers_and_events_are_an_admins_to_read() {
    for path in ["/ops/overview", "/ops/events", "/ops/metrics"] {
        assert!(!api::user_may(&Method::GET, path), "{path} must not be a user's");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_sign_in_is_in_the_log_without_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let config = AgentConfig::parse(&config_toml(dir.path())).unwrap();
    let agent = match Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await {
        Ok(a) => Arc::new(a),
        Err(e) => {
            eprintln!("skipping: {e:#}");
            return;
        }
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = api::router_with(agent.clone(), Some(TOKEN.to_string()), None, None, api::Extension::default());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let c = reqwest::Client::new();
    let url = |p: &str| format!("http://{addr}{p}");
    let setup = c
        .post(url("/auth/setup"))
        .json(&serde_json::json!({"token": TOKEN, "username": "op", "password": "correct horse battery"}))
        .send()
        .await
        .unwrap();
    assert_eq!(setup.status(), 200);

    let secret = "a-password-that-must-not-be-logged";
    let bad = c.post(url("/auth/login")).json(&serde_json::json!({"username": "op", "password": secret})).send().await.unwrap();
    assert_eq!(bad.status(), 401);
    let good = c
        .post(url("/auth/login"))
        .json(&serde_json::json!({"username": "op", "password": "correct horse battery"}))
        .send()
        .await
        .unwrap();
    assert_eq!(good.status(), 200);

    let events = agent.ops().events_since(0, 100);
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"sign_in_failed") && kinds.contains(&"sign_in"), "{kinds:?}");
    let raw = std::fs::read_to_string(dir.path().join("ops-events.jsonl")).unwrap();
    assert!(!raw.contains(secret), "a password reached the ops log");

    let anon = c.get(url("/ops/overview")).send().await.unwrap();
    assert_eq!(anon.status(), 401, "the numbers are not readable without the token");
    let ok: serde_json::Value = c.get(url("/ops/overview")).bearer_auth(TOKEN).send().await.unwrap().json().await.unwrap();
    assert_eq!(ok["counts_24h"]["sign_in_failed"], 1, "{ok}");
    let filtered: serde_json::Value = c
        .get(url("/ops/events?kind=sign_in_failed"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(filtered.as_array().map(Vec::len), Some(1), "{filtered}");
}
