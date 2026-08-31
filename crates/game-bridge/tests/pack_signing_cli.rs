//! `game-bridge sign` and `verify` — `PLAN.md` §11.3's missing half.
//!
//! The library could verify a signature and classify a signer long before
//! anything could *produce* one, so the trust tiers were enforceable and
//! unreachable at the same time. These tests run the real binary, because the
//! gap was never in the library: `PackSignature::sign` had no caller outside a
//! unit test, and a unit test calling it again would not have noticed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_game-bridge");

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gb-signing-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/sven-coop.toml");
    std::fs::copy(shipped, dir.join("pack.toml")).expect("a shipped pack to sign");
    dir
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("the binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// The whole point: a signature can be made, and a fresh node reading it with
/// no trust list gets `SignedUnknown` — valid, but unvouched. Naming the key is
/// what makes the next step (trusting it) possible at all.
#[test]
fn a_pack_can_be_signed_and_then_verifies_as_an_unknown_key() {
    let dir = scratch("roundtrip");
    let pack = dir.join("pack.toml");
    let key = dir.join("key");

    let signed = run(&["sign", pack.to_str().unwrap(), "--identity", key.to_str().unwrap()]);
    assert!(signed.status.success(), "{}", stderr(&signed));
    assert!(pack.with_extension("toml.sig").exists(), "no .sig was written");

    let out = run(&["verify", pack.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("signed by an unknown key"), "{text}");
    assert!(text.contains("signer"), "{text}");
}

/// And the same signature reads as `signed community` once the verifier says
/// it trusts that key — the tier is a property of the reader's policy, not of
/// the file.
#[test]
fn the_same_signature_is_community_to_someone_who_trusts_the_key() {
    let dir = scratch("trusted");
    let pack = dir.join("pack.toml");
    let key = dir.join("key");
    run(&["sign", pack.to_str().unwrap(), "--identity", key.to_str().unwrap()]);

    let out = run(&[
        "verify",
        pack.to_str().unwrap(),
        "--identity",
        key.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("signed community"), "{}", stdout(&out));
}

/// Editing the pack after signing must break the signature. Obvious, and the
/// reason for a test is that it is the one property everything else rests on.
#[test]
fn editing_the_pack_after_signing_invalidates_it() {
    let dir = scratch("tamper-pack");
    let pack = dir.join("pack.toml");
    let key = dir.join("key");
    run(&["sign", pack.to_str().unwrap(), "--identity", key.to_str().unwrap()]);

    let mut src = std::fs::read_to_string(&pack).unwrap();
    src.push_str("\n# added after signing\n");
    std::fs::write(&pack, src).unwrap();

    let out = run(&["verify", pack.to_str().unwrap()]);
    assert!(!out.status.success(), "a tampered pack verified: {}", stdout(&out));
}

/// The window lives *inside* the signed material, so a holder cannot extend
/// their own signature by editing the file. A window someone could edit is not
/// a window.
#[test]
fn extending_the_window_by_hand_invalidates_it() {
    let dir = scratch("tamper-window");
    let pack = dir.join("pack.toml");
    let key = dir.join("key");
    run(&["sign", pack.to_str().unwrap(), "--identity", key.to_str().unwrap(), "--days", "1"]);

    let sig_path = pack.with_extension("toml.sig");
    let src = std::fs::read_to_string(&sig_path).unwrap();
    let edited: String = src
        .lines()
        .map(|l| if l.starts_with("not_after") { "not_after = 9999999999" } else { l })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&sig_path, edited).unwrap();

    let out = run(&["verify", pack.to_str().unwrap()]);
    assert!(!out.status.success(), "an extended window verified: {}", stdout(&out));
}

/// §11.3's designed end state, reachable from the command line: a signature
/// that was good goes stale, and the message says to refresh it rather than
/// implying the pack is forged.
#[test]
fn a_signature_goes_stale_and_says_so() {
    let dir = scratch("stale");
    let pack = dir.join("pack.toml");
    let key = dir.join("key");
    run(&["sign", pack.to_str().unwrap(), "--identity", key.to_str().unwrap(), "--days", "1"]);

    let out = run(&["verify", pack.to_str().unwrap(), "--at", "9999999999"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("expired"), "{err}");
    assert!(err.contains("refresh"), "{err}");
}

/// Signing a file that is not a valid pack would produce a signature that
/// verifies over bytes nothing can load — a correct answer to the wrong
/// question. Parse first.
#[test]
fn a_file_that_is_not_a_pack_is_not_signed() {
    let dir = scratch("not-a-pack");
    let junk = dir.join("junk.toml");
    std::fs::write(&junk, "this is not a pack\n").unwrap();
    let key = dir.join("key");

    let out = run(&["sign", junk.to_str().unwrap(), "--identity", key.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(!junk.with_extension("toml.sig").exists(), "a .sig was written anyway");
}

/// Replacing a signature is a decision, not a default: an accidental re-sign
/// would silently reset a window somebody is relying on.
#[test]
fn an_existing_signature_is_not_replaced_without_force() {
    let dir = scratch("force");
    let pack = dir.join("pack.toml");
    let key = dir.join("key");
    let args = ["sign", pack.to_str().unwrap(), "--identity", key.to_str().unwrap()];
    assert!(run(&args).status.success());

    let second = run(&args);
    assert!(!second.status.success());
    assert!(stderr(&second).contains("--force"), "{}", stderr(&second));

    let mut forced = args.to_vec();
    forced.push("--force");
    assert!(run(&forced).status.success());
}
