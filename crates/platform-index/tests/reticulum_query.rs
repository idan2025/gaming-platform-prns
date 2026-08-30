//! The offline path, end to end: an index answering a query over Reticulum with
//! no HTTP and no internet anywhere in the picture.
//!
//! This is the test that makes `DESIGN.md` §0 true rather than aspirational. If
//! an index could only be reached over HTTPS, then a directory would require
//! internet, and "no internet required" would be a claim about the game traffic
//! only.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use game_bridge::announce::{AnnounceFlags, AnnounceInfo, AnnounceRecord};
use game_bridge::config::BrowserArgs;
use game_bridge::{BridgeSession, DiscoveredServer};
use personal_rns::prelude::DestinationHash;
use platform_auth::Authenticator;
use platform_index::client::query_index;
use platform_index::http::IndexState;
use platform_index::registry::Registry;
use platform_index::wire::IndexQuery;
use prns_core::identity::PrivateIdentityMaterial;
use prns_core::interfaces::InterfaceId;
use tokio::sync::Mutex;

fn free_tcp_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

fn row(n: u8, game: &str, name: &str, players: u8) -> DiscoveredServer {
    DiscoveredServer {
        destination_hash: DestinationHash::from_slice(&[n; 16]).unwrap(),
        last_seen: Instant::now(),
        hops: 1,
        source_interface: InterfaceId::new([n; 8]),
        info: AnnounceInfo::Record(AnnounceRecord {
            protocol_version: 1,
            flags: AnnounceFlags::default(),
            min_link_class: 1,
            players,
            max_players: 8,
            game_id: game.to_string(),
            name: name.to_string(),
            map: "svencoop1".to_string(),
            tlvs: Vec::new(),
        }),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_launcher_queries_an_index_over_the_mesh_with_no_internet() {
    let tcp_port = free_tcp_port();

    // An index that already knows about three servers.
    let mut registry = Registry::new();
    registry.ingest(
        vec![
            row(1, "sven-coop", "alpha", 3),
            row(2, "sven-coop", "beta", 0),
            row(3, "minecraft", "gamma", 5),
        ],
        Instant::now(),
    );
    let index_key = PrivateIdentityMaterial::from_slice(&[77u8; 64]).unwrap();
    let state = Arc::new(IndexState {
        registry: Mutex::new(registry),
        auth: Mutex::new(Authenticator::new(index_key.identity_hash())),
        hosting: None,
    });

    let secret = personal_rns::prelude::Zeroizing::new([77u8; 64]);
    let mut index = platform_index::node::start(
        state,
        secret,
        Some(format!("0.0.0.0:{tcp_port}")),
        false,
    )
    .await
    .expect("the index node should start");

    // A launcher: a plain browse node, no identity, no destination.
    let mut launcher = BridgeSession::start_browser(BrowserArgs {
        tcp: Some(format!("127.0.0.1:{tcp_port}")),
        auto: false,
    })
    .await
    .expect("the launcher should start");

    // Ask for every game.
    let all = with_retries(&launcher, index.destination(), &IndexQuery {
        include_legacy: true,
        ..Default::default()
    })
    .await;

    let all = all.expect("the index should answer over Reticulum");
    assert_eq!(all.total_matched, 3, "the index knows three servers");
    assert_eq!(all.rows.len(), 3);
    assert!(!all.truncated);

    // And a filtered query is answered by the index, not by the client.
    let sven = query_index(
        launcher.handle(),
        index.destination(),
        &IndexQuery {
            game_id: "sven-coop".to_string(),
            include_legacy: false,
            ..Default::default()
        },
    )
    .await
    .expect("a filtered query should answer");
    assert_eq!(sven.total_matched, 2, "only the two Sven servers match");
    for r in &sven.rows {
        match &r.info {
            AnnounceInfo::Record(rec) => assert_eq!(rec.game_id, "sven-coop"),
            other => panic!("expected a record, got {other:?}"),
        }
    }

    // has_players narrows it further, server-side.
    let busy = query_index(
        launcher.handle(),
        index.destination(),
        &IndexQuery {
            game_id: "sven-coop".to_string(),
            has_players: true,
            ..Default::default()
        },
    )
    .await
    .expect("a has-players query should answer");
    assert_eq!(busy.total_matched, 1, "only alpha has players");

    launcher.stop().await;
    index.stop().await;
}

/// The link needs the interface to come up and a path to resolve, so the first
/// attempt can lose a race that has nothing to do with the index.
async fn with_retries(
    launcher: &BridgeSession,
    index: DestinationHash,
    query: &IndexQuery,
) -> anyhow::Result<platform_index::wire::QueryResult> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match query_index(launcher.handle(), index, query).await {
            Ok(result) => return Ok(result),
            Err(e) if tokio::time::Instant::now() >= deadline => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}
