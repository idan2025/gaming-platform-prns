//! An operator's own routes are inside the same gate as the built-in ones.
//!
//! Skips itself without Docker, like the other API tests: `Agent::new` pings
//! the daemon on the way up.

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use platform_agent::agent::Agent;
use platform_agent::api::{self, ApiState};
use platform_agent::config::AgentConfig;

const TOKEN: &str = "extension-test-token-0123456789";

fn config_toml(data_root: &std::path::Path) -> String {
    format!(
        r#"
data_root = "{}"
max_instances = 2
api_bind = "127.0.0.1:0"

[port_range]
start = 27160
end = 27169

[games.sven-coop]
image = "gpp-test-sleeper:latest"
content_root = "/game"
content_version = "test"
"#,
        data_root.display()
    )
}

/// An extension route answering with a fixed body, and one that checks the
/// node's name through the state it was given.
fn extension() -> Router<ApiState> {
    Router::new().route("/ext/hello", get(|| async { "hello from an extension" })).route(
        "/ext/node",
        get(|axum::extract::State(state): axum::extract::State<ApiState>| async move {
            state.agent.config().data_root.display().to_string()
        }),
    )
}

async fn serve(token: Option<&str>) -> Option<(std::net::SocketAddr, tempfile::TempDir)> {
    let dir = tempfile::tempdir().ok()?;
    let config = AgentConfig::parse(&config_toml(dir.path())).ok()?;
    let agent = Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await.ok()?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let addr = listener.local_addr().ok()?;
    let router = api::router_extended(
        Arc::new(agent),
        token.map(str::to_string),
        None,
        None,
        vec![extension()],
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Some((addr, dir))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_extension_route_needs_the_token_like_a_built_in_one() {
    let Some((addr, _dir)) = serve(Some(TOKEN)).await else {
        eprintln!("skipped: no Docker daemon");
        return;
    };
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/ext/hello");

    let refused = client.get(&url).send().await.unwrap();
    assert_eq!(refused.status(), 401, "an extension route answered without the token");

    let wrong = client.get(&url).bearer_auth("not-the-token").send().await.unwrap();
    assert_eq!(wrong.status(), 401);

    let ok = client.get(&url).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.text().await.unwrap(), "hello from an extension");

    // Built-in routes are gated the same way, and the extension sees the agent.
    let health = client.get(format!("http://{addr}/health")).send().await.unwrap();
    assert_eq!(health.status(), 401, "/health answered without the token");
    let health =
        client.get(format!("http://{addr}/health")).bearer_auth(TOKEN).send().await.unwrap();
    assert!(health.status().is_success());
    let node =
        client.get(format!("http://{addr}/ext/node")).bearer_auth(TOKEN).send().await.unwrap();
    assert!(node.text().await.unwrap().contains(&_dir.path().display().to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_token_extension_routes_stay_loopback_only_like_the_rest() {
    let Some((addr, _dir)) = serve(None).await else {
        eprintln!("skipped: no Docker daemon");
        return;
    };
    // No token configured: the same open-loopback rule as every built-in route.
    let res = reqwest::get(format!("http://{addr}/ext/hello")).await.unwrap();
    assert_eq!(res.status(), 200);
}
