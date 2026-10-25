//! Admins of the index beyond the hosting file (`ROADMAP.md` §13).
//!
//! The hosting file's `admins` is where the first admin comes from, written by
//! whoever runs the index. After that an account can ask to become an admin,
//! and an existing admin grants or refuses it here. A grant is kept in
//! `index-admins.json` beside the hosting file and can be revoked here; an
//! admin from the file is the file's, and is changed only there.
//!
//! The store records; who may decide, and whether a grant may be revoked, are
//! `hosting.rs`'s.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::regions::RequestState;

/// What a grant says granted it once the admin who did has asked to be
/// forgotten.
pub const FORGOTTEN: &str = "a removed account";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The account made an admin, by identity hash.
    pub identity: String,
    pub granted_by: String,
    pub granted_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminRequest {
    pub id: String,
    pub identity: String,
    /// What the asker wrote, shown to admins as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_unix: u64,
    pub state: RequestState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_unix: Option<u64>,
    /// Why it was refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    granted: Vec<Grant>,
    #[serde(default)]
    requests: Vec<AdminRequest>,
}

pub struct AdminStore {
    path: Option<PathBuf>,
    store: Mutex<Store>,
}

impl Default for AdminStore {
    fn default() -> Self {
        Self { path: None, store: Mutex::new(Store::default()) }
    }
}

impl AdminStore {
    /// Read the store at `path`, or an empty one if it is not there. A file
    /// that does not parse is an error, never an empty store: that would drop
    /// every grant without a word.
    pub fn load(path: &Path) -> Result<Self> {
        let store = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path: Some(path.to_path_buf()), store: Mutex::new(store) })
    }

    fn save(&self, store: &Store) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let partial = path.with_extension("json.partial");
        std::fs::write(&partial, serde_json::to_vec_pretty(store)?)
            .with_context(|| format!("writing {}", partial.display()))?;
        std::fs::rename(&partial, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn granted(&self) -> Vec<Grant> {
        self.lock().granted.clone()
    }

    pub fn is_granted(&self, identity: &str) -> bool {
        self.lock().granted.iter().any(|g| g.identity.eq_ignore_ascii_case(identity))
    }

    /// Record a request. One pending per account.
    pub fn ask(&self, identity: &str, note: Option<String>, now: u64) -> Result<AdminRequest> {
        let mut store = self.lock();
        if store.requests.iter().any(|r| r.identity == identity && r.state == RequestState::Pending)
        {
            return Err(anyhow!("you already asked; an admin has not decided yet"));
        }
        let mut bytes = [0u8; 8];
        getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("no entropy: {e}"))?;
        let request = AdminRequest {
            id: hex::encode(bytes),
            identity: identity.to_string(),
            note,
            created_unix: now,
            state: RequestState::Pending,
            decided_by: None,
            decided_unix: None,
            reason: None,
        };
        store.requests.push(request.clone());
        self.save(&store)?;
        Ok(request)
    }

    /// Every request, newest first.
    pub fn requests(&self) -> Vec<AdminRequest> {
        let mut list = self.lock().requests.clone();
        list.sort_by(|a, b| b.created_unix.cmp(&a.created_unix));
        list
    }

    pub fn latest_for(&self, identity: &str) -> Option<AdminRequest> {
        self.requests().into_iter().find(|r| r.identity == identity)
    }

    /// Grant a pending request: the asker becomes an admin.
    pub fn approve(&self, id: &str, by: &str, now: u64) -> Result<AdminRequest> {
        let mut store = self.lock();
        let r = store
            .requests
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow!("no such request"))?;
        if r.state != RequestState::Pending {
            return Err(anyhow!("that request was already decided"));
        }
        r.state = RequestState::Approved;
        r.decided_by = Some(by.to_string());
        r.decided_unix = Some(now);
        let out = r.clone();
        if !store.granted.iter().any(|g| g.identity == out.identity) {
            store.granted.push(Grant {
                identity: out.identity.clone(),
                granted_by: by.to_string(),
                granted_unix: now,
            });
        }
        self.save(&store)?;
        Ok(out)
    }

    pub fn deny(
        &self,
        id: &str,
        by: &str,
        now: u64,
        reason: Option<String>,
    ) -> Result<AdminRequest> {
        let mut store = self.lock();
        let r = store
            .requests
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow!("no such request"))?;
        if r.state != RequestState::Pending {
            return Err(anyhow!("that request was already decided"));
        }
        r.state = RequestState::Denied;
        r.decided_by = Some(by.to_string());
        r.decided_unix = Some(now);
        r.reason = reason;
        let out = r.clone();
        self.save(&store)?;
        Ok(out)
    }

    /// An account asked to be forgotten: its grant and its requests go, and
    /// where it granted or decided for someone else, that record stays without
    /// its name. A file admin is the hosting file's, not this store's. Returns
    /// how many records went.
    pub fn forget(&self, identity: &str) -> Result<usize> {
        let mut store = self.lock();
        let before = store.granted.len() + store.requests.len();
        store.granted.retain(|g| !g.identity.eq_ignore_ascii_case(identity));
        store.requests.retain(|r| !r.identity.eq_ignore_ascii_case(identity));
        let removed = before - store.granted.len() - store.requests.len();
        let mut changed = removed > 0;
        for g in store.granted.iter_mut() {
            if g.granted_by.eq_ignore_ascii_case(identity) {
                g.granted_by = FORGOTTEN.to_string();
                changed = true;
            }
        }
        for r in store.requests.iter_mut() {
            if r.decided_by.as_deref().is_some_and(|by| by.eq_ignore_ascii_case(identity)) {
                r.decided_by = None;
                changed = true;
            }
        }
        if changed {
            self.save(&store)?;
        }
        Ok(removed)
    }

    /// Take a grant away. A file admin is not here to take.
    pub fn revoke(&self, identity: &str) -> Result<()> {
        let mut store = self.lock();
        let before = store.granted.len();
        store.granted.retain(|g| !g.identity.eq_ignore_ascii_case(identity));
        if store.granted.len() == before {
            return Err(anyhow!("that account is not an admin granted here"));
        }
        self.save(&store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request is decided once and a grant follows an approval; a denial
    /// grants nothing; grants survive a restart and can be revoked.
    #[test]
    fn a_grant_follows_an_approval_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index-admins.json");
        let store = AdminStore::load(&path).unwrap();
        let asked = store.ask("aa", Some("I run the eu workers".into()), 1).unwrap();
        assert!(store.ask("aa", None, 2).is_err(), "one pending per account");
        let other = store.ask("bb", None, 3).unwrap();
        store.deny(&other.id, "root", 4, Some("not yet".into())).unwrap();
        assert!(!store.is_granted("bb"), "a denial grants nothing");
        store.approve(&asked.id, "root", 5).unwrap();
        assert!(store.approve(&asked.id, "root", 6).is_err(), "decided twice");
        assert!(store.is_granted("AA"), "identities compare without case");

        let again = AdminStore::load(&path).unwrap();
        assert!(again.is_granted("aa"));
        assert_eq!(again.latest_for("bb").unwrap().reason.as_deref(), Some("not yet"));
        again.revoke("aa").unwrap();
        assert!(again.revoke("aa").is_err());
        assert!(!AdminStore::load(&path).unwrap().is_granted("aa"));
    }

    #[test]
    fn an_admins_file_that_does_not_parse_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index-admins.json");
        std::fs::write(&path, b"[").unwrap();
        assert!(AdminStore::load(&path).is_err());
    }
}
