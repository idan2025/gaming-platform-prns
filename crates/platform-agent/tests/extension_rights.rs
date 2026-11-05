//! A build's own routes: which a user account may call, and the ones that sit
//! outside the gate. A build can open its own routes to users and none of the
//! node's.
//!
//! Skips itself without Docker, like the other API tests: `Agent::new` pings
//! the daemon on the way up.

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use platform_agent::agent::Agent;
use platform_agent::api::{self, ApiState, Extension};
use platform_agent::config::AgentConfig;

const TOKEN: &str = "extension-rights-token-0123456789";
const PASSWORD: &str = "a-long-enough-test-password";

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

/// Opens everything it is asked about: the node's own refusals must still hold.
fn opens_everything(_: &axum::http::Method, _: &str) -> Option<bool> {
    Some(true)
}

fn opens_its_own(_: &axum::http::Method, path: &str) -> Option<bool> {
    path.starts_with("/ext/").then_some(true)
}

/// Closes a GET the node would let a user make, and its own route.
fn closes_some(method: &axum::http::Method, path: &str) -> Option<bool> {
    (path == "/usage" || (path == "/ext/act" && method == axum::http::Method::POST))
        .then_some(false)
}

async fn serve(
    user_may: Option<fn(&axum::http::Method, &str) -> Option<bool>>,
) -> Option<(String, String, tempfile::TempDir)> {
    let dir = tempfile::tempdir().ok()?;
    let config = AgentConfig::parse(&config_toml(dir.path())).ok()?;
    let agent = Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await.ok()?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let addr = listener.local_addr().ok()?;
    let routes: Router<ApiState> = Router::new().route("/ext/act", post(|| async { "acted" }));
    let open_routes: Router<ApiState> = Router::new()
        .route("/ext/open", get(|| async { "open to all" }))
        .route("/ext/open/ask", post(|| async { "asked" }));
    let router = api::router_with(
        Arc::new(agent),
        Some(TOKEN.to_string()),
        None,
        None,
        Extension { routes, open_routes, user_may },
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let ok = |r: reqwest::Response| r.status().is_success();
    let setup = client
        .post(format!("{base}/auth/setup"))
        .json(&serde_json::json!({ "token": TOKEN, "username": "ana", "password": PASSWORD }))
        .send()
        .await
        .ok()?;
    assert!(ok(setup));
    let made = client
        .post(format!("{base}/auth/users"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({ "username": "bo", "password": PASSWORD }))
        .send()
        .await
        .ok()?;
    assert!(ok(made));
    let login: serde_json::Value = client
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "username": "bo", "password": PASSWORD }))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let session = login["session"].as_str()?.to_string();
    Some((base, session, dir))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_opens_its_own_routes_to_users_and_none_of_the_nodes() {
    let client = reqwest::Client::new();

    // No answer from the build: its route is an admin's, like any unnamed one.
    let Some((base, user, _dir)) = serve(None).await else {
        eprintln!("skipped: no Docker daemon");
        return;
    };
    let act = client.post(format!("{base}/ext/act")).bearer_auth(&user).send().await.unwrap();
    assert_eq!(act.status(), 403, "a user reached a build's route nobody opened");
    let admin = client.post(format!("{base}/ext/act")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(admin.status(), 200);

    // The build opens its own route.
    let (base, user, _dir2) = serve(Some(opens_its_own)).await.unwrap();
    let act = client.post(format!("{base}/ext/act")).bearer_auth(&user).send().await.unwrap();
    assert_eq!(act.status(), 200);
    assert_eq!(act.text().await.unwrap(), "acted");

    // A build that says yes to everything still cannot open what the node
    // refuses by name: the account list stays an admin's.
    let (base, user, _dir3) = serve(Some(opens_everything)).await.unwrap();
    let users = client
        .post(format!("{base}/auth/users"))
        .bearer_auth(&user)
        .json(&serde_json::json!({ "username": "cy", "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(users.status(), 403, "a build opened the node's account list to a user");
    let identity =
        client.put(format!("{base}/instances/x/identity")).bearer_auth(&user).send().await.unwrap();
    assert_eq!(identity.status(), 403, "a build opened a server's identity to a user");

    // A build can close what the node would open: a GET a user may make.
    let (base, user, _dir4) = serve(Some(closes_some)).await.unwrap();
    let usage = client.get(format!("{base}/usage")).bearer_auth(&user).send().await.unwrap();
    assert_eq!(usage.status(), 403, "a build could not close a GET to users");
    let usage = client.get(format!("{base}/usage")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(usage.status(), 200, "closing it to users closed it to the admin");
    let health = client.get(format!("{base}/health")).bearer_auth(&user).send().await.unwrap();
    assert_eq!(health.status(), 200, "a build's refusal reached past what it named");
}

#[tokio::test(flavor = "multi_thread")]
async fn open_routes_need_no_sign_in_and_keep_the_pages_policy() {
    let Some((base, _user, _dir)) = serve(None).await else {
        eprintln!("skipped: no Docker daemon");
        return;
    };
    let client = reqwest::Client::new();
    let open = client.get(format!("{base}/ext/open")).send().await.unwrap();
    assert_eq!(open.status(), 200);
    assert!(open.headers().contains_key("content-security-policy"));
    assert_eq!(open.text().await.unwrap(), "open to all");
    let ask = client.post(format!("{base}/ext/open/ask")).send().await.unwrap();
    assert_eq!(ask.status(), 200);
    // A browser on another site is refused there too, by the browser guard.
    let cross = client
        .post(format!("{base}/ext/open/ask"))
        .header("Sec-Fetch-Site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_eq!(cross.status(), 403);
    // The gated routes stay gated.
    let act = client.post(format!("{base}/ext/act")).send().await.unwrap();
    assert_eq!(act.status(), 401);
}
