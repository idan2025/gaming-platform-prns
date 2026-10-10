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

    #[test]
    fn update_view_keys_are_the_frontend_contract() {
        let v = serde_json::to_value(UpdateView::none("1.0.0", InstallMode::LinkOnly, true)).unwrap();
        for key in ["current", "available", "notes", "install", "page", "auto_check"] {
            assert!(v.get(key).is_some(), "the UI reads `{key}` and it is missing");
        }
        assert_eq!(v["install"], "link-only");
    }
}
