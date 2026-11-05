//! A build that listens hears the node's account events, and only those it
//! was told about: a sign-in and a removal, through the page's own routes.
//!
//! Skips itself without Docker, like the other API tests: `Agent::new` pings
//! the daemon on the way up.

use std::sync::{Arc, Mutex};

use platform_agent::agent::Agent;
use platform_agent::api;
use platform_agent::config::AgentConfig;
use platform_agent::events::NodeEvents;

const TOKEN: &str = "node-events-test-token-0123456789";
const PASSWORD: &str = "a-long-enough-test-password";

#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl NodeEvents for Recorder {
    fn session_started(&self, username: &str) {
        self.0.lock().unwrap().push(format!("session {username}"));
    }
    fn account_removed(&self, username: &str) {
        self.0.lock().unwrap().push(format!("removed {username}"));
    }
}

fn config_toml(data_root: &std::path::Path) -> String {
    format!(
        r#"
data_root = "{}"
max_instances = 2
api_bind = "127.0.0.1:0"

[port_range]
start = 27180
end = 27189

[games.sven-coop]
image = "gpp-test-sleeper:latest"
content_root = "/game"
content_version = "test"
"#,
        data_root.display()
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listener_hears_sign_in_and_removal_and_nothing_else() {
    let Ok(dir) = tempfile::tempdir() else { return };
    let Ok(config) = AgentConfig::parse(&config_toml(dir.path())) else { return };
    let Ok(mut agent) = Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await else {
        eprintln!("skipped: no Docker daemon");
        return;
    };
    let recorder = Arc::new(Recorder::default());
    agent.set_events(recorder.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router =
        api::router_extended(Arc::new(agent), Some(TOKEN.to_string()), None, None, Vec::new());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let setup = client
        .post(format!("{base}/auth/setup"))
        .json(&serde_json::json!({ "token": TOKEN, "username": "ana", "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert!(setup.status().is_success(), "setup refused: {}", setup.status());
    let made = client
        .post(format!("{base}/auth/users"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({ "username": "bo", "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert!(made.status().is_success(), "adding bo refused: {}", made.status());
    assert!(
        recorder.0.lock().unwrap().is_empty(),
        "nothing is heard for making an account or the setup"
    );

    let refused = client
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "username": "bo", "password": "wrong-password-here" }))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);
    assert!(recorder.0.lock().unwrap().is_empty(), "a refused sign-in is not a session");

    let ok = client
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "username": "bo", "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert!(ok.status().is_success());
    let removed =
        client.delete(format!("{base}/auth/users/bo")).bearer_auth(TOKEN).send().await.unwrap();
    assert!(removed.status().is_success(), "removing bo refused: {}", removed.status());

    assert_eq!(
        *recorder.0.lock().unwrap(),
        vec!["session bo".to_string(), "removed bo".to_string()],
        "a sign-in and a removal, in that order, and no others"
    );
}
