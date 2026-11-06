//! An owner's tools carried over the uplink (`OP_OWNER`) run against the
//! node's own routes, so every check there applies, and only from the closed
//! list: the console never runs, whatever the index asks.
//!
//! Skips itself without Docker, like the other API tests: `Agent::new` pings
//! the daemon on the way up.

use std::sync::Arc;

use platform_agent::agent::Agent;
use platform_agent::config::AgentConfig;
use platform_agent::uplink::owner_call;
use platform_agent::uplink_wire::OwnerReq;

fn config_toml(data_root: &std::path::Path) -> String {
    format!(
        r#"
data_root = "{}"
max_instances = 2
api_bind = "127.0.0.1:0"

[port_range]
start = 27190
end = 27199

[games.sven-coop]
image = "gpp-test-sleeper:latest"
content_root = "/game"
content_version = "test"
"#,
        data_root.display()
    )
}

fn req(method: &str, path: &str, query: Option<&str>, body: Option<serde_json::Value>) -> OwnerReq {
    OwnerReq {
        token: String::new(),
        instance_id: "sven-coop-0a1b2c".into(),
        method: method.into(),
        path: path.into(),
        query: query.map(str::to_string),
        body,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owners_tool_runs_the_nodes_own_route_and_the_console_never_runs() {
    let dir = tempfile::tempdir().unwrap();
    let config = AgentConfig::parse(&config_toml(dir.path())).unwrap();
    let agent = match Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await {
        Ok(a) => Arc::new(a),
        Err(e) => {
            eprintln!("skipping: {e:#}");
            return;
        }
    };

    // Off the list: refused before any route runs.
    let console = owner_call(&agent, &req("POST", "console", None, Some(serde_json::json!({"line": "quit"})))).await;
    assert!(console.unwrap_err().to_string().contains("not one of the owner's tools"));
    assert!(owner_call(&agent, &req("POST", "chat/../console", None, None)).await.is_err());
    let mut bad_id = req("GET", "players", None, None);
    bad_id.instance_id = "../health".into();
    assert!(owner_call(&agent, &bad_id).await.is_err(), "an id that is a path");

    // On the list: the node's own route answers, with its own checks.
    let players = owner_call(&agent, &req("GET", "players", None, None)).await.unwrap();
    assert_eq!(players.status, 404, "no such server here: {}", players.body);
    let crash = owner_call(&agent, &req("POST", "restart-on-crash", None, Some(serde_json::json!({"on": true})))).await.unwrap();
    assert_eq!(crash.status, 404, "the route's own check, not the tunnel's: {}", crash.body);
    let schedule = owner_call(&agent, &req("GET", "restart-schedule", None, None)).await.unwrap();
    assert_eq!((schedule.status, schedule.body.as_str()), (200, "{}"), "no schedule, and nobody else's");
}
