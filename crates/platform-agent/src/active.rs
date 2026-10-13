//! Which installed copy of a game new servers use, once an update has moved it.
//!
//! `agent.toml` names one `content_version` per game and is the operator's
//! file: the agent never rewrites it. An update (`Provisioner::update`) puts
//! the newest build in a **new** version directory, because existing content
//! is never replaced in place, so something has to say "new servers use that
//! one now". This is that something: a small file beside the content, written
//! only when the operator asks for an update.
//!
//! # The rules
//!
//! * **The operator's config still wins when it changes.** Each entry records
//!   the `content_version` it was derived from (`base`). Edit `agent.toml` to
//!   name another version and the entry no longer applies — an override that
//!   outlived the config it overrode would make the config a lie.
//! * **A missing directory is not an answer.** If the active copy was deleted,
//!   the game falls back to the config's version rather than to a name with
//!   nothing behind it (`Agent::active_version`).
//! * **Old copies queued for removal are removed only when nothing mounts
//!   them, and never the active one** (`Agent::sweep_old_copies`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One game's override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The `content_version` in `agent.toml` this was derived from.
    pub base: String,
    /// The version new servers use.
    pub active: String,
    /// Old versions the operator asked to remove once nothing uses them.
    #[serde(default)]
    pub pending_removal: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    games: BTreeMap<String, Entry>,
}

pub struct ActiveVersions {
    path: PathBuf,
    state: std::sync::Mutex<State>,
}

impl ActiveVersions {
    pub const FILE: &'static str = "content-active.json";

    /// Read the file under `data_root`. Missing is empty; unreadable is empty
    /// too, with a warning — the cost is that new servers use the config's
    /// version, which is what they did before any update.
    pub fn load(data_root: &Path) -> Self {
        let path = data_root.join(Self::FILE);
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "unreadable; new servers use agent.toml's versions");
                State::default()
            }),
            Err(_) => State::default(),
        };
        Self { path, state: std::sync::Mutex::new(state) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The version an update made active for `game`, if one applies to this
    /// `config_version`.
    pub fn active(&self, game: &str, config_version: &str) -> Option<String> {
        self.lock()
            .games
            .get(game)
            .filter(|e| e.base == config_version)
            .map(|e| e.active.clone())
    }

    /// Make `active` the version new servers use. Pending removals carry over
    /// unless `active` is one of them.
    pub fn set_active(&self, game: &str, base: &str, active: &str) -> std::io::Result<()> {
        let mut state = self.lock();
        let entry = state.games.entry(game.to_string()).or_insert_with(|| Entry {
            base: base.to_string(),
            active: active.to_string(),
            pending_removal: Vec::new(),
        });
        entry.base = base.to_string();
        entry.active = active.to_string();
        entry.pending_removal.retain(|v| v != active);
        self.save(&state)
    }

    pub fn add_pending(&self, game: &str, version: &str) -> std::io::Result<()> {
        let mut state = self.lock();
        let Some(entry) = state.games.get_mut(game) else { return Ok(()) };
        if entry.active != version && !entry.pending_removal.iter().any(|v| v == version) {
            entry.pending_removal.push(version.to_string());
        }
        self.save(&state)
    }

    pub fn remove_pending(&self, game: &str, version: &str) -> std::io::Result<()> {
        let mut state = self.lock();
        let Some(entry) = state.games.get_mut(game) else { return Ok(()) };
        entry.pending_removal.retain(|v| v != version);
        self.save(&state)
    }

    /// Every `(game, version)` waiting to be removed, never an active one.
    pub fn pending(&self) -> Vec<(String, String)> {
        self.lock()
            .games
            .iter()
            .flat_map(|(g, e)| {
                e.pending_removal
                    .iter()
                    .filter(|v| **v != e.active)
                    .map(|v| (g.clone(), v.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Written beside and renamed over, so a crash mid-write never leaves a
    /// half file that reads as "no overrides".
    fn save(&self, state: &State) -> std::io::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_override_applies_only_to_the_config_it_was_made_from() {
        let dir = tempfile::tempdir().unwrap();
        let a = ActiveVersions::load(dir.path());
        a.set_active("tf2", "base", "base-20261013").unwrap();
        assert_eq!(a.active("tf2", "base").as_deref(), Some("base-20261013"));
        // The operator pointed agent.toml somewhere else: their word wins.
        assert_eq!(a.active("tf2", "hand-made"), None);
        // And it survives a restart.
        let b = ActiveVersions::load(dir.path());
        assert_eq!(b.active("tf2", "base").as_deref(), Some("base-20261013"));
    }

    #[test]
    fn the_active_version_is_never_pending_removal() {
        let dir = tempfile::tempdir().unwrap();
        let a = ActiveVersions::load(dir.path());
        a.set_active("g", "v1", "v2").unwrap();
        a.add_pending("g", "v1").unwrap();
        a.add_pending("g", "v2").unwrap();
        assert_eq!(a.pending(), vec![("g".to_string(), "v1".to_string())]);
        // Rolling back to v1 takes it off the list.
        a.set_active("g", "v1", "v1").unwrap();
        assert!(a.pending().is_empty());
    }

    #[test]
    fn a_corrupt_file_reads_as_no_overrides() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ActiveVersions::FILE), b"{not json").unwrap();
        assert_eq!(ActiveVersions::load(dir.path()).active("g", "v"), None);
    }
}
