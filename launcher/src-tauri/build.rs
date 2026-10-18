use std::path::Path;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        android();
    }
    tauri_build::build()
}

/// An Android app's bundled resources are APK assets, which no `std::fs` call
/// can read, so the shipped packs are compiled in and written to the app's own
/// folder at start (`lib.rs`, `android_pack_dir`). The `versionCode` the APK
/// carries is what the Android update check compares, so it is read from the
/// one place it is set.
fn android() {
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("bundled_packs.rs");
    let packs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
    println!("cargo:rerun-if-changed={}", packs.display());
    let mut files: Vec<_> = std::fs::read_dir(&packs)
        .expect("reading packs/")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml" || x == "sig"))
        .collect();
    files.sort();
    let mut src = String::from("pub const BUNDLED_PACKS: &[(&str, &[u8])] = &[\n");
    for f in &files {
        let name = f.file_name().unwrap().to_str().expect("a pack file name is UTF-8");
        let path = f.canonicalize().unwrap();
        src.push_str(&format!("    ({name:?}, include_bytes!({:?})),\n", path.display().to_string()));
    }
    src.push_str("];\n");
    std::fs::write(out, src).unwrap();

    let conf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    println!("cargo:rerun-if-changed={}", conf.display());
    let text = std::fs::read_to_string(&conf).unwrap();
    let code = text
        .split("\"versionCode\"")
        .nth(1)
        .and_then(|rest| rest.trim_start().strip_prefix(':'))
        .map(|rest| rest.trim_start().chars().take_while(char::is_ascii_digit).collect::<String>())
        .filter(|digits| !digits.is_empty())
        .expect("tauri.conf.json sets bundle.android.versionCode");
    println!("cargo:rustc-env=LANTHORN_ANDROID_VERSION_CODE={code}");
}
