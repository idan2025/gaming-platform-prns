//! What the index remembers, and for how long.
//!
//! # An index is a cache of the mesh, and this is where that is literally true
//!
//! The registry is fed by a `game-bridge` **Browse** session — the same code
//! path a launcher uses to find servers with no index at all. The index has no
//! privileged source: it hears announces, like everyone else. That is what makes
//! `DESIGN.md` §0's "an index is a cache of the mesh, never the source of truth"
//! a fact about the code rather than an intention.
//!
//! Two consequences fall out of it, and both are features:
//!
//! - **It lists servers nobody deployed through the platform.** A person running
//!   a bare `game-bridge` server on their own machine appears here, because the
//!   index cannot tell the difference and should not try. The browser is a view
//!   of the mesh, not a view of a database.
//! - **It cannot lie about a server's metadata.** Every row carries the
//!   announce's own Ed25519 signature over its `app_data`, verified by the
//!   engine before the row ever reaches us. An index can decline to list
//!   something; it cannot alter what it relays.
//!
//! # Retention is the one thing an index does differently
//!
//! A launcher forgets a server the moment it stops announcing, which is right
//! for a live list. An index keeps it for a while, so a query during a brief
//! outage does not report the server as gone, and so `first_seen` can say how
//! long something has been around. Retention is therefore the index's whole
//! value-add over listening yourself — and it is bounded, because a cache that
//! never forgets is a database pretending to be a cache.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use game_bridge::browse::{browse, BrowseQuery};
use game_bridge::DiscoveredServer;

/// How long a server stays listed after its last announce.
///
/// Long enough to ride out a reboot or a routing hiccup; short enough that a
/// decommissioned server does not haunt the list for a day. An announce
/// interval is typically 15s, so this is many missed announces, not one.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(15 * 60);

/// A remembered server: what the mesh said, plus when this index first heard it.
#[derive(Debug, Clone)]
pub struct IndexedServer {
    pub server: DiscoveredServer,
    /// When this index first heard this destination. Not a claim about when the
    /// server started — only about this index's own memory, which is all an
    /// index can honestly know.
    pub first_seen: Instant,
}

pub struct Registry {
    servers: HashMap<[u8; 16], IndexedServer>,
    retention: Duration,
}

impl Registry {
    pub fn new() -> Self {
        Self { servers: HashMap::new(), retention: DEFAULT_RETENTION }
    }

    pub fn with_retention(retention: Duration) -> Self {
        Self { servers: HashMap::new(), retention }
    }

    pub fn retention(&self) -> Duration {
        self.retention
    }

    /// Fold in a snapshot from a browse session.
    ///
    /// Takes a snapshot rather than individual events because that is what a
    /// `BridgeSession` offers, and because an index that fell behind an event
    /// stream would develop its own idea of the mesh — the failure this whole
    /// design is arranged to avoid.
    pub fn ingest(&mut self, heard: Vec<DiscoveredServer>, now: Instant) {
        for server in heard {
            let key = *server.destination_hash.as_bytes();
            match self.servers.get_mut(&key) {
                Some(existing) => {
                    // Overwrite the announce wholesale; keep first_seen. The
                    // freshest announce is the truth about the server now, but
                    // when this index first met it is the index's own memory.
                    existing.server = server;
                }
                None => {
                    self.servers.insert(key, IndexedServer { server, first_seen: now });
                }
            }
        }
        self.expire(now);
    }

    /// Forget anything not heard within the retention window.
    pub fn expire(&mut self, now: Instant) {
        let retention = self.retention;
        self.servers.retain(|_, s| {
            now.checked_duration_since(s.server.last_seen)
                .map(|age| age <= retention)
                // A last_seen in the future means a clock or an instant we
                // cannot reason about. Keep it rather than silently dropping a
                // live server.
                .unwrap_or(true)
        });
    }

    pub fn len(&self) -> usize {
        self.servers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Everything currently remembered, unfiltered.
    pub fn all(&self) -> Vec<&IndexedServer> {
        let mut all: Vec<&IndexedServer> = self.servers.values().collect();
        all.sort_by(|a, b| {
            a.server
                .destination_hash
                .as_bytes()
                .cmp(b.server.destination_hash.as_bytes())
        });
        all
    }

    /// Run a browse query over what is remembered.
    ///
    /// Deliberately the *same* filter and sort code a launcher runs locally
    /// (`game_bridge::browse`), so an index and a launcher listening to the same
    /// mesh give the same answer. An index with its own query semantics would be
    /// a second source of truth wearing a cache's clothes.
    pub fn query(&self, query: &BrowseQuery, now: Instant) -> Vec<DiscoveredServer> {
        let rows: Vec<DiscoveredServer> =
            self.servers.values().map(|s| s.server.clone()).collect();
        browse(&rows, query, now).into_iter().cloned().collect()
    }

    pub fn first_seen(&self, destination: &[u8; 16]) -> Option<Instant> {
        self.servers.get(destination).map(|s| s.first_seen)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_bridge::announce::{AnnounceFlags, AnnounceInfo, AnnounceRecord};
    use personal_rns::prelude::DestinationHash;
    use prns_core::interfaces::InterfaceId;

    fn row(n: u8, last_seen: Instant) -> DiscoveredServer {
        DiscoveredServer {
            destination_hash: DestinationHash::from_slice(&[n; 16]).unwrap(),
            last_seen,
            hops: 1,
            source_interface: InterfaceId::new([n; 8]),
            info: AnnounceInfo::Record(AnnounceRecord {
                protocol_version: 1,
                flags: AnnounceFlags::default(),
                min_link_class: 1,
                players: 2,
                max_players: 8,
                game_id: "sven-coop".to_string(),
                name: format!("server-{n}"),
                map: "svencoop1".to_string(),
                tlvs: Vec::new(),
            }),
        }
    }

    fn legacy_row(n: u8, last_seen: Instant) -> DiscoveredServer {
        DiscoveredServer {
            destination_hash: DestinationHash::from_slice(&[n; 16]).unwrap(),
            last_seen,
            hops: 2,
            source_interface: InterfaceId::new([n; 8]),
            info: AnnounceInfo::Legacy { name: Some(format!("old-{n}")) },
        }
    }

    #[test]
    fn ingest_dedupes_by_destination_and_keeps_the_newest_announce() {
        let t = Instant::now();
        let mut reg = Registry::new();
        reg.ingest(vec![row(1, t)], t);

        let mut updated = row(1, t);
        if let AnnounceInfo::Record(r) = &mut updated.info {
            r.players = 7;
        }
        reg.ingest(vec![updated], t);

        assert_eq!(reg.len(), 1);
        let stored = reg.all()[0];
        assert_eq!(stored.server.record().unwrap().players, 7);
    }

    #[test]
    fn first_seen_survives_later_announces() {
        let t = Instant::now();
        let mut reg = Registry::new();
        reg.ingest(vec![row(1, t)], t);
        let first = reg.first_seen(&[1; 16]).unwrap();

        let later = t + Duration::from_secs(60);
        reg.ingest(vec![row(1, later)], later);
        assert_eq!(reg.first_seen(&[1; 16]).unwrap(), first);
    }

    #[test]
    fn a_server_is_forgotten_after_the_retention_window() {
        let t = Instant::now();
        let mut reg = Registry::with_retention(Duration::from_secs(60));
        reg.ingest(vec![row(1, t)], t);
        assert_eq!(reg.len(), 1);

        reg.expire(t + Duration::from_secs(59));
        assert_eq!(reg.len(), 1, "still inside the window");
        reg.expire(t + Duration::from_secs(61));
        assert_eq!(reg.len(), 0, "past the window it should be gone");
    }

    /// An index keeps a server through a brief outage; that is the whole reason
    /// to run one rather than just listening yourself.
    #[test]
    fn a_brief_outage_does_not_drop_a_server() {
        let t = Instant::now();
        let mut reg = Registry::with_retention(Duration::from_secs(15 * 60));
        reg.ingest(vec![row(1, t)], t);
        // Five minutes of silence: many missed announces, still listed.
        reg.expire(t + Duration::from_secs(300));
        assert_eq!(reg.len(), 1);
    }

    /// A server nobody deployed through the platform is still listed. The index
    /// is a view of the mesh, not of a database.
    #[test]
    fn a_legacy_server_the_platform_never_deployed_is_listed() {
        let t = Instant::now();
        let mut reg = Registry::new();
        reg.ingest(vec![legacy_row(9, t)], t);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.all()[0].server.name(), Some("old-9"));
        assert_eq!(reg.all()[0].server.game_id(), None);
    }

    /// The index answers with the same filter code a launcher runs locally, so
    /// the two cannot disagree about what matches.
    #[test]
    fn query_uses_the_launchers_own_filtering() {
        use game_bridge::browse::BrowseFilter;
        let t = Instant::now();
        let mut reg = Registry::new();
        reg.ingest(vec![row(1, t), row(2, t), legacy_row(3, t)], t);

        let all = reg.query(&BrowseQuery::default(), t);
        assert_eq!(all.len(), 3, "an unfiltered query lists legacy rows too");

        let sven = reg.query(
            &BrowseQuery {
                filter: BrowseFilter {
                    game_id: Some("sven-coop".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
            t,
        );
        assert_eq!(sven.len(), 2, "a game filter excludes the row that cannot answer");
    }

    /// A `last_seen` in the future is a clock we cannot reason about. Keeping
    /// the row is the safe direction: dropping a live server is worse than
    /// listing one a moment too long.
    #[test]
    fn a_future_timestamp_does_not_drop_a_server() {
        let t = Instant::now();
        let mut reg = Registry::with_retention(Duration::from_secs(60));
        reg.ingest(vec![row(1, t + Duration::from_secs(600))], t);
        reg.expire(t);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn listing_is_deterministic() {
        let t = Instant::now();
        let mut reg = Registry::new();
        reg.ingest(vec![row(3, t), row(1, t), row(2, t)], t);
        let order: Vec<u8> = reg.all().iter().map(|s| s.server.destination_hash.as_bytes()[0]).collect();
        assert_eq!(order, vec![1, 2, 3]);
    }
}
