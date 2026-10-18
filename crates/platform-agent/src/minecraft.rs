//! Fetching Minecraft builds onto a node (`MINECRAFT.md` §3).
//!
//! `game_bridge::minecraft` decides what a build id may be and reads the
//! publishers' listings; this is the half that touches the network and the
//! disk. Three rules:
//!
//! - **Only [`game_bridge::minecraft::HOSTS`], only `https`**, redirects
//!   included: a redirect off the list is an error, not a hop.
//! - **The digest decides.** A download is hashed against the digest its
//!   publisher listed before it is renamed anywhere a server can see; bytes
//!   that do not match are discarded.
//! - **Stage and rename**, as every content driver does, so an interrupted
//!   download never looks installed.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use game_bridge::minecraft::{
    self as mc, BuildInfo, Digest, Download, MinecraftBuild, MinecraftServer,
};
use serde::Serialize;
use sha2::Digest as _;

use crate::content::{InstallProgress, ProvisionError};

/// Largest listing read. Mojang's manifest is ~200 KB.
const MAX_LISTING_BYTES: usize = 8 * 1024 * 1024;
/// Largest jar or installer accepted. Real ones are under 100 MB.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);
/// How long a version list is kept before the publishers are asked again.
const LIST_TTL: Duration = Duration::from_secs(60 * 60);
/// How many versions a list offers. The newest are what people pick; older
/// ones are a typed build id away.
const LIST_LEN: usize = 60;

/// One build a person can pick, as the page shows it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VersionChoice {
    /// The build id, which is what a create sends.
    pub build: String,
    pub game_version: String,
    pub loader: Option<String>,
    /// Forge's "recommended", Fabric's stable loader. Shown first.
    pub recommended: bool,
}

fn http_err(url: &str, detail: impl ToString) -> ProvisionError {
    ProvisionError::Http { url: url.to_string(), detail: detail.to_string() }
}

fn client() -> Result<reqwest::Client, ProvisionError> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("Lanthorn/", env!("CARGO_PKG_VERSION"), " (+https://github.com/idan2025/gaming-platform-prns)"))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 5 {
                attempt.error("too many redirects")
            } else if mc::host_allowed(attempt.url().as_str()) {
                attempt.follow()
            } else {
                let to = attempt.url().to_string();
                attempt.error(format!("redirected to {to}, which is not a host this node fetches Minecraft from"))
            }
        }))
        .build()
        .map_err(|e| http_err("(client)", e))
}

/// Read a listing, as text, from an allowed host.
pub async fn get_text(url: &str) -> Result<String, ProvisionError> {
    if !mc::host_allowed(url) {
        return Err(http_err(url, "not a host this node fetches Minecraft from"));
    }
    let mut response = client()?.get(url).send().await.map_err(|e| http_err(url, e))?;
    if !response.status().is_success() {
        return Err(http_err(url, format!("server answered {}", response.status())));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| http_err(url, e))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_LISTING_BYTES {
            return Err(http_err(url, "the listing is too large to be one"));
        }
    }
    String::from_utf8(body).map_err(|_| http_err(url, "the listing is not text"))
}

fn listing(e: mc::ListingError) -> ProvisionError {
    ProvisionError::Listing(e.0)
}

fn build_err(e: mc::BuildError) -> ProvisionError {
    ProvisionError::Listing(e.to_string())
}

/// Download `d` to `dest` and check it against its digest. On a mismatch the
/// file is removed and the error names both digests.
pub async fn download(d: &Download, dest: &Path, progress: Option<&InstallProgress>) -> Result<u64, ProvisionError> {
    if !mc::host_allowed(&d.url) {
        return Err(http_err(&d.url, "not a host this node fetches Minecraft from"));
    }
    let mut response = client()?.get(&d.url).send().await.map_err(|e| http_err(&d.url, e))?;
    if !response.status().is_success() {
        return Err(http_err(&d.url, format!("server answered {}", response.status())));
    }
    let mut file = std::fs::File::create(dest)?;
    let mut total = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| http_err(&d.url, e))? {
        if progress.is_some_and(InstallProgress::is_cancelled) {
            return Err(ProvisionError::Cancelled);
        }
        total += chunk.len() as u64;
        if total > MAX_DOWNLOAD_BYTES {
            return Err(ProvisionError::TooLarge { limit: MAX_DOWNLOAD_BYTES });
        }
        file.write_all(&chunk)?;
    }
    file.flush()?;
    drop(file);
    if let Err(e) = check_digest(dest, &d.digest) {
        let _ = std::fs::remove_file(dest);
        return Err(e);
    }
    Ok(total)
}

/// Hash a file with the digest's own algorithm and compare.
pub fn check_digest(path: &Path, digest: &Digest) -> Result<(), ProvisionError> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    let (actual, expected) = match digest {
        Digest::Sha1(hex) => {
            let mut h = sha1::Sha1::new();
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            (hex::encode(h.finalize()), hex)
        }
        Digest::Sha256(hex) => {
            let mut h = sha2::Sha256::new();
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            (hex::encode(h.finalize()), hex)
        }
    };
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(ProvisionError::DigestMismatch { expected: expected.clone(), actual });
    }
    Ok(())
}

/// The version file Mojang lists for `game_version`, which carries the vanilla
/// server jar and the Java major every server type on that version needs.
async fn vanilla_version_file(game_version: &str) -> Result<String, ProvisionError> {
    let manifest = get_text(mc::VANILLA_MANIFEST).await?;
    let releases = mc::vanilla_releases(&manifest).map_err(listing)?;
    let (_, file) = releases
        .iter()
        .find(|(id, _)| id == game_version)
        .ok_or_else(|| ProvisionError::Listing(format!("Mojang lists no release {game_version}")))?;
    // The version file is checked against the SHA-1 the manifest gives for
    // it, so the server jar's digest inside it is Mojang's too.
    let text = get_text(&file.url).await?;
    let mut h = sha1::Sha1::new();
    h.update(text.as_bytes());
    let actual = hex::encode(h.finalize());
    let Digest::Sha1(expected) = &file.digest else { unreachable!("Mojang lists SHA-1") };
    if &actual != expected {
        return Err(ProvisionError::DigestMismatch { expected: expected.clone(), actual });
    }
    Ok(text)
}

/// What to download for a build, and the Java major it needs.
pub async fn resolve(build: &MinecraftBuild) -> Result<(Download, u32), ProvisionError> {
    let version_file = vanilla_version_file(&build.game_version).await?;
    let (vanilla, java) = mc::vanilla_server(&version_file).map_err(listing)?;
    let download = match build.server {
        MinecraftServer::Vanilla => vanilla,
        MinecraftServer::Paper => {
            let url = mc::paper_latest_build_url(&build.game_version).map_err(build_err)?;
            mc::paper_server(&get_text(&url).await?).map_err(listing)?
        }
        MinecraftServer::Fabric => {
            let (url, sha_url) = mc::fabric_installer(&get_text(mc::FABRIC_INSTALLERS).await?).map_err(listing)?;
            sidecar(url, &sha_url).await?
        }
        MinecraftServer::Forge => {
            let loader = build.loader.as_deref().unwrap_or_default();
            let (url, sha_url) = mc::forge_installer(&build.game_version, loader).map_err(build_err)?;
            sidecar(url, &sha_url).await?
        }
    };
    Ok((download, java))
}

async fn sidecar(url: String, sha_url: &str) -> Result<Download, ProvisionError> {
    let digest = mc::sidecar_sha256(&get_text(sha_url).await?)
        .ok_or_else(|| ProvisionError::Listing(format!("{sha_url} is not a SHA-256")))?;
    Ok(Download { url, digest })
}

/// Fetch a build into `dir`, which must not exist yet: the verified file, the
/// build description the image reads, and the empty `server/` mountpoint
/// every instance's own directory is bound over.
pub async fn provision(build: &MinecraftBuild, dir: &Path, progress: Option<&InstallProgress>) -> Result<u64, ProvisionError> {
    let (file, java) = resolve(build).await?;
    let info = BuildInfo::new(build.clone(), java);
    std::fs::create_dir_all(dir.join("server"))?;
    let bytes = download(&file, &dir.join(info.file), progress).await?;
    std::fs::write(dir.join(BuildInfo::FILE_NAME), info.to_text())?;
    Ok(bytes)
}

/// Builds a person can pick for one server type, newest first, from the
/// publishers' listings. Cached for an hour per type.
#[derive(Default)]
pub struct Catalog {
    lists: Mutex<HashMap<MinecraftServer, (Instant, Vec<VersionChoice>)>>,
}

impl Catalog {
    pub async fn versions(&self, server: MinecraftServer) -> Result<Vec<VersionChoice>, ProvisionError> {
        if let Some((at, list)) = self.lists.lock().unwrap().get(&server) {
            if at.elapsed() < LIST_TTL {
                return Ok(list.clone());
            }
        }
        let list = fetch_versions(server).await?;
        self.lists.lock().unwrap().insert(server, (Instant::now(), list.clone()));
        Ok(list)
    }
}

async fn fetch_versions(server: MinecraftServer) -> Result<Vec<VersionChoice>, ProvisionError> {
    let choice = |game_version: &str, loader: Option<&str>, recommended: bool| {
        MinecraftBuild::new(server, game_version, loader).ok().map(|b| VersionChoice {
            build: b.id(),
            game_version: b.game_version,
            loader: b.loader,
            recommended,
        })
    };
    let mut out: Vec<VersionChoice> = match server {
        MinecraftServer::Vanilla => {
            let manifest = get_text(mc::VANILLA_MANIFEST).await?;
            mc::vanilla_releases(&manifest)
                .map_err(listing)?
                .iter()
                .filter_map(|(v, _)| choice(v, None, false))
                .collect()
        }
        MinecraftServer::Paper => mc::paper_versions(&get_text(mc::PAPER_PROJECT).await?)
            .map_err(listing)?
            .iter()
            .filter_map(|v| choice(v, None, false))
            .collect(),
        MinecraftServer::Fabric => {
            // Fabric's loader runs on every release it supports; the newest
            // stable loader is the one offered. Its supported game versions
            // are Fabric's own list.
            let loaders = get_text("https://meta.fabricmc.net/v2/versions/loader").await?;
            let loader = loader_from_global(&loaders)?;
            let games = get_text("https://meta.fabricmc.net/v2/versions/game").await?;
            fabric_games(&games)?.iter().filter_map(|v| choice(v, Some(&loader), true)).collect()
        }
        MinecraftServer::Forge => mc::forge_builds(&get_text(mc::FORGE_PROMOTIONS).await?)
            .map_err(listing)?
            .iter()
            .filter_map(|(v, forge, rec)| choice(v, Some(forge), *rec))
            .collect(),
    };
    out.truncate(LIST_LEN);
    Ok(out)
}

/// The newest stable loader in Fabric's global loader list.
fn loader_from_global(text: &str) -> Result<String, ProvisionError> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| ProvisionError::Listing(e.to_string()))?;
    v.as_array()
        .and_then(|list| {
            list.iter().find_map(|e| {
                let version = e["version"].as_str()?;
                (e["stable"] == true && mc::is_release_version(version)).then(|| version.to_string())
            })
        })
        .ok_or_else(|| ProvisionError::Listing("Fabric lists no stable loader".to_string()))
}

/// Fabric's stable game versions that are releases.
fn fabric_games(text: &str) -> Result<Vec<String>, ProvisionError> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| ProvisionError::Listing(e.to_string()))?;
    Ok(v.as_array()
        .map(|list| {
            list.iter()
                .filter(|e| e["stable"] == true)
                .filter_map(|e| e["version"].as_str())
                .filter(|v| mc::is_release_version(v))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_checked_with_its_publishers_algorithm() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("server.jar");
        std::fs::write(&f, b"jar bytes").unwrap();
        let sha1 = hex::encode(sha1::Sha1::digest(b"jar bytes"));
        let sha256 = hex::encode(sha2::Sha256::digest(b"jar bytes"));
        assert!(check_digest(&f, &Digest::Sha1(sha1)).is_ok());
        assert!(check_digest(&f, &Digest::Sha256(sha256)).is_ok());
        let wrong = Digest::Sha256("0".repeat(64));
        assert!(matches!(check_digest(&f, &wrong), Err(ProvisionError::DigestMismatch { .. })));
    }

    #[tokio::test]
    async fn a_host_off_the_list_is_never_fetched() {
        let off = Download { url: "https://example.com/server.jar".into(), digest: Digest::Sha1("0".repeat(40)) };
        let tmp = tempfile::tempdir().unwrap();
        let err = download(&off, &tmp.path().join("x"), None).await.unwrap_err();
        assert!(err.to_string().contains("not a host"), "{err}");
        assert!(!tmp.path().join("x").exists());
        assert!(get_text("http://piston-meta.mojang.com/x").await.is_err(), "plain http is refused");
    }

    #[test]
    fn fabric_listings_give_the_newest_stable_loader_and_release_games() {
        let loaders = r#"[{"version":"0.20.0","stable":false},{"version":"0.19.2","stable":true}]"#;
        assert_eq!(loader_from_global(loaders).unwrap(), "0.19.2");
        let games = r#"[{"version":"26.4-snapshot-4","stable":false},{"version":"26.3","stable":true},{"version":"1.21.11-rc3","stable":true}]"#;
        assert_eq!(fabric_games(games).unwrap(), ["26.3"]);
    }
}
