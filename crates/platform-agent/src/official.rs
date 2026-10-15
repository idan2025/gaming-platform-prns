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
//! - **A node can seed the official packs over Reticulum** (off by default):
//!   each pass mirrors them exactly into `<data_root>/mirrors/official`
//!   (`repo::mirror`, held to the project key) and serves that folder on the
//!   mesh through the node's own hub, as `game-bridge repo seed` does. A
//!   failed mirror keeps serving the last good copy, whose signatures every
//!   reader checks anyway. Seeding serves only the mirror's files.

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
    #[serde(default)]
    seed: bool,
}

/// The mirror this node seeds, under the folder a node accepts as a
/// repository (`api::node_repository_allowed`).
pub const MIRROR_DIR: &str = "mirrors/official";

/// The seed's own identity, so its address stays the same across restarts.
pub const SEED_IDENTITY: &str = "repository-seed.identity";

/// Whether this node seeds the official packs on the mesh, and how.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SeedStatus {
    pub on: bool,
    /// `rns://<destination>/`, once the seed is up.
    pub address: Option<String>,
    pub packs: Option<u16>,
    pub mirrored_unix: Option<u64>,
    pub error: Option<String>,
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
    /// Where it was read from: the official address, or a seed on the mesh.
    pub source: Option<String>,
    pub seed: SeedStatus,
}

pub struct Official {
    settings: PathBuf,
    data_root: PathBuf,
    seed: tokio::sync::Mutex<Option<game_bridge::BridgeSession>>,
    seed_last: std::sync::Mutex<SeedStatus>,
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
            data_root: data_root.to_path_buf(),
            seed: Default::default(),
            seed_last: Default::default(),
            url: url.to_string(),
            key,
            last: Default::default(),
            running: Default::default(),
        }
    }

    fn stored(&self) -> Stored {
        std::fs::read(&self.settings)
            .ok()
            .and_then(|b| serde_json::from_slice::<Stored>(&b).ok())
            .unwrap_or_default()
    }

    fn store(&self, stored: &Stored) -> std::io::Result<()> {
        if let Some(dir) = self.settings.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.settings, serde_json::to_vec(stored)?)
    }

    pub fn on(&self) -> bool {
        !self.stored().off
    }

    pub fn set_on(&self, on: bool) -> std::io::Result<()> {
        self.store(&Stored { off: !on, ..self.stored() })?;
        self.last.lock().unwrap_or_else(|e| e.into_inner()).on = on;
        Ok(())
    }

    /// Whether this node seeds the official packs on the mesh. Off unless
    /// the operator turns it on.
    pub fn seeding(&self) -> bool {
        self.stored().seed
    }

    /// Turn seeding on or off. Takes effect at the next pass; the caller
    /// runs one (`POST /packs/official/sync`, or the PUT that set it).
    pub fn set_seeding(&self, on: bool) -> std::io::Result<()> {
        self.store(&Stored { seed: on, ..self.stored() })
    }

    pub fn status(&self) -> OfficialStatus {
        let mut s = self.last.lock().unwrap_or_else(|e| e.into_inner()).clone();
        s.on = self.on();
        s.url = self.url.clone();
        s.seed = self.seed_last.lock().unwrap_or_else(|e| e.into_inner()).clone();
        s.seed.on = self.seeding();
        s
    }

    /// Mirror the official packs and keep them on the mesh, or stop doing so.
    async fn seed_pass(&self, agent: &Agent, source: Option<&str>) {
        let mut session = self.seed.lock().await;
        let mut status = self.seed_last.lock().unwrap_or_else(|e| e.into_inner()).clone();
        status.on = self.seeding();
        if !status.on {
            if let Some(mut s) = session.take() {
                s.stop().await;
                tracing::info!("stopped seeding the official packs");
            }
            *self.seed_last.lock().unwrap_or_else(|e| e.into_inner()) = SeedStatus::default();
            return;
        }
        status.error = None;
        let dir = self.data_root.join(MIRROR_DIR);
        // The source the pass read, else the official address: an exact copy
        // either way, held to the same key.
        let from = source.unwrap_or(&self.url).to_string();
        match game_bridge::repo::mirror(&from, &dir, self.key).await {
            Ok(report) => {
                status.packs = Some(report.packs.min(u16::MAX as usize) as u16);
                status.mirrored_unix = SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs());
            }
            Err(e) => status.error = Some(format!("mirroring: {e:#}")),
        }
        if dir.join(game_bridge::repo::INDEX_FILE).is_file() {
            match session.as_ref() {
                Some(s) => s.request_announce(),
                None => match self.start_seed(agent, &dir).await {
                    Ok(s) => {
                        status.address = s.own_hash().map(|h| format!("rns://{}/", hex::encode(h.as_bytes())));
                        tracing::info!(address = ?status.address, "seeding the official packs on the mesh");
                        *session = Some(s);
                    }
                    Err(e) => {
                        status.error.get_or_insert_with(|| format!("seeding: {e:#}"));
                    }
                },
            }
        }
        *self.seed_last.lock().unwrap_or_else(|e| e.into_inner()) = status;
    }

    async fn start_seed(&self, agent: &Agent, dir: &Path) -> anyhow::Result<game_bridge::BridgeSession> {
        let port = agent
            .mesh()
            .hub_port()
            .await
            .map_err(|e| anyhow::anyhow!("this node is not on the mesh: {e}"))?;
        game_bridge::repo_mesh::start_seed(game_bridge::repo_mesh::SeedArgs {
            dir: dir.to_path_buf(),
            identity: self.data_root.join(SEED_IDENTITY),
            tcp: None,
            auto: false,
            shared_instance: Some(port),
            announce_every: game_bridge::repo_mesh::ANNOUNCE_EVERY,
        })
        .await
    }

    /// One pass: take what the official list has that `dir` lacks or holds
    /// older, then reload the agent's packs if anything changed.
    pub async fn sync(&self, agent: &Agent, dir: &Path) -> OfficialStatus {
        let _one = self.running.lock().await;
        let mut status = OfficialStatus { on: self.on(), url: self.url.clone(), ..Default::default() };
        if !status.on {
            self.seed_pass(agent, None).await;
            *self.last.lock().unwrap_or_else(|e| e.into_inner()) = status.clone();
            return self.status();
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
        self.seed_pass(agent, status.source.as_deref()).await;
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = status.clone();
        self.status()
    }

    async fn pass(
        &self,
        agent: &Agent,
        dir: &Path,
        now: SystemTime,
        status: &mut OfficialStatus,
    ) -> anyhow::Result<()> {
        // The official address first; with no internet, a seed on the mesh
        // that says it carries the same list, held to the same key.
        let mut sources = vec![self.url.clone()];
        sources.extend(
            game_bridge::repo_mesh::heard()
                .into_iter()
                .filter(|h| h.announce.signer == Some(self.key))
                .map(|h| h.address()),
        );
        let mut found = None;
        let mut last_error = None;
        for source in sources {
            let attempt = async {
                let read = read_index(&check_repo_url(&source)?).await?;
                match &read.signature {
                    Some((who, _)) if *who == self.key => Ok(read),
                    _ => Err(anyhow::anyhow!(
                        "the official list is not signed by the project key; nothing was taken"
                    )),
                }
            };
            match attempt.await {
                Ok(read) => {
                    found = Some((source, read));
                    break;
                }
                Err(e) => last_error = Some(e),
            }
        }
        let Some((source, read)) = found else {
            return Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no source for the official packs")));
        };
        status.source = Some(source.clone());
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
                repository: Some(source.clone()),
                id: Some(e.id.clone()),
                replace: true,
                pinned_key: Some(self.key),
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
