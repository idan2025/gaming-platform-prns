//! Installing game packs from a repository (`PLAN.md` §13.3 step 5,
//! ROADMAP item 8): the launcher half of `game_bridge::repo`.
//!
//! **Opt-in**: a launcher ships with no repository, and one is a URL the player
//! adds. A repository is a cache of packs and never the source of truth
//! (`DESIGN.md` §0), so nothing here works differently because a pack came
//! from one: an installed pack is a file in the player's own pack directory,
//! loaded and classified exactly like the shipped ones.
//!
//! Rules a later change could quietly break:
//! - **Hash, then parse, then verify, then write.** A pack whose bytes are not
//!   the index's `sha256` is refused before it is read as TOML; a pack whose
//!   signature does not verify is refused, never installed as unsigned
//!   (`signing.rs`'s rule, at the moment of install as at load).
//! - **The installed file is named from the pack's id**, never the index's
//!   `file`, and the id inside the pack must be the one the index listed.
//! - **A shipped game is never replaced from a repository.** The bundled pack
//!   is part of the release the player installed; a repository pack with the
//!   same id is listed as shipped and cannot be installed over it.
//! - **The launcher shows a tier, it does not gate on one** — as at load
//!   (`Launcher::from_pack_dir`). A pack points a client somewhere and runs
//!   nothing here, and its `[launch]` block is the typed template of
//!   `PLAN.md` §13.1, whatever repository it came from.

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{anyhow, bail, Context, Result};
use game_bridge::pack::TrustedPack;
use game_bridge::repo::{self, beside, check_repo_url, RepoIndex, MAX_PACK_BYTES};
use game_bridge::signing::{verify_pack, PackSignature, PackTrust, SIGNATURE_SUFFIX};
use game_bridge::GamePack;
use serde::Serialize;

use crate::Launcher;

use game_bridge::repo::MAX_SIG_BYTES;

/// One repository, as the Packs view shows it.
#[derive(Debug, Clone, Serialize)]
pub struct PackRepoView {
    pub url: String,
    /// The official repository: held to the project key, on unless the
    /// player turned it off, and the one that may update a shipped game.
    pub official: bool,
    /// Who signed its list, hex identity hash, when it is signed. Pinned when
    /// the repository was added.
    pub signer: Option<String>,
    /// What the repository calls itself, once its index was read.
    pub name: Option<String>,
    /// Why its index could not be read this time.
    pub error: Option<String>,
    pub packs: Vec<RepoPackView>,
}

/// One pack a repository lists.
#[derive(Debug, Clone, Serialize)]
pub struct RepoPackView {
    pub id: String,
    pub display_name: String,
    pub art_url: Option<String>,
    pub lan: bool,
    pub signed: bool,
    /// `"available"`, `"installed"`, `"update"` (installed, and the repository
    /// has other bytes), or `"shipped"` (this launcher has the game built in).
    pub state: String,
}

/// What an install did.
#[derive(Debug, Clone, Serialize)]
pub struct InstalledPackView {
    pub id: String,
    pub display_name: String,
    /// §11.4's words, as `GameSummary` carries them.
    pub trust: String,
    pub trust_detail: String,
    /// True when this replaced an earlier install of the same pack.
    pub replaced: bool,
}

async fn fetch(url: &str, max: usize) -> Result<Vec<u8>> {
    repo::fetch(url, max).await
}

/// The index at `url` and who signed it, if anyone: `index.json.sig` beside
/// it, under the index domain. A signature that is there and does not verify
/// is an error, never an unsigned list (`repo::read_index`).
async fn fetch_index(url: &str) -> Result<(RepoIndex, Option<String>)> {
    let read = repo::read_index(url).await?;
    Ok((read.index, read.signature.map(|(who, _)| hex::encode(who.as_bytes()))))
}

impl Launcher {
    /// Where packs installed from a repository live: `packs/` beside the
    /// settings file, so a portable launcher keeps them in its folder.
    pub(crate) fn user_pack_dir(&self) -> Option<PathBuf> {
        self.settings_path.as_ref()?.parent().map(|p| p.join("packs"))
    }

    /// Load the packs installed from repositories, after the shipped ones. A
    /// shipped id is never replaced, and a broken file is skipped with a
    /// warning, as at load.
    pub(crate) fn load_user_packs(self) -> Self {
        let Some(dir) = self.user_pack_dir() else { return self };
        if !dir.is_dir() {
            return self;
        }
        let policy = crate::trust_policy();
        match GamePack::load_dir_verified(&dir, &policy, SystemTime::now()) {
            Ok(loaded) => {
                for (path, e) in &loaded.errors {
                    tracing::warn!(path = %path, error = %e, "skipping an installed game pack");
                }
                let mut packs = self.packs.write().unwrap_or_else(|e| e.into_inner());
                for p in loaded.packs {
                    let official = matches!(p.trust, PackTrust::FirstParty { .. });
                    match packs.iter().position(|have| have.pack.id == p.pack.id) {
                        // The official repository's newer copy of a shipped
                        // game: the same project, signed by its key.
                        Some(i) if official => packs[i] = p,
                        Some(_) => {
                            tracing::warn!(id = %p.pack.id, "an installed pack has a shipped game's id; the shipped one stays")
                        }
                        None => packs.push(p),
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not read installed packs"),
        }
        self
    }

    pub(crate) fn is_user_pack(&self, p: &TrustedPack) -> bool {
        self.user_pack_dir()
            .is_some_and(|dir| std::path::Path::new(&p.file).parent() == Some(dir.as_path()))
    }

    /// The repositories this launcher reads: the official one first, unless
    /// the player turned it off, then the ones they added.
    pub async fn pack_repos(&self) -> Vec<String> {
        let settings = self.settings.lock().await;
        let mut out = Vec::new();
        if !settings.official_packs_off {
            out.push(self.official.0.clone());
        }
        out.extend(settings.pack_repos.iter().filter(|u| **u != self.official.0).cloned());
        out
    }

    /// The official repository, or the seed standing in for it now.
    pub(crate) fn is_official(&self, url: &str) -> bool {
        url == self.official.0
            || self.official_mirror.lock().unwrap_or_else(|e| e.into_inner()).as_deref() == Some(url)
    }

    /// Repositories seeded on the mesh that this launcher heard
    /// (`game_bridge::repo_mesh`), newest first.
    pub async fn heard_pack_repos(&self) -> Vec<HeardRepoView> {
        let added = self.pack_repos().await;
        let official_key = self.official.1.clone();
        game_bridge::repo_mesh::heard()
            .into_iter()
            .map(|h| {
                let signer = h.announce.signer.map(|s| hex::encode(s.as_bytes()));
                let address = h.address();
                let index = check_repo_url(&address).unwrap_or_default();
                HeardRepoView {
                    official: signer.as_deref() == Some(official_key.as_str()),
                    added: added.contains(&index),
                    address,
                    name: h.announce.name,
                    packs: h.announce.packs,
                    signer,
                    hops: h.hops,
                }
            })
            .collect()
    }

    /// Add a repository, after reading its index once: an address that is not
    /// a repository is refused now rather than listed as broken forever.
    pub async fn add_pack_repo(&self, url: &str) -> Result<PackRepoView> {
        let url = check_repo_url(url)?;
        if self.is_official(&url) {
            let index = self.pinned_index(&url).await?;
            let mut settings = self.settings.lock().await;
            settings.official_packs_off = false;
            self.persist(&settings)?;
            drop(settings);
            return Ok(self.repo_view(url, Ok(index)));
        }
        let (index, signer) = fetch_index(&url).await?;
        {
            let mut settings = self.settings.lock().await;
            if !settings.pack_repos.contains(&url) {
                settings.pack_repos.push(url.clone());
                // Pinned at add, the one moment the player chose this list.
                match &signer {
                    Some(key) => settings.pack_repo_keys.insert(url.clone(), key.clone()),
                    None => settings.pack_repo_keys.remove(&url),
                };
                self.persist(&settings)?;
            }
        }
        Ok(self.repo_view(url, Ok((index, signer))))
    }

    /// The index at `url`, held to the key pinned for it, if one was.
    async fn pinned_index(&self, url: &str) -> Result<(RepoIndex, Option<String>)> {
        // The official list is held to the project key, never to whatever
        // key it was first seen with.
        let pinned = if self.is_official(url) {
            Some(self.official.1.clone())
        } else {
            self.settings.lock().await.pack_repo_keys.get(url).cloned()
        };
        let (index, signer) = fetch_index(url).await?;
        if let Some(pin) = pinned {
            if signer.as_deref() != Some(pin.as_str()) {
                bail!(
                    "this repository's list is {} the key it was added with ({}…). \
                     Someone else may control it now; remove it and add it again only if you trust the change",
                    if signer.is_some() { "signed by another key than" } else { "no longer signed by" },
                    &pin[..8.min(pin.len())]
                );
            }
        }
        Ok((index, signer))
    }

    pub async fn remove_pack_repo(&self, url: &str) -> Result<()> {
        let mut settings = self.settings.lock().await;
        if self.is_official(url) {
            settings.official_packs_off = true;
            return self.persist(&settings);
        }
        settings.pack_repos.retain(|u| u != url);
        settings.pack_repo_keys.remove(url);
        self.persist(&settings)?;
        Ok(())
    }

    /// Every added repository and what it lists, read now.
    pub async fn browse_pack_repos(&self) -> Vec<PackRepoView> {
        let mut out = Vec::new();
        for url in self.pack_repos().await {
            let index = self.pinned_index(&url).await;
            out.push(self.repo_view(url, index));
        }
        out
    }

    fn repo_view(&self, url: String, index: Result<(RepoIndex, Option<String>)>) -> PackRepoView {
        let (index, signer) = match index {
            Ok(i) => i,
            Err(e) => {
                return PackRepoView {
                    official: self.is_official(&url),
                    url,
                    signer: None,
                    name: None,
                    error: Some(crate::error_text(e)),
                    packs: Vec::new(),
                }
            }
        };
        let packs = self.packs();
        let official = self.is_official(&url);
        let rows = index
            .packs
            .iter()
            .map(|e| {
                let have = packs.iter().find(|p| p.pack.id == e.id);
                let differs = |p: &TrustedPack| {
                    std::fs::read(&p.file).map(|b| repo::sha256_hex(&b) != e.sha256).unwrap_or(true)
                };
                let first_party = |p: &TrustedPack| matches!(p.trust, PackTrust::FirstParty { .. });
                let state = match have {
                    None => "available",
                    // Only the official repository updates a shipped game:
                    // when its bytes moved on, or to take the signed copy.
                    Some(p) if !self.is_user_pack(p) && official && (differs(p) || !first_party(p)) => {
                        "update"
                    }
                    Some(p) if !self.is_user_pack(p) => "shipped",
                    // An official copy whose signature runs out within the
                    // refresh window is taken again before it can hide the game.
                    Some(p)
                        if official
                            && first_party(p)
                            && repo::signature_due(p.expires_at, SystemTime::now()) =>
                    {
                        "update"
                    }
                    Some(p) => match std::fs::read(&p.file) {
                        Ok(bytes) if repo::sha256_hex(&bytes) == e.sha256 => "installed",
                        _ => "update",
                    },
                };
                RepoPackView {
                    id: e.id.clone(),
                    display_name: e.display_name.clone(),
                    art_url: e.art.clone(),
                    lan: e.lan,
                    signed: e.signed,
                    state: state.to_string(),
                }
            })
            .collect();
        PackRepoView { official, url, signer, name: Some(index.name), error: None, packs: rows }
    }

    /// Install (or update) pack `id` from the repository at `url`.
    pub async fn install_repo_pack(&self, url: &str, id: &str) -> Result<InstalledPackView> {
        let url = check_repo_url(url)?;
        let dir = self
            .user_pack_dir()
            .ok_or_else(|| anyhow!("this launcher has no settings folder to keep packs in"))?;
        if let Some(p) = self.packs().iter().find(|p| p.pack.id == id) {
            if !self.is_user_pack(p) && !self.is_official(&url) {
                bail!(
                    "{} ships with Lanthorn; a repository cannot replace it",
                    p.pack.display_name
                );
            }
        }
        let (index, _) = self.pinned_index(&url).await?;
        let entry = index.entry(id).ok_or_else(|| anyhow!("{url} does not list {id:?}"))?;

        // Hash before anything reads the bytes as a pack.
        let bytes = fetch(&beside(&url, &entry.file), MAX_PACK_BYTES).await?;
        if repo::sha256_hex(&bytes) != entry.sha256 {
            bail!("the pack's bytes are not the ones the repository lists; nothing was installed");
        }
        let src = std::str::from_utf8(&bytes).map_err(|_| anyhow!("the pack is not UTF-8 text"))?;
        let pack =
            GamePack::parse(src).map_err(|e| anyhow!("that is not a usable game pack: {e}"))?;
        if pack.id != entry.id {
            bail!("the repository lists {:?} but the pack inside is {:?}", entry.id, pack.id);
        }
        // A pack no honest author needs is refused here too, for a repository
        // that never ran `game-bridge review` (`review.rs`).
        let others: Vec<GamePack> =
            self.packs().iter().filter(|p| p.pack.id != pack.id).map(|p| p.pack.clone()).collect();
        if let Some(why) = game_bridge::review::refusal(&game_bridge::review::review(&pack, &others)) {
            bail!("{why}; nothing was installed");
        }

        let policy = crate::trust_policy();
        let now = SystemTime::now();
        let sig = if entry.signed {
            let sig_bytes =
                fetch(&beside(&url, &format!("{}{SIGNATURE_SUFFIX}", entry.file)), MAX_SIG_BYTES)
                    .await?;
            let text = String::from_utf8(sig_bytes)
                .map_err(|_| anyhow!("the pack's signature is not text"))?;
            let sig = PackSignature::parse(&text)
                .map_err(|e| anyhow!("the pack's signature is unreadable: {e}"))?;
            Some((sig, text))
        } else {
            None
        };
        // A signature that fails is an error, never a quiet unsigned install.
        verify_pack(&bytes, sig.as_ref().map(|(s, _)| s), &policy, now).map_err(|e| {
            anyhow!("the pack's signature does not hold: {e}; nothing was installed")
        })?;

        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{}.toml", pack.id));
        let sig_path = dir.join(format!("{}.toml{SIGNATURE_SUFFIX}", pack.id));
        let replaced = path.exists();
        // Signature first, pack last: until the pack is renamed into place the
        // loader finds nothing new, and a stale signature never sits beside a
        // pack it was not made for.
        match &sig {
            Some((_, text)) => write_beside(&sig_path, text.as_bytes())?,
            None => {
                let _ = std::fs::remove_file(&sig_path);
            }
        }
        write_beside(&path, &bytes)?;

        let trusted = GamePack::load_verified(&path, &policy, now)
            .map_err(|e| anyhow!("the installed pack would not load: {e}"))?;
        let view = InstalledPackView {
            id: trusted.pack.id.clone(),
            display_name: trusted.pack.display_name.clone(),
            trust: trusted.trust.label().to_string(),
            trust_detail: trusted.trust.explanation().to_string(),
            replaced,
        };
        let mut packs = self.packs.write().unwrap_or_else(|e| e.into_inner());
        packs.retain(|p| p.pack.id != trusted.pack.id);
        packs.push(trusted);
        Ok(view)
    }

    /// Remove a pack installed from a repository. A shipped pack is refused.
    pub async fn remove_installed_pack(&self, id: &str) -> Result<()> {
        let official_off = self.settings.lock().await.official_packs_off;
        let file = {
            let packs = self.packs();
            let p =
                packs.iter().find(|p| p.pack.id == id).ok_or_else(|| anyhow!("no pack {id:?}"))?;
            if !self.is_user_pack(p) {
                bail!("{} ships with Lanthorn and cannot be removed", p.pack.display_name);
            }
            if matches!(p.trust, PackTrust::FirstParty { .. }) && !official_off {
                bail!(
                    "{} comes from the official repository and would come straight back. \
                     Turn the official repository off first to stop taking its packs",
                    p.pack.display_name
                );
            }
            PathBuf::from(&p.file)
        };
        let mut sig = file.clone().into_os_string();
        sig.push(SIGNATURE_SUFFIX);
        std::fs::remove_file(&file).with_context(|| format!("removing {}", file.display()))?;
        let _ = std::fs::remove_file(PathBuf::from(sig));
        self.packs.write().unwrap_or_else(|e| e.into_inner()).retain(|p| p.pack.id != id);
        Ok(())
    }
}

/// A seed heard on the mesh, as the Games page lists it.
#[derive(Debug, Clone, Serialize)]
pub struct HeardRepoView {
    /// `rns://<destination>/`, what Add takes.
    pub address: String,
    /// What it calls itself. Its words: shown, never trusted.
    pub name: String,
    pub packs: u16,
    /// Who it says signed its list. A hint: the signature is checked on read.
    pub signer: Option<String>,
    /// It says it carries the official packs (the project key's list).
    pub official: bool,
    /// Already one of this launcher's repositories.
    pub added: bool,
    pub hops: u8,
}

/// What the last look at the official repository did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OfficialSyncView {
    /// Whether the official repository is on.
    pub on: bool,
    /// When it was last read, unix seconds; `None` before the first time.
    pub checked_unix: Option<u64>,
    /// Games that arrived, and games whose pack was updated, that time.
    pub added: Vec<String>,
    pub updated: Vec<String>,
    /// Why it could not be read, or a pack not taken: offline is normal.
    pub error: Option<String>,
    /// Where it was read from: the official address, or a seed on the mesh
    /// standing in for it.
    pub source: Option<String>,
}

impl Launcher {
    /// Take every pack the official repository lists that this launcher does
    /// not have, or has other bytes of, through the same install as a click
    /// (`install_repo_pack`: digest, review, signature). A launcher with the
    /// official repository off reads nothing. Never fails loudly: offline is
    /// a normal state, and the view says what happened.
    pub async fn sync_official_packs(&self) -> OfficialSyncView {
        let on = !self.settings.lock().await.official_packs_off;
        let mut view = OfficialSyncView { on, ..Default::default() };
        if !on {
            *self.official_sync.lock().unwrap_or_else(|e| e.into_inner()) = view.clone();
            return view;
        }
        view.checked_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());
        let mut url = self.official.0.clone();
        *self.official_mirror.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let mut index = self.pinned_index(&url).await;
        if index.is_err() {
            // No internet, or no GitHub: a seed on the mesh that says it
            // carries the official list is asked, and held to the same key.
            for h in self.heard_pack_repos().await.into_iter().filter(|h| h.official) {
                let Ok(seed) = check_repo_url(&h.address) else { continue };
                *self.official_mirror.lock().unwrap_or_else(|e| e.into_inner()) = Some(seed.clone());
                let read = self.pinned_index(&seed).await;
                if read.is_ok() {
                    url = seed;
                    index = read;
                    break;
                }
                *self.official_mirror.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
        view.source = Some(url.clone());
        match index {
            Err(e) => view.error = Some(crate::error_text(e)),
            Ok((index, _)) => {
                let repo = self.repo_view(url.clone(), Ok((index, None)));
                for p in repo.packs.iter().filter(|p| p.state == "available" || p.state == "update") {
                    match self.install_repo_pack(&url, &p.id).await {
                        Ok(done) if done.replaced || p.state == "update" => view.updated.push(p.id.clone()),
                        Ok(_) => view.added.push(p.id.clone()),
                        Err(e) => {
                            view.error.get_or_insert_with(|| format!("{}: {}", p.id, crate::error_text(e)));
                        }
                    }
                }
            }
        }
        *self.official_sync.lock().unwrap_or_else(|e| e.into_inner()) = view.clone();
        view
    }

    pub async fn official_packs_on(&self) -> bool {
        !self.settings.lock().await.official_packs_off
    }

    /// Turn the official repository on or off. Off reads nothing from it;
    /// what it installed stays.
    pub async fn set_official_packs(&self, on: bool) -> Result<()> {
        let mut settings = self.settings.lock().await;
        settings.official_packs_off = !on;
        self.persist(&settings)?;
        drop(settings);
        self.official_sync.lock().unwrap_or_else(|e| e.into_inner()).on = on;
        Ok(())
    }

    /// What the last [`sync_official_packs`](Self::sync_official_packs) did.
    pub fn official_sync(&self) -> OfficialSyncView {
        self.official_sync.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Point the official repository somewhere else, for a test that signed
    /// its own: a release always reads the constants.
    #[doc(hidden)]
    pub fn with_official_repo(self, index_url: &str, key_hex: &str) -> Self {
        Self { official: std::sync::Arc::new((index_url.to_string(), key_hex.to_string())), ..self }
    }
}

fn write_beside(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("pack");
    let staging = path.with_file_name(format!(".{name}.partial"));
    std::fs::write(&staging, bytes).with_context(|| format!("writing {}", staging.display()))?;
    std::fs::rename(&staging, path).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        anyhow!("writing {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_repository_address_is_https_or_this_machine_and_names_its_index() {
        assert_eq!(
            check_repo_url("https://example.org/packs/").unwrap(),
            "https://example.org/packs/index.json"
        );
        assert_eq!(
            check_repo_url("https://example.org/p/index.json").unwrap(),
            "https://example.org/p/index.json"
        );
        assert!(check_repo_url("http://127.0.0.1:8000").is_ok());
        // A folder on this machine is one too: a mirror kept locally.
        #[cfg(unix)]
        assert!(check_repo_url("file:///srv/packs/").unwrap().ends_with("/srv/packs/index.json"));
        for bad in [
            "http://example.org/packs",
            "ftp://x/y",
            "https://x/y?a=b",
            "https://x/y#z",
            "http://localhost.evil.org/",
        ] {
            assert!(check_repo_url(bad).is_err(), "{bad}");
        }
        assert_eq!(beside("https://x/a/index.json", "b.toml"), "https://x/a/b.toml");
    }

    /// Serve `dir` over plain HTTP on loopback, one request per connection.
    fn serve(dir: std::path::PathBuf) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let _ = reader.read_line(&mut line);
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/');
                let reply = match std::fs::read(dir.join(path)) {
                    Ok(body) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(&body);
                        r
                    }
                    Err(_) => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&reply);
            }
        });
        port
    }

    fn sign_into(dir: &std::path::Path, file: &str, bytes: &[u8]) {
        let key = prns_core::identity::PrivateIdentityMaterial::from_slice(&[7; 64]).unwrap();
        let sig = PackSignature::sign(
            bytes,
            &key,
            SystemTime::now() - Duration::from_secs(60),
            Duration::from_secs(86_400),
        )
        .unwrap();
        std::fs::write(dir.join(format!("{file}{SIGNATURE_SUFFIX}")), sig.to_toml()).unwrap();
    }

    fn build(dir: &std::path::Path) {
        let index = RepoIndex::build(dir, "Test packs").unwrap();
        std::fs::write(dir.join(repo::INDEX_FILE), serde_json::to_vec(&index).unwrap()).unwrap();
    }

    fn state_of(view: &PackRepoView, id: &str) -> String {
        view.packs.iter().find(|p| p.id == id).map(|p| p.state.clone()).unwrap_or_default()
    }

    /// The whole path: add a repository, install an unsigned and a signed
    /// pack, refuse bytes that are not the listed ones and a signature that
    /// does not hold (leaving what was installed alone), see and take an
    /// update, keep it all across a restart, and remove it. A shipped game is
    /// neither installed over nor removed.
    #[tokio::test(flavor = "multi_thread")]
    async fn packs_install_from_a_repository_only_as_listed_and_signed() {
        let packs_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
        let served = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for f in ["half-life.toml", "openttd.toml", "sven-coop.toml"] {
            std::fs::copy(packs_src.join(f), served.path().join(f)).unwrap();
        }
        let openttd = std::fs::read(served.path().join("openttd.toml")).unwrap();
        sign_into(served.path(), "openttd.toml", &openttd);
        build(served.path());
        let port = serve(served.path().to_path_buf());
        let url = format!("http://127.0.0.1:{port}/");
        let settings = home.path().join("launcher.json");
        // These are about repositories a player adds; the official one is
        // its own test.
        std::fs::write(&settings, r#"{"official_packs_off":true}"#).unwrap();
        let launcher =
            || Launcher::new(vec![GamePack::sven_coop()]).with_settings_file(settings.clone());
        let l = launcher();

        let view = l.add_pack_repo(&url).await.unwrap();
        assert_eq!(view.name.as_deref(), Some("Test packs"));
        assert_eq!(state_of(&view, "half-life"), "available");
        assert_eq!(state_of(&view, "sven-coop"), "shipped");
        assert!(l.add_pack_repo("http://127.0.0.1:1/").await.is_err(), "not a repository");

        let hl = l.install_repo_pack(&url, "half-life").await.unwrap();
        assert_eq!(hl.trust, "unsigned local");
        let ottd = l.install_repo_pack(&url, "openttd").await.unwrap();
        assert_eq!(ottd.trust, "signed by an unknown key");
        assert!(l.list_games().iter().any(|g| g.id == "half-life"));
        assert!(l.install_repo_pack(&url, "sven-coop").await.is_err(), "a shipped game stays");
        let installed = home.path().join("packs/half-life.toml");
        let before = std::fs::read(&installed).unwrap();

        // Other bytes than the index lists: refused, and the install untouched.
        std::fs::write(
            served.path().join("half-life.toml"),
            [&before[..], b"\n# changed\n"].concat(),
        )
        .unwrap();
        let e = l.install_repo_pack(&url, "half-life").await.unwrap_err().to_string();
        assert!(e.contains("not the ones the repository lists"), "{e}");
        assert_eq!(std::fs::read(&installed).unwrap(), before);

        // The author publishes it: an update, then installed.
        build(served.path());
        let view = &l.browse_pack_repos().await[0];
        assert_eq!(state_of(view, "half-life"), "update");
        assert!(l.install_repo_pack(&url, "half-life").await.unwrap().replaced);
        assert_eq!(state_of(&l.browse_pack_repos().await[0], "half-life"), "installed");

        // A repository that never ran a review lists a pack that runs a config
        // file on the player's game: refused here, the install untouched.
        let good = std::fs::read(&installed).unwrap();
        let hostile = String::from_utf8(good.clone())
            .unwrap()
            .replace("args = [\"+connect {address}\"", "args = [\"+exec evil.cfg\", \"+connect {address}\"");
        assert_ne!(hostile.as_bytes(), &good[..], "half-life's launch args moved");
        std::fs::write(served.path().join("half-life.toml"), &hostile).unwrap();
        let index_path = served.path().join(repo::INDEX_FILE);
        let mut index = RepoIndex::parse(&std::fs::read(&index_path).unwrap()).unwrap();
        index.packs.iter_mut().find(|e| e.id == "half-life").unwrap().sha256 =
            repo::sha256_hex(hostile.as_bytes());
        std::fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
        let e = l.install_repo_pack(&url, "half-life").await.unwrap_err().to_string();
        assert!(e.contains("+exec"), "{e}");
        assert_eq!(std::fs::read(&installed).unwrap(), good);
        std::fs::write(served.path().join("half-life.toml"), &good).unwrap();
        build(served.path());

        // A signature for other bytes: refused, never installed as unsigned.
        sign_into(served.path(), "openttd.toml", b"something else");
        let e = l.install_repo_pack(&url, "openttd").await.unwrap_err().to_string();
        assert!(e.contains("does not hold"), "{e}");
        assert!(home.path().join("packs/openttd.toml.sig").exists(), "the good install stays");

        // A restart keeps the repository and the packs.
        let l = launcher();
        assert_eq!(l.pack_repos().await.len(), 1);
        let ids: Vec<_> = l.list_games().into_iter().map(|g| g.id).collect();
        assert!(ids.contains(&"half-life".into()) && ids.contains(&"openttd".into()), "{ids:?}");

        l.remove_installed_pack("half-life").await.unwrap();
        assert!(!installed.exists());
        assert!(!l.list_games().iter().any(|g| g.id == "half-life"));
        assert!(l.remove_installed_pack("sven-coop").await.is_err());
        l.remove_pack_repo(&check_repo_url(&url).unwrap()).await.unwrap();
        assert!(l.pack_repos().await.is_empty());
    }

    /// The official repository: listed first unless turned off, held to its
    /// key whatever key it was first seen with, and synced — new games
    /// arrive, a shipped game takes the signed copy, and a list signed by
    /// anyone else changes nothing. An official game cannot be removed while
    /// the repository would only send it back.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_official_repository_is_on_by_default_held_to_its_key_and_synced() {
        let packs_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
        let served = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for f in ["half-life.toml", "sven-coop.toml"] {
            std::fs::copy(packs_src.join(f), served.path().join(f)).unwrap();
            let bytes = std::fs::read(served.path().join(f)).unwrap();
            let sig = PackSignature::sign(
                &bytes,
                &prns_core::identity::PrivateIdentityMaterial::from_slice(&[9; 64]).unwrap(),
                SystemTime::now() - Duration::from_secs(60),
                Duration::from_secs(game_bridge::repo::PACK_SIGNATURE_DAYS * 86_400),
            )
            .unwrap();
            std::fs::write(served.path().join(format!("{f}{SIGNATURE_SUFFIX}")), sig.to_toml()).unwrap();
        }
        build(served.path());
        sign_index(served.path(), 9);
        let key = hex::encode(
            prns_core::identity::PrivateIdentityMaterial::from_slice(&[9; 64])
                .unwrap()
                .public()
                .identity_hash()
                .as_bytes(),
        );
        let url = check_repo_url(&served.path().to_string_lossy()).unwrap();
        let l = Launcher::new(vec![GamePack::sven_coop()])
            .with_settings_file(home.path().join("launcher.json"))
            .with_official_repo(&url, &key);
        // The key is first-party only for the real project key; a test key
        // reads as signed community at most, which is enough to see it held.
        assert_eq!(l.pack_repos().await, vec![url.clone()], "on by default, first");

        let first = l.sync_official_packs().await;
        assert!(first.error.is_none(), "{first:?}");
        assert_eq!(first.added, vec!["half-life".to_string()]);
        assert_eq!(first.updated, vec!["sven-coop".to_string()], "the shipped game takes the signed copy");
        assert!(l.list_games().iter().any(|g| g.id == "half-life"));
        let again = l.sync_official_packs().await;
        assert!(again.added.is_empty() && again.updated.is_empty(), "{again:?}");

        // Re-signed by someone else: refused, nothing touched.
        sign_index(served.path(), 4);
        let refused = l.sync_official_packs().await;
        assert!(refused.error.is_some(), "{refused:?}");

        l.remove_pack_repo(&url).await.unwrap();
        assert!(l.pack_repos().await.is_empty());
        assert!(!l.sync_official_packs().await.on);
    }

    /// No internet: the official address does not answer, and a seed on the
    /// mesh that carries the official list (signed by its key) stands in for
    /// it — packs arrive over Reticulum, held to the same key.
    #[tokio::test(flavor = "multi_thread")]
    async fn without_internet_the_official_packs_come_from_a_seed_on_the_mesh() {
        let packs_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
        let served = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let k = prns_core::identity::PrivateIdentityMaterial::from_slice(&[13; 64]).unwrap();
        {
            let f = "half-life.toml";
            std::fs::copy(packs_src.join(f), served.path().join(f)).unwrap();
            let sig = PackSignature::sign(
                &std::fs::read(served.path().join(f)).unwrap(),
                &k,
                SystemTime::now() - Duration::from_secs(60),
                Duration::from_secs(game_bridge::repo::PACK_SIGNATURE_DAYS * 86_400),
            )
            .unwrap();
            std::fs::write(served.path().join(format!("{f}{SIGNATURE_SUFFIX}")), sig.to_toml()).unwrap();
        }
        build(served.path());
        sign_index(served.path(), 13);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let seed = game_bridge::repo_mesh::start_seed(game_bridge::repo_mesh::SeedArgs {
            dir: served.path().to_path_buf(),
            identity: home.path().join("seed.identity"),
            tcp: Some(format!("0.0.0.0:{port}")),
            auto: false,
            announce_every: Duration::from_secs(5),
        })
        .await
        .unwrap();
        let mut args = game_bridge::config::BrowserArgs::new();
        args.tcp = Some(format!("127.0.0.1:{port}"));
        let browser = game_bridge::BridgeSession::start_browser(args).await.unwrap();
        game_bridge::repo_mesh::set_mesh_handle(Some(browser.handle().clone()));
        let hash = seed.own_hash().unwrap();
        for _ in 0..120 {
            if game_bridge::repo_mesh::heard().iter().any(|h| h.destination == hash) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let key = hex::encode(k.public().identity_hash().as_bytes());
        let l = Launcher::new(vec![GamePack::sven_coop()])
            .with_settings_file(home.path().join("launcher.json"))
            .with_official_repo("http://127.0.0.1:1/index.json", &key);
        let heard = l.heard_pack_repos().await;
        assert!(heard.iter().any(|h| h.official && h.packs == 1), "{heard:?}");
        let done = l.sync_official_packs().await;
        assert!(done.source.as_deref().is_some_and(|s| s.starts_with("rns://")), "{done:?}");
        assert_eq!(done.added, vec!["half-life".to_string()], "{done:?}");
        assert!(l.list_games().iter().any(|g| g.id == "half-life"));
    }

    fn sign_index(dir: &std::path::Path, seed: u8) {
        let key = prns_core::identity::PrivateIdentityMaterial::from_slice(&[seed; 64]).unwrap();
        let bytes = std::fs::read(dir.join(repo::INDEX_FILE)).unwrap();
        let sig = PackSignature::sign_index(
            &bytes,
            &key,
            SystemTime::now() - Duration::from_secs(60),
            Duration::from_secs(86_400),
        )
        .unwrap();
        std::fs::write(dir.join(format!("{}{SIGNATURE_SUFFIX}", repo::INDEX_FILE)), sig.to_toml())
            .unwrap();
    }

    /// A list signed when it was added stays held to that key: signed by
    /// another, or no longer signed, it is refused, and nothing installs
    /// from it. A signature that does not hold is refused at add.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_signed_list_is_held_to_the_key_it_was_added_with() {
        let packs_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
        let served = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::copy(packs_src.join("half-life.toml"), served.path().join("half-life.toml"))
            .unwrap();
        build(served.path());
        sign_index(served.path(), 3);
        let url = format!("http://127.0.0.1:{}/", serve(served.path().to_path_buf()));
        std::fs::write(home.path().join("launcher.json"), r#"{"official_packs_off":true}"#).unwrap();
        let l = Launcher::new(vec![GamePack::sven_coop()])
            .with_settings_file(home.path().join("launcher.json"));
        let view = l.add_pack_repo(&url).await.unwrap();
        assert!(view.signer.is_some(), "a signed list says who signed it");

        sign_index(served.path(), 4);
        let view = &l.browse_pack_repos().await[0];
        assert!(view.error.as_deref().unwrap_or("").contains("another key"), "{:?}", view.error);
        assert!(l.install_repo_pack(&url, "half-life").await.is_err());

        std::fs::remove_file(served.path().join("index.json.sig")).unwrap();
        let view = &l.browse_pack_repos().await[0];
        assert!(
            view.error.as_deref().unwrap_or("").contains("no longer signed"),
            "{:?}",
            view.error
        );

        sign_index(served.path(), 3);
        assert!(l.install_repo_pack(&url, "half-life").await.is_ok(), "the pinned key again");

        // A signature for other bytes is refused when adding.
        build(served.path());
        std::fs::write(
            served.path().join("index.json"),
            b"{\"format\":1,\"name\":\"x\",\"packs\":[]}",
        )
        .unwrap();
        let other = tempfile::tempdir().unwrap();
        let l2 = Launcher::new(Vec::new()).with_settings_file(other.path().join("l.json"));
        let e = l2.add_pack_repo(&url).await.unwrap_err().to_string();
        assert!(e.contains("does not hold"), "{e}");
    }
}
