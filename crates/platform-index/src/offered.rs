//! Games an admin of the index offers beyond the hosting file's `games`.
//!
//! The hosting file is the operator's, and this index never writes it. An
//! admin who installs a game on the workers from the page offers it here
//! instead: `index-offered-games.json` beside the hosting file. The games in
//! force are the file's and these together (`Hosting::reload`). A game the file
//! lists is the file's, and stops being offered only there.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub game_id: String,
    /// The admin who offered it, by identity hash.
    pub offered_by: String,
    pub offered_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    offered: Vec<Offer>,
}

pub struct OfferedGames {
    path: Option<PathBuf>,
    store: Mutex<Store>,
}

impl Default for OfferedGames {
    fn default() -> Self {
        Self { path: None, store: Mutex::new(Store::default()) }
    }
}

impl OfferedGames {
    /// Read the store at `path`, or an empty one if it is not there. A file
    /// that does not parse is an error, never an empty store.
    pub fn load(path: &Path) -> Result<Self> {
        let store = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path: Some(path.to_path_buf()), store: Mutex::new(store) })
    }

    fn save(&self, store: &Store) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let partial = path.with_extension("json.partial");
        std::fs::write(&partial, serde_json::to_vec_pretty(store)?).with_context(|| format!("writing {}", partial.display()))?;
        std::fs::rename(&partial, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn list(&self) -> Vec<Offer> {
        self.lock().offered.clone()
    }

    pub fn game_ids(&self) -> Vec<String> {
        self.lock().offered.iter().map(|o| o.game_id.clone()).collect()
    }

    /// Offer a game. Offering one already offered changes nothing.
    pub fn offer(&self, game_id: &str, by: &str, now: u64) -> Result<()> {
        if !game_bridge::pack::id_is_file_safe(game_id) || game_id.len() > 64 {
            return Err(anyhow!("that is not a game id"));
        }
        let mut store = self.lock();
        if store.offered.iter().any(|o| o.game_id == game_id) {
            return Ok(());
        }
        store.offered.push(Offer { game_id: game_id.to_string(), offered_by: by.to_string(), offered_unix: now });
        self.save(&store)
    }

    /// Stop offering a game. False when it was not offered here.
    pub fn withdraw(&self, game_id: &str) -> Result<bool> {
        let mut store = self.lock();
        let before = store.offered.len();
        store.offered.retain(|o| o.game_id != game_id);
        let gone = store.offered.len() != before;
        if gone {
            self.save(&store)?;
        }
        Ok(gone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offer_is_kept_across_a_restart_and_a_broken_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index-offered-games.json");
        let store = OfferedGames::load(&path).unwrap();
        assert!(store.offer("../x", "ad", 1).is_err(), "a game id is data");
        store.offer("team-fortress-2", "ad", 1).unwrap();
        store.offer("team-fortress-2", "ad", 2).unwrap();
        assert_eq!(OfferedGames::load(&path).unwrap().game_ids(), ["team-fortress-2"], "kept, once");
        assert!(store.withdraw("team-fortress-2").unwrap());
        assert!(!store.withdraw("team-fortress-2").unwrap());
        assert!(OfferedGames::load(&path).unwrap().game_ids().is_empty());
        std::fs::write(&path, "{ broken").unwrap();
        assert!(OfferedGames::load(&path).is_err());
    }
}
