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

use serde::{Deserialize, Serialize};

use crate::profile::{GameProfile, GameTransport, ProfileError, QueryProtocol};

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
    /// Free-text note for a human reading the pack. Never parsed.
    #[serde(default)]
    pub notes: Option<String>,
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
            writable_paths: vec![
                "svencoop/maps".to_string(),
                "svencoop/logs".to_string(),
                "svencoop/scripts".to_string(),
            ],
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

impl From<ProfileError> for PackError {
    fn from(e: ProfileError) -> Self {
        Self::Invalid(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVEN_TOML: &str = include_str!("../../../packs/sven-coop.toml");

    /// The shipped pack and the built-in must not drift apart, or a user who
    /// edits the file gets different behaviour from one who does not.
    #[test]
    fn shipped_sven_pack_matches_the_builtin() {
        assert_eq!(GamePack::parse(SVEN_TOML).unwrap(), GamePack::sven_coop());
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

    #[test]
    fn load_dir_reads_the_shipped_pack_and_reports_a_broken_one() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packs");
        let loaded = GamePack::load_dir(&dir).unwrap();
        assert!(loaded.errors.is_empty(), "shipped packs must all load: {:?}", loaded.errors);
        assert!(loaded.packs.iter().any(|p| p.id == "sven-coop"));
    }

    /// The manifest understands `tcp`; the relay does not yet. So the pack
    /// *parses* the field and `parse` — which validates — refuses it, rather
    /// than a TCP game being silently bridged as UDP at run time.
    #[test]
    fn a_tcp_pack_parses_its_field_but_is_refused_until_the_relay_can_run_it() {
        let src = SVEN_TOML
            .replace("transport = \"udp\"", "transport = \"tcp\"")
            .replace("min_link_class = 1", "min_link_class = 2");
        assert!(
            matches!(
                GamePack::parse(&src),
                Err(PackError::Invalid(ProfileError::TransportNotImplemented(
                    GameTransport::Tcp
                )))
            ),
            "a TCP pack must be refused with a reason, not accepted and mis-run"
        );

        // The field itself still deserializes, so the manifest format is ready
        // for the day StreamRelay lands.
        let raw: GamePack = toml::from_str(&src).unwrap();
        assert_eq!(raw.transport, PackTransport::Tcp);
    }
}
