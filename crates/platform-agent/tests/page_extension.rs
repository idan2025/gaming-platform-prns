//! The operator's page code is served beside the page, outside the gate, from
//! two fixed names and no other.
//!
//! Skips itself without Docker, like the other API tests: `Agent::new` pings
//! the daemon on the way up.

use std::sync::Arc;

use platform_agent::agent::Agent;
use platform_agent::api;
use platform_agent::config::AgentConfig;

const TOKEN: &str = "page-extension-test-token-0123456789";
const EXT_JS: &str = "window.__extLoaded = true;";
const EXT_CSS: &str = ".tab-panel { outline: 0; }";

fn config_toml(data_root: &std::path::Path, ui_extension: Option<&std::path::Path>) -> String {
    let ext =
        ui_extension.map(|p| format!("ui_extension = \"{}\"\n", p.display())).unwrap_or_default();
    format!(
        r#"
data_root = "{}"
max_instances = 2
api_bind = "127.0.0.1:0"
{ext}
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

async fn serve(
    ui_extension: Option<&std::path::Path>,
) -> Option<(std::net::SocketAddr, tempfile::TempDir)> {
    let dir = tempfile::tempdir().ok()?;
    let config = AgentConfig::parse(&config_toml(dir.path(), ui_extension)).ok()?;
    let agent = Agent::new(config, vec![game_bridge::GamePack::sven_coop()]).await.ok()?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let addr = listener.local_addr().ok()?;
    let router =
        api::router_extended(Arc::new(agent), Some(TOKEN.to_string()), None, None, Vec::new());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Some((addr, dir))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_operators_page_code_is_served_beside_the_page_and_nothing_else_is() {
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("ext.js"), EXT_JS).unwrap();
    std::fs::write(folder.path().join("ext.css"), EXT_CSS).unwrap();
    std::fs::write(folder.path().join("secret.txt"), "not for the page").unwrap();
    let Some((addr, _dir)) = serve(Some(folder.path())).await else {
        eprintln!("skipped: no Docker daemon");
        return;
    };
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // The page names both files, so it loads them.
    let page = client.get(format!("{base}/")).send().await.unwrap().text().await.unwrap();
    assert!(
        page.contains("ext/ext.js") && page.contains("ext/ext.css"),
        "the page does not load its extension"
    );

    // Served without a token, like the page's own files: a script tag cannot send one.
    let js = client.get(format!("{base}/ext/ext.js")).send().await.unwrap();
    assert_eq!(js.status(), 200);
    assert!(js.headers()["content-type"].to_str().unwrap().starts_with("text/javascript"));
    assert!(
        js.headers().contains_key("content-security-policy"),
        "the page's policy must still apply"
    );
    assert_eq!(js.text().await.unwrap(), EXT_JS);
    let css = client.get(format!("{base}/ext/ext.css")).send().await.unwrap();
    assert_eq!(css.text().await.unwrap(), EXT_CSS);

    // Only the two fixed names: nothing else in the folder, and no path tricks.
    for path in ["/ext/secret.txt", "/ext/../secret.txt", "/ext/..%2Fsecret.txt", "/ext/ext.html"] {
        let res = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(res.status(), 404, "{path} was served");
    }
    // The node's own API stays behind the token.
    let api = client.get(format!("{base}/instances")).send().await.unwrap();
    assert_eq!(api.status(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_folder_the_page_loads_empty_extension_files() {
    let Some((addr, _dir)) = serve(None).await else {
        eprintln!("skipped: no Docker daemon");
        return;
    };
    let client = reqwest::Client::new();
    let js = client.get(format!("http://{addr}/ext/ext.js")).send().await.unwrap();
    assert_eq!(js.status(), 200);
    assert!(js.text().await.unwrap().is_empty());
    let css = client.get(format!("http://{addr}/ext/ext.css")).send().await.unwrap();
    assert_eq!(css.status(), 200);
    assert!(css.text().await.unwrap().is_empty());
}
