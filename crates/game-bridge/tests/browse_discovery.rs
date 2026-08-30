//! The zero-infrastructure baseline, as a test.
//!
//! `DESIGN.md` §0 and `PLAN.md` §8 phase 2 both rest on one claim: two nodes on
//! any shared Reticulum interface find each other with **no index, no account
//! and no internet**. This test is that claim, executed.
//!
//! Topology, entirely on localhost:
//!
//!   bridge server (announces `sven-coop.server`)
//!        |  TCP interface on 127.0.0.1
//!   browse node (announces nothing, binds no game port, forwards nothing)
//!
//! The browse node must end up listing the server, with the game id it
//! advertised — which is the whole point of putting the game id in `app_data`
//! (`PLAN.md` §3.1), since the destination hash cannot be reversed to a game.

use std::time::Duration;

use game_bridge::browse::{BrowseFilter, BrowseQuery};
use game_bridge::config::{BrowserArgs, ServerArgs};
use game_bridge::profile::GameProfile;
use game_bridge::BridgeSession;

mod common;

/// Poll `browse` until it returns something matching, or give up.
async fn wait_for<F>(session: &BridgeSession, query: &BrowseQuery, timeout: Duration, ok: F) -> bool
where
    F: Fn(&[game_bridge::DiscoveredServer]) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let rows = session.browse(query).await;
        if ok(&rows) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_browse_node_finds_a_server_with_no_index_and_no_internet() {
    let tcp_port = common::free_tcp_port();
    let dir = common::scratch_dir("browse");

    let mut server_args = ServerArgs::new(GameProfile::sven_coop());
    server_args.identity = dir.join("server.identity");
    server_args.tcp = Some(format!("0.0.0.0:{tcp_port}"));
    server_args.announce_interval = 1;
    server_args.name = Some("Loopback Test Server".to_string());
    server_args.players = 3;
    server_args.max_players = 8;
    server_args.map = Some("svencoop1".to_string());
    // The game port is never contacted in this test: browsing must not require
    // a running game, on either side.
    server_args.game_port = 1;

    let mut server = BridgeSession::start_server(server_args).await.expect("server starts");

    let mut browser_args = BrowserArgs::new();
    browser_args.tcp = Some(format!("127.0.0.1:{tcp_port}"));
    let mut browser = BridgeSession::start_browser(browser_args).await.expect("browser starts");

    let query = BrowseQuery::default();
    let found = wait_for(&browser, &query, Duration::from_secs(30), |rows| {
        rows.iter().any(|r| r.game_id() == Some("sven-coop"))
    })
    .await;

    let rows = browser.browse(&query).await;
    assert!(found, "browse node never heard the server; saw {} row(s)", rows.len());

    let row = rows
        .iter()
        .find(|r| r.game_id() == Some("sven-coop"))
        .expect("the row is present");
    assert_eq!(row.name(), Some("Loopback Test Server"));
    let record = row.record().expect("a platform server announces a record");
    assert_eq!((record.players, record.max_players), (3, 8));
    assert_eq!(record.map, "svencoop1");
    assert_eq!(record.min_link_class, 1);
    assert_eq!(
        row.destination_hash,
        server.own_hash().expect("the server has a destination"),
        "the browser must list the server's own destination, not some other node's"
    );

    browser.stop().await;
    server.stop().await;
}

/// A server announcing the way deployed `svencoop-prns` v0.1.10 does — a bare
/// UTF-8 name and nothing else — must still appear in the browser, by name.
///
/// This is `PLAN.md` §5 at the network level rather than the codec level: the
/// fallback decode is what keeps every already-deployed Sven server listable.
/// It appears without a game id, because a legacy announce carries none and the
/// destination hash cannot be reversed to one (`PLAN.md` §3.1). A browser must
/// show it as unattributed rather than guess.
#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_announce_is_still_listed_by_name() {
    let tcp_port = common::free_tcp_port();
    let dir = common::scratch_dir("browse-legacy");

    let mut server_args = ServerArgs::new(GameProfile::sven_coop());
    server_args.identity = dir.join("server.identity");
    server_args.tcp = Some(format!("0.0.0.0:{tcp_port}"));
    server_args.announce_interval = 1;
    server_args.game_port = 1;
    server_args.name = Some("A v0.1.10 Server".to_string());
    server_args.announce_format = game_bridge::config::AnnounceFormat::Legacy;
    let mut server = BridgeSession::start_server(server_args).await.expect("server starts");

    let mut browser_args = BrowserArgs::new();
    browser_args.tcp = Some(format!("127.0.0.1:{tcp_port}"));
    let mut browser = BridgeSession::start_browser(browser_args).await.expect("browser starts");

    let query = BrowseQuery::default();
    assert!(
        wait_for(&browser, &query, Duration::from_secs(30), |rows| {
            rows.iter().any(|r| r.name() == Some("A v0.1.10 Server"))
        })
        .await,
        "a legacy announce vanished from the browser"
    );

    let rows = browser.browse(&query).await;
    let row = rows
        .iter()
        .find(|r| r.name() == Some("A v0.1.10 Server"))
        .expect("the legacy row is present");
    assert_eq!(row.game_id(), None, "a legacy announce carries no game id");
    assert!(row.record().is_none(), "a legacy announce is not a record");

    browser.stop().await;
    server.stop().await;
}

/// A filter that asks about a field only a record carries must not smuggle in
/// legacy rows — and must still find the record ones.
#[tokio::test(flavor = "multi_thread")]
async fn filtering_by_game_id_finds_the_server() {
    let tcp_port = common::free_tcp_port();
    let dir = common::scratch_dir("browse-filter");

    let mut server_args = ServerArgs::new(GameProfile::sven_coop());
    server_args.identity = dir.join("server.identity");
    server_args.tcp = Some(format!("0.0.0.0:{tcp_port}"));
    server_args.announce_interval = 1;
    server_args.game_port = 1;
    let mut server = BridgeSession::start_server(server_args).await.expect("server starts");

    let mut browser_args = BrowserArgs::new();
    browser_args.tcp = Some(format!("127.0.0.1:{tcp_port}"));
    let mut browser = BridgeSession::start_browser(browser_args).await.expect("browser starts");

    let matching = BrowseQuery {
        filter: BrowseFilter { game_id: Some("sven-coop".to_string()), ..Default::default() },
        ..Default::default()
    };
    assert!(
        wait_for(&browser, &matching, Duration::from_secs(30), |rows| !rows.is_empty()).await,
        "a game_id filter did not find the announced server"
    );

    let other = BrowseQuery {
        filter: BrowseFilter { game_id: Some("minecraft".to_string()), ..Default::default() },
        ..Default::default()
    };
    assert!(
        browser.browse(&other).await.is_empty(),
        "a game_id filter matched a server running a different game"
    );

    browser.stop().await;
    server.stop().await;
}
