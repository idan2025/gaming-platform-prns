//! `platform-agent --control-address` is what the worker setup prints into an
//! index's hosting file. It must print the same destination the agent serves,
//! stay the same across runs, and never print the identity secret. It needs no
//! Docker and no network, so this runs everywhere.

use std::process::Command;

fn config_in(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("agent.toml");
    std::fs::write(
        &path,
        format!(
            "data_root = \"{d}/data\"\n\n[port_range]\nstart = 27100\nend = 27199\n\n\
             [uplink]\nidentity_secret_path = \"{d}/agent.identity\"\ntrusted_indexes = []\n",
            d = dir.display()
        ),
    )
    .unwrap();
    path
}

fn run(config: &std::path::Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_platform-agent"))
        .arg("--control-address")
        .arg(config)
        .output()
        .expect("runs the agent binary");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn field<'a>(text: &'a str, name: &str) -> &'a str {
    text.lines()
        .find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix(' ')))
        .unwrap_or_else(|| panic!("no {name} line in {text:?}"))
}

#[test]
fn the_control_address_is_stable_public_and_never_shows_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_in(dir.path());

    let first = run(&config);
    let second = run(&config);
    assert_eq!(first, second, "the same identity must print the same addresses");

    let control = field(&first, "control");
    let identity = field(&first, "identity");
    assert_eq!(control.len(), 32, "a destination hash is 16 bytes");
    assert_eq!(identity.len(), 32, "an identity hash is 16 bytes");
    assert_ne!(control, identity, "the control destination is not the identity");

    // The secret is 64 bytes on disk; its hex must appear nowhere in the output.
    let secret = std::fs::read(dir.path().join("agent.identity")).unwrap();
    assert_eq!(secret.len(), 64, "the identity file is the 64-byte secret");
    assert!(!first.contains(&hex::encode(&secret)), "the identity secret was printed as hex");
    assert!(
        !first.as_bytes().windows(16).any(|w| secret.windows(16).any(|s| s == w)),
        "a run of the identity secret's bytes was printed"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("agent.identity")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the identity file is owner-only");
    }
}

#[test]
fn a_config_without_an_uplink_has_no_address_to_print() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.toml");
    std::fs::write(
        &path,
        format!("data_root = \"{}/data\"\n\n[port_range]\nstart = 27100\nend = 27199\n", dir.path().display()),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_platform-agent"))
        .arg("--control-address")
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no [uplink] section"));
}
