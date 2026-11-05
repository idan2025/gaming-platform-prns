//! A listing policy keeps the directory to some servers, and only an index
//! that sets one does.

use std::sync::Arc;
use std::time::Instant;

use game_bridge::announce::{AnnounceFlags, AnnounceInfo, AnnounceRecord};
use game_bridge::DiscoveredServer;
use personal_rns::prelude::DestinationHash;
use platform_auth::Authenticator;
use platform_index::access::{Access, OwnerOnly};
use platform_index::hosting::{HostedInstance, Hosting};
use platform_index::http::{router, IndexState};
use platform_index::quota::AccountId;
use platform_index::registry::Registry;
use prns_core::identity::PrivateIdentityMaterial;
use prns_core::interfaces::InterfaceId;
use tokio::sync::Mutex;

fn row(n: u8) -> DiscoveredServer {
    DiscoveredServer {
        destination_hash: DestinationHash::from_slice(&[n; 16]).unwrap(),
        last_seen: Instant::now(),
        hops: 1,
        source_interface: InterfaceId::new([n; 8]),
        info: AnnounceInfo::Record(AnnounceRecord {
            protocol_version: 1,
            flags: AnnounceFlags::default(),
            min_link_class: 1,
            players: 0,
            max_players: 8,
            game_id: "sven-coop".to_string(),
            name: format!("server-{n}"),
            map: "svencoop1".to_string(),
            tlvs: Vec::new(),
        }),
    }
}

fn hosting_config() -> platform_index::hosting::HostingConfig {
    platform_index::hosting::HostingConfig {
        games: vec!["sven-coop".to_string()],
        nodes: vec![platform_index::hosting::NodeConfig {
            name: "local".to_string(),
            api: "http://127.0.0.1:1".to_string(),
            agent: None,
            priority: 0,
            class: None,
            region: None,
        }],
        quota: Default::default(),
        admins: Vec::new(),
    }
}

/// Lists only the server ending in 1; owners keep today's rule.
struct OnlyOne;

impl Access for OnlyOne {
    fn may_see(&self, account: &AccountId, server: &HostedInstance) -> bool {
        OwnerOnly.may_see(account, server)
    }
    fn may_operate(&self, account: &AccountId, server: &HostedInstance) -> bool {
        OwnerOnly.may_operate(account, server)
    }
    fn listed(
        &self,
        _config: &platform_index::hosting::HostingConfig,
        destination: &[u8; 16],
    ) -> bool {
        destination == &[1u8; 16]
    }
}

async fn names(hosting: Option<Hosting>) -> Vec<String> {
    let key = PrivateIdentityMaterial::from_slice(&[9u8; 64]).unwrap();
    let mut registry = Registry::new();
    registry.ingest(vec![row(1), row(2)], Instant::now());
    let state = Arc::new(IndexState {
        registry: Mutex::new(registry),
        auth: Mutex::new(Authenticator::new(key.identity_hash())),
        hosting,
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    let rows: Vec<serde_json::Value> =
        reqwest::get(format!("http://{addr}/servers")).await.unwrap().json().await.unwrap();
    let mut names: Vec<String> =
        rows.iter().map(|r| r["name"].as_str().unwrap().to_string()).collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listing_policy_keeps_the_directory_to_the_servers_it_names() {
    assert_eq!(
        names(None).await,
        vec!["server-1", "server-2"],
        "an index with no hosting lists everything"
    );
    assert_eq!(
        names(Some(Hosting::new(hosting_config()))).await,
        vec!["server-1", "server-2"],
        "the default policy lists everything"
    );
    assert_eq!(
        names(Some(Hosting::new(hosting_config()).with_access(Arc::new(OnlyOne)))).await,
        vec!["server-1"],
        "a policy that lists one server listed another"
    );
}
