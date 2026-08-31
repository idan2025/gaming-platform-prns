//! Persisted launcher settings — the small amount of state a player expects to
//! survive a restart: where their own copy of each game lives, and the name
//! they join under.
//!
//! # Why this exists and where it sits in the design
//!
//! `PLAN.md` §13.3 step 1 names the two halves of "the Play button": *game
//! location* ("find a Steam library, or let the player pick their game once and
//! remember it") and the button itself. This module is the "remember it" half.
//! It is deliberately in `launcher-core`, not the Tauri shell, so it is testable
//! without a webview — the same split every other shape in this crate keeps.
//!
//! # The safety line this module must not cross
//!
//! A saved path is **the player's own choice**, never a pack's. `PLAN.md`
//! §13.1 rule 1 is that a pack can never name an executable; nothing here lets
//! it. A `game_path` is written only by [`crate::Launcher::set_game_path`],
//! which a person drives, and it is the launcher's stored value — a pack cannot
//! reach it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What the launcher remembers between runs.
///
/// Every field defaults to empty, and an unreadable or partial file decodes to
/// defaults rather than failing: a launcher that will not start because its
/// settings file is corrupt is worse than one that forgot a game path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherSettings {
    /// Per-game absolute path to the player's own game executable, keyed by
    /// pack id. Set once by the player and reused; a pack never writes here.
    pub game_paths: BTreeMap<String, PathBuf>,
    /// The display name the player joins under, reused across games so it is
    /// typed once rather than per join.
    pub player_name: Option<String>,
}

impl LauncherSettings {
    /// Read settings from disk, tolerating both "no file yet" and "file is
    /// junk" by returning defaults. A warning is logged for the junk case
    /// because it means a previous write or a hand-edit went wrong, which the
    /// silent-empty-file case does not.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(body) => serde_json::from_str(&body).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "launcher settings are not valid JSON; starting from defaults");
                Self::default()
            }),
            // No file is the ordinary first-run case, not an error.
            Err(_) => Self::default(),
        }
    }

    /// Write settings to disk, creating the parent directory if needed.
    ///
    /// Writes to a sibling temporary file and renames into place, so an
    /// interrupted write can never leave a half-written settings file that the
    /// next [`load`](Self::load) would then discard — losing every saved game
    /// path, not just the one being changed.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// The default place to keep the settings file, per platform, or `None` when no
/// home/config directory can be found (a stripped environment, a daemon with no
/// `$HOME`). The caller then runs with in-memory settings that do not persist,
/// which is a degraded mode rather than a failure.
///
/// Paths follow each platform's convention rather than inventing one:
/// `$XDG_CONFIG_HOME` (or `~/.config`) on Linux, Application Support on macOS,
/// `%APPDATA%` on Windows.
pub fn default_settings_path() -> Option<PathBuf> {
    let dir = config_dir()?.join("gaming-platform-prns");
    Some(dir.join("launcher.json"))
}

#[cfg(target_os = "windows")]
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_reads_as_defaults_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = LauncherSettings::load(&dir.path().join("nope.json"));
        assert!(s.game_paths.is_empty());
        assert!(s.player_name.is_none());
    }

    #[test]
    fn a_corrupt_file_reads_as_defaults_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("launcher.json");
        std::fs::write(&path, "this is not json {").unwrap();
        let s = LauncherSettings::load(&path);
        assert!(s.game_paths.is_empty(), "a junk file must not carry over as data");
    }

    #[test]
    fn settings_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/launcher.json");
        let mut s = LauncherSettings::default();
        s.game_paths.insert("sven-coop".into(), PathBuf::from("/games/svends"));
        s.player_name = Some("idan".into());
        s.save(&path).unwrap();
        assert!(path.exists(), "save creates the parent directory");

        let back = LauncherSettings::load(&path);
        assert_eq!(back.game_paths.get("sven-coop"), Some(&PathBuf::from("/games/svends")));
        assert_eq!(back.player_name.as_deref(), Some("idan"));
    }

    /// A partial file — one key present, another absent — fills the rest from
    /// defaults rather than refusing, because the settings file gains keys over
    /// releases and an old file must still load.
    #[test]
    fn a_partial_file_fills_the_rest_from_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("launcher.json");
        std::fs::write(&path, r#"{"player_name":"idan"}"#).unwrap();
        let s = LauncherSettings::load(&path);
        assert_eq!(s.player_name.as_deref(), Some("idan"));
        assert!(s.game_paths.is_empty());
    }

    /// An interrupted write must not leave a temp file where the real one goes,
    /// and the real file must be complete after a save.
    #[test]
    fn a_save_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("launcher.json");
        LauncherSettings::default().save(&path).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "a completed save leaves no .tmp sibling");
    }
}
