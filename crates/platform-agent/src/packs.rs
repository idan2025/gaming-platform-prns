//! Which packs on this node's disk are allowed to be deployed.
//!
//! `PLAN.md` §11.4. The tiers and the signature checking live in
//! `game_bridge::signing`; what lives here is the node's half of the decision —
//! that a pack the operator's policy will not deploy is **never loaded**, and
//! that the operator is told which pack was refused and what to write to change
//! that.
//!
//! # Why deploy, and why here
//!
//! §11.4 allows import at every tier and gates deploy, because deploy is what
//! puts a description onto a node. The agent has no import: every pack it reads
//! it reads in order to run. So for this process the two collapse, and the gate
//! belongs at load.
//!
//! # The rule a later change could quietly break
//!
//! **A pack whose signature failed is a refusal, not an unsigned pack.** That
//! rule is `signing.rs`'s, and it survives here only because a failed
//! `load_verified` lands in [`DeployablePacks::errors`] and stops there — it is
//! never retried through the unsigned `GamePack::load_dir` path. Retrying would
//! turn a forged signature into a deployable pack on any node with
//! `allow_unsigned = true`, which is exactly the operator who most wanted to
//! hear about it. Pinned by
//! `a_forged_signature_is_not_retried_as_an_unsigned_pack`.

use std::path::Path;
use std::time::SystemTime;

use game_bridge::pack::{PackError, TrustedPack};
use game_bridge::signing::TrustPolicy;
use game_bridge::GamePack;

/// A pack this node read but will not run, and why.
pub struct RefusedPack {
    pub pack: TrustedPack,
}

impl RefusedPack {
    /// One line for the operator's log, naming the pack, the tier, and the
    /// config key that would change the answer.
    pub fn why(&self) -> String {
        let fix = match self.pack.trust.signer() {
            Some(signer) => format!(
                "add {} to pack_trust.trusted_keys, or set pack_trust.allow_unsigned = true",
                hex::encode(signer.as_bytes())
            ),
            None => "set pack_trust.allow_unsigned = true, or sign the pack".to_string(),
        };
        format!(
            "{} — {} To run it here, {fix}.",
            self.pack.trust.label(),
            self.pack.trust.explanation()
        )
    }
}

/// What a pack directory yielded once trust was applied.
pub struct DeployablePacks {
    /// Packs that verified and that the policy will deploy.
    pub packs: Vec<GamePack>,
    /// Packs that verified but that the policy refuses to deploy.
    pub refused: Vec<RefusedPack>,
    /// Packs that could not be read, parsed, or whose signature did not verify.
    /// A stale or forged signature is here, never in `packs`.
    pub errors: Vec<(String, PackError)>,
}

/// Read `dir`, verify every pack's provenance, and keep the ones `policy` will
/// deploy.
pub fn load_deployable(
    dir: &Path,
    policy: &TrustPolicy,
    now: SystemTime,
) -> Result<DeployablePacks, PackError> {
    let verified = GamePack::load_dir_verified(dir, policy, now)?;
    let mut packs = Vec::new();
    let mut refused = Vec::new();
    for trusted in verified.packs {
        if policy.may_deploy(&trusted.trust) {
            packs.push(trusted.pack);
        } else {
            refused.push(RefusedPack { pack: trusted });
        }
    }
    Ok(DeployablePacks { packs, refused, errors: verified.errors })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use game_bridge::signing::PackSignature;
    use prns_core::identity::{IdentityHash, PrivateIdentityMaterial};

    use super::*;

    const PACK: &str = r#"
schema_version = 1
id = "test-game"
display_name = "Test Game"
app_name = "test-game"
default_port = 27015
transport = "udp"
min_link_class = 1
query = "a2s"
"#;

    fn secret(seed: u8) -> PrivateIdentityMaterial {
        PrivateIdentityMaterial::from_bytes([seed; 64])
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    /// Writes `pack.toml`, optionally signed by `signer`, into a fresh dir.
    fn dir_with(signer: Option<&PrivateIdentityMaterial>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pack.toml");
        std::fs::write(&path, PACK).unwrap();
        if let Some(s) = signer {
            let sig = PackSignature::sign(PACK.as_bytes(), s, now(), Duration::from_secs(86_400))
                .unwrap();
            std::fs::write(dir.path().join("pack.toml.sig"), sig.to_toml()).unwrap();
        }
        dir
    }

    fn hash_of(s: &PrivateIdentityMaterial) -> IdentityHash {
        s.public().identity_hash()
    }

    #[test]
    fn a_workstation_policy_runs_the_unsigned_pack_on_disk() {
        let dir = dir_with(None);
        let out = load_deployable(dir.path(), &TrustPolicy::allowing_unsigned(), now()).unwrap();
        assert_eq!(out.packs.len(), 1);
        assert!(out.refused.is_empty());
        assert!(out.errors.is_empty());
    }

    /// §11.4 tier 3: unsigned is refused for deploy unless the operator said
    /// otherwise. This is the whole point of the gate — before it was wired,
    /// `strict()` and `allowing_unsigned()` behaved identically on a node.
    #[test]
    fn a_strict_policy_refuses_the_same_pack_and_says_what_to_write() {
        let dir = dir_with(None);
        let out = load_deployable(dir.path(), &TrustPolicy::strict(), now()).unwrap();
        assert!(out.packs.is_empty());
        assert_eq!(out.refused.len(), 1);
        let why = out.refused[0].why();
        assert!(why.contains("unsigned local"), "{why}");
        assert!(why.contains("allow_unsigned"), "{why}");
    }

    /// A signature that verifies by a key nobody named is `SignedUnknown`, and
    /// §11.4 gates it exactly like unsigned — but the fix names the key, so an
    /// operator can trust it without opening the door to everything.
    #[test]
    fn a_stranger_key_is_refused_but_the_message_names_it() {
        let signer = secret(7);
        let dir = dir_with(Some(&signer));
        let out = load_deployable(dir.path(), &TrustPolicy::strict(), now()).unwrap();
        assert!(out.packs.is_empty());
        let why = out.refused[0].why();
        assert!(why.contains("signed by an unknown key"), "{why}");
        assert!(why.contains(&hex::encode(hash_of(&signer).as_bytes())), "{why}");
    }

    #[test]
    fn a_trusted_key_deploys_under_a_policy_that_allows_nothing_else() {
        let signer = secret(9);
        let dir = dir_with(Some(&signer));
        let policy = TrustPolicy::strict().trusting(hash_of(&signer));
        let out = load_deployable(dir.path(), &policy, now()).unwrap();
        assert_eq!(out.packs.len(), 1);
        assert!(out.refused.is_empty());
    }

    /// The load-bearing one. A pack whose signature is forged must not fall
    /// back to the unsigned path on a node that allows unsigned packs: the
    /// operator who allowed unsigned local files did not thereby ask to be lied
    /// to about who wrote one.
    #[test]
    fn a_forged_signature_is_not_retried_as_an_unsigned_pack() {
        let dir = dir_with(Some(&secret(3)));
        // Same window, same key, one byte of the signature flipped.
        let sig_path = dir.path().join("pack.toml.sig");
        let src = std::fs::read_to_string(&sig_path).unwrap();
        let sig = PackSignature::parse(&src).unwrap();
        let mut raw = hex::decode(&sig.signature).unwrap();
        raw[0] ^= 0x01;
        let forged = PackSignature { signature: hex::encode(raw), ..sig };
        std::fs::write(&sig_path, forged.to_toml()).unwrap();

        let out = load_deployable(dir.path(), &TrustPolicy::allowing_unsigned(), now()).unwrap();
        assert!(out.packs.is_empty(), "a forged signature must not deploy");
        assert!(out.refused.is_empty(), "it is an error, not a tier");
        assert_eq!(out.errors.len(), 1);
    }

    /// Same for a signature that was good and has gone stale: §11.3's designed
    /// end state is that the node stops running the pack, not that it demotes
    /// it to a local file.
    #[test]
    fn an_expired_signature_does_not_deploy_even_where_unsigned_would() {
        let signer = secret(4);
        let dir = dir_with(Some(&signer));
        let policy = TrustPolicy::allowing_unsigned().trusting(hash_of(&signer));
        let later = now() + Duration::from_secs(86_400 * 2);
        let out = load_deployable(dir.path(), &policy, later).unwrap();
        assert!(out.packs.is_empty());
        assert!(out.refused.is_empty());
        assert_eq!(out.errors.len(), 1);
    }
}
