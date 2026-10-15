//! The official pack repository on a node (`game_bridge::repo::OFFICIAL_REPOSITORY`).
//!
//! On unless the operator turns it off. A pass reads the official list, held
//! to the project key, and takes every pack this node lacks, or has other
//! bytes of, or holds with a signature near its end, through the same import
//! as "Add a game…" (`pack_import::import`: digest, review, signature).
//!
//! Rules a later change could quietly break:
//! - **A pack is never replaced under a server.** While any server of the
//!   game exists, running or stopped, its update waits and the page says so:
//!   a pack decides ports, and a stopped server restarts on what it was made
//!   with.
//! - **Nothing is removed.** A game the list drops stays until the operator
//!   removes it.
//! - **Taking a pack runs nothing.** It only makes a game available; servers
//!   are still the operator's to create, under `[pack_trust]`.
//! - **The choice is a file in the data root** (`official-packs.json`), never
//!   `agent.toml`, like the update settings.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use game_bridge::repo::{check_repo_url, read_index, sha256_hex, signature_due};
use game_bridge::GamePack;
use prns_core::identity::IdentityHash;
use serde::{Deserialize, Serialize};

use crate::agent::Agent;
use crate::pack_import::{import, ImportRequest};

/// Where the operator's choice is kept, in the data root.
pub const SETTINGS_FILE: &str = "official-packs.json";

/// How often a node reads the official list.
pub const SYNC_EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// How long after start the first pass waits, so a restart is not a fetch.
pub const FIRST_SYNC_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    off: bool,
}

/// What the last pass did, as Lanthorn Host shows it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OfficialStatus {
    pub on: bool,
    pub url: String,
    pub checked_unix: Option<u64>,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    /// Games whose pack has an update that waits for their servers to go.
    pub waiting: Vec<String>,
    pub error: Option<String>,
}

pub struct Official {
    settings: PathBuf,
    url: String,
    key: IdentityHash,
    last: std::sync::Mutex<OfficialStatus>,
    /// One pass at a time: the timer and a "Check now" never interleave.
    running: tokio::sync::Mutex<()>,
}

impl Official {
    pub fn new(data_root: &Path) -> Self {
        Self::with_source(
            data_root,
            game_bridge::repo::OFFICIAL_REPOSITORY,
            game_bridge::signing::project_key(),
        )
    }

    /// Another source and key, for a test that signed its own list.
    pub fn with_source(data_root: &Path, url: &str, key: IdentityHash) -> Self {
        Self {
            settings: data_root.join(SETTINGS_FILE),
            url: url.to_string(),
            key,
            last: Default::default(),
            running: Default::default(),
        }
    }

    pub fn on(&self) -> bool {
        std::fs::read(&self.settings)
            .ok()
            .and_then(|b| serde_json::from_slice::<Stored>(&b).ok())
            .is_none_or(|s| !s.off)
    }

    pub fn set_on(&self, on: bool) -> std::io::Result<()> {
        if let Some(dir) = self.settings.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.settings, serde_json::to_vec(&Stored { off: !on })?)?;
        self.last.lock().unwrap_or_else(|e| e.into_inner()).on = on;
        Ok(())
    }

    pub fn status(&self) -> OfficialStatus {
        let mut s = self.last.lock().unwrap_or_else(|e| e.into_inner()).clone();
        s.on = self.on();
        s.url = self.url.clone();
        s
    }

    /// One pass: take what the official list has that `dir` lacks or holds
    /// older, then reload the agent's packs if anything changed.
    pub async fn sync(&self, agent: &Agent, dir: &Path) -> OfficialStatus {
        let _one = self.running.lock().await;
        let mut status = OfficialStatus { on: self.on(), url: self.url.clone(), ..Default::default() };
        if !status.on {
            *self.last.lock().unwrap_or_else(|e| e.into_inner()) = status.clone();
            return status;
        }
        let now = SystemTime::now();
        status.checked_unix =
            now.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs());
        if let Err(e) = self.pass(agent, dir, now, &mut status).await {
            status.error = Some(format!("{e:#}"));
        }
        if !(status.added.is_empty() && status.updated.is_empty()) {
            if let Err(e) = agent.reload_packs(dir, now).await {
                status.error.get_or_insert_with(|| format!("reloading packs: {e:#}"));
            }
        }
        if !status.added.is_empty() || !status.updated.is_empty() {
            tracing::info!(added = ?status.added, updated = ?status.updated, "official packs taken");
        }
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = status.clone();
        status
    }

    async fn pass(
        &self,
        agent: &Agent,
        dir: &Path,
        now: SystemTime,
        status: &mut OfficialStatus,
    ) -> anyhow::Result<()> {
        let index_url = check_repo_url(&self.url)?;
        let read = read_index(&index_url).await?;
        match &read.signature {
            Some((who, _)) if *who == self.key => {}
            _ => anyhow::bail!(
                "the official list is not signed by the project key; nothing was taken"
            ),
        }
        let policy = agent.config().pack_trust_policy();
        // What is here now, by id: the bytes' digest, and whether the copy
        // is the project's own with time left on its signature.
        let mut here: std::collections::HashMap<String, (String, bool)> = Default::default();
        for entry in std::fs::read_dir(dir)?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(src) = std::str::from_utf8(&bytes) else { continue };
            let Ok(pack) = GamePack::parse(src) else { continue };
            let current = GamePack::load_verified(&path, &policy, now).is_ok_and(|t| {
                t.trust.signer() == Some(self.key) && !signature_due(t.expires_at, now)
            });
            here.insert(pack.id, (sha256_hex(&bytes), current));
        }
        let busy: std::collections::HashSet<String> =
            agent.list().await?.into_iter().map(|i| i.game_id).collect();
        for e in &read.index.packs {
            let existed = match here.get(&e.id) {
                None => false,
                Some((sha, current)) if *sha == e.sha256 && *current => continue,
                Some(_) => true,
            };
            if existed && busy.contains(&e.id) {
                status.waiting.push(e.id.clone());
                continue;
            }
            let request = ImportRequest {
                toml: None,
                url: None,
                repository: Some(self.url.clone()),
                id: Some(e.id.clone()),
                replace: true,
            };
            match import(&request, dir, &policy, now, |id| agent.config().runtime_for(id).is_some()).await {
                Ok(_) if existed => status.updated.push(e.id.clone()),
                Ok(_) => status.added.push(e.id.clone()),
                Err(err) => {
                    status.error.get_or_insert_with(|| format!("{}: {err}", e.id));
                }
            }
        }
        Ok(())
    }
}
