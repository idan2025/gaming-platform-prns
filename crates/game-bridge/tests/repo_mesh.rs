//! A pack repository seeded over Reticulum (`repo_mesh.rs`), on loopback.
//!
//!   seed (a signed folder) → Reticulum (TCP interface) → browser node
//!
//! The browser hears the seed, never lists it as a server, and mirrors it
//! back over `rns://` byte for byte, held to the key that signed it. No
//! internet, no web host.

use std::path::Path;
use std::time::{Duration, SystemTime};

use game_bridge::config::BrowserArgs;
use game_bridge::repo::{RepoIndex, INDEX_FILE};
use game_bridge::repo_mesh::{self, SeedArgs};
use game_bridge::signing::{PackSignature, SIGNATURE_SUFFIX};
use game_bridge::BridgeSession;
use prns_core::identity::PrivateIdentityMaterial;

mod common;

fn key() -> PrivateIdentityMaterial {
    PrivateIdentityMaterial::from_slice(&[11; 64]).unwrap()
}

fn signed_repo(dir: &Path) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
    let now = SystemTime::now() - Duration::from_secs(60);
    for f in ["sven-coop.toml", "fistful-of-frags.toml"] {
        std::fs::copy(shipped.join(f), dir.join(f)).unwrap();
        let sig = PackSignature::sign(&std::fs::read(dir.join(f)).unwrap(), &key(), now, Duration::from_secs(86_400)).unwrap();
        std::fs::write(dir.join(format!("{f}{SIGNATURE_SUFFIX}")), sig.to_toml()).unwrap();
    }
    let json = serde_json::to_vec_pretty(&RepoIndex::build(dir, "Seeded packs").unwrap()).unwrap();
    std::fs::write(dir.join(INDEX_FILE), &json).unwrap();
    let sig = PackSignature::sign_index(&json, &key(), now, Duration::from_secs(86_400)).unwrap();
    std::fs::write(dir.join(format!("{INDEX_FILE}{SIGNATURE_SUFFIX}")), sig.to_toml()).unwrap();
}

fn listing(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (p.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read(&p).unwrap())
        })
        .collect();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_seed_is_heard_never_listed_and_mirrors_exactly_over_the_mesh() {
    let scratch = common::scratch_dir("repo-mesh");
    let repo = scratch.join("repo");
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).unwrap();
    signed_repo(&repo);
    let mesh_port = common::free_tcp_port();
    let seed = repo_mesh::start_seed(SeedArgs {
        dir: repo.clone(),
        identity: scratch.join("seed.identity"),
        tcp: Some(format!("0.0.0.0:{mesh_port}")),
        auto: false,
        announce_every: Duration::from_secs(5),
    })
    .await
    .expect("the seed starts");
    let seed_hash = seed.own_hash().unwrap();

    let mut args = BrowserArgs::new();
    args.tcp = Some(format!("127.0.0.1:{mesh_port}"));
    let browser = BridgeSession::start_browser(args).await.expect("the browser starts");
    repo_mesh::set_mesh_handle(Some(browser.handle().clone()));

    let signer = key().public().identity_hash();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let heard = loop {
        if let Some(h) = repo_mesh::heard().into_iter().find(|h| h.destination == seed_hash) {
            break h;
        }
        assert!(tokio::time::Instant::now() < deadline, "the seed was never heard");
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert_eq!(heard.announce.signer, Some(signer));
    assert_eq!(heard.announce.packs, 2);
    assert_eq!(heard.announce.name, "Seeded packs");
    assert!(
        !browser.discovered().await.iter().any(|s| s.destination_hash == seed_hash),
        "a seed is never a server row"
    );

    let dest = scratch.join("mirrored");
    let _ = std::fs::remove_dir_all(&dest);
    let report = game_bridge::repo::mirror(&heard.address(), &dest, signer).await.expect("mirrored over the mesh");
    assert_eq!(report.packs, 2);
    assert_eq!(listing(&dest), listing(&repo), "byte for byte, over Reticulum");
    assert!(
        game_bridge::repo::mirror(&heard.address(), &scratch.join("other"), PrivateIdentityMaterial::from_slice(&[12; 64]).unwrap().public().identity_hash())
            .await
            .is_err(),
        "held to the key it is given"
    );
}
