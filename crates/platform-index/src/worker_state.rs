//! Whether a worker may be given new servers (`ROADMAP.md` §13).
//!
//! A worker is `active` unless an admin has set it aside. **Cordoned** takes no
//! new server, by create or by move, and keeps the ones it has. **Draining** is
//! cordoned, and the index also moves its servers off to other workers. The
//! state is kept in `worker-states.json` beside the hosting file, keyed by the
//! worker's name, rather than written into the hosting file: that file is the
//! operator's own text and a program should not rewrite it for this.
//!
//! The store records; what a state does is `hosting.rs`'s.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerState {
    #[default]
    Active,
    Cordoned,
    Draining,
}

impl WorkerState {
    /// Whether new servers may be placed on a worker in this state.
    pub fn takes_new_servers(self) -> bool {
        self == WorkerState::Active
    }

    pub fn word(self) -> &'static str {
        match self {
            WorkerState::Active => "active",
            WorkerState::Cordoned => "cordoned",
            WorkerState::Draining => "draining",
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    workers: BTreeMap<String, WorkerState>,
}

#[derive(Default)]
pub struct WorkerStates {
    path: Option<PathBuf>,
    store: Mutex<Store>,
}

impl WorkerStates {
    /// Read the store at `path`, or an empty one if it is not there. A file that
    /// does not parse is an error, never an empty store: that would quietly put
    /// every worker being drained back into service.
    pub fn load(path: &Path) -> Result<Self> {
        let store = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path: Some(path.to_path_buf()), store: Mutex::new(store) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, worker: &str) -> WorkerState {
        self.lock().workers.get(worker).copied().unwrap_or_default()
    }

    /// Set a state. `Active` removes the entry, so the file lists only what is
    /// set aside.
    pub fn set(&self, worker: &str, state: WorkerState) -> Result<()> {
        let mut store = self.lock();
        if state == WorkerState::Active {
            store.workers.remove(worker);
        } else {
            store.workers.insert(worker.to_string(), state);
        }
        if let Some(path) = &self.path {
            let partial = path.with_extension("json.partial");
            std::fs::write(&partial, serde_json::to_vec_pretty(&*store)?)
                .with_context(|| format!("writing {}", partial.display()))?;
            std::fs::rename(&partial, path)
                .with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(())
    }

    /// Every worker that is not active, by name.
    pub fn set_aside(&self) -> Vec<(String, WorkerState)> {
        self.lock().workers.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state survives a restart, active is the absence of one, and only
    /// active takes new servers.
    #[test]
    fn a_worker_set_aside_stays_aside_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker-states.json");
        let states = WorkerStates::load(&path).unwrap();
        assert_eq!(states.get("w1"), WorkerState::Active);
        states.set("w1", WorkerState::Draining).unwrap();
        states.set("w2", WorkerState::Cordoned).unwrap();
        let again = WorkerStates::load(&path).unwrap();
        assert_eq!(again.get("w1"), WorkerState::Draining);
        assert_eq!(again.set_aside().len(), 2);
        again.set("w1", WorkerState::Active).unwrap();
        assert_eq!(
            WorkerStates::load(&path).unwrap().set_aside(),
            [("w2".to_string(), WorkerState::Cordoned)]
        );
        assert!(WorkerState::Active.takes_new_servers());
        assert!(
            !WorkerState::Cordoned.takes_new_servers()
                && !WorkerState::Draining.takes_new_servers()
        );
    }

    #[test]
    fn a_states_file_that_does_not_parse_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker-states.json");
        std::fs::write(&path, b"[").unwrap();
        assert!(WorkerStates::load(&path).is_err());
    }
}
