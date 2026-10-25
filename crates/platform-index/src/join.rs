//! Join codes: how a new machine becomes a worker without anyone editing the
//! hosting file by hand (`HOSTING.md`, "Adding a worker").
//!
//! An admin makes a code (a one-time secret) that fixes where the worker will
//! sit: its region, class, priority and, optionally, its name. The new machine
//! presents the code with its own public address and the index adds it to the
//! hosting file. A code works once, expires, and can be revoked.
//!
//! The code is a secret, so only its SHA-256 is kept; it is shown once, when it
//! is made. The store records; who may make one and what becomes of a claim are
//! `hosting.rs`'s.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Codes waiting at once. A code is a way in; a pile of them is a pile of ways.
pub const MAX_PENDING: usize = 20;
/// Longest a code may live.
pub const MAX_TTL_SECS: u64 = 24 * 3600;
/// Failed claims in a minute before every claim is refused for the rest of it.
const MAX_FAILURES_PER_MINUTE: usize = 10;

/// What a code decides about the worker that uses it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(default)]
    pub priority: i32,
    /// An account the worker is reserved for (`NodeConfig::reserved_for`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_for: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodeState {
    Pending,
    Used,
    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Code {
    id: String,
    /// Hex SHA-256 of the code. The code itself is never kept.
    token_sha256: String,
    spec: JoinSpec,
    created_by: String,
    created_unix: u64,
    expires_unix: u64,
    state: CodeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    used_unix: Option<u64>,
    /// The worker that used it, by control address, and the name it was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    used_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    used_name: Option<String>,
}

/// A code as an admin sees it: everything but the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinView {
    pub id: String,
    pub spec: JoinSpec,
    pub created_unix: u64,
    pub expires_unix: u64,
    /// `pending`, `used`, `revoked`, or `expired` (a pending one past its time).
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_name: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    codes: Vec<Code>,
}

pub struct JoinStore {
    path: Option<PathBuf>,
    store: Mutex<Store>,
    failures: Mutex<VecDeque<u64>>,
}

impl Default for JoinStore {
    fn default() -> Self {
        Self {
            path: None,
            store: Mutex::new(Store::default()),
            failures: Mutex::new(VecDeque::new()),
        }
    }
}

fn digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn view(c: &Code, now: u64) -> JoinView {
    let state = match c.state {
        CodeState::Pending if c.expires_unix <= now => "expired".to_string(),
        CodeState::Pending => "pending".to_string(),
        CodeState::Used => "used".to_string(),
        CodeState::Revoked => "revoked".to_string(),
    };
    JoinView {
        id: c.id.clone(),
        spec: c.spec.clone(),
        created_unix: c.created_unix,
        expires_unix: c.expires_unix,
        state,
        used_unix: c.used_unix,
        used_name: c.used_name.clone(),
    }
}

impl JoinStore {
    /// Read the store at `path`, or an empty one if it is not there. A file that
    /// does not parse is an error, never an empty store: that would quietly
    /// forget which codes were spent.
    pub fn load(path: &Path) -> Result<Self> {
        let store = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            path: Some(path.to_path_buf()),
            store: Mutex::new(store),
            failures: Mutex::new(VecDeque::new()),
        })
    }

    /// Written beside and renamed, owner-only: the file holds code hashes.
    fn save(&self, store: &Store) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let partial = path.with_extension("json.partial");
        {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            // Owner-only where the platform has modes; elsewhere the folder's own rules.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options.open(&partial).with_context(|| format!("writing {}", partial.display()))?;
            f.write_all(&serde_json::to_vec_pretty(store)?)?;
        }
        std::fs::rename(&partial, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Make a code. Returns it for listing and the secret, which is shown once.
    pub fn create(
        &self,
        spec: JoinSpec,
        ttl_secs: u64,
        by: &str,
        now: u64,
    ) -> Result<(JoinView, String)> {
        let mut store = self.lock();
        if store.codes.iter().filter(|c| view(c, now).state == "pending").count() >= MAX_PENDING {
            return Err(anyhow!("{MAX_PENDING} codes are waiting; revoke or use one first"));
        }
        let mut secret = [0u8; 24];
        getrandom::getrandom(&mut secret).map_err(|e| anyhow!("no entropy: {e}"))?;
        let token = hex::encode(secret);
        let mut idb = [0u8; 6];
        getrandom::getrandom(&mut idb).map_err(|e| anyhow!("no entropy: {e}"))?;
        let code = Code {
            id: hex::encode(idb),
            token_sha256: digest(&token),
            spec,
            created_by: by.to_string(),
            created_unix: now,
            expires_unix: now + ttl_secs.clamp(60, MAX_TTL_SECS),
            state: CodeState::Pending,
            used_unix: None,
            used_agent: None,
            used_name: None,
        };
        let out = view(&code, now);
        store.codes.push(code);
        self.save(&store)?;
        Ok((out, token))
    }

    /// Every code, newest first, without its secret.
    pub fn list(&self, now: u64) -> Vec<JoinView> {
        let mut out: Vec<JoinView> = self.lock().codes.iter().map(|c| view(c, now)).collect();
        out.sort_by(|a, b| b.created_unix.cmp(&a.created_unix));
        out
    }

    /// Take a waiting code back. A used one stays as the record it is.
    pub fn revoke(&self, id: &str) -> Result<()> {
        let mut store = self.lock();
        let c = store
            .codes
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or_else(|| anyhow!("no such join code"))?;
        if c.state != CodeState::Pending {
            return Err(anyhow!("that code is already {}", view(c, 0).state));
        }
        c.state = CodeState::Revoked;
        self.save(&store)
    }

    /// Spend a code. A wrong, spent, revoked or expired code is the same
    /// answer, so nothing says which codes exist. Marked used before anything
    /// is done with it, so two machines presenting it together cannot both win;
    /// [`release`](Self::release) puts it back if the worker could not be added.
    pub fn claim(&self, token: &str, now: u64) -> Result<(String, JoinSpec)> {
        {
            let mut fails = self.failures.lock().unwrap_or_else(|e| e.into_inner());
            while fails.front().is_some_and(|t| *t + 60 <= now) {
                fails.pop_front();
            }
            if fails.len() >= MAX_FAILURES_PER_MINUTE {
                return Err(anyhow!("too many wrong codes; wait a minute"));
            }
        }
        let wanted = digest(token);
        let mut store = self.lock();
        let found = store.codes.iter_mut().find(|c| {
            // Both sides are fixed-length hex of a hash; a plain compare leaks
            // nothing about the code behind it.
            c.token_sha256 == wanted
        });
        match found {
            Some(c) if c.state == CodeState::Pending && c.expires_unix > now => {
                c.state = CodeState::Used;
                c.used_unix = Some(now);
                let out = (c.id.clone(), c.spec.clone());
                self.save(&store)?;
                Ok(out)
            }
            _ => {
                drop(store);
                self.failures.lock().unwrap_or_else(|e| e.into_inner()).push_back(now);
                Err(anyhow!("that join code is not valid"))
            }
        }
    }

    /// Record who used a code.
    pub fn finish(&self, id: &str, agent: &str, name: &str) -> Result<()> {
        let mut store = self.lock();
        if let Some(c) = store.codes.iter_mut().find(|c| c.id == id) {
            c.used_agent = Some(agent.to_string());
            c.used_name = Some(name.to_string());
        }
        self.save(&store)
    }

    /// Put a claimed code back, because adding the worker failed and the
    /// person should be able to try again.
    pub fn release(&self, id: &str) -> Result<()> {
        let mut store = self.lock();
        if let Some(c) = store.codes.iter_mut().find(|c| c.id == id) {
            c.state = CodeState::Pending;
            c.used_unix = None;
        }
        self.save(&store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> JoinSpec {
        JoinSpec { name: None, region: Some("eu".into()), class: None, priority: 1, reserved_for: None }
    }

    /// A code works once, only before it expires, and is the same refusal
    /// whether it is wrong, spent, revoked or late; the secret is not kept.
    #[test]
    fn a_code_works_once_and_every_refusal_reads_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("join-codes.json");
        let store = JoinStore::load(&path).unwrap();
        let (made, token) = store.create(spec(), 600, "root", 100).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains(&token), "only the hash is kept");

        let (id, got) = store.claim(&token, 150).unwrap();
        assert_eq!((id.as_str(), got.region.as_deref()), (made.id.as_str(), Some("eu")));
        let spent = store.claim(&token, 151).unwrap_err().to_string();
        let wrong = store.claim("0000", 152).unwrap_err().to_string();
        assert_eq!(spent, wrong, "a spent code and a wrong one read the same");

        let (_, late) = store.create(spec(), 60, "root", 200).unwrap();
        assert_eq!(store.claim(&late, 300).unwrap_err().to_string(), wrong, "late reads the same");
        let (revoked, gone) = store.create(spec(), 600, "root", 400).unwrap();
        store.revoke(&revoked.id).unwrap();
        assert_eq!(
            store.claim(&gone, 401).unwrap_err().to_string(),
            wrong,
            "revoked reads the same"
        );
        assert!(store.revoke(&revoked.id).is_err(), "a decided code is not revoked again");

        let states: Vec<String> = store.list(500).into_iter().map(|v| v.state).collect();
        assert_eq!(states, ["revoked", "expired", "used"]);
        let again = JoinStore::load(&path).unwrap();
        assert!(again.claim(&token, 500).is_err(), "a spent code stays spent across a restart");
    }

    /// A claim whose worker could not be added gives the code back.
    #[test]
    fn a_released_code_can_be_used_again() {
        let store = JoinStore::default();
        let (_, token) = store.create(spec(), 600, "root", 1).unwrap();
        let (id, _) = store.claim(&token, 2).unwrap();
        store.release(&id).unwrap();
        assert!(store.claim(&token, 3).is_ok());
    }

    /// Guessing is throttled for everyone, and the code that is right stops
    /// working while it lasts rather than telling the guesser it was right.
    #[test]
    fn wrong_codes_in_a_burst_lock_claims_for_a_minute() {
        let store = JoinStore::default();
        let (_, token) = store.create(spec(), 600, "root", 1).unwrap();
        for i in 0..MAX_FAILURES_PER_MINUTE {
            assert!(store.claim("nope", 10 + i as u64).is_err());
        }
        let locked = store.claim(&token, 30).unwrap_err().to_string();
        assert!(locked.contains("too many"), "{locked}");
        assert!(store.claim(&token, 80).is_ok(), "a minute later it is fine");
    }

    #[test]
    fn only_so_many_codes_wait_at_once_and_a_bad_file_is_an_error() {
        let store = JoinStore::default();
        for _ in 0..MAX_PENDING {
            store.create(spec(), 600, "root", 1).unwrap();
        }
        assert!(store.create(spec(), 600, "root", 2).is_err());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("join-codes.json");
        std::fs::write(&path, b"[").unwrap();
        assert!(JoinStore::load(&path).is_err());
    }
}
