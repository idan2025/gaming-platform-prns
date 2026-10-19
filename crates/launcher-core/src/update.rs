//! Updating the launcher (`ROADMAP.md` §1).
//!
//! The download, its signature check and the install are Tauri's updater
//! plugin, in the shell. What lives here is what is worth testing without a
//! webview: **whether this copy can replace itself**, and the shape the UI
//! reads.
//!
//! Rules a later change could quietly break:
//! - **An update check is the internet, and the baseline is none.** The shell
//!   checks after start, off the UI's path, and a failure is "no update", never
//!   an error the player has to dismiss. The check can be turned off.
//! - **The signature decides, not the URL** — the plugin verifies every
//!   download against the public key in `tauri.conf.json` and discards it
//!   otherwise, the same rule as content archives (`content.rs`).
//! - **A portable launcher never replaces itself** (`portable.rs`: it writes
//!   nothing outside its folder, and it is somebody's folder on a USB stick).
//!   It is told a new version exists and given the release page.
//! - **Only formats the plugin can replace are offered Install**: the Windows
//!   installer build, the macOS app, and an AppImage. A `.deb`, `.rpm` or the
//!   Arch package belongs to the system's package manager; replacing its files
//!   from under it would leave the package database lying.

use serde::Serialize;

/// Where a player downloads a release by hand, for the builds that do not
/// update themselves.
pub const RELEASES_PAGE: &str = "https://github.com/idan2025/gaming-platform-prns/releases/latest";

/// How this copy of the launcher can take an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallMode {
    /// Download, verify, replace, restart.
    InPlace,
    /// Say a new version exists and link to the release page.
    LinkOnly,
}

/// What the shell knows about how this copy was installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallFacts {
    pub portable: bool,
    /// `std::env::consts::OS`.
    pub os: &'static str,
    /// Running from an AppImage: the AppImage runtime sets `APPIMAGE`.
    pub appimage: bool,
}

impl InstallFacts {
    /// The facts for the running process.
    pub fn here(portable: bool) -> Self {
        Self {
            portable,
            os: std::env::consts::OS,
            appimage: std::env::var_os("APPIMAGE").is_some(),
        }
    }
}

pub fn install_mode(f: InstallFacts) -> InstallMode {
    if f.portable {
        return InstallMode::LinkOnly;
    }
    match f.os {
        "windows" | "macos" => InstallMode::InPlace,
        "linux" if f.appimage => InstallMode::InPlace,
        _ => InstallMode::LinkOnly,
    }
}

/// What the UI shows about updates. Its keys are the frontend contract.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UpdateView {
    /// The version running now.
    pub current: String,
    /// A newer version, when one was found.
    pub available: Option<String>,
    /// Its release notes, as published.
    pub notes: Option<String>,
    /// How this copy can take it.
    pub install: InstallMode,
    /// Where to download it by hand.
    pub page: String,
    /// Whether the launcher looks for updates by itself at start.
    pub auto_check: bool,
}

impl UpdateView {
    pub fn none(current: &str, install: InstallMode, auto_check: bool) -> Self {
        Self {
            current: current.to_string(),
            available: None,
            notes: None,
            install,
            page: RELEASES_PAGE.to_string(),
            auto_check,
        }
    }
}

/// The Android build's updates (`android` below). The Tauri updater has no
/// Android side, and an app cannot replace its own APK: the system's package
/// installer does that, and refuses an APK signed by any other key than the one
/// installed. So the check finds the newest Android release, and the player
/// downloads its APK and opens it.
pub mod android {
    use serde::Deserialize;

    /// Every release, prereleases included: the Android builds are prereleases.
    pub const RELEASES_API: &str = "https://api.github.com/repos/idan2025/gaming-platform-prns/releases?per_page=30";
    /// The only place an APK is downloaded from.
    pub const DOWNLOAD_PREFIX: &str = "https://github.com/idan2025/gaming-platform-prns/releases/download/";
    const TAG_PREFIX: &str = "android-";

    /// An Android build on the release page.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Release {
        /// The launcher version, as in the asset name.
        pub version: String,
        /// The Android `versionCode`, which is what decides "newer".
        pub code: u32,
        pub apk_url: String,
        /// GitHub's own mark on the release. Previews and official builds are
        /// told apart by this flag, never by the tag's name.
        pub prerelease: bool,
        /// The APK's SHA-256 as GitHub reports it, lowercase hex. `None` when
        /// GitHub reports no digest; the install then rests on Android's own
        /// check that the file is signed by the installed key.
        pub sha256: Option<String>,
        /// The APK's size in bytes, as GitHub reports it (0 when unknown).
        pub size: u64,
        pub notes: Option<String>,
    }

    impl Release {
        /// What the UI shows as the version: the build number tells two
        /// Android builds of one launcher version apart.
        pub fn label(&self) -> String {
            format!("{} (build {})", self.version, self.code % 1000)
        }
    }

    #[derive(Deserialize)]
    struct ApiRelease {
        tag_name: String,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        prerelease: bool,
        body: Option<String>,
        #[serde(default)]
        assets: Vec<ApiAsset>,
    }

    #[derive(Deserialize)]
    struct ApiAsset {
        name: String,
        browser_download_url: String,
        #[serde(default)]
        size: u64,
        /// `sha256:<hex>` on GitHub's own listing; absent on older releases.
        digest: Option<String>,
    }

    /// `lanthorn-<version>-android-arm64-<versionCode>.apk`, or `None` for any
    /// other name (older Android releases did not carry the code).
    fn parse_asset_name(name: &str) -> Option<(String, u32)> {
        let rest = name.strip_prefix("lanthorn-")?.strip_suffix(".apk")?;
        let (version, code) = rest.split_once("-android-arm64-")?;
        let valid = !version.is_empty() && version.chars().all(|c| c.is_ascii_digit() || c == '.');
        valid.then_some(())?;
        Some((version.to_string(), code.parse().ok()?))
    }

    /// The SHA-256 of a `sha256:<hex>` digest, or `None` for any other form.
    fn parse_digest(digest: Option<&str>) -> Option<String> {
        let hex = digest?.strip_prefix("sha256:")?;
        let valid = hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit());
        valid.then(|| hex.to_ascii_lowercase())
    }

    /// The newest Android release in a GitHub releases listing that is newer
    /// than `current_code`. A prerelease is offered only when `include_previews`
    /// is set. An APK linked from anywhere but this repository's own downloads
    /// is ignored.
    pub fn newest(listing: &str, current_code: u32, include_previews: bool) -> anyhow::Result<Option<Release>> {
        let releases: Vec<ApiRelease> = serde_json::from_str(listing)?;
        let mut best: Option<Release> = None;
        let candidates = releases
            .into_iter()
            .filter(|r| !r.draft && r.tag_name.starts_with(TAG_PREFIX))
            .filter(|r| include_previews || !r.prerelease);
        for r in candidates {
            for a in &r.assets {
                let Some((version, code)) = parse_asset_name(&a.name) else { continue };
                if !a.browser_download_url.starts_with(DOWNLOAD_PREFIX) || code <= current_code {
                    continue;
                }
                if best.as_ref().is_none_or(|b| code > b.code) {
                    best = Some(Release {
                        version,
                        code,
                        apk_url: a.browser_download_url.clone(),
                        prerelease: r.prerelease,
                        sha256: parse_digest(a.digest.as_deref()),
                        size: a.size,
                        notes: r.body.clone(),
                    });
                }
            }
        }
        Ok(best)
    }

    /// Ask GitHub for the newest Android release newer than `current_code`.
    pub async fn check(current_code: u32, include_previews: bool) -> anyhow::Result<Option<Release>> {
        let client = game_bridge::repo::http_client(game_bridge::repo::FETCH_TIMEOUT)?;
        let response = client
            .get(RELEASES_API)
            // GitHub's API refuses a request without one.
            .header("User-Agent", concat!("Lanthorn/", env!("CARGO_PKG_VERSION")))
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        if !response.status().is_success() {
            anyhow::bail!("the release page answered {}", response.status());
        }
        newest(&response.text().await?, current_code, include_previews)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(portable: bool, os: &'static str, appimage: bool) -> InstallFacts {
        InstallFacts { portable, os, appimage }
    }

    #[test]
    fn a_portable_launcher_never_replaces_itself() {
        for os in ["windows", "macos", "linux"] {
            assert_eq!(install_mode(facts(true, os, true)), InstallMode::LinkOnly, "{os}");
        }
    }

    #[test]
    fn on_linux_only_an_appimage_updates_in_place() {
        assert_eq!(install_mode(facts(false, "linux", true)), InstallMode::InPlace);
        // A .deb, .rpm or the Arch package: the package manager's files.
        assert_eq!(install_mode(facts(false, "linux", false)), InstallMode::LinkOnly);
    }

    #[test]
    fn installed_windows_and_macos_update_in_place() {
        assert_eq!(install_mode(facts(false, "windows", false)), InstallMode::InPlace);
        assert_eq!(install_mode(facts(false, "macos", false)), InstallMode::InPlace);
    }

    fn listing(assets: &[(&str, &str, &str)]) -> String {
        let releases: Vec<_> = assets
            .iter()
            .map(|(tag, name, url)| {
                serde_json::json!({ "tag_name": tag, "draft": false, "prerelease": true, "body": "notes",
                    "assets": [{ "name": name, "browser_download_url": url, "size": 36_000_000,
                        "digest": "sha256:AB".to_string() + &"cd".repeat(31) }] })
            })
            .collect();
        serde_json::to_string(&releases).unwrap()
    }

    const DL: &str = "https://github.com/idan2025/gaming-platform-prns/releases/download/";

    #[test]
    fn the_newest_android_build_is_picked_by_its_version_code() {
        let a = format!("{DL}android-preview-0.2.45-3/lanthorn-0.2.45-android-arm64-2045003.apk");
        let b = format!("{DL}android-preview-0.2.46-1/lanthorn-0.2.46-android-arm64-2046001.apk");
        let list = listing(&[
            ("android-preview-0.2.45-3", "lanthorn-0.2.45-android-arm64-2045003.apk", &a),
            ("android-preview-0.2.46-1", "lanthorn-0.2.46-android-arm64-2046001.apk", &b),
            // A desktop release and an old Android asset without a code.
            ("v0.2.47", "lanthorn-0.2.47-android-arm64-2047001.apk", &b),
            ("android-preview-0.2.45-2", "lanthorn-0.2.45-android-arm64-2.apk", &a),
        ]);
        let r = android::newest(&list, 2045003, true).unwrap().unwrap();
        assert_eq!((r.version.as_str(), r.code, r.apk_url.as_str()), ("0.2.46", 2046001, b.as_str()));
        assert_eq!(r.label(), "0.2.46 (build 1)");
        assert_eq!(android::newest(&list, 2046001, true).unwrap(), None, "nothing newer");
    }

    #[test]
    fn a_release_carries_its_digest_size_and_channel() {
        let b = format!("{DL}android-preview-0.2.46-1/lanthorn-0.2.46-android-arm64-2046001.apk");
        let list = listing(&[("android-preview-0.2.46-1", "lanthorn-0.2.46-android-arm64-2046001.apk", &b)]);
        let r = android::newest(&list, 1, true).unwrap().unwrap();
        assert_eq!(r.sha256.as_deref(), Some(("ab".to_string() + &"cd".repeat(31)).as_str()));
        assert_eq!((r.size, r.prerelease), (36_000_000, true));
        // A digest that is not a sha256 of the right length is no digest at all.
        let bad = serde_json::json!([{ "tag_name": "android-preview-0.2.46-1", "prerelease": true,
            "assets": [{ "name": "lanthorn-0.2.46-android-arm64-2046001.apk",
                "browser_download_url": b, "digest": "md5:abc" }] }]);
        assert_eq!(android::newest(&bad.to_string(), 1, true).unwrap().unwrap().sha256, None);
    }

    #[test]
    fn previews_are_offered_only_when_the_player_asks_for_them() {
        let b = format!("{DL}android-preview-0.2.46-1/lanthorn-0.2.46-android-arm64-2046001.apk");
        let c = format!("{DL}android-0.2.46-2/lanthorn-0.2.46-android-arm64-2046002.apk");
        let mut list: serde_json::Value = serde_json::from_str(&listing(&[
            ("android-preview-0.2.46-1", "lanthorn-0.2.46-android-arm64-2046001.apk", &b),
            ("android-0.2.46-2", "lanthorn-0.2.46-android-arm64-2046002.apk", &c),
        ]))
        .unwrap();
        // The second release is the official one: not a prerelease.
        list[1]["prerelease"] = serde_json::json!(false);
        let list = list.to_string();
        // Official only: the preview is newer by code but not offered.
        assert_eq!(android::newest(&list, 1, false).unwrap().unwrap().code, 2046002);
        // With previews, the newer code wins, preview or not.
        assert_eq!(android::newest(&list, 1, true).unwrap().unwrap().code, 2046002);
        // Only a preview exists: official-only finds nothing.
        list_only_preview(&b);
    }

    fn list_only_preview(b: &str) {
        let list = listing(&[("android-preview-0.2.46-1", "lanthorn-0.2.46-android-arm64-2046001.apk", b)]);
        assert_eq!(android::newest(&list, 1, false).unwrap(), None);
        assert!(android::newest(&list, 1, true).unwrap().is_some());
    }

    #[test]
    fn an_apk_from_anywhere_but_this_repositorys_downloads_is_ignored() {
        let list = listing(&[(
            "android-preview-9.9.9-1",
            "lanthorn-9.9.9-android-arm64-9009001.apk",
            "https://example.com/releases/download/lanthorn.apk",
        )]);
        assert_eq!(android::newest(&list, 1, true).unwrap(), None);
    }

    #[test]
    fn update_view_keys_are_the_frontend_contract() {
        let v = serde_json::to_value(UpdateView::none("1.0.0", InstallMode::LinkOnly, true)).unwrap();
        for key in ["current", "available", "notes", "install", "page", "auto_check"] {
            assert!(v.get(key).is_some(), "the UI reads `{key}` and it is missing");
        }
        assert_eq!(v["install"], "link-only");
    }
}
