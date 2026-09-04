//! Game packs: the manifest that turns "a game" into a `GameProfile`.
//!
//! `PLAN.md` §8 step 6. A pack is what makes this a platform rather than a
//! Sven Co-op bridge with the strings changed: adding a game becomes writing a
//! file, not editing the relay.
//!
//! # What a pack deliberately cannot do
//!
//! **It cannot run anything, and it cannot name what runs.** No command, no
//! argv, no path to an executable, no install script — and, since phase 3, no
//! container image either. An image name is argv with extra steps: it selects
//! the code a node executes. So the image a game runs in is **agent
//! configuration**, chosen by whoever owns the node, keyed by game id
//! (`platform-agent`'s `config.rs`). A pack describes a game; an operator
//! decides what runs on their hardware.
//!
//! `writable_paths` is the boundary case that proves the rule: it is a game
//! fact a planner cannot guess, so the pack supplies it — but every entry is
//! validated as a relative path that cannot escape the install directory before
//! it is turned into a mount. Data the node checks, never an instruction the
//! node obeys. `PLAN.md` §10 lists "whether community packs are allowed
//! at all, given a pack is argv on a node" as an *open* question, and the
//! honest way to hold a question open is to not build the thing that settles
//! it. A pack that could name a command would make every shared pack a remote
//! code execution primitive, on a network whose whole premise is that you talk
//! to strangers.
//!
//! So a pack today is descriptive only: what the game is called, what wire name
//! it announces under, which port it listens on, whether it speaks UDP or TCP,
//! and how much bandwidth it needs. Launching a server is the launcher's job,
//! against a game it already knows how to launch.
//!
//! # The field that is a wire contract
//!
//! `app_name` derives the destination hash. Two peers with different
//! `app_name`s cannot see or reach each other, and changing a published pack's
//! `app_name` silently orphans every server already running it. Treat it as
//! frozen once a pack ships — `sven-coop` in particular is frozen by
//! `PLAN.md` §5, because deployed v0.1.10 servers announce under it.

use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::console::ConsoleProtocol;
use crate::content::{ContentError, PackContent};
use crate::launch::{LaunchError, LaunchKind, LaunchProfile};
use crate::profile::{GamePort, GameProfile, GameTransport, ProfileError, QueryProtocol};
use crate::signing::{self, PackTrust, SigFileError, TrustPolicy};

/// Manifest schema version. Bumped when a field changes meaning, not when one
/// is added — unknown fields are rejected (see `deny_unknown_fields`), but a
/// *newer* pack read by an older build should fail loudly rather than load
/// half-understood.
pub const PACK_SCHEMA_VERSION: u32 = 1;

/// A game pack, as it appears on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GamePack {
    /// Must equal `PACK_SCHEMA_VERSION`.
    pub schema_version: u32,
    /// Stable short id, ASCII, at most 24 bytes — it travels in every announce
    /// (`PLAN.md` §3.3).
    pub id: String,
    /// Human-readable name.
    pub display_name: String,
    /// Reticulum app name. **A wire contract.** See the module docs.
    pub app_name: String,
    /// Port the game server listens on by default.
    pub default_port: u16,
    /// `"udp"` or `"tcp"`.
    pub transport: PackTransport,
    /// Viability tier, `GAMES.md` §4: 1 low-rate UDP tick, 2 TCP or bursty,
    /// 3 modern high-bitrate.
    pub min_link_class: u8,
    /// How to ask a running server for live stats: `"a2s"` for GoldSrc and
    /// Source, omitted for everything else. A game with no query answers a
    /// detail probe with its announced numbers, flagged as announced.
    #[serde(default)]
    pub query: Option<PackQuery>,
    /// Paths this game writes to, relative to wherever its install lives.
    ///
    /// A node runs many instances off **one** read-only copy of the content
    /// (`PLAN.md` §8 phase 3 — a 2.74 GB copy per instance does not scale), so
    /// each instance gets writable space only where the game actually needs it.
    /// The planner cannot guess those paths, so the pack declares them.
    ///
    /// This is game *data*, not an instruction: every entry is validated as a
    /// relative path that cannot escape the install directory before it becomes
    /// a mount. Compare the image a container runs, which is deliberately **not**
    /// a pack field — see the module docs.
    #[serde(default)]
    pub writable_paths: Vec<String>,
    /// Where the game's files come from (`PLAN.md` §11.2).
    ///
    /// Absent means `manual`: an operator installs the content by hand, which
    /// is what every pack did before this field existed. A driver names code
    /// this build already carries and hands it typed parameters — it is not a
    /// place to put a command; see `content.rs`.
    #[serde(default)]
    pub content: PackContent,
    /// Extra ports this game wants reachable beyond its own, each on a framing
    /// channel (`GAMES.md` §3). Absent for a single-port game, which is most of
    /// them.
    ///
    /// A port is data — a number, a transport and a label. The label never
    /// selects behaviour; a pack that could say "run the RCON handler" would be
    /// naming what runs.
    #[serde(default)]
    pub extra_ports: Vec<PackPort>,
    /// Where this game's maps live, relative to its install directory —
    /// `"svencoop/maps"`, `"valve/maps"`. Absent means a node cannot offer a
    /// list of maps for this game and a person types the name instead.
    ///
    /// **Data the node checks, never an instruction it obeys**, exactly like
    /// [`writable_paths`](Self::writable_paths): the agent validates it as a
    /// relative path that cannot escape the install before it reads anything,
    /// and only ever *lists* it. A pack still cannot reach outside the content
    /// copy, and listing a directory is not running anything.
    #[serde(default)]
    pub maps_dir: Option<String>,
    /// Which console a node may talk to this game's running server through, so
    /// an operator can change the map without restarting it (`console.rs`).
    /// Absent means the node cannot: it will say so rather than guess a
    /// command.
    ///
    /// **A protocol, never a command.** The variant selects words this build
    /// already carries. A pack that could write the console line could type
    /// anything at a dedicated server's console on somebody else's node, which
    /// is naming what runs by another route — see the module docs.
    #[serde(default)]
    pub console: Option<PackConsole>,
    /// Free-text note for a human reading the pack. Never parsed.
    #[serde(default)]
    pub notes: Option<String>,
    /// How a launcher starts the player's own copy of this game and points it
    /// at a server (`PLAN.md` §13.1).
    ///
    /// **Client-side only, and it names no executable.** The program comes from
    /// the player's own installation; this block only says which engine family
    /// it is and where the launcher's own values go in its arguments. A node
    /// never reads this — a pack still cannot say what a node executes, which
    /// is a different machine belonging to a different person. See
    /// `launch.rs`'s module docs for the four rules that carry it.
    #[serde(default)]
    pub launch: Option<LaunchProfile>,
}

/// One extra port, as it appears in a pack's `[[extra_ports]]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackPort {
    /// Framing channel, 1..=7. Channel 0 is the game port and is implicit.
    pub channel: u8,
    /// What this port is, for humans: `"rcon"`, `"tv"`.
    pub name: String,
    pub port: u16,
    /// This port's own transport — RCON is TCP beside a UDP game port.
    pub transport: PackTransport,
}

impl From<&PackPort> for GamePort {
    fn from(p: &PackPort) -> Self {
        Self {
            channel: p.channel,
            name: p.name.clone(),
            port: p.port,
            transport: p.transport.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackTransport {
    Udp,
    Tcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackQuery {
    A2s,
}

/// The console a pack's game speaks, as it appears in a pack file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackConsole {
    Goldsrc,
    Source,
}

impl From<PackConsole> for ConsoleProtocol {
    fn from(c: PackConsole) -> Self {
        match c {
            PackConsole::Goldsrc => ConsoleProtocol::Goldsrc,
            PackConsole::Source => ConsoleProtocol::Source,
        }
    }
}

impl From<PackQuery> for QueryProtocol {
    fn from(q: PackQuery) -> Self {
        match q {
            PackQuery::A2s => QueryProtocol::A2s,
        }
    }
}

impl From<PackTransport> for GameTransport {
    fn from(t: PackTransport) -> Self {
        match t {
            PackTransport::Udp => GameTransport::Udp,
            PackTransport::Tcp => GameTransport::Tcp,
        }
    }
}

/// Why a pack could not be loaded.
#[derive(Debug)]
pub enum PackError {
    /// The file could not be read.
    Io(std::io::Error),
    /// The TOML did not parse, or carried a field this build does not know.
    Parse(toml::de::Error),
    /// A schema version this build does not implement.
    UnsupportedSchema { found: u32, supported: u32 },
    /// The pack parsed but describes an unusable game.
    Invalid(ProfileError),
    /// The pack's `[content]` block is not usable.
    InvalidContent(ContentError),
    /// The signature beside the pack was unreadable, malformed, forged or
    /// stale. Never a demotion to unsigned — see `signing.rs`.
    Signature(SigFileError),
    /// The pack's `[launch]` block is not usable.
    InvalidLaunch(LaunchError),
}

impl core::fmt::Display for PackError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "reading pack: {e}"),
            Self::Parse(e) => write!(f, "parsing pack: {e}"),
            Self::UnsupportedSchema { found, supported } => write!(
                f,
                "pack schema_version {found} is not supported by this build (which implements {supported})"
            ),
            Self::Invalid(e) => write!(f, "pack describes an unusable game: {e}"),
            Self::InvalidContent(e) => write!(f, "pack describes unusable content: {e}"),
            Self::Signature(e) => write!(f, "pack signature: {e}"),
            Self::InvalidLaunch(e) => write!(f, "pack describes an unusable launch: {e}"),
        }
    }
}

impl std::error::Error for PackError {}

impl GamePack {
    /// Sven Co-op, pack #1.
    ///
    /// Built in rather than read from disk so the reference game works with no
    /// pack directory at all, and so `packs/sven-coop.toml` has something to be
    /// checked against — see `shipped_sven_pack_matches_the_builtin`.
    pub fn sven_coop() -> Self {
        Self {
            schema_version: PACK_SCHEMA_VERSION,
            id: "sven-coop".to_string(),
            display_name: "Sven Co-op".to_string(),
            app_name: "sven-coop".to_string(),
            default_port: 27015,
            transport: PackTransport::Udp,
            min_link_class: 1,
            query: Some(PackQuery::A2s),
            // Only what a *server* writes. A writable path is an empty
            // directory bind-mounted over the content, so listing one that the
            // install ships files at hides them — `svencoop/maps` used to be
            // here and masked all 108 shipped maps. Kept in step with
            // `packs/sven-coop.toml`.
            // `svencoop/maps/soundcache` is writable because the server writes
            // `<map>.txt` there on every map load and cannot under a read-only
            // mount (`Error #30`, EROFS) — which makes it re-precache from
            // scratch at every changelevel. Safe to list only because a
            // steamcmd install ships the directory and ships it *empty*: the
            // mountpoint exists, and nothing is hidden by mounting over it.
            writable_paths: vec![
                "svencoop/logs".to_string(),
                "svencoop/maps/soundcache".to_string(),
            ],
            // App 276060 is the Sven Co-op Dedicated Server and it fetches
            // anonymously, which is what `steamcmd` requires. Kept in step with
            // `packs/sven-coop.toml`, which
            // `shipped_sven_pack_matches_the_builtin` enforces.
            content: PackContent::Steamcmd { app_id: 276060 },
            extra_ports: Vec::new(),
            // Where the 108 shipped maps are, so a node can offer them as a
            // list instead of asking someone to remember a name.
            maps_dir: Some("svencoop/maps".to_string()),
            // GoldSrc, so a node can `changelevel` a live server. Kept in step
            // with `packs/sven-coop.toml`.
            console: Some(PackConsole::Goldsrc),
            // Kept in step with `packs/sven-coop.toml`, which
            // `shipped_sven_pack_matches_the_builtin` enforces: the fallback a
            // fresh install uses must not be a different game from the file.
            launch: Some(LaunchProfile {
                kind: LaunchKind::Goldsrc,
                steam_app_id: Some(225840),
                args: vec!["+connect {address}".to_string(), "+password {password}".to_string()],
            }),
            notes: Some(
                "GoldSrc. app_name is frozen by PLAN.md §5: deployed svencoop-prns \
                 v0.1.10 servers announce under it."
                    .to_string(),
            ),
        }
    }

    pub fn parse(toml_src: &str) -> Result<Self, PackError> {
        let pack: Self = toml::from_str(toml_src).map_err(PackError::Parse)?;
        if pack.schema_version != PACK_SCHEMA_VERSION {
            return Err(PackError::UnsupportedSchema {
                found: pack.schema_version,
                supported: PACK_SCHEMA_VERSION,
            });
        }
        // Validate here, not at first use: a broken pack should fail when it is
        // loaded, with the file in hand, rather than when someone tries to host.
        pack.to_profile()?;
        pack.content.validate().map_err(PackError::InvalidContent)?;
        // A bad template is a bad pack, reported with the file in hand rather
        // than at the moment a player presses Join.
        if let Some(launch) = &pack.launch {
            launch.validate().map_err(PackError::InvalidLaunch)?;
        }
        Ok(pack)
    }

    pub fn load(path: &Path) -> Result<Self, PackError> {
        let src = std::fs::read_to_string(path).map_err(PackError::Io)?;
        Self::parse(&src)
    }

    /// Load every `*.toml` in a directory.
    ///
    /// A single bad pack does not sink the rest: it is returned in the errors
    /// list so a launcher can say which file is broken and still start.
    pub fn load_dir(dir: &Path) -> Result<LoadedPacks, PackError> {
        let mut packs = Vec::new();
        let mut errors = Vec::new();
        let entries = std::fs::read_dir(dir).map_err(PackError::Io)?;
        for entry in entries {
            let entry = entry.map_err(PackError::Io)?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            match Self::load(&path) {
                Ok(pack) => packs.push(pack),
                Err(e) => errors.push((path.display().to_string(), e)),
            }
        }
        packs.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(LoadedPacks { packs, errors })
    }

    pub fn to_profile(&self) -> Result<GameProfile, ProfileError> {
        let profile = GameProfile {
            id: self.id.clone(),
            app_name: self.app_name.clone(),
            display_name: self.display_name.clone(),
            default_port: self.default_port,
            transport: self.transport.into(),
            min_link_class: self.min_link_class,
            query: self.query.map(Into::into),
            writable_paths: self.writable_paths.clone(),
            extra_ports: self.extra_ports.iter().map(GamePort::from).collect(),
        };
        profile.validate()?;
        Ok(profile)
    }
}

/// What a directory of packs yielded: the packs that loaded, and one entry per
/// file that did not, so a launcher can name the broken file and still start.
pub struct LoadedPacks {
    pub packs: Vec<GamePack>,
    pub errors: Vec<(String, PackError)>,
}

impl From<ContentError> for PackError {
    fn from(e: ContentError) -> Self {
        Self::InvalidContent(e)
    }
}

impl From<ProfileError> for PackError {
    fn from(e: ProfileError) -> Self {
        Self::Invalid(e)
    }
}

/// A pack, where it was read from, and what its provenance turned out to be
/// (`PLAN.md` §11.4).
///
/// The tier travels with the pack rather than being looked up later, because
/// §11.4 wants it shown at import and at deploy — two places that would
/// otherwise each have to remember to ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedPack {
    pub pack: GamePack,
    pub trust: PackTrust,
    /// The file this was read from, for a message that names it.
    pub file: String,
    /// Unix seconds this pack's signature goes stale, if it has one. A
    /// launcher shows it so a signature can be refreshed before it lapses,
    /// rather than at the moment a deploy fails.
    pub expires_at: Option<u64>,
}

/// What a directory of packs yielded when provenance was checked too.
pub struct VerifiedPacks {
    pub packs: Vec<TrustedPack>,
    pub errors: Vec<(String, PackError)>,
}

impl GamePack {
    /// Load a pack and establish its trust tier from the signature beside it.
    ///
    /// The bytes verified are the bytes parsed — read once, used for both — so
    /// there is no window in which the file could change between the two.
    pub fn load_verified(
        path: &Path,
        policy: &TrustPolicy,
        now: SystemTime,
    ) -> Result<TrustedPack, PackError> {
        let src = std::fs::read_to_string(path).map_err(PackError::Io)?;
        let pack = Self::parse(&src)?;
        let signature = signing::read_signature_beside(path).map_err(PackError::Signature)?;
        let trust = signing::verify_pack(src.as_bytes(), signature.as_ref(), policy, now)
            .map_err(|e| PackError::Signature(SigFileError::Signature(e)))?;
        Ok(TrustedPack {
            pack,
            trust,
            file: path.display().to_string(),
            expires_at: signature.map(|s| s.not_after),
        })
    }

    /// Load every `*.toml` in a directory, with its tier.
    ///
    /// A pack whose signature failed lands in `errors` like a malformed pack
    /// does — it is not loaded as an unsigned one. A `.sig` file is not itself
    /// a pack and is skipped by extension.
    pub fn load_dir_verified(
        dir: &Path,
        policy: &TrustPolicy,
        now: SystemTime,
    ) -> Result<VerifiedPacks, PackError> {
        let mut packs = Vec::new();
        let mut errors = Vec::new();
        let entries = std::fs::read_dir(dir).map_err(PackError::Io)?;
        for entry in entries {
            let entry = entry.map_err(PackError::Io)?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            match Self::load_verified(&path, policy, now) {
                Ok(pack) => packs.push(pack),
                Err(e) => errors.push((path.display().to_string(), e)),
            }
        }
        packs.sort_by(|a, b| a.pack.id.cmp(&b.pack.id));
        Ok(VerifiedPacks { packs, errors })
    }
}

impl From<SigFileError> for PackError {
    fn from(e: SigFileError) -> Self {
        Self::Signature(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentError;

    const SVEN_TOML: &str = include_str!("../../../packs/sven-coop.toml");

    /// A minimal pack with **no** `[content]` block, for the tests that append
    /// one. Deliberately not the shipped Sven pack: that one now declares a
    /// steamcmd driver, and a test that appended a second `[content]` table to
    /// it would be testing TOML's duplicate-key handling rather than this
    /// schema.
    const NO_CONTENT_TOML: &str = r#"
schema_version = 1
id = "content-test"
display_name = "Content Test"
app_name = "content-test"
default_port = 27015
transport = "udp"
min_link_class = 1
query = "a2s"
"#;

    /// The shipped pack and the built-in must not drift apart, or a user who
    /// edits the file gets different behaviour from one who does not.
    #[test]
    fn shipped_sven_pack_matches_the_builtin() {
        assert_eq!(GamePack::parse(SVEN_TOML).unwrap(), GamePack::sven_coop());
    }

    /// A Sven Co-op server writes `maps/soundcache/<map>.txt` on every map
    /// load. Under a read-only content mount that fails with `Error #30`
    /// (EROFS) and the server re-precaches its sounds from scratch at every
    /// changelevel — observed on a live node as an apparent precache loop.
    ///
    /// The path is only safe to make writable because a steamcmd install ships
    /// the directory and ships it **empty**: a writable path is an empty
    /// directory mounted *over* the content, so listing one that has files in
    /// it hides them. That is what listing `svencoop/maps` did to 108 shipped
    /// maps. If a future content version starts shipping files under
    /// `soundcache`, this entry becomes the same bug and must go.
    #[test]
    fn the_sound_cache_is_writable_or_every_map_change_re_precaches() {
        let pack = GamePack::sven_coop();
        assert!(
            pack.writable_paths.iter().any(|p| p == "svencoop/maps/soundcache"),
            "writable_paths were {:?}",
            pack.writable_paths
        );
        // The rule that keeps it safe: never the parent, which ships the maps.
        assert!(
            !pack.writable_paths.iter().any(|p| p == "svencoop/maps"),
            "making svencoop/maps writable hides every shipped map"
        );
    }

    /// The pack's app_name is the destination hash, and §5 freezes it.
    #[test]
    fn sven_pack_produces_the_frozen_profile() {
        let profile = GamePack::sven_coop().to_profile().unwrap();
        assert_eq!(profile, GameProfile::sven_coop());
        assert_eq!(profile.app_name, "sven-coop");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let src = SVEN_TOML.to_string() + "\nlaunch_command = \"rm -rf /\"\n";
        assert!(
            matches!(GamePack::parse(&src), Err(PackError::Parse(_))),
            "a pack must not silently ignore a field it does not understand"
        );
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_half_read() {
        let src = SVEN_TOML.replace("schema_version = 1", "schema_version = 2");
        assert!(matches!(
            GamePack::parse(&src),
            Err(PackError::UnsupportedSchema { found: 2, supported: 1 })
        ));
    }

    #[test]
    fn an_unusable_pack_fails_at_load_not_at_host() {
        let src = SVEN_TOML.replace("min_link_class = 1", "min_link_class = 9");
        assert!(matches!(
            GamePack::parse(&src),
            Err(PackError::Invalid(ProfileError::UnknownLinkClass(9)))
        ));

        let src = SVEN_TOML.replace("id = \"sven-coop\"", "id = \"\"");
        assert!(matches!(
            GamePack::parse(&src),
            Err(PackError::Invalid(ProfileError::EmptyId))
        ));
    }

    /// `GAMES.md` §7 step 1: a GoldSrc sibling must be reachable by writing a
    /// file. These three packs share an engine, a query protocol and a
    /// transport, and differ only in the data — so if this test ever needs a
    /// Rust change to pass, the pack abstraction is wrong, not the test.
    #[test]
    fn the_goldsrc_siblings_are_data_and_nothing_else() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
        let loaded = GamePack::load_dir(&dir).unwrap();
        for id in ["sven-coop", "half-life", "counter-strike-16"] {
            let pack = loaded
                .packs
                .iter()
                .find(|p| p.id == id)
                .unwrap_or_else(|| panic!("{id} is shipped"));
            let profile = pack.to_profile().expect("a shipped pack is usable");
            assert_eq!(profile.query, Some(QueryProtocol::A2s));
            assert_eq!(profile.default_port, 27015);
            assert_eq!(profile.min_link_class, 1);
        }

        // Distinct app names, so the three do not share a destination. A pack
        // that copied another's `app_name` would put its servers in the other
        // game's browser, silently.
        let names: std::collections::BTreeSet<&str> =
            loaded.packs.iter().map(|p| p.app_name.as_str()).collect();
        assert_eq!(names.len(), loaded.packs.len(), "two packs share an app_name");
    }

    /// The two siblings pull the same steamcmd app and differ in mod directory.
    /// Which mod actually runs is a runtime argument, so it stays out of the
    /// pack — see the module docs on why a pack cannot name what runs.
    #[test]
    fn both_goldsrc_packs_fetch_app_90_and_differ_only_in_their_mod_dir() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
        let loaded = GamePack::load_dir(&dir).unwrap();
        let get = |id: &str| {
            loaded.packs.iter().find(|p| p.id == id).expect("shipped").clone()
        };
        let hl = get("half-life");
        let cs = get("counter-strike-16");
        assert_eq!(hl.content, PackContent::Steamcmd { app_id: 90 });
        assert_eq!(cs.content, hl.content);
        assert_eq!(hl.writable_paths, ["valve/maps", "valve/logs"]);
        assert_eq!(cs.writable_paths, ["cstrike/maps", "cstrike/logs"]);
    }

    #[test]
    fn load_dir_reads_the_shipped_pack_and_reports_a_broken_one() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packs");
        let loaded = GamePack::load_dir(&dir).unwrap();
        assert!(loaded.errors.is_empty(), "shipped packs must all load: {:?}", loaded.errors);
        assert!(loaded.packs.iter().any(|p| p.id == "sven-coop"));
    }

    /// Every pack written before `[content]` existed keeps working, and keeps
    /// meaning what it always meant: the operator installs the files.
    #[test]
    fn a_pack_with_no_content_block_loads_as_manual() {
        let pack = GamePack::parse(NO_CONTENT_TOML).unwrap();
        assert_eq!(pack.content, PackContent::Manual { note: None });
        assert!(!pack.content.is_automatic());
    }

    /// A broken `[content]` block fails with the file in hand, like every other
    /// pack defect — not at deploy, on a node, in front of a user.
    #[test]
    fn a_bad_content_block_fails_at_load() {
        let src = NO_CONTENT_TOML.to_string()
            + "\n[content]\ndriver = \"archive\"\nurl = \"file:///etc/passwd\"\n\
               sha256 = \"9f2c00000000000000000000000000000000000000000000000000000000abcd\"\n";
        assert!(matches!(
            GamePack::parse(&src),
            Err(PackError::InvalidContent(ContentError::UnsupportedUrlScheme(_)))
        ));
    }

    #[test]
    fn an_archive_content_block_round_trips_through_a_pack() {
        let src = NO_CONTENT_TOML.to_string()
            + "\n[content]\ndriver = \"archive\"\n\
               url = \"https://example.org/sven.tar.xz\"\n\
               sha256 = \"9f2c00000000000000000000000000000000000000000000000000000000abcd\"\n\
               strip_components = 1\n";
        let pack = GamePack::parse(&src).unwrap();
        assert!(pack.content.is_automatic());
        assert_eq!(pack.content.driver_name(), "archive");
    }

    /// A TCP pack loads and produces a usable profile now that `stream.rs`
    /// splices streams (`DESIGN.md` §2.1). Before that it was refused at load,
    /// because the relay pumped datagrams regardless of the field and would
    /// have bridged the wrong protocol.
    #[test]
    fn a_tcp_pack_loads_now_that_the_stream_relay_exists() {
        let src = SVEN_TOML
            .replace("transport = \"udp\"", "transport = \"tcp\"")
            .replace("min_link_class = 1", "min_link_class = 2");
        let pack = GamePack::parse(&src).expect("a TCP pack is usable now");
        assert_eq!(pack.transport, PackTransport::Tcp);
        assert_eq!(pack.to_profile().unwrap().transport, GameTransport::Tcp);
    }
}
