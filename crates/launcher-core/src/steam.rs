//! Finding the player's own copy of a game, and the Steam client that can start
//! it — the *detection* half of `PLAN.md` §13.3 step 1.
//!
//! # What this does and, more importantly, what it never does
//!
//! It answers one question: "does this machine have Steam, and is app *N*
//! installed?" It does that by reading Steam's own on-disk manifests, files the
//! Steam client wrote. It **never** reads anything a pack supplied to decide
//! what to run — a pack contributes only a numeric `steam_app_id`, a hint for
//! *locating* an installation, exactly as `PLAN.md` §13.1 rule 1 requires.
//!
//! # Why launch goes through Steam rather than a guessed binary
//!
//! For the GoldSrc and Source families in scope, the robust way to start the
//! player's game pointed at a server is `steam -applaunch <app_id> +connect
//! <addr>` — Steam owns the exact binary, the runtime, and the per-OS launch
//! quirks, and it is one known executable instead of a guess like `hl.exe` that
//! differs by platform and install. The launcher finds Steam; the pack names
//! nothing. The arguments are still spawned as a **vector, never a shell**
//! (`crate::Launcher::play`), so every §13.1 safety rule still holds — a `;` in
//! a pack is one byte of one argument whether the program is the game or Steam.
//!
//! Every function that decides anything takes its inputs explicitly (candidate
//! roots, file contents) so it is unit-testable without a real Steam install;
//! the thin wrappers that read `$HOME`/`%ProgramFiles%` are the only untested
//! surface, and they only *gather candidates*.

use std::path::PathBuf;

/// Locate the Steam executable to spawn, searching this platform's usual
/// locations. `None` means Steam is not installed where it can be found, and
/// the player must point the launcher at their game directly instead.
pub fn steam_executable() -> Option<PathBuf> {
    first_existing(steam_executable_candidates())
}

/// Does this machine have `app_id` installed under Steam? Returns the install
/// directory (`steamapps/common/<installdir>`) when so — useful to the UI as
/// proof the game is present, even though launch itself goes through Steam.
pub fn installed_app_dir(app_id: u32) -> Option<PathBuf> {
    let libraries = library_dirs(&steam_library_roots());
    find_app_install_dir(app_id, &libraries)
}

// ---- pure, testable core ---------------------------------------------------

/// The first path in `candidates` that exists on disk.
fn first_existing<I: IntoIterator<Item = PathBuf>>(candidates: I) -> Option<PathBuf> {
    candidates.into_iter().find(|p| p.exists())
}

/// Parse a Steam `libraryfolders.vdf` into the library root directories it
/// lists. Steam's VDF is a nested key/quote format; every `"path" "<dir>"` line
/// names a library, so those are what we extract, tolerating the format's two
/// historical shapes (a bare string value in old clients, a nested block in
/// new ones) by keying only on the `path` field.
///
/// A parse that finds nothing returns an empty vector rather than erroring: a
/// machine with a malformed VDF simply has no detected libraries, and the
/// player can still point the launcher at their game.
pub fn parse_library_folders(vdf: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in vdf.lines() {
        let line = line.trim();
        // Lines look like: "path"              "/mnt/games/SteamLibrary"
        let mut parts = line.splitn(2, '"').skip(1);
        let Some(rest) = parts.next() else { continue };
        // `rest` now starts with the key; re-split it on quotes.
        let fields: Vec<&str> = rest.split('"').collect();
        // fields[0] = key, fields[2] = value (fields[1] is the gap between).
        if fields.first() == Some(&"path") {
            if let Some(value) = fields.get(2) {
                if !value.is_empty() {
                    out.push(PathBuf::from(unescape_vdf(value)));
                }
            }
        }
    }
    out
}

/// VDF escapes backslashes (common in Windows paths written as `C:\\Games`).
fn unescape_vdf(s: &str) -> String {
    s.replace("\\\\", "\\")
}

/// Read the `installdir` field out of a Steam `appmanifest_<id>.acf`. Same VDF
/// shape as [`parse_library_folders`]; `installdir` is a directory name
/// relative to `<library>/steamapps/common`.
pub fn parse_app_installdir(acf: &str) -> Option<String> {
    for line in acf.lines() {
        let fields: Vec<&str> = line.trim().split('"').collect();
        // "installdir"         "Sven Co-op"  -> ["", "installdir", "\t\t", "Sven Co-op", ""]
        if fields.get(1) == Some(&"installdir") {
            if let Some(value) = fields.get(3) {
                if !value.is_empty() {
                    return Some(unescape_vdf(value));
                }
            }
        }
    }
    None
}

/// Given the Steam library roots, expand them to the actual library directories
/// by reading each root's `steamapps/libraryfolders.vdf`. The root that holds
/// Steam itself is always a library too, so it is included even if the VDF does
/// not name it.
pub fn library_dirs(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for root in roots {
        if !dirs.contains(root) {
            dirs.push(root.clone());
        }
        let vdf = root.join("steamapps/libraryfolders.vdf");
        if let Ok(body) = std::fs::read_to_string(&vdf) {
            for lib in parse_library_folders(&body) {
                if !dirs.contains(&lib) {
                    dirs.push(lib);
                }
            }
        }
    }
    dirs
}

/// Find which library has `app_id` installed and return the resolved install
/// directory. The first library whose `appmanifest_<id>.acf` exists and whose
/// named `installdir` is actually present on disk wins — a manifest can outlive
/// the files it describes, and pointing a launch at a directory that is not
/// there is worse than reporting the game as absent.
pub fn find_app_install_dir(app_id: u32, libraries: &[PathBuf]) -> Option<PathBuf> {
    for lib in libraries {
        let manifest = lib.join(format!("steamapps/appmanifest_{app_id}.acf"));
        let Ok(body) = std::fs::read_to_string(&manifest) else { continue };
        let Some(installdir) = parse_app_installdir(&body) else { continue };
        let full = lib.join("steamapps/common").join(installdir);
        if full.is_dir() {
            return Some(full);
        }
    }
    None
}

// ---- untested candidate gathering (reads the environment) -------------------

/// Directories that might be a Steam root (the folder containing `steamapps`),
/// per platform. Gathering only; the caller checks which exist.
fn steam_library_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for var in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(pf) = std::env::var_os(var) {
                roots.push(PathBuf::from(pf).join("Steam"));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join("Library/Application Support/Steam"));
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            // Both the modern and the legacy Linux layouts.
            roots.push(home.join(".local/share/Steam"));
            roots.push(home.join(".steam/steam"));
            roots.push(home.join(".steam/root"));
        }
    }
    roots.into_iter().filter(|p| p.exists()).collect()
}

/// Candidate Steam executables, per platform, most-preferred first.
fn steam_executable_candidates() -> Vec<PathBuf> {
    let mut c = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for var in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(pf) = std::env::var_os(var) {
                c.push(PathBuf::from(pf).join("Steam/steam.exe"));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        c.push(PathBuf::from("/Applications/Steam.app/Contents/MacOS/steam_osx"));
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // A PATH lookup covers a distro-packaged `steam` wrapper; the explicit
        // paths cover a Steam that is installed but not on PATH.
        if let Ok(path) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path) {
                c.push(dir.join("steam"));
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            c.push(home.join(".steam/steam/steam.sh"));
            c.push(home.join(".local/share/Steam/steam.sh"));
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_folders_vdf_yields_every_named_path() {
        // The modern nested shape Steam writes today.
        let vdf = r#"
"libraryfolders"
{
        "0"
        {
                "path"          "/home/idan/.local/share/Steam"
                "label"         ""
        }
        "1"
        {
                "path"          "/mnt/games/SteamLibrary"
        }
}
"#;
        let libs = parse_library_folders(vdf);
        assert_eq!(
            libs,
            vec![
                PathBuf::from("/home/idan/.local/share/Steam"),
                PathBuf::from("/mnt/games/SteamLibrary"),
            ]
        );
    }

    #[test]
    fn a_windows_path_with_escaped_backslashes_is_unescaped() {
        let vdf = "\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"";
        assert_eq!(
            parse_library_folders(vdf),
            vec![PathBuf::from("C:\\Program Files (x86)\\Steam")]
        );
    }

    #[test]
    fn a_malformed_vdf_yields_no_libraries_rather_than_panicking() {
        assert!(parse_library_folders("garbage with no quoted path").is_empty());
        assert!(parse_library_folders("").is_empty());
    }

    #[test]
    fn installdir_is_read_from_an_app_manifest() {
        let acf = r#"
"AppState"
{
        "appid"         "276060"
        "installdir"            "Sven Co-op"
        "name"          "Sven Co-op Dedicated Server"
}
"#;
        assert_eq!(parse_app_installdir(acf).as_deref(), Some("Sven Co-op"));
        assert_eq!(parse_app_installdir("no installdir here"), None);
    }

    #[test]
    fn an_installed_app_is_found_only_when_its_files_are_actually_there() {
        let lib = tempfile::tempdir().unwrap();
        let steamapps = lib.path().join("steamapps");
        std::fs::create_dir_all(&steamapps).unwrap();
        std::fs::write(
            steamapps.join("appmanifest_276060.acf"),
            "\"AppState\"\n{\n\t\"installdir\"\t\t\"Sven Co-op\"\n}\n",
        )
        .unwrap();

        let libs = vec![lib.path().to_path_buf()];
        // The manifest exists but the game directory does not yet.
        assert_eq!(
            find_app_install_dir(276060, &libs),
            None,
            "a manifest without files is not an install"
        );

        // Now the files are present.
        let common = steamapps.join("common/Sven Co-op");
        std::fs::create_dir_all(&common).unwrap();
        assert_eq!(find_app_install_dir(276060, &libs), Some(common));
    }

    #[test]
    fn an_uninstalled_app_is_not_found() {
        let lib = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(lib.path().join("steamapps")).unwrap();
        assert_eq!(find_app_install_dir(90, &[lib.path().to_path_buf()]), None);
    }

    #[test]
    fn library_dirs_includes_the_root_itself_and_what_its_vdf_names() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let steamapps = root.path().join("steamapps");
        std::fs::create_dir_all(&steamapps).unwrap();
        std::fs::write(
            steamapps.join("libraryfolders.vdf"),
            format!("\t\t\"path\"\t\t\"{}\"", other.path().display()),
        )
        .unwrap();

        let dirs = library_dirs(&[root.path().to_path_buf()]);
        assert!(dirs.contains(&root.path().to_path_buf()), "the root is always a library");
        assert!(
            dirs.contains(&other.path().to_path_buf()),
            "a library named in the vdf is included"
        );
    }

    #[test]
    fn first_existing_picks_the_first_present_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let present = dir.path().join("here");
        std::fs::write(&present, "x").unwrap();
        assert_eq!(first_existing(vec![missing, present.clone()]), Some(present));
    }
}
