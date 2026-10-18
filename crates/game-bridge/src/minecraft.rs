//! Minecraft: Java Edition builds (`MINECRAFT.md` §3).
//!
//! A **build** is a server type and a version, written as one id:
//! `vanilla-1.21.11`, `paper-26.3`, `fabric-26.3-0.20.0`,
//! `forge-1.21.1-52.1.0`. It is the content version of the
//! `minecraft-java` game, so each build is its own read-only copy on a node and
//! every server on it shares that copy.
//!
//! # A build id is data before it is anything else
//!
//! It names a directory, it goes into a URL, and the image reads it, so
//! [`MinecraftBuild::parse`] is an allowlist: a known type, then versions made
//! of digits and dots only. That admits every release and refuses every
//! snapshot, pre-release and release candidate, on purpose: v1 hosts releases.
//! A failing id is refused, never repaired.
//!
//! # Where builds come from
//!
//! Each type's publisher lists its versions and the digest of each download.
//! The parsers here read those listings; they fetch nothing. The node fetches,
//! only from [`HOSTS`], only over `https`, and hashes every download against
//! the digest the publisher listed before it is written anywhere a server can
//! see it. Mojang publishes SHA-1 only; everyone else here publishes SHA-256.
//! The listing arrives over TLS from the publisher itself, and that is the
//! trust: there is no pack-supplied digest to check against, because the
//! version is the player's choice, not the pack's.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Every host a node may fetch Minecraft metadata or downloads from.
pub const HOSTS: [&str; 8] = [
    "piston-meta.mojang.com",
    "piston-data.mojang.com",
    "fill.papermc.io",
    "fill-data.papermc.io",
    "meta.fabricmc.net",
    "maven.fabricmc.net",
    "files.minecraftforge.net",
    "maven.minecraftforge.net",
];

/// Mojang's list of every version.
pub const VANILLA_MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
/// Paper's list of versions.
pub const PAPER_PROJECT: &str = "https://fill.papermc.io/v3/projects/paper";
/// Fabric's list of installers.
pub const FABRIC_INSTALLERS: &str = "https://meta.fabricmc.net/v2/versions/installer";
/// Forge's recommended and latest build for each Minecraft version.
pub const FORGE_PROMOTIONS: &str = "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json";

/// The Java runtimes the `images/minecraft` image carries.
pub const JAVA_RUNTIMES: [u32; 4] = [8, 17, 21, 25];

/// Longest version string accepted. Real ones are under 12 characters.
const MAX_VERSION_LEN: usize = 24;

/// Which server software a build runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MinecraftServer {
    /// Mojang's own server.
    Vanilla,
    /// Paper: vanilla gameplay, faster, with plugin support (no plugins in v1).
    Paper,
    /// Fabric loader (no mods in v1).
    Fabric,
    /// Forge loader (no mods in v1).
    Forge,
}

impl MinecraftServer {
    pub const ALL: [MinecraftServer; 4] = [Self::Vanilla, Self::Paper, Self::Fabric, Self::Forge];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vanilla => "vanilla",
            Self::Paper => "paper",
            Self::Fabric => "fabric",
            Self::Forge => "forge",
        }
    }

    pub fn from_word(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == word)
    }

    /// Whether a build of this type also names a loader version.
    pub fn has_loader(self) -> bool {
        matches!(self, Self::Fabric | Self::Forge)
    }
}

impl fmt::Display for MinecraftServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One Minecraft build: a server type, a Minecraft version, and for Fabric and
/// Forge the loader's version.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MinecraftBuild {
    pub server: MinecraftServer,
    pub game_version: String,
    pub loader: Option<String>,
}

/// Why a build id was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    UnknownServer(String),
    /// Not `<type>-<version>`, or a loader missing or extra for the type.
    Shape(String),
    /// A version that is not digits and dots.
    Version(String),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownServer(s) => {
                write!(f, "{s:?} is not a Minecraft server type this node knows (vanilla, paper, fabric, forge)")
            }
            Self::Shape(id) => write!(
                f,
                "{id:?} is not a Minecraft build: write <type>-<version>, and for fabric and forge <type>-<version>-<loader>"
            ),
            Self::Version(v) => {
                write!(f, "{v:?} is not a release version: only digits and dots, like 1.21.11 or 26.3")
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// A release version: digits separated by single dots, at least two parts.
pub fn is_release_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    v.len() <= MAX_VERSION_LEN
        && parts.len() >= 2
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

impl MinecraftBuild {
    pub fn new(server: MinecraftServer, game_version: &str, loader: Option<&str>) -> Result<Self, BuildError> {
        let build = Self { server, game_version: game_version.to_string(), loader: loader.map(str::to_string) };
        Self::parse(&build.id())
    }

    /// Read a build id. See the module docs for why this is an allowlist.
    pub fn parse(id: &str) -> Result<Self, BuildError> {
        let parts: Vec<&str> = id.split('-').collect();
        let server = MinecraftServer::from_word(parts[0]).ok_or_else(|| BuildError::UnknownServer(parts[0].to_string()))?;
        let want = if server.has_loader() { 3 } else { 2 };
        if parts.len() != want {
            return Err(BuildError::Shape(id.to_string()));
        }
        for v in &parts[1..] {
            if !is_release_version(v) {
                return Err(BuildError::Version(v.to_string()));
            }
        }
        Ok(Self {
            server,
            game_version: parts[1].to_string(),
            loader: parts.get(2).map(|s| s.to_string()),
        })
    }

    /// The id, which is also the content version's directory name.
    pub fn id(&self) -> String {
        match &self.loader {
            Some(loader) => format!("{}-{}-{loader}", self.server, self.game_version),
            None => format!("{}-{}", self.server, self.game_version),
        }
    }
}

impl fmt::Display for MinecraftBuild {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id())
    }
}

/// Whether `url` is `https` on one of [`HOSTS`], with no credentials or port.
pub fn host_allowed(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.contains('@') && !host.contains(':') && HOSTS.contains(&host)
}

/// A digest a publisher lists for a download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Digest {
    Sha1(String),
    Sha256(String),
}

impl Digest {
    /// A lowercase hex digest of the right length, or `None`.
    pub fn sha1(hex: &str) -> Option<Self> {
        is_hex(hex, 40).then(|| Self::Sha1(hex.to_ascii_lowercase()))
    }

    pub fn sha256(hex: &str) -> Option<Self> {
        is_hex(hex, 64).then(|| Self::Sha256(hex.to_ascii_lowercase()))
    }
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A file to download, and the digest it must hash to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub url: String,
    pub digest: Digest,
}

/// What the image needs to know about a build, written beside its files as
/// `lanthorn-build.txt` (one `key=value` per line, read without being run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    pub build: MinecraftBuild,
    pub java: u32,
    /// The downloaded file's name inside the copy: `server.jar` or
    /// `installer.jar`.
    pub file: &'static str,
}

impl BuildInfo {
    pub fn new(build: MinecraftBuild, java_major: u32) -> Self {
        let file = if build.server.has_loader() { "installer.jar" } else { "server.jar" };
        Self { java: java_runtime_for(java_major), build, file }
    }

    pub fn to_text(&self) -> String {
        format!(
            "build={}\nserver={}\nversion={}\nloader={}\njava={}\nfile={}\n",
            self.build.id(),
            self.build.server,
            self.build.game_version,
            self.build.loader.as_deref().unwrap_or(""),
            self.java,
            self.file
        )
    }

    pub const FILE_NAME: &'static str = "lanthorn-build.txt";
}

/// The runtime to run a version that asks for `major`: that one if the image
/// has it, else the next newer, else the newest.
pub fn java_runtime_for(major: u32) -> u32 {
    JAVA_RUNTIMES.into_iter().find(|&j| j >= major).unwrap_or(JAVA_RUNTIMES[JAVA_RUNTIMES.len() - 1])
}

/// Why a publisher's listing could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingError(pub String);

impl fmt::Display for ListingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ListingError {}

fn bad(what: impl Into<String>) -> ListingError {
    ListingError(what.into())
}

fn json(text: &str) -> Result<serde_json::Value, ListingError> {
    serde_json::from_str(text).map_err(|e| bad(format!("the listing is not JSON: {e}")))
}

/// Mojang's releases, newest first: `(version, url of its version file, that
/// file's SHA-1)`.
pub fn vanilla_releases(manifest: &str) -> Result<Vec<(String, Download)>, ListingError> {
    let v = json(manifest)?;
    let list = v["versions"].as_array().ok_or_else(|| bad("Mojang's manifest has no versions"))?;
    Ok(list
        .iter()
        .filter(|e| e["type"] == "release")
        .filter_map(|e| {
            let id = e["id"].as_str()?;
            let url = e["url"].as_str()?;
            let digest = Digest::sha1(e["sha1"].as_str()?)?;
            (is_release_version(id) && host_allowed(url))
                .then(|| (id.to_string(), Download { url: url.to_string(), digest }))
        })
        .collect())
}

/// From one version file: the server jar and the Java major it asks for (8
/// when the file predates the field).
pub fn vanilla_server(version_file: &str) -> Result<(Download, u32), ListingError> {
    let v = json(version_file)?;
    let server = &v["downloads"]["server"];
    let url = server["url"].as_str().ok_or_else(|| bad("this version has no server download"))?;
    let digest = Digest::sha1(server["sha1"].as_str().unwrap_or("")).ok_or_else(|| bad("the server download has no SHA-1"))?;
    if !host_allowed(url) {
        return Err(bad(format!("the server download is on a host this node does not fetch from: {url}")));
    }
    let java = v["javaVersion"]["majorVersion"].as_u64().map(|j| j as u32).unwrap_or(8);
    Ok((Download { url: url.to_string(), digest }, java))
}

/// Paper's release versions, newest first.
pub fn paper_versions(project: &str) -> Result<Vec<String>, ListingError> {
    let v = json(project)?;
    let groups = v["versions"].as_object().ok_or_else(|| bad("Paper's project has no versions"))?;
    let mut out: Vec<String> = groups
        .values()
        .filter_map(|g| g.as_array())
        .flatten()
        .filter_map(|v| v.as_str())
        .filter(|v| is_release_version(v))
        .map(str::to_string)
        .collect();
    out.sort_by(|a, b| version_key(b).cmp(&version_key(a)));
    out.dedup();
    Ok(out)
}

/// The URL of a Paper version's latest build.
pub fn paper_latest_build_url(version: &str) -> Result<String, BuildError> {
    if !is_release_version(version) {
        return Err(BuildError::Version(version.to_string()));
    }
    Ok(format!("{PAPER_PROJECT}/versions/{version}/builds/latest"))
}

/// A Paper build's server jar. Only a `STABLE` build is used: the others are
/// Paper saying the build is not ready for players.
pub fn paper_server(build: &str) -> Result<Download, ListingError> {
    let v = json(build)?;
    if v["channel"] != "STABLE" {
        return Err(bad(format!(
            "Paper has no stable build of this version yet (the latest is {})",
            v["channel"].as_str().unwrap_or("unknown")
        )));
    }
    let d = &v["downloads"]["server:default"];
    let url = d["url"].as_str().ok_or_else(|| bad("the Paper build has no server download"))?;
    let digest = Digest::sha256(d["checksums"]["sha256"].as_str().unwrap_or(""))
        .ok_or_else(|| bad("the Paper build has no SHA-256"))?;
    if !host_allowed(url) {
        return Err(bad(format!("the Paper download is on a host this node does not fetch from: {url}")));
    }
    Ok(Download { url: url.to_string(), digest })
}

/// The URL listing Fabric loader versions for one Minecraft version.
pub fn fabric_loaders_url(game_version: &str) -> Result<String, BuildError> {
    if !is_release_version(game_version) {
        return Err(BuildError::Version(game_version.to_string()));
    }
    Ok(format!("https://meta.fabricmc.net/v2/versions/loader/{game_version}"))
}

/// Fabric loader versions for one Minecraft version, newest first, as
/// `(version, stable)`.
pub fn fabric_loaders(listing: &str) -> Result<Vec<(String, bool)>, ListingError> {
    let v = json(listing)?;
    let list = v.as_array().ok_or_else(|| bad("Fabric's loader listing is not a list"))?;
    Ok(list
        .iter()
        .filter_map(|e| {
            let version = e["loader"]["version"].as_str()?;
            is_release_version(version).then(|| (version.to_string(), e["loader"]["stable"] == true))
        })
        .collect())
}

/// The newest stable Fabric installer, and where its SHA-256 is published.
pub fn fabric_installer(listing: &str) -> Result<(String, String), ListingError> {
    let v = json(listing)?;
    let list = v.as_array().ok_or_else(|| bad("Fabric's installer listing is not a list"))?;
    let e = list
        .iter()
        .find(|e| e["stable"] == true && e["version"].as_str().is_some_and(is_release_version))
        .ok_or_else(|| bad("Fabric lists no stable installer"))?;
    let url = e["url"].as_str().ok_or_else(|| bad("the Fabric installer has no URL"))?;
    if !host_allowed(url) {
        return Err(bad(format!("the Fabric installer is on a host this node does not fetch from: {url}")));
    }
    Ok((url.to_string(), format!("{url}.sha256")))
}

/// Forge builds per Minecraft release, newest Minecraft first: `(minecraft
/// version, forge version, recommended)`. Recommended comes before latest when
/// a version has both.
pub fn forge_builds(promotions: &str) -> Result<Vec<(String, String, bool)>, ListingError> {
    let v = json(promotions)?;
    let promos = v["promos"].as_object().ok_or_else(|| bad("Forge's promotions have no promos"))?;
    let mut out: Vec<(String, String, bool)> = promos
        .iter()
        .filter_map(|(key, forge)| {
            let (mc, kind) = key.rsplit_once('-')?;
            let forge = forge.as_str()?;
            (is_release_version(mc) && is_release_version(forge) && matches!(kind, "recommended" | "latest"))
                .then(|| (mc.to_string(), forge.to_string(), kind == "recommended"))
        })
        .collect();
    out.sort_by(|a, b| version_key(&b.0).cmp(&version_key(&a.0)).then(b.2.cmp(&a.2)));
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    Ok(out)
}

/// The Forge installer for a build, and where its SHA-256 is published.
pub fn forge_installer(game_version: &str, forge: &str) -> Result<(String, String), BuildError> {
    for v in [game_version, forge] {
        if !is_release_version(v) {
            return Err(BuildError::Version(v.to_string()));
        }
    }
    let full = format!("{game_version}-{forge}");
    let url = format!("https://maven.minecraftforge.net/net/minecraftforge/forge/{full}/forge-{full}-installer.jar");
    Ok((url.clone(), format!("{url}.sha256")))
}

/// A `.sha256` file as Fabric's and Forge's mavens publish it: the hex digest,
/// optionally followed by a file name.
pub fn sidecar_sha256(text: &str) -> Option<Digest> {
    Digest::sha256(text.split_whitespace().next()?)
}

/// Sort key for a release version, so `1.21.11` comes after `1.21.9`.
pub fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_id_is_a_type_and_release_versions_or_it_is_refused() {
        for (id, server, loader) in [
            ("vanilla-1.21.11", MinecraftServer::Vanilla, None),
            ("paper-26.3", MinecraftServer::Paper, None),
            ("fabric-26.3-0.20.0", MinecraftServer::Fabric, Some("0.20.0")),
            ("forge-1.21.1-52.1.0", MinecraftServer::Forge, Some("52.1.0")),
        ] {
            let b = MinecraftBuild::parse(id).unwrap();
            assert_eq!((b.server, b.loader.as_deref()), (server, loader), "{id}");
            assert_eq!(b.id(), id);
        }
        for bad in [
            "",
            "paper",
            "paper-",
            "bukkit-1.21",
            "paper-26.4-snapshot-4",
            "paper-1.21.11-rc3",
            "vanilla-24w14a",
            "paper-../1.21",
            "paper-1.21/..",
            "paper-1.21 ",
            "fabric-26.3",
            "vanilla-1.21-0.1",
            "paper-1..21",
            "paper-1",
            "Paper-1.21",
            "paper-1.21\n",
        ] {
            assert!(MinecraftBuild::parse(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn only_https_on_the_listed_hosts_is_fetched() {
        assert!(host_allowed("https://piston-data.mojang.com/v1/objects/ab/server.jar"));
        assert!(host_allowed("https://fill-data.papermc.io/v1/objects/x/paper.jar"));
        for bad in [
            "http://piston-data.mojang.com/x",
            "https://piston-data.mojang.com.evil.example/x",
            "https://evil.example/piston-data.mojang.com/x",
            "https://user@piston-data.mojang.com/x",
            "https://piston-data.mojang.com:8443/x",
            "file:///etc/passwd",
        ] {
            assert!(!host_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn mojangs_listing_yields_releases_with_their_sha1() {
        let manifest = r#"{"versions":[
            {"id":"26.4-snapshot-4","type":"snapshot","url":"https://piston-meta.mojang.com/a.json","sha1":"650b368231fa39ca6701be0f306ec5cc76beb8e0"},
            {"id":"26.3","type":"release","url":"https://piston-meta.mojang.com/b.json","sha1":"750b368231fa39ca6701be0f306ec5cc76beb8e0"},
            {"id":"26.2","type":"release","url":"https://evil.example/c.json","sha1":"850b368231fa39ca6701be0f306ec5cc76beb8e0"},
            {"id":"26.1","type":"release","url":"https://piston-meta.mojang.com/d.json","sha1":"short"}]}"#;
        let r = vanilla_releases(manifest).unwrap();
        assert_eq!(r.len(), 1, "a snapshot, an off-list host and a bad digest are all dropped");
        assert_eq!(r[0].0, "26.3");
        let version = r#"{"javaVersion":{"majorVersion":25},"downloads":{"server":{"sha1":"33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c","url":"https://piston-data.mojang.com/v1/objects/33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c/server.jar"}}}"#;
        let (d, java) = vanilla_server(version).unwrap();
        assert_eq!(java, 25);
        assert_eq!(d.digest, Digest::Sha1("33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c".into()));
        let old = r#"{"downloads":{"server":{"sha1":"33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c","url":"https://piston-data.mojang.com/s.jar"}}}"#;
        assert_eq!(vanilla_server(old).unwrap().1, 8, "a version file with no javaVersion runs on 8");
    }

    #[test]
    fn paper_uses_only_a_stable_build_with_a_sha256() {
        let project = r#"{"versions":{"26.3":["26.3","26.3-rc-3"],"1.21":["1.21.11","1.21.11-rc3","1.21.9","1.21"]}}"#;
        assert_eq!(paper_versions(project).unwrap(), ["26.3", "1.21.11", "1.21.9", "1.21"]);
        let stable = r#"{"channel":"STABLE","downloads":{"server:default":{"checksums":{"sha256":"a0a58f532f08054ea92eb916d3edb6ed3c86429d77175317304f6e995cce9968"},"url":"https://fill-data.papermc.io/v1/objects/a0/paper-26.3-205.jar"}}}"#;
        assert!(matches!(paper_server(stable).unwrap().digest, Digest::Sha256(_)));
        let beta = stable.replace("STABLE", "BETA");
        assert!(paper_server(&beta).is_err());
        assert!(paper_latest_build_url("26.3/../x").is_err());
    }

    #[test]
    fn forge_and_fabric_listings_parse_and_name_their_digest_files() {
        let promos = r#"{"promos":{"1.21.1-latest":"52.1.5","1.21.1-recommended":"52.1.0","26.3-latest":"66.0.13","1.7.10-latest_1.7.10":"10.13.4.1614"}}"#;
        let b = forge_builds(promos).unwrap();
        assert_eq!(b[0], ("26.3".to_string(), "66.0.13".to_string(), false));
        assert_eq!(b[1], ("1.21.1".to_string(), "52.1.0".to_string(), true), "recommended before latest");
        let (url, sha) = forge_installer("1.21.1", "52.1.0").unwrap();
        assert!(host_allowed(&url) && sha.ends_with(".sha256"));
        assert!(forge_installer("1.21.1", "52.1.0/../../x").is_err());

        let installers = r#"[{"url":"https://maven.fabricmc.net/net/fabricmc/fabric-installer/1.1.2/fabric-installer-1.1.2.jar","version":"1.1.2","stable":true}]"#;
        let (url, sha) = fabric_installer(installers).unwrap();
        assert_eq!(sha, format!("{url}.sha256"));
        let loaders = r#"[{"loader":{"version":"0.20.0","stable":false}},{"loader":{"version":"0.19.2","stable":true}}]"#;
        assert_eq!(fabric_loaders(loaders).unwrap(), [("0.20.0".into(), false), ("0.19.2".into(), true)]);
        assert_eq!(
            sidecar_sha256("c20c407d5ca4b119aae97847eb2fbb1d34c92969b647fbbb1de2fd36ca00738e  fabric-installer.jar\n"),
            Digest::sha256("c20c407d5ca4b119aae97847eb2fbb1d34c92969b647fbbb1de2fd36ca00738e")
        );
        assert_eq!(sidecar_sha256("<html>not found</html>"), None);
    }

    #[test]
    fn each_version_gets_a_java_the_image_carries() {
        assert_eq!(java_runtime_for(8), 8);
        assert_eq!(java_runtime_for(16), 17);
        assert_eq!(java_runtime_for(21), 21);
        assert_eq!(java_runtime_for(25), 25);
        assert_eq!(java_runtime_for(30), 25);
        let info = BuildInfo::new(MinecraftBuild::parse("forge-1.21.1-52.1.0").unwrap(), 21);
        assert_eq!(info.file, "installer.jar");
        assert!(info.to_text().contains("java=21\n"));
    }
}
