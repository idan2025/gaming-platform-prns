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
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use game_bridge::pack::TrustedPack;
use game_bridge::repo::{self, beside, check_repo_url, RepoIndex, MAX_INDEX_BYTES, MAX_PACK_BYTES};
use game_bridge::signing::{verify_pack, PackSignature, TrustPolicy, SIGNATURE_SUFFIX};
use game_bridge::GamePack;
use serde::Serialize;

use crate::Launcher;

/// How long one fetch may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Largest signature file fetched. One is a few hundred bytes.
const MAX_SIG_BYTES: usize = 16 * 1024;

/// One repository, as the Packs view shows it.
#[derive(Debug, Clone, Serialize)]
pub struct PackRepoView {
    pub url: String,
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
    // The shell's updater also uses rustls with ring; whichever installs the
    // provider first wins, and the other's attempt is a harmless error.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder().timeout(FETCH_TIMEOUT).build()?;
    let mut response = client.get(url).send().await.with_context(|| format!("fetching {url}"))?;
    if !response.status().is_success() {
        bail!("{url} answered {}", response.status());
    }
    if response.content_length().is_some_and(|n| n as usize > max) {
        bail!("{url} is over {max} bytes");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.with_context(|| format!("reading {url}"))? {
        body.extend_from_slice(&chunk);
        if body.len() > max {
            bail!("{url} is over {max} bytes");
        }
    }
    Ok(body)
}

async fn fetch_index(url: &str) -> Result<RepoIndex> {
    let bytes = fetch(url, MAX_INDEX_BYTES).await?;
    Ok(RepoIndex::parse(&bytes)?)
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
        let policy = TrustPolicy::allowing_unsigned();
        match GamePack::load_dir_verified(&dir, &policy, SystemTime::now()) {
            Ok(loaded) => {
                for (path, e) in &loaded.errors {
                    tracing::warn!(path = %path, error = %e, "skipping an installed game pack");
                }
                let mut packs = self.packs.write().unwrap_or_else(|e| e.into_inner());
                for p in loaded.packs {
                    if packs.iter().any(|have| have.pack.id == p.pack.id) {
                        tracing::warn!(id = %p.pack.id, "an installed pack has a shipped game's id; the shipped one stays");
                    } else {
                        packs.push(p);
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

    pub async fn pack_repos(&self) -> Vec<String> {
        self.settings.lock().await.pack_repos.clone()
    }

    /// Add a repository, after reading its index once: an address that is not
    /// a repository is refused now rather than listed as broken forever.
    pub async fn add_pack_repo(&self, url: &str) -> Result<PackRepoView> {
        let url = check_repo_url(url)?;
        let index = fetch_index(&url).await?;
        {
            let mut settings = self.settings.lock().await;
            if !settings.pack_repos.contains(&url) {
                settings.pack_repos.push(url.clone());
                self.persist(&settings)?;
            }
        }
        Ok(self.repo_view(url, Ok(index)))
    }

    pub async fn remove_pack_repo(&self, url: &str) -> Result<()> {
        let mut settings = self.settings.lock().await;
        settings.pack_repos.retain(|u| u != url);
        self.persist(&settings)?;
        Ok(())
    }

    /// Every added repository and what it lists, read now.
    pub async fn browse_pack_repos(&self) -> Vec<PackRepoView> {
        let mut out = Vec::new();
        for url in self.pack_repos().await {
            let index = fetch_index(&url).await;
            out.push(self.repo_view(url, index));
        }
        out
    }

    fn repo_view(&self, url: String, index: Result<RepoIndex>) -> PackRepoView {
        let index = match index {
            Ok(i) => i,
            Err(e) => {
                return PackRepoView {
                    url,
                    name: None,
                    error: Some(crate::error_text(e)),
                    packs: Vec::new(),
                }
            }
        };
        let packs = self.packs();
        let rows = index
            .packs
            .iter()
            .map(|e| {
                let have = packs.iter().find(|p| p.pack.id == e.id);
                let state = match have {
                    None => "available",
                    Some(p) if !self.is_user_pack(p) => "shipped",
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
        PackRepoView { url, name: Some(index.name), error: None, packs: rows }
    }

    /// Install (or update) pack `id` from the repository at `url`.
    pub async fn install_repo_pack(&self, url: &str, id: &str) -> Result<InstalledPackView> {
        let url = check_repo_url(url)?;
        let dir = self
            .user_pack_dir()
            .ok_or_else(|| anyhow!("this launcher has no settings folder to keep packs in"))?;
        if let Some(p) = self.packs().iter().find(|p| p.pack.id == id) {
            if !self.is_user_pack(p) {
                bail!(
                    "{} ships with Lanthorn; a repository cannot replace it",
                    p.pack.display_name
                );
            }
        }
        let index = fetch_index(&url).await?;
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

        let policy = TrustPolicy::allowing_unsigned();
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
        let file = {
            let packs = self.packs();
            let p =
                packs.iter().find(|p| p.pack.id == id).ok_or_else(|| anyhow!("no pack {id:?}"))?;
            if !self.is_user_pack(p) {
                bail!("{} ships with Lanthorn and cannot be removed", p.pack.display_name);
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
        for bad in [
            "http://example.org/packs",
            "file:///etc",
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
}
