//! `GAMES.md` §7 step 1, executed: a second game reaches the browser by
//! writing a file.
//!
//! The claim that step makes is strong and worth testing rather than asserting:
//! *"Proves the pack is genuinely data — if this needs any Rust change, the
//! abstraction is wrong."* So this test never mentions Half-Life in code. It
//! reads `packs/half-life.toml` off disk, runs a bridge server from it, and
//! makes a browse node — which was built before that file existed — list it
//! under its own game id and its own destination.
//!
//! Topology is the browse baseline's, deliberately: no index, no account, no
//! internet (`DESIGN.md` §0).

use std::time::Duration;

use game_bridge::browse::BrowseQuery;
use game_bridge::config::{BrowserArgs, ServerArgs};
use game_bridge::pack::GamePack;
use game_bridge::profile::GameProfile;
use game_bridge::BridgeSession;

mod common;

fn shipped(id: &str) -> GamePack {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
    GamePack::load_dir(&dir)
        .expect("the shipped pack directory reads")
        .packs
        .into_iter()
        .find(|p| p.id == id)
        .unwrap_or_else(|| panic!("{id} is shipped"))
}

async fn wait_for<F>(session: &BridgeSession, query: &BrowseQuery, timeout: Duration, ok: F) -> bool
where
    F: Fn(&[game_bridge::DiscoveredServer]) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if ok(&session.browse(query).await) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_game_added_as_a_file_shows_up_in_the_browser() {
    let mesh_port = common::free_tcp_port();
    let dir = common::scratch_dir("second-game");

    let pack = shipped("half-life");
    let profile: GameProfile = pack.to_profile().expect("a shipped pack is usable");

    let mut server_args = ServerArgs::new(profile.clone());
    server_args.identity = dir.join("server.identity");
    server_args.tcp = Some(format!("0.0.0.0:{mesh_port}"));
    server_args.announce_interval = 1;
    server_args.name = Some("Sibling Test Server".to_string());
    server_args.players = 2;
    server_args.max_players = 16;
    server_args.map = Some("crossfire".to_string());
    // Never contacted: browsing must not require a running game.
    server_args.game_port = 1;
    let _server = BridgeSession::start_server(server_args)
        .await
        .expect("a bridge server starts from a pack it has never seen before");

    let mut browser_args = BrowserArgs::new();
    browser_args.tcp = Some(format!("127.0.0.1:{mesh_port}"));
    let browser = BridgeSession::start_browser(browser_args)
        .await
        .expect("browser starts");

    let query = BrowseQuery::default();
    let found = wait_for(&browser, &query, Duration::from_secs(30), |rows| {
        rows.iter().any(|r| r.game_id() == Some("half-life"))
    })
    .await;

    let rows = browser.browse(&query).await;
    assert!(found, "the browser never heard the new game; saw {} row(s)", rows.len());

    let row = rows
        .iter()
        .find(|r| r.game_id() == Some("half-life"))
        .expect("the row is present");
    assert_eq!(row.name(), Some("Sibling Test Server"));
    let record = row.record().expect("the announce parsed as a §3.3 record");
    assert_eq!(record.players, 2);
    assert_eq!(record.max_players, 16);

    // Its own destination, derived from its own app_name: a second game is not
    // a label on the first one's servers. The browse node saw exactly one row,
    // and it is not Sven Co-op's.
    assert_ne!(profile.app_name, GameProfile::sven_coop().app_name);
    assert!(
        !rows.iter().any(|r| r.game_id() == Some("sven-coop")),
        "nothing announced sven-coop here, so nothing may list it"
    );
}
