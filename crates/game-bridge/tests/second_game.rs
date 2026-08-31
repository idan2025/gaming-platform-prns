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
//!
//! `a_multi_port_game_is_data_too` is §7 **step 2**'s half of the same claim.
//! Multi-port was the rung that forced Rust work (framing generation 2, stream
//! ids, the announce gate) — so the thing worth pinning is that having paid for
//! it once, a Source-engine game is a file again. It names no game either, and
//! finds its subject by the property under test rather than by id.

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

/// `GAMES.md` §7 step 2: adding a Source-engine game is data, now that
/// multi-port is built.
///
/// The pack is found by *declaring extra ports*, not by name — the same
/// discipline as the test above, for the same reason. A test that reached for
/// "team-fortress-2" would keep passing if the abstraction were replaced by a
/// hardcoded special case for that one id.
#[test]
fn a_multi_port_game_is_data_too() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
    let packs = GamePack::load_dir(&dir).expect("the shipped pack directory reads").packs;

    let multi: Vec<GameProfile> = packs
        .iter()
        .map(|p| p.to_profile().expect("a shipped pack is usable"))
        .filter(|p| !p.extra_ports.is_empty())
        .collect();
    assert!(
        !multi.is_empty(),
        "no shipped pack declares extra ports, so nothing proves a multi-port game is data"
    );

    for profile in &multi {
        // Channel 0 is the game and is synthesized, never declared: the frozen
        // port is written down in exactly one place.
        let ports = profile.ports();
        assert_eq!(ports.len(), 1 + profile.extra_ports.len(), "{}", profile.id);
        assert_eq!(ports[0].channel, 0, "{}", profile.id);
        assert_eq!(ports[0].port, profile.default_port, "{}", profile.id);

        // Only a multi-port game announces generation 2. That number is what
        // tells a peer it may send a non-zero channel id here, and a legacy
        // peer that never hears it keeps working.
        assert_eq!(profile.protocol_version(), 2, "{}", profile.id);

        // A Source server binds RCON on the same number as its game port. That
        // is not a conflict on the wire, because the channel separates them —
        // so the pack must be allowed to say it, and this is the case that
        // would break if a validator ever "helpfully" rejected a duplicate
        // port number.
        let channels: std::collections::BTreeSet<u8> = ports.iter().map(|p| p.channel).collect();
        assert_eq!(channels.len(), ports.len(), "{} reuses a channel", profile.id);
    }

    // And the single-port packs are untouched by any of it: a game that needs
    // no channels must not start advertising a capability it never exercises.
    for pack in &packs {
        let profile = pack.to_profile().expect("a shipped pack is usable");
        if profile.extra_ports.is_empty() {
            assert_eq!(profile.protocol_version(), 1, "{}", profile.id);
        }
    }
}
