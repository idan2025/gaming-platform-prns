//! Launcher logic, with no UI framework in it.
//!
//! The Tauri shell in `launcher/src-tauri` is a thin wrapper: every
//! `#[tauri::command]` there forwards to a method here. That split is
//! deliberate — it keeps the part worth testing testable without a webview, a
//! display server, or a platform toolchain, and it means the browser could get
//! a CLI or a different shell without rewriting anything that matters.
//!
//! Everything here is phase 2 of `PLAN.md` §8: **browse and join, from
//! announces alone.** No index, no account, no internet. Hosting, orchestration
//! and any central service come later and must never become a prerequisite for
//! what is here.
//!
//! # The shapes in this module are a contract with the frontend
//!
//! `ServerRow`, `BrowseQuery`, `ServerDetailsView` and `BrowseStatus` serialize
//! straight into the web view. Renaming a field breaks the UI silently, because
//! JavaScript reads a missing property as `undefined` rather than failing. The
//! tests at the bottom pin the JSON key names for exactly that reason.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use game_bridge::announce::AnnounceInfo;
use game_bridge::browse::{BrowseFilter, SortBy};
use game_bridge::config::{BrowserArgs, ClientArgs};
use game_bridge::details::StatsSource;
use game_bridge::launch::{LaunchProfile, LaunchValues};
use game_bridge::pack::TrustedPack;
use game_bridge::profile::GameProfile;
use game_bridge::signing::{PackTrust, TrustPolicy};
use game_bridge::{BridgeSession, GamePack};
use personal_rns::prelude::DestinationHash;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// One row of the server list, as the UI sees it.
///
/// Every field that a legacy announce cannot supply is `Option`, and the UI is
/// required to render `None` as unknown rather than as zero — a "0/0 players"
/// for a server whose count nobody knows is a lie the list would be telling on
/// the server's behalf. `legacy` is the flag that says to expect those `None`s.
#[derive(Debug, Clone, Serialize)]
pub struct ServerRow {
    pub destination_hash: String,
    pub name: Option<String>,
    pub game_id: Option<String>,
    pub map: Option<String>,
    pub players: Option<u8>,
    pub max_players: Option<u8>,
    pub hops: u8,
    pub interface_label: String,
    pub min_link_class: Option<u8>,
    pub passworded: Option<bool>,
    pub allowlisted: Option<bool>,
    pub dedicated: Option<bool>,
    pub transport_mode: Option<u8>,
    pub last_seen_secs: u64,
    pub legacy: bool,
}

/// The query the UI builds from its filter bar.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BrowseQueryInput {
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
    pub sort: Option<String>,
    pub descending: bool,
    pub max_age_secs: Option<u64>,
}

impl BrowseQueryInput {
    fn to_query(&self) -> game_bridge::browse::BrowseQuery {
        game_bridge::browse::BrowseQuery {
            filter: BrowseFilter {
                game_id: self.game_id.clone().filter(|s| !s.is_empty()),
                text: self.text.clone().filter(|s| !s.trim().is_empty()),
                max_hops: self.max_hops,
                max_link_class: self.max_link_class,
                has_players: self.has_players,
                not_full: self.not_full,
                exclude_passworded: self.exclude_passworded,
                exclude_allowlisted: self.exclude_allowlisted,
                transport_modes: self.transport_modes.clone(),
                dedicated_only: self.dedicated_only,
                include_legacy: self.include_legacy,
            },
            // An unknown sort name falls back to the default rather than
            // erroring: a typo in the UI should not empty the list.
            sort: match self.sort.as_deref() {
                Some("players") => SortBy::Players,
                Some("name") => SortBy::Name,
                Some("last_seen") => SortBy::LastSeen,
                _ => SortBy::Hops,
            },
            descending: self.descending,
            max_age: self.max_age_secs.map(Duration::from_secs),
        }
    }
}

/// A game the launcher knows how to name, from the loaded packs.
///
/// The trust fields are `PLAN.md` §11.4's "shown rather than buried" half. The
/// launcher **shows and never gates**: it is a client, not a node, so a pack
/// here decides how this machine talks to a server, not what code some host
/// runs. The refusing is the agent's job (`platform-agent/src/packs.rs`). What
/// the launcher owes a user is the tier at the moment they act on it.
#[derive(Debug, Clone, Serialize)]
pub struct GameSummary {
    pub id: String,
    pub display_name: String,
    /// §11.4's words, verbatim — "first-party", "signed community", "signed by
    /// an unknown key", "unsigned local", "built in".
    pub trust: String,
    /// The one-line explanation that goes with the label.
    pub trust_detail: String,
    /// The signer's identity hash, hex, when there is one.
    pub signer: Option<String>,
    /// Unix seconds this pack's signature goes stale, when it has one. Shown
    /// so a signature can be refreshed before it lapses rather than at the
    /// moment something stops working.
    pub signature_expires_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InterfaceSummary {
    pub id: String,
    pub label: String,
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BrowseStatus {
    pub running: bool,
    pub interfaces: Vec<InterfaceSummary>,
    /// Every server heard, before any filter. The UI subtracts to say how many
    /// a filter is hiding.
    pub heard_total: usize,
}

/// The detail-pane payload.
///
/// `stats_source` and `stats_age_secs` are the fields that keep this pane more
/// trustworthy than the row it opened from, and the UI must show them: a "5
/// players" that is really the server's config file is worse than no number,
/// because it looks live.
#[derive(Debug, Clone, Serialize)]
pub struct ServerDetailsView {
    pub destination_hash: String,
    pub reachable: bool,
    pub rtt_ms: Option<u32>,
    pub players_online: Option<u8>,
    pub max_players: Option<u8>,
    pub player_names: Option<Vec<String>>,
    pub roster_truncated: bool,
    pub map: Option<String>,
    pub uptime_secs: Option<u32>,
    pub bridge_clients: Option<u16>,
    /// `"live"` or `"announced"`.
    pub stats_source: Option<String>,
    /// Seconds since the live read. Meaningless unless `stats_source` is live.
    pub stats_age_secs: Option<u16>,
    pub error: Option<String>,
}

/// How the browse node attaches to the mesh.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BrowseOpts {
    /// `host:port` to dial, or `0.0.0.0:port` to bind. `None` with `auto` off
    /// means the node has no interfaces and will hear nothing.
    pub tcp: Option<String>,
    pub auto: bool,
}

/// What a join produced.
///
/// Joining starts a client bridge on a local port and stops there. It does
/// **not** launch the game: a pack cannot name a command
/// (`crates/game-bridge/src/pack.rs`), and `PLAN.md` §10 still holds open
/// whether community packs may ever be trusted with argv. So the launcher tells
/// the player where to point their game, and the player points it.
#[derive(Debug, Clone, Serialize)]
pub struct JoinResult {
    pub listen_addr: String,
    pub game_id: Option<String>,
    /// Whether this game's pack knows how to start the player's own copy
    /// (`PLAN.md` §13.1). `false` means the old behaviour: the launcher shows
    /// the address and the player points their game at it.
    pub can_launch: bool,
}

/// The launcher's whole state.
pub struct Launcher {
    inner: Arc<Mutex<Inner>>,
    packs: Vec<TrustedPack>,
}

struct Inner {
    browse: Option<BridgeSession>,
    client: Option<BridgeSession>,
}

impl Launcher {
    /// Build a launcher over packs of unestablished provenance, falling back to
    /// the built-in Sven Co-op pack so a fresh install with no pack directory
    /// still works.
    ///
    /// Every pack passed here reads as `UnsignedLocal`, because that is what a
    /// `GamePack` with no signature beside it is. The fallback is `BuiltIn`
    /// instead: it came out of this binary, not off a disk.
    pub fn new(packs: Vec<GamePack>) -> Self {
        let mut packs: Vec<TrustedPack> = packs
            .into_iter()
            .map(|pack| TrustedPack {
                pack,
                trust: PackTrust::UnsignedLocal,
                file: String::new(),
                expires_at: None,
            })
            .collect();
        if packs.is_empty() {
            packs.push(TrustedPack {
                pack: GamePack::sven_coop(),
                trust: PackTrust::BuiltIn,
                file: String::new(),
                expires_at: None,
            });
        }
        Self { inner: Arc::new(Mutex::new(Inner { browse: None, client: None })), packs }
    }

    /// Build a launcher over packs whose provenance is already established.
    pub fn from_verified(packs: Vec<TrustedPack>) -> Self {
        if packs.is_empty() {
            return Self::new(Vec::new());
        }
        Self { inner: Arc::new(Mutex::new(Inner { browse: None, client: None })), packs }
    }

    /// Load packs from a directory, ignoring individual broken ones, and
    /// establish each one's trust tier (`PLAN.md` §11.4).
    ///
    /// The policy is `allowing_unsigned`, and deliberately so: **the launcher
    /// shows a tier, it does not enforce one.** Refusing to load an unsigned
    /// pack here would stop a user browsing with the file they wrote, while
    /// protecting nothing — no code runs on this machine because of a pack. A
    /// pack whose signature *failed* is still skipped, because a failed
    /// signature is an error and never a demotion to unsigned; that rule holds
    /// wherever packs are read.
    pub fn from_pack_dir(dir: &std::path::Path) -> Self {
        let policy = TrustPolicy::allowing_unsigned();
        match GamePack::load_dir_verified(dir, &policy, std::time::SystemTime::now()) {
            Ok(loaded) => {
                for (path, e) in &loaded.errors {
                    tracing::warn!(path = %path, error = %e, "skipping an unreadable game pack");
                }
                Self::from_verified(loaded.packs)
            }
            Err(e) => {
                tracing::warn!(error = %e, "no game pack directory; using the built-in pack");
                Self::new(Vec::new())
            }
        }
    }

    pub fn list_games(&self) -> Vec<GameSummary> {
        self.packs
            .iter()
            .map(|p| GameSummary {
                id: p.pack.id.clone(),
                display_name: p.pack.display_name.clone(),
                trust: p.trust.label().to_string(),
                trust_detail: p.trust.explanation().to_string(),
                signer: p.trust.signer().map(|s| hex::encode(s.as_bytes())),
                signature_expires_at: p.expires_at,
            })
            .collect()
    }

    fn profile_for(&self, game_id: &str) -> Option<GameProfile> {
        self.packs
            .iter()
            .find(|p| p.pack.id == game_id)
            .and_then(|p| p.pack.to_profile().ok())
    }

    pub async fn start_browse(&self, opts: BrowseOpts) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if inner.browse.is_some() {
            return Ok(());
        }
        let args = BrowserArgs { tcp: opts.tcp, auto: opts.auto };
        inner.browse = Some(BridgeSession::start_browser(args).await?);
        Ok(())
    }

    pub async fn stop_browse(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if let Some(mut session) = inner.browse.take() {
            session.stop().await;
        }
        Ok(())
    }

    pub async fn browse_status(&self) -> BrowseStatus {
        let inner = self.inner.lock().await;
        match &inner.browse {
            None => BrowseStatus { running: false, interfaces: Vec::new(), heard_total: 0 },
            Some(session) => {
                let interfaces = session
                    .handle()
                    .interfaces()
                    .into_iter()
                    .map(|s| InterfaceSummary {
                        id: hex::encode(s.id.as_bytes()),
                        label: interface_label(&s),
                        connected: matches!(
                            s.connection,
                            prns_core::interfaces::ConnectionState::Connected
                        ),
                    })
                    .collect();
                BrowseStatus {
                    running: true,
                    interfaces,
                    heard_total: session.discovered().await.len(),
                }
            }
        }
    }

    pub async fn list_servers(&self, input: BrowseQueryInput) -> Result<Vec<ServerRow>> {
        let inner = self.inner.lock().await;
        let Some(session) = inner.browse.as_ref() else {
            // Not an error: "not started" is a state the UI renders, not a
            // failure it should show in red.
            return Ok(Vec::new());
        };
        let rows = session.browse(&input.to_query()).await;
        Ok(rows.iter().map(row_view).collect())
    }

    pub async fn server_details(&self, destination_hash: &str) -> ServerDetailsView {
        let hash = match parse_hash(destination_hash) {
            Ok(h) => h,
            Err(e) => return unreachable_view(destination_hash, e.to_string()),
        };
        let inner = self.inner.lock().await;
        let Some(session) = inner.browse.as_ref() else {
            return unreachable_view(destination_hash, "the browse node is not running".into());
        };
        match session.probe_details(hash).await {
            Ok((d, rtt_ms)) => ServerDetailsView {
                destination_hash: destination_hash.to_string(),
                reachable: true,
                rtt_ms: Some(rtt_ms),
                players_online: Some(d.players),
                max_players: Some(d.max_players),
                player_names: Some(d.player_names),
                roster_truncated: d.roster_truncated,
                map: Some(d.map).filter(|m| !m.is_empty()),
                uptime_secs: Some(d.uptime_secs),
                bridge_clients: Some(d.bridge_clients),
                stats_source: Some(
                    match d.stats_source {
                        StatsSource::Live => "live",
                        StatsSource::Announced => "announced",
                    }
                    .to_string(),
                ),
                stats_age_secs: match d.stats_source {
                    StatsSource::Live => Some(d.stats_age_secs),
                    StatsSource::Announced => None,
                },
                error: None,
            },
            // A failed probe is not "offline". Mesh routing is asymmetric: an
            // announce can reach us over a path that does not exist in reverse,
            // and an allowlisted server refuses probes on purpose. The UI must
            // say "did not answer", never "down".
            Err(e) => unreachable_view(destination_hash, e.to_string()),
        }
    }

    /// Start a client bridge pointed at one server. Does not launch a game.
    pub async fn join_server(&self, destination_hash: &str, game_id: Option<&str>) -> Result<JoinResult> {
        let hash = parse_hash(destination_hash)?;
        let game_id = match game_id {
            Some(id) => id.to_string(),
            // With no game named, fall back to the only pack, if there is only
            // one. Guessing among several would be picking a wire protocol for
            // the user.
            None if self.packs.len() == 1 => self.packs[0].pack.id.clone(),
            None => {
                return Err(anyhow!(
                    "this server did not say which game it runs, so a game must be chosen"
                ))
            }
        };
        let profile = self
            .profile_for(&game_id)
            .ok_or_else(|| anyhow!("no game pack installed for {game_id:?}"))?;

        let listen_port = profile.default_port;
        let mut args = ClientArgs::new(profile);
        args.server_hash = Some(hex::encode(hash.as_bytes()));

        let mut inner = self.inner.lock().await;
        // Stop any previous join first: two clients cannot share a listen port,
        // and `stop` waits for the socket to actually be released.
        if let Some(mut old) = inner.client.take() {
            old.stop().await;
        }
        inner.client = Some(BridgeSession::start_client(args).await?);
        Ok(JoinResult {
            listen_addr: format!("127.0.0.1:{listen_port}"),
            can_launch: self.launch_profile(&game_id).is_some(),
            game_id: Some(game_id),
        })
    }

    fn launch_profile(&self, game_id: &str) -> Option<&LaunchProfile> {
        self.packs.iter().find(|p| p.pack.id == game_id)?.pack.launch.as_ref()
    }

    /// Start the player's own copy of the game, pointed at the port `join_server`
    /// bound (`PLAN.md` §13.1).
    ///
    /// **The program is the player's, not the pack's.** `executable` is a path
    /// this machine's owner chose — detected in a Steam library or picked once
    /// in a file dialog — and the pack contributes only the arguments after it.
    ///
    /// The arguments are spawned as a **vector, never through a shell**. That
    /// is the whole safety property of §13.1: a `;` or a `$(...)` in a
    /// stranger's pack is one byte of one argument. Never route this through
    /// `sh -c`, and never join these into a string.
    pub fn launch_game(
        &self,
        game_id: &str,
        executable: &std::path::Path,
        values: &LaunchValues,
    ) -> Result<()> {
        let profile = self
            .launch_profile(game_id)
            .ok_or_else(|| anyhow!("no launch profile for {game_id:?}"))?;
        let args = profile.build_args(values).map_err(|e| anyhow!("{e}"))?;

        // The player's own binary must exist and be a file. A pack cannot reach
        // this argument, but a stale saved path can, and "failed to launch" is
        // a better answer than a confusing spawn error.
        if !executable.is_file() {
            return Err(anyhow!(
                "{} is not a file; point the launcher at your game again",
                executable.display()
            ));
        }

        std::process::Command::new(executable)
            .args(&args)
            .spawn()
            .map_err(|e| anyhow!("could not start {}: {e}", executable.display()))?;
        Ok(())
    }

    pub async fn leave(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if let Some(mut session) = inner.client.take() {
            session.stop().await;
        }
        Ok(())
    }
}

fn unreachable_view(hash: &str, error: String) -> ServerDetailsView {
    ServerDetailsView {
        destination_hash: hash.to_string(),
        reachable: false,
        rtt_ms: None,
        players_online: None,
        max_players: None,
        player_names: None,
        roster_truncated: false,
        map: None,
        uptime_secs: None,
        bridge_clients: None,
        stats_source: None,
        stats_age_secs: None,
        error: Some(error),
    }
}

fn parse_hash(hex_str: &str) -> Result<DestinationHash> {
    let bytes = hex::decode(hex_str.trim()).map_err(|e| anyhow!("not hex: {e}"))?;
    DestinationHash::from_slice(&bytes)
        .ok_or_else(|| anyhow!("a destination hash is 16 bytes (32 hex chars)"))
}

fn interface_label(s: &prns_core::interfaces::InterfaceSnapshot) -> String {
    match s.id.kind() {
        Some(kind) => format!("{kind:?}"),
        None => hex::encode(&s.id.as_bytes()[..2]),
    }
}

fn row_view(row: &game_bridge::DiscoveredServer) -> ServerRow {
    let record = row.record();
    ServerRow {
        destination_hash: hex::encode(row.destination_hash.as_bytes()),
        name: row.name().map(str::to_string),
        game_id: row.game_id().map(str::to_string),
        map: record.map(|r| r.map.clone()).filter(|m| !m.is_empty()),
        players: record.map(|r| r.players),
        max_players: record.map(|r| r.max_players),
        hops: row.hops,
        interface_label: match row.source_interface.kind() {
            Some(kind) => format!("{kind:?}"),
            None => hex::encode(&row.source_interface.as_bytes()[..2]),
        },
        min_link_class: record.map(|r| r.min_link_class),
        passworded: record.map(|r| r.flags.passworded),
        allowlisted: record.map(|r| r.flags.allowlisted),
        dedicated: record.map(|r| r.flags.dedicated),
        transport_mode: record.map(|r| r.flags.transport_mode),
        last_seen_secs: row.last_seen.elapsed().as_secs(),
        legacy: matches!(row.info, AnnounceInfo::Legacy { .. }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frontend reads these keys by name. A rename here is a silent break
    /// there, because JavaScript reads a missing property as `undefined` rather
    /// than throwing — so the contract is pinned as a test.
    #[test]
    fn server_row_json_keys_are_the_frontend_contract() {
        let row = ServerRow {
            destination_hash: "aa".repeat(16),
            name: Some("n".into()),
            game_id: None,
            map: None,
            players: None,
            max_players: None,
            hops: 2,
            interface_label: "Tcp".into(),
            min_link_class: None,
            passworded: None,
            allowlisted: None,
            dedicated: None,
            transport_mode: None,
            last_seen_secs: 3,
            legacy: true,
        };
        let v: serde_json::Value = serde_json::to_value(&row).unwrap();
        for key in [
            "destination_hash", "name", "game_id", "map", "players", "max_players",
            "hops", "interface_label", "min_link_class", "passworded", "allowlisted",
            "dedicated", "transport_mode", "last_seen_secs", "legacy",
        ] {
            assert!(v.get(key).is_some(), "the UI reads `{key}` and it is missing");
        }
        // Unknown must serialize as null, never as 0 — the UI branches on it.
        assert!(v["players"].is_null());
        assert!(v["game_id"].is_null());
    }

    #[test]
    fn details_view_json_keys_are_the_frontend_contract() {
        let v = serde_json::to_value(unreachable_view("aa", "no".into())).unwrap();
        for key in [
            "destination_hash", "reachable", "rtt_ms", "players_online", "max_players",
            "player_names", "roster_truncated", "map", "uptime_secs", "bridge_clients",
            "stats_source", "stats_age_secs", "error",
        ] {
            assert!(v.get(key).is_some(), "the UI reads `{key}` and it is missing");
        }
        assert_eq!(v["reachable"], serde_json::json!(false));
    }

    /// An announced figure must not carry an age, or the UI will render
    /// "announced, 0s ago" and imply freshness the number does not have.
    #[test]
    fn an_announced_stat_carries_no_age() {
        let v = serde_json::to_value(unreachable_view("aa", "x".into())).unwrap();
        assert!(v["stats_age_secs"].is_null());
    }

    #[test]
    fn an_unknown_sort_name_falls_back_rather_than_failing() {
        let input = BrowseQueryInput { sort: Some("nonsense".into()), ..Default::default() };
        assert_eq!(input.to_query().sort, SortBy::Hops);
        let input = BrowseQueryInput { sort: Some("players".into()), ..Default::default() };
        assert_eq!(input.to_query().sort, SortBy::Players);
    }

    /// Blank strings from an empty text box are not filters.
    #[test]
    fn blank_filter_strings_are_treated_as_absent() {
        let input = BrowseQueryInput {
            text: Some("   ".into()),
            game_id: Some(String::new()),
            ..Default::default()
        };
        let q = input.to_query();
        assert!(q.filter.text.is_none());
        assert!(q.filter.game_id.is_none(), "an empty game id would hide every legacy row");
    }

    #[test]
    fn a_launcher_with_no_packs_still_knows_sven() {
        let l = Launcher::new(Vec::new());
        let games = l.list_games();
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].id, "sven-coop");
    }

    /// The fallback pack comes out of this binary, so it is `BuiltIn`, not
    /// `UnsignedLocal`. Labelling it "unsigned local" would invent a
    /// provenance question about a file that does not exist.
    #[test]
    fn the_fallback_pack_is_built_in_not_an_unsigned_file() {
        let games = Launcher::new(Vec::new()).list_games();
        assert_eq!(games[0].trust, "built in");
        assert!(games[0].signer.is_none());
        assert!(games[0].signature_expires_at.is_none());
    }

    /// A pack handed in as a bare `GamePack` has no signature beside it and
    /// nothing has verified it, so it reads as unsigned — never as first-party
    /// because it came through an internal constructor.
    #[test]
    fn a_pack_passed_in_without_provenance_reads_as_unsigned() {
        let games = Launcher::new(vec![GamePack::sven_coop()]).list_games();
        assert_eq!(games[0].trust, "unsigned local");
    }

    /// A join tells the frontend whether a Play button is possible. `can_launch`
    /// is a key the UI branches on, so it is part of the contract.
    #[test]
    fn join_result_json_keys_are_the_frontend_contract() {
        let v = serde_json::to_value(JoinResult {
            listen_addr: "127.0.0.1:27015".into(),
            game_id: Some("sven-coop".into()),
            can_launch: true,
        })
        .unwrap();
        for key in ["listen_addr", "game_id", "can_launch"] {
            assert!(v.get(key).is_some(), "the UI reads `{key}` and it is missing");
        }
    }

    /// The built-in pack carries a launch profile, so a fresh install with no
    /// pack directory can still offer Play rather than an address to copy.
    #[test]
    fn the_builtin_pack_can_start_the_players_own_game() {
        let l = Launcher::new(Vec::new());
        assert!(l.launch_profile("sven-coop").is_some());
    }

    /// Refusing early with a clear message beats a spawn error a player cannot
    /// act on. The path is the player's own saved choice, and it goes stale.
    #[test]
    fn launching_with_a_missing_executable_says_so_rather_than_spawning() {
        let l = Launcher::new(Vec::new());
        let err = l
            .launch_game(
                "sven-coop",
                std::path::Path::new("/nonexistent/hl.exe"),
                &LaunchValues::default(),
            )
            .expect_err("a missing binary must not launch");
        assert!(err.to_string().contains("not a file"), "{err}");
    }

    #[test]
    fn launching_a_game_with_no_profile_is_a_clear_error() {
        let l = Launcher::new(Vec::new());
        assert!(l
            .launch_game("quake-3", std::path::Path::new("/bin/sh"), &LaunchValues::default())
            .is_err());
    }

    /// §11.4 wants the tier shown, so it has to reach the UI. These are the
    /// keys the detail pane reads by name.
    #[test]
    fn game_summary_json_keys_are_the_frontend_contract() {
        let games = Launcher::new(Vec::new()).list_games();
        let v = serde_json::to_value(&games[0]).unwrap();
        for key in ["id", "display_name", "trust", "trust_detail", "signer", "signature_expires_at"]
        {
            assert!(v.get(key).is_some(), "the UI reads `{key}` and it is missing");
        }
        assert!(v["signer"].is_null());
        assert!(!v["trust_detail"].as_str().unwrap().is_empty());
    }

    /// The launcher shows a tier; it does not enforce one. A pack nobody
    /// signed must still load, or a user could not browse with a file they
    /// wrote themselves — and nothing on this machine runs because of a pack.
    #[test]
    fn an_unsigned_pack_on_disk_still_loads_in_the_launcher() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("g.toml"), PACK_TOML).unwrap();
        let games = Launcher::from_pack_dir(dir.path()).list_games();
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].id, "test-game");
        assert_eq!(games[0].trust, "unsigned local");
    }

    /// But a signature that is present and does not verify is an error, so the
    /// pack is skipped rather than shown as unsigned. An empty directory then
    /// falls back to the built-in pack, which is why the assertion is on the
    /// id rather than on the count.
    #[test]
    fn a_pack_with_a_broken_signature_is_skipped_not_shown_as_unsigned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("g.toml"), PACK_TOML).unwrap();
        std::fs::write(dir.path().join("g.toml.sig"), "not a signature file").unwrap();
        let games = Launcher::from_pack_dir(dir.path()).list_games();
        assert!(
            games.iter().all(|g| g.id != "test-game"),
            "a pack whose signature failed must not appear as an unsigned one"
        );
    }

    const PACK_TOML: &str = r#"
schema_version = 1
id = "test-game"
display_name = "Test Game"
app_name = "test-game"
default_port = 27015
transport = "udp"
min_link_class = 1
query = "a2s"
"#;

    #[tokio::test]
    async fn listing_before_the_browse_node_starts_is_empty_not_an_error() {
        let l = Launcher::new(Vec::new());
        assert!(l.list_servers(BrowseQueryInput::default()).await.unwrap().is_empty());
        assert!(!l.browse_status().await.running);
    }

    #[tokio::test]
    async fn probing_a_malformed_hash_reports_it_rather_than_panicking() {
        let l = Launcher::new(Vec::new());
        let v = l.server_details("not-a-hash").await;
        assert!(!v.reachable);
        assert!(v.error.is_some());
    }
}
