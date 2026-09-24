//! Portable mode: a launcher that leaves nothing on the machine
//! (`PLAN.md` §14, step 5b).
//!
//! A launcher is portable when a `portable-data` folder sits beside its
//! executable — which is how the portable `.zip` and `.tar.gz` ship — or, for
//! an AppImage, when an `<AppImage>.home` folder sits beside the image (the
//! AppImage convention). Everything the launcher and the libraries under it
//! would write — settings, identities, remembered servers, the web view's
//! storage and caches, font and shader caches — then goes into that folder.
//! Delete the folder and nothing of the launcher is left.
//!
//! Rooms follow the same rule: in portable mode nothing is granted or
//! installed. On Linux the helper runs through `pkexec` per room from a copy
//! that is deleted as soon as it runs; on Windows the Wintun driver is removed
//! again when the room ends (`lan_adapter`).
//!
//! # The rule a later change could quietly break
//!
//! **[`confine`] runs before anything else**, in particular before any thread
//! exists: it redirects the per-user directories by environment, which every
//! library reads once and early. Called later, a web view or font cache has
//! already chosen the real home and writes there.

use std::path::{Path, PathBuf};

/// The folder a portable launcher keeps everything in.
pub const PORTABLE_DIR: &str = "portable-data";

/// A portable run: where everything goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Portable {
    pub data_dir: PathBuf,
}

/// Whether this process runs from an AppImage.
pub fn is_appimage() -> bool {
    std::env::var_os("APPIMAGE").is_some()
}

/// Decide portability from the facts, so it is testable without an install.
pub fn detect_from(exe: Option<&Path>, appimage: Option<&Path>) -> Option<Portable> {
    if let Some(image) = appimage {
        let mut home = image.as_os_str().to_os_string();
        home.push(".home");
        let home = PathBuf::from(home);
        return home.is_dir().then_some(Portable { data_dir: home });
    }
    let dir = exe?.parent()?.join(PORTABLE_DIR);
    dir.is_dir().then_some(Portable { data_dir: dir })
}

/// Whether this launcher runs portable, and from where.
pub fn detect() -> Option<Portable> {
    let exe = std::env::current_exe().ok();
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    detect_from(exe.as_deref(), appimage.as_deref())
}

/// The per-user directories to redirect, and where to: every one a library
/// might write into.
pub fn confined_env(p: &Portable) -> Vec<(&'static str, PathBuf)> {
    let d = &p.data_dir;
    if cfg!(windows) {
        vec![
            ("APPDATA", d.join("appdata")),
            ("LOCALAPPDATA", d.join("localappdata")),
            ("WEBVIEW2_USER_DATA_FOLDER", d.join("webview2")),
        ]
    } else {
        vec![
            ("XDG_CONFIG_HOME", d.join("config")),
            ("XDG_DATA_HOME", d.join("data")),
            ("XDG_CACHE_HOME", d.join("cache")),
            ("XDG_STATE_HOME", d.join("state")),
        ]
    }
}

/// Point every per-user directory into the portable folder, creating each.
///
/// Mutates the process environment: call it first thing in `main`, before any
/// thread is spawned — see the module docs.
pub fn confine(p: &Portable) {
    for (var, dir) in confined_env(p) {
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var(var, &dir);
    }
}

/// Where the web view keeps its storage in portable mode.
pub fn webview_dir(p: &Portable) -> PathBuf {
    p.data_dir.join("webview")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_portable_data_folder_beside_the_executable_makes_it_portable() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("mesh-game-servers");
        assert_eq!(detect_from(Some(&exe), None), None, "no folder, not portable");
        std::fs::create_dir(dir.path().join(PORTABLE_DIR)).unwrap();
        assert_eq!(
            detect_from(Some(&exe), None),
            Some(Portable { data_dir: dir.path().join(PORTABLE_DIR) })
        );
    }

    #[test]
    fn an_appimage_is_portable_only_with_its_home_folder() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("Mesh.Game.Servers_0.2.21_amd64.AppImage");
        // The executable is inside the mounted image; a portable-data folder
        // beside *it* is not what an AppImage user can create.
        let inner = Path::new("/tmp/.mount_MeshXYZ/usr/bin/mesh-game-servers");
        assert_eq!(detect_from(Some(inner), Some(&image)), None);
        let home = dir.path().join("Mesh.Game.Servers_0.2.21_amd64.AppImage.home");
        std::fs::create_dir(&home).unwrap();
        assert_eq!(detect_from(Some(inner), Some(&image)), Some(Portable { data_dir: home }));
    }

    #[test]
    fn everything_a_library_writes_is_redirected_inside_the_folder() {
        let p = Portable { data_dir: PathBuf::from("/x/portable-data") };
        let env = confined_env(&p);
        assert!(!env.is_empty());
        for (var, dir) in &env {
            assert!(dir.starts_with(&p.data_dir), "{var} points outside the portable folder");
        }
        if cfg!(windows) {
            assert!(env.iter().any(|(v, _)| *v == "WEBVIEW2_USER_DATA_FOLDER"));
        } else {
            for v in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"] {
                assert!(env.iter().any(|(k, _)| *k == v), "{v} is not redirected");
            }
        }
    }
}
