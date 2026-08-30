//! Filtering and sorting for the server browser (`PLAN.md` §3.4).
//!
//! Everything here runs over what this node has *heard*. There is no index and
//! no query to anyone: the list is `BridgeSession::discovered()`, filtered and
//! ordered locally. That is the zero-infrastructure baseline of `PLAN.md` §8
//! phase 2, and it is why this module is pure — no I/O, no network, and no
//! clock of its own (`now` is passed in).
//!
//! # Two decisions worth knowing before reading the code
//!
//! **There is no ping, and there will not be one.** The default sort is
//! ascending `hops`, which arrives free in every announce. A ping would have to
//! be measured by opening a Link to every server in the list — exactly the
//! traffic a browser anyone can run must not generate, and a number that would
//! be stale the moment it was taken. `hops` plus the source interface is the
//! honest answer to "will this feel bad".
//!
//! **A legacy row is excluded by any filter that asks about data it does not
//! carry.** Deployed `svencoop-prns` v0.1.x peers announce a bare display name
//! and nothing else — no game id, no player count, no flags, no tier
//! (`PLAN.md` §5). Those servers are real and joinable, so an unfiltered list
//! shows them. But once a caller narrows the list by, say, `game_id`, showing a
//! row whose game is unknown would be answering a question with a guess. So:
//! any filter field that needs record data excludes every legacy row, and
//! `include_legacy` governs whether they appear in an *unnarrowed* list.
//! `filter_requires_record` is the single place that rule lives.
//!
//! Drafted with GLM 5.2 against the §3.4 spec, then reviewed and corrected.

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use crate::relay::DiscoveredServer;

/// Every field is optional or off by default; set fields are ANDed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseFilter {
    /// Exact match. Excludes every legacy row, which has no game id.
    pub game_id: Option<String>,
    pub text: Option<String>,
    pub max_hops: Option<u8>,
    pub max_link_class: Option<u8>,
    pub has_players: bool,
    pub not_full: bool,
    pub exclude_passworded: bool,
    pub exclude_allowlisted: bool,
    pub transport_modes: Option<Vec<u8>>,
    pub dedicated_only: bool,
    pub include_legacy: bool,
}

impl Default for BrowseFilter {
    fn default() -> Self {
        BrowseFilter {
            game_id: None,
            text: None,
            max_hops: None,
            max_link_class: None,
            has_players: false,
            not_full: false,
            exclude_passworded: false,
            exclude_allowlisted: false,
            transport_modes: None,
            dedicated_only: false,
            include_legacy: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    /// Mesh distance, ascending. The default, and the only free one.
    #[default]
    Hops,
    /// Player count, **descending** — a busy server first is what a
    /// player wants. `BrowseQuery::descending` therefore makes this
    /// ascending, since it reverses whatever the primary key already is.
    Players,
    Name,
    /// Most recently heard first. `descending` reverses that too.
    LastSeen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseQuery {
    pub filter: BrowseFilter,
    pub sort: SortBy,
    /// Reverses the **primary** key only. Tie-breakers never reverse, so
    /// a reversed list stays deterministic rather than shuffling within
    /// groups. See `SortBy::Players` for the case where this reads
    /// backwards.
    pub descending: bool,
    /// Drop rows not heard within this long. `None` keeps everything,
    /// which is right for a short-lived list and wrong for a long-running
    /// launcher: a server that went away keeps announcing nothing, and
    /// nothing is indistinguishable from a quiet mesh without an age
    /// limit.
    pub max_age: Option<Duration>,
}

impl Default for BrowseQuery {
    fn default() -> Self {
        BrowseQuery {
            filter: BrowseFilter::default(),
            sort: SortBy::Hops,
            descending: false,
            max_age: None,
        }
    }
}

/// Returns `true` if any filter field requires data that only a full
/// `AnnounceRecord` carries.  Legacy rows (bare display name only) are
/// excluded whenever this returns `true`, regardless of `include_legacy`.
fn filter_requires_record(f: &BrowseFilter) -> bool {
    f.game_id.is_some()
        || f.max_link_class.is_some()
        || f.has_players
        || f.not_full
        || f.exclude_passworded
        || f.exclude_allowlisted
        || f.transport_modes.is_some()
        || f.dedicated_only
}

fn text_matches(name: Option<&str>, map: Option<&str>, needle: &str) -> bool {
    let needle_lower = needle.to_lowercase();
    if needle_lower.is_empty() {
        return true;
    }
    let name_hit = name
        .map(|n| n.to_lowercase().contains(&needle_lower))
        .unwrap_or(false);
    let map_hit = map
        .map(|m| m.to_lowercase().contains(&needle_lower))
        .unwrap_or(false);
    name_hit || map_hit
}

fn is_stale(row: &DiscoveredServer, max_age: Option<Duration>, now: Instant) -> bool {
    match max_age {
        None => false,
        Some(max) => match now.checked_duration_since(row.last_seen) {
            // `now` is earlier than `last_seen` — cannot determine age; keep.
            None => false,
            Some(d) => d > max,
        },
    }
}

fn passes_filter(
    row: &DiscoveredServer,
    f: &BrowseFilter,
    max_age: Option<Duration>,
    now: Instant,
) -> bool {
    if is_stale(row, max_age, now) {
        return false;
    }

    // --- fields that apply to ALL rows (legacy + record) ---

    if let Some(max_hops) = f.max_hops {
        if row.hops > max_hops {
            return false;
        }
    }

    if let Some(ref text) = f.text {
        let name = row.name();
        let map = row.record().map(|r| r.map.as_str());
        if !text_matches(name, map, text) {
            return false;
        }
    }

    // --- record-only fields ---

    let rec = match row.record() {
        Some(r) => r,
        None => {
            // Legacy row: excluded if any record-requiring filter is active,
            // or if the caller explicitly turned off legacy inclusion.
            if filter_requires_record(f) {
                return false;
            }
            if !f.include_legacy {
                return false;
            }
            return true;
        }
    };

    if let Some(ref gid) = f.game_id {
        if rec.game_id != *gid {
            return false;
        }
    }
    if let Some(max_lc) = f.max_link_class {
        if rec.min_link_class > max_lc {
            return false;
        }
    }
    if f.has_players && rec.players == 0 {
        return false;
    }
    if f.not_full && rec.players >= rec.max_players {
        return false;
    }
    if f.exclude_passworded && rec.flags.passworded {
        return false;
    }
    if f.exclude_allowlisted && rec.flags.allowlisted {
        return false;
    }
    if let Some(ref modes) = f.transport_modes {
        if !modes.contains(&rec.flags.transport_mode) {
            return false;
        }
    }
    if f.dedicated_only && !rec.flags.dedicated {
        return false;
    }

    true
}

fn player_count(row: &DiscoveredServer) -> Option<u8> {
    row.record().map(|r| r.players)
}

fn name_lower(row: &DiscoveredServer) -> Option<String> {
    row.name().map(|s| s.to_lowercase())
}

/// Comparator that never panics.  `descending` reverses only the primary key;
/// tie-breakers always run in the same direction.
fn compare_rows(a: &DiscoveredServer, b: &DiscoveredServer, query: &BrowseQuery) -> Ordering {
    let primary = match query.sort {
        SortBy::Hops => a.hops.cmp(&b.hops),
        SortBy::Players => {
            let pa = player_count(a);
            let pb = player_count(b);
            match (pa, pb) {
                (Some(x), Some(y)) => y.cmp(&x), // descending by default
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        }
        SortBy::Name => {
            let na = name_lower(a);
            let nb = name_lower(b);
            match (na, nb) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        }
        SortBy::LastSeen => b.last_seen.cmp(&a.last_seen), // most recent first
    };

    let primary = if query.descending {
        primary.reverse()
    } else {
        primary
    };

    if primary != Ordering::Equal {
        return primary;
    }

    // --- tie-breakers (never reversed) ---

    // 1. hops ascending
    let tb = a.hops.cmp(&b.hops);
    if tb != Ordering::Equal {
        return tb;
    }

    // 2. players descending (None = legacy sorts last)
    let tb = {
        let pa = player_count(a);
        let pb = player_count(b);
        match (pa, pb) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    };
    if tb != Ordering::Equal {
        return tb;
    }

    // 3. name ascending, case-insensitive (None sorts last)
    let na = name_lower(a);
    let nb = name_lower(b);
    match (na, nb) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

pub fn browse<'a>(
    rows: &'a [DiscoveredServer],
    query: &BrowseQuery,
    now: Instant,
) -> Vec<&'a DiscoveredServer> {
    let mut result: Vec<&DiscoveredServer> = rows
        .iter()
        .filter(|row| passes_filter(row, &query.filter, query.max_age, now))
        .collect();
    // `slice::sort_by` is stable.
    result.sort_by(|a, b| compare_rows(a, b, query));
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use crate::announce::{AnnounceFlags, AnnounceInfo, AnnounceRecord};
    use personal_rns::prelude::DestinationHash;
    use prns_core::interfaces::InterfaceId;

    fn dest(n: u8) -> DestinationHash {
        DestinationHash::from_slice(&[n; 16]).expect("16 bytes is a destination hash")
    }

    fn iface(n: u8) -> InterfaceId {
        InterfaceId::new([n; 8])
    }

    fn fl(
        passworded: bool,
        allowlisted: bool,
        dedicated: bool,
        transport_mode: u8,
    ) -> AnnounceFlags {
        AnnounceFlags {
            passworded,
            allowlisted,
            dedicated,
            transport_mode,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rec_row(
        n: u8,
        hops: u8,
        name: &str,
        game_id: &str,
        map: &str,
        players: u8,
        max_players: u8,
        flags: AnnounceFlags,
        min_link_class: u8,
        last_seen: Instant,
    ) -> DiscoveredServer {
        DiscoveredServer {
            destination_hash: dest(n),
            last_seen,
            hops,
            source_interface: iface(n),
            info: AnnounceInfo::Record(AnnounceRecord {
                protocol_version: 1,
                flags,
                min_link_class,
                players,
                max_players,
                game_id: game_id.to_string(),
                name: name.to_string(),
                map: map.to_string(),
                tlvs: vec![],
            }),
        }
    }

    fn legacy_row(n: u8, hops: u8, name: Option<&str>, last_seen: Instant) -> DiscoveredServer {
        DiscoveredServer {
            destination_hash: dest(n),
            last_seen,
            hops,
            source_interface: iface(n),
            info: AnnounceInfo::Legacy {
                name: name.map(|s| s.to_string()),
            },
        }
    }

    fn now() -> Instant {
        Instant::now()
    }

    // -- tests --

    #[test]
    fn empty_filter_returns_all_including_legacy_sorted_by_hops() {
        let t = now();
        let rows = vec![
            rec_row(1, 5, "Far", "g", "m", 2, 8, fl(false, false, false, 0), 0, t),
            rec_row(2, 1, "Near", "g", "m", 4, 8, fl(false, false, false, 0), 0, t),
            legacy_row(3, 3, Some("Legacy"), t),
        ];
        let result = browse(&rows, &BrowseQuery::default(), t);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].hops, 1);
        assert_eq!(result[1].hops, 3);
        assert_eq!(result[2].hops, 5);
    }

    #[test]
    fn game_id_filter_excludes_legacy_and_mismatched() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "A", "game_a", "m", 1, 8, fl(false, false, false, 0), 0, t),
            rec_row(2, 1, "B", "game_b", "m", 1, 8, fl(false, false, false, 0), 0, t),
            legacy_row(3, 1, Some("Legacy"), t),
        ];
        let q = BrowseQuery {
            filter: BrowseFilter {
                game_id: Some("game_a".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name(), Some("A"));
    }

    #[test]
    fn include_legacy_false_removes_legacy() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "Record", "g", "m", 1, 8, fl(false, false, false, 0), 0, t),
            legacy_row(2, 1, Some("Legacy"), t),
        ];
        let q = BrowseQuery {
            filter: BrowseFilter {
                include_legacy: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name(), Some("Record"));
    }

    #[test]
    fn text_matches_name_map_and_legacy() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "My Test Server", "g", "other", 1, 8, fl(false, false, false, 0), 0, t),
            rec_row(2, 1, "NoMatch", "g", "test map", 1, 8, fl(false, false, false, 0), 0, t),
            rec_row(3, 1, "NoMatch", "g", "nomap", 1, 8, fl(false, false, false, 0), 0, t),
            legacy_row(4, 1, Some("Test Legacy"), t),
        ];
        let q = BrowseQuery {
            filter: BrowseFilter {
                text: Some("test".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 3);
        let names: Vec<_> = result.iter().map(|r| r.name()).collect();
        assert!(names.contains(&Some("My Test Server")));
        assert!(names.contains(&Some("NoMatch"))); // row 2 matched on map
        assert!(names.contains(&Some("Test Legacy")));
    }

    #[test]
    fn max_hops_applies_to_legacy() {
        let t = now();
        let rows = vec![
            rec_row(1, 3, "Near", "g", "m", 1, 8, fl(false, false, false, 0), 0, t),
            legacy_row(2, 5, Some("Far Legacy"), t),
        ];
        let q = BrowseQuery {
            filter: BrowseFilter {
                max_hops: Some(4),
                ..Default::default()
            },
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name(), Some("Near"));
    }

    #[test]
    fn not_full_keeps_partial_drops_full_and_legacy() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "Partial", "g", "m", 3, 8, fl(false, false, false, 0), 0, t),
            rec_row(2, 1, "Full", "g", "m", 8, 8, fl(false, false, false, 0), 0, t),
            legacy_row(3, 1, Some("Legacy"), t),
        ];
        let q = BrowseQuery {
            filter: BrowseFilter {
                not_full: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name(), Some("Partial"));
    }

    #[test]
    fn has_players_drops_zero() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "Empty", "g", "m", 0, 8, fl(false, false, false, 0), 0, t),
            rec_row(2, 1, "Has", "g", "m", 3, 8, fl(false, false, false, 0), 0, t),
        ];
        let q = BrowseQuery {
            filter: BrowseFilter {
                has_players: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name(), Some("Has"));
    }

    #[test]
    fn exclude_passworded_and_allowlisted() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "Normal", "g", "m", 1, 8, fl(false, false, false, 0), 0, t),
            rec_row(2, 1, "Passworded", "g", "m", 1, 8, fl(true, false, false, 0), 0, t),
            rec_row(3, 1, "Allowlisted", "g", "m", 1, 8, fl(false, true, false, 0), 0, t),
        ];

        let q = BrowseQuery {
            filter: BrowseFilter {
                exclude_passworded: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|r| r.name() != Some("Passworded")));

        let q = BrowseQuery {
            filter: BrowseFilter {
                exclude_allowlisted: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|r| r.name() != Some("Allowlisted")));
    }

    #[test]
    fn transport_modes_filter() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "Mode1", "g", "m", 1, 8, fl(false, false, false, 1), 0, t),
            rec_row(2, 1, "Mode2", "g", "m", 1, 8, fl(false, false, false, 2), 0, t),
            rec_row(3, 1, "Mode3", "g", "m", 1, 8, fl(false, false, false, 3), 0, t),
        ];
        let q = BrowseQuery {
            filter: BrowseFilter {
                transport_modes: Some(vec![1, 3]),
                ..Default::default()
            },
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 2);
        let names: Vec<_> = result.iter().map(|r| r.name()).collect();
        assert!(names.contains(&Some("Mode1")));
        assert!(names.contains(&Some("Mode3")));
        assert!(!names.contains(&Some("Mode2")));
    }

    #[test]
    fn max_link_class_filter() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "Tier1", "g", "m", 1, 8, fl(false, false, false, 0), 1, t),
            rec_row(2, 1, "Tier2", "g", "m", 1, 8, fl(false, false, false, 0), 2, t),
        ];
        let q = BrowseQuery {
            filter: BrowseFilter {
                max_link_class: Some(1),
                ..Default::default()
            },
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name(), Some("Tier1"));
    }

    #[test]
    fn sort_by_hops_tiebreak_players() {
        let t = now();
        let rows = vec![
            rec_row(1, 2, "A", "g", "m", 3, 8, fl(false, false, false, 0), 0, t),
            rec_row(2, 1, "B", "g", "m", 1, 8, fl(false, false, false, 0), 0, t),
            rec_row(3, 2, "C", "g", "m", 5, 8, fl(false, false, false, 0), 0, t),
        ];
        let result = browse(&rows, &BrowseQuery::default(), t);
        // hops 1 first, then hops 2 tie broken by players desc (5 before 3)
        assert_eq!(result[0].name(), Some("B"));
        assert_eq!(result[1].name(), Some("C"));
        assert_eq!(result[2].name(), Some("A"));
    }

    #[test]
    fn sort_by_players_legacy_last() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "Zero", "g", "m", 0, 8, fl(false, false, false, 0), 0, t),
            rec_row(2, 1, "Five", "g", "m", 5, 8, fl(false, false, false, 0), 0, t),
            legacy_row(3, 1, Some("Legacy"), t),
        ];
        let q = BrowseQuery {
            sort: SortBy::Players,
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result[0].name(), Some("Five"));
        assert_eq!(result[1].name(), Some("Zero"));
        assert_eq!(result[2].name(), Some("Legacy"));
    }

    #[test]
    fn sort_by_name_case_insensitive_unnamed_last() {
        let t = now();
        let rows = vec![
            legacy_row(1, 1, None, t),
            rec_row(2, 1, "Bravo", "g", "m", 1, 8, fl(false, false, false, 0), 0, t),
            rec_row(3, 1, "alpha", "g", "m", 1, 8, fl(false, false, false, 0), 0, t),
        ];
        let q = BrowseQuery {
            sort: SortBy::Name,
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result[0].name(), Some("alpha"));
        assert_eq!(result[1].name(), Some("Bravo"));
        assert_eq!(result[2].name(), None);
    }

    #[test]
    fn descending_reverses_primary() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "A", "g", "m", 1, 8, fl(false, false, false, 0), 0, t),
            rec_row(2, 3, "B", "g", "m", 1, 8, fl(false, false, false, 0), 0, t),
            rec_row(3, 2, "C", "g", "m", 1, 8, fl(false, false, false, 0), 0, t),
        ];
        let q = BrowseQuery {
            sort: SortBy::Hops,
            descending: true,
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result[0].hops, 3);
        assert_eq!(result[1].hops, 2);
        assert_eq!(result[2].hops, 1);
    }

    #[test]
    fn max_age_drops_old_keeps_fresh() {
        let t = now();
        let old_seen = t - Duration::from_secs(120);
        let fresh_seen = t - Duration::from_secs(10);
        let rows = vec![
            rec_row(1, 1, "Old", "g", "m", 1, 8, fl(false, false, false, 0), 0, old_seen),
            rec_row(2, 1, "Fresh", "g", "m", 1, 8, fl(false, false, false, 0), 0, fresh_seen),
        ];
        let q = BrowseQuery {
            max_age: Some(Duration::from_secs(60)),
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name(), Some("Fresh"));
    }

    #[test]
    fn now_earlier_than_last_seen_does_not_panic() {
        let t = now();
        let future = t + Duration::from_secs(30);
        let rows = vec![
            rec_row(1, 1, "Future", "g", "m", 1, 8, fl(false, false, false, 0), 0, future),
        ];
        let q = BrowseQuery {
            max_age: Some(Duration::from_secs(60)),
            ..Default::default()
        };
        let result = browse(&rows, &q, t);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn stability_identical_rows_keep_input_order() {
        let t = now();
        let rows = vec![
            rec_row(1, 2, "Same", "g", "m", 3, 8, fl(false, false, false, 0), 0, t),
            rec_row(2, 2, "Same", "g", "m", 3, 8, fl(false, false, false, 0), 0, t),
        ];
        let result = browse(&rows, &BrowseQuery::default(), t);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].destination_hash.as_bytes()[0], 1);
        assert_eq!(result[1].destination_hash.as_bytes()[0], 2);
    }

    // ---- added in review; gaps the draft's suite left open ----

    /// `dedicated_only` had no test, and it is the one flag whose absence a
    /// listen-server host would notice.
    #[test]
    fn dedicated_only_drops_listen_servers_and_legacy_rows() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "dedicated", "sven-coop", "m", 1, 8, fl(false, false, true, 1), 1, t),
            rec_row(2, 1, "listen", "sven-coop", "m", 1, 8, fl(false, false, false, 1), 1, t),
            legacy_row(3, 1, Some("old peer"), t),
        ];
        let query = BrowseQuery {
            filter: BrowseFilter { dedicated_only: true, ..Default::default() },
            ..Default::default()
        };
        let out = browse(&rows, &query, t);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name(), Some("dedicated"));
    }

    /// Staleness is not a record field, so it must age out legacy rows too —
    /// otherwise a departed v0.1.10 server haunts the list forever.
    #[test]
    fn max_age_ages_out_legacy_rows_too() {
        let t = now();
        let rows = vec![
            legacy_row(1, 1, Some("gone"), t - Duration::from_secs(600)),
            legacy_row(2, 1, Some("here"), t),
        ];
        let query = BrowseQuery {
            max_age: Some(Duration::from_secs(60)),
            ..Default::default()
        };
        let out = browse(&rows, &query, t);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name(), Some("here"));
    }

    /// The documented oddity, pinned: `SortBy::Players` is already descending,
    /// so `descending: true` makes it ascending. If that ever reads wrong to
    /// someone, the fix is a differently named field, not a silent flip.
    #[test]
    fn descending_inverts_the_already_descending_player_sort() {
        let t = now();
        let rows = vec![
            rec_row(1, 1, "quiet", "sven-coop", "m", 1, 8, fl(false, false, true, 1), 1, t),
            rec_row(2, 1, "busy", "sven-coop", "m", 7, 8, fl(false, false, true, 1), 1, t),
        ];
        let busy_first = BrowseQuery { sort: SortBy::Players, ..Default::default() };
        assert_eq!(browse(&rows, &busy_first, t)[0].name(), Some("busy"));

        let quiet_first = BrowseQuery {
            sort: SortBy::Players,
            descending: true,
            ..Default::default()
        };
        assert_eq!(browse(&rows, &quiet_first, t)[0].name(), Some("quiet"));
    }

    /// A default query is what a launcher shows on first paint, so its shape
    /// is a product decision, not an implementation detail: everything, legacy
    /// included, nearest first.
    #[test]
    fn the_default_query_is_nearest_first_and_hides_nothing() {
        let q = BrowseQuery::default();
        assert_eq!(q.sort, SortBy::Hops);
        assert!(!q.descending);
        assert!(q.max_age.is_none());
        assert!(q.filter.include_legacy);
        assert_eq!(q.filter, BrowseFilter::default());
    }
}
