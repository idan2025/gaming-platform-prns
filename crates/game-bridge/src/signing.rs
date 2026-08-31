//! Pack signatures, their validity window, and the trust tier a pack gets.
//!
//! `PLAN.md` §11.3 and §11.4. A pack is a description, and signing it raises
//! confidence in **who wrote that description** — it never turns the
//! description into code (§11.5). Everything `pack.rs` refuses to let a pack
//! say is still refused at every tier; a first-party signature does not buy a
//! pack the right to name a command, an image, or an executable.
//!
//! # Why a validity window instead of revocation
//!
//! A curated repository's safety is a key plus a person who revokes bad packs.
//! Revocation is the part that does not survive contact with a mesh: nothing
//! guarantees a node ever fetches a revocation list, and a node that never
//! fetches one trusts a compromised pack forever. So a signature carries
//! `not_before`/`not_after` and **goes stale on its own**. An unrefreshed node
//! fails closed, and it fails closed offline too, which is where a CRL is worth
//! nothing.
//!
//! The window is inside the signed material, not beside it. A window a holder
//! could edit is not a window.
//!
//! # The rule a later change could quietly break
//!
//! **A signature that does not verify is an error, never a downgrade.** An
//! expired, forged or truncated `.sig` must not read as "this pack is
//! unsigned", because an operator who allowed unsigned local packs would then
//! silently accept a pack whose signature *failed* — the one case they most
//! wanted to hear about. [`verify_pack`] returns `Err` there; only the total
//! absence of a signature is [`PackTrust::UnsignedLocal`]. Pinned by
//! `an_expired_signature_is_an_error_not_an_unsigned_pack`.
//!
//! # Signature files
//!
//! Detached, beside the pack: `sven-coop.toml` is signed by
//! `sven-coop.toml.sig`. Detached because the signed bytes are then exactly the
//! file on disk — no canonicalization step, and no way for a signed and an
//! unsigned reading of the same TOML to disagree.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prns_core::identity::{IdentityHash, PrivateIdentityMaterial, PublicIdentityMaterial};
use serde::{Deserialize, Serialize};

/// Domain separator for pack signatures.
///
/// The same Ed25519 key signs Reticulum announces and `platform-auth`
/// challenges, so pack bytes are signed under their own fixed-length,
/// NUL-padded context. Distinct from `platform_auth::AUTH_DOMAIN`: a login
/// signature must never verify as a pack signature.
pub const PACK_SIGNING_DOMAIN: &[u8; 32] = b"gaming-platform-prns/pack/v1\0\0\0\0";

/// Signature-file schema version.
pub const SIGNATURE_SCHEMA_VERSION: u32 = 1;

/// Suffix appended to a pack's file name to find its signature.
pub const SIGNATURE_SUFFIX: &str = ".sig";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureError {
    Parse(String),
    UnsupportedSchema {
        found: u32,
        supported: u32,
    },
    MalformedPublicKey,
    MalformedSignature,
    /// The signature did not verify over these pack bytes and this window.
    BadSignature,
    /// `not_after` is not after `not_before`, so the signature is valid for no
    /// instant at all. Refused at parse rather than puzzled over at verify.
    EmptyWindow,
    /// Signed for later. A clock skewed backwards on the node looks like this.
    NotYetValid {
        valid_from: u64,
    },
    /// The window closed. §11.3's designed end state, not a defect.
    Expired {
        expired_at: u64,
    },
    /// `now` is before the Unix epoch.
    ClockWentBackwards,
}

impl core::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "signature file is not valid TOML: {e}"),
            Self::UnsupportedSchema { found, supported } => write!(
                f,
                "signature schema {found} is newer than this build understands ({supported})"
            ),
            Self::MalformedPublicKey => write!(f, "public key must be 64 bytes of hex"),
            Self::MalformedSignature => write!(f, "signature must be 64 bytes of hex"),
            Self::BadSignature => write!(f, "signature did not verify over this pack"),
            Self::EmptyWindow => write!(f, "not_after must be after not_before"),
            Self::NotYetValid { valid_from } => {
                write!(f, "signature is not valid until unix {valid_from}")
            }
            Self::Expired { expired_at } => {
                write!(f, "signature expired at unix {expired_at}; ask the signer to refresh it")
            }
            Self::ClockWentBackwards => write!(f, "the system clock is before the unix epoch"),
        }
    }
}

impl std::error::Error for SignatureError {}

/// A detached pack signature, as it appears on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackSignature {
    /// Must equal [`SIGNATURE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The signer's full 64-byte Reticulum public key, hex: encryption key ‖
    /// signing key. The key, never a hash — a hash cannot verify anything, and
    /// the identity hash is derived from the key here, so a file cannot claim
    /// to be from an identity it has no key for.
    pub public_key: String,
    /// Unix seconds. Not valid before this.
    pub not_before: u64,
    /// Unix seconds. Not valid after this. Both bounds are signed.
    pub not_after: u64,
    /// Ed25519 signature over [`signing_material`], hex.
    pub signature: String,
}

/// Exactly the bytes a signer signs.
///
/// `PACK_SIGNING_DOMAIN ‖ not_before ‖ not_after ‖ len(pack) ‖ pack`, every
/// field before the pack fixed-length and big-endian, so no two different
/// (window, pack) pairs produce the same material and a signature cannot be
/// lifted from one pack onto another by shifting a boundary.
pub fn signing_material(pack_bytes: &[u8], not_before: u64, not_after: u64) -> Vec<u8> {
    let mut msg = Vec::with_capacity(PACK_SIGNING_DOMAIN.len() + 24 + pack_bytes.len());
    msg.extend_from_slice(PACK_SIGNING_DOMAIN);
    msg.extend_from_slice(&not_before.to_be_bytes());
    msg.extend_from_slice(&not_after.to_be_bytes());
    msg.extend_from_slice(&(pack_bytes.len() as u64).to_be_bytes());
    msg.extend_from_slice(pack_bytes);
    msg
}

/// Unix seconds for a `SystemTime`.
pub fn unix_secs(t: SystemTime) -> Result<u64, SignatureError> {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| SignatureError::ClockWentBackwards)
}

impl PackSignature {
    /// Sign `pack_bytes` for a window of `valid_for`, starting at `not_before`.
    ///
    /// `valid_for` is truncated to whole seconds, because that is the
    /// resolution both bounds are stored and signed at; a sub-second window is
    /// an [`SignatureError::EmptyWindow`], not a signature valid for an
    /// instant. A `valid_for` large enough to overflow saturates `not_after` at
    /// `u64::MAX` rather than wrapping into a window that has already closed.
    pub fn sign(
        pack_bytes: &[u8],
        secret: &PrivateIdentityMaterial,
        not_before: SystemTime,
        valid_for: Duration,
    ) -> Result<Self, SignatureError> {
        let nb = unix_secs(not_before)?;
        let na = nb.saturating_add(valid_for.as_secs());
        if na <= nb {
            return Err(SignatureError::EmptyWindow);
        }
        let signature = secret.sign(&signing_material(pack_bytes, nb, na));
        Ok(Self {
            schema_version: SIGNATURE_SCHEMA_VERSION,
            public_key: hex::encode(secret.public().as_bytes()),
            not_before: nb,
            not_after: na,
            signature: hex::encode(signature.0),
        })
    }

    pub fn parse(src: &str) -> Result<Self, SignatureError> {
        let sig: Self = toml::from_str(src).map_err(|e| SignatureError::Parse(e.to_string()))?;
        if sig.schema_version != SIGNATURE_SCHEMA_VERSION {
            return Err(SignatureError::UnsupportedSchema {
                found: sig.schema_version,
                supported: SIGNATURE_SCHEMA_VERSION,
            });
        }
        if sig.not_after <= sig.not_before {
            return Err(SignatureError::EmptyWindow);
        }
        Ok(sig)
    }

    /// Render the file. Written by hand rather than through a TOML serializer
    /// so this crate keeps `toml`'s parse-only feature set.
    pub fn to_toml(&self) -> String {
        format!(
            "# Detached signature for the pack of the same name (PLAN.md §11.3).\n\
             schema_version = {}\n\
             public_key = \"{}\"\n\
             not_before = {}\n\
             not_after = {}\n\
             signature = \"{}\"\n",
            self.schema_version, self.public_key, self.not_before, self.not_after, self.signature
        )
    }

    /// Verify over the pack's bytes, at `now`. Returns the signer's identity
    /// hash, **derived from the key in the file**, never read from it.
    pub fn verify(
        &self,
        pack_bytes: &[u8],
        now: SystemTime,
    ) -> Result<IdentityHash, SignatureError> {
        if self.not_after <= self.not_before {
            return Err(SignatureError::EmptyWindow);
        }
        let key_bytes =
            hex::decode(self.public_key.trim()).map_err(|_| SignatureError::MalformedPublicKey)?;
        let public = PublicIdentityMaterial::from_slice(&key_bytes)
            .map_err(|_| SignatureError::MalformedPublicKey)?;

        let sig_bytes =
            hex::decode(self.signature.trim()).map_err(|_| SignatureError::MalformedSignature)?;
        let sig_array: [u8; 64] =
            sig_bytes.as_slice().try_into().map_err(|_| SignatureError::MalformedSignature)?;
        let signature = prns_core::crypto::Ed25519Signature(sig_array);

        let material = signing_material(pack_bytes, self.not_before, self.not_after);
        public.verify(&material, &signature).map_err(|_| SignatureError::BadSignature)?;

        // Cryptography first, clock second: a forged signature is reported as
        // forged even when it is also stale.
        let now = unix_secs(now)?;
        if now < self.not_before {
            return Err(SignatureError::NotYetValid { valid_from: self.not_before });
        }
        if now > self.not_after {
            return Err(SignatureError::Expired { expired_at: self.not_after });
        }

        Ok(public.identity_hash())
    }

    /// Seconds of validity left at `now`, or 0 once the window has closed.
    /// What a launcher shows beside "signed community" so a user can see a
    /// signature going stale before it does.
    pub fn remaining_secs(&self, now: SystemTime) -> u64 {
        let n = match unix_secs(now) {
            Ok(n) => n,
            Err(_) => return 0,
        };
        // 0 before the window opens, not `not_after - now`: a future-dated
        // signature is not "valid for a long time", it is not valid yet.
        if n < self.not_before {
            return 0;
        }
        self.not_after.saturating_sub(n)
    }
}

/// What a pack's provenance turned out to be (`PLAN.md` §11.4).
///
/// Surfaced at import and at deploy **in these words** — see [`PackTrust::label`].
/// A user importing a community pack is told what that means at the moment they
/// do it, not in a document they will not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackTrust {
    /// Signed by a key the build or the operator names as the project's.
    FirstParty { signer: IdentityHash },
    /// Signed by a key this operator trusts.
    SignedCommunity { signer: IdentityHash },
    /// Signed, verified, in-window — by a key nobody here has said anything
    /// about.
    ///
    /// §11.4 lists three tiers; this is the fourth because the three assume a
    /// trust decision has been made. Calling this "signed community" would
    /// launder a stranger's key into an operator's trust list, and calling it
    /// "unsigned local" would hide that a key claimed the pack. It is gated
    /// exactly like unsigned.
    SignedUnknown { signer: IdentityHash },
    /// No signature file at all. A file someone wrote.
    UnsignedLocal,
}

impl PackTrust {
    /// The words §11.4 requires, for a UI to show verbatim.
    pub fn label(&self) -> &'static str {
        match self {
            Self::FirstParty { .. } => "first-party",
            Self::SignedCommunity { .. } => "signed community",
            Self::SignedUnknown { .. } => "signed by an unknown key",
            Self::UnsignedLocal => "unsigned local",
        }
    }

    /// One line for the moment of import or deploy.
    pub fn explanation(&self) -> &'static str {
        match self {
            Self::FirstParty { .. } => "signed by the project key.",
            Self::SignedCommunity { .. } => "signed by a key this node's operator trusts.",
            Self::SignedUnknown { .. } => {
                "the signature is valid, but nobody here vouches for the key that made it."
            }
            Self::UnsignedLocal => {
                "nobody signed this; it is a file someone wrote, trusted only as far as its author is."
            }
        }
    }

    pub fn signer(&self) -> Option<IdentityHash> {
        match self {
            Self::FirstParty { signer }
            | Self::SignedCommunity { signer }
            | Self::SignedUnknown { signer } => Some(*signer),
            Self::UnsignedLocal => None,
        }
    }
}

/// Which keys an operator trusts, and whether unvouched packs may be deployed.
///
/// Both key lists are operator configuration, `first_party_keys` included: a
/// key hardcoded in a binary cannot be rotated without a release, and this
/// project has no shipped key yet. Empty lists plus `allow_unsigned: false` is
/// the safe default — nothing deploys until someone says what they trust.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustPolicy {
    pub first_party_keys: Vec<IdentityHash>,
    pub trusted_keys: Vec<IdentityHash>,
    /// Deploy a pack nobody vouched for. Off by default; §11.4's tier 3 is
    /// "refused for deploy unless the operator explicitly allowed unsigned
    /// packs", and this is that switch.
    pub allow_unsigned: bool,
}

impl TrustPolicy {
    /// Trust nothing and refuse the unvouched. What a node runs before its
    /// operator has said anything.
    pub fn strict() -> Self {
        Self::default()
    }

    /// A local workstation's policy: run what is on disk.
    pub fn allowing_unsigned() -> Self {
        Self { allow_unsigned: true, ..Self::default() }
    }

    pub fn trusting(mut self, key: IdentityHash) -> Self {
        self.trusted_keys.push(key);
        self
    }

    pub fn with_first_party(mut self, key: IdentityHash) -> Self {
        self.first_party_keys.push(key);
        self
    }

    /// Tier for a signer that has already verified — or for no signature.
    pub fn classify(&self, signer: Option<IdentityHash>) -> PackTrust {
        match signer {
            None => PackTrust::UnsignedLocal,
            Some(s) if self.first_party_keys.contains(&s) => PackTrust::FirstParty { signer: s },
            Some(s) if self.trusted_keys.contains(&s) => PackTrust::SignedCommunity { signer: s },
            Some(s) => PackTrust::SignedUnknown { signer: s },
        }
    }

    /// May a pack at this tier be deployed?
    ///
    /// Import is always allowed — a user may look at anything — and deploy is
    /// where the tier bites, because deploy is what puts a description onto a
    /// node.
    pub fn may_deploy(&self, trust: &PackTrust) -> bool {
        match trust {
            PackTrust::FirstParty { .. } | PackTrust::SignedCommunity { .. } => true,
            PackTrust::SignedUnknown { .. } | PackTrust::UnsignedLocal => self.allow_unsigned,
        }
    }
}

/// Classify a pack from its bytes and its signature, if it has one.
///
/// A present signature must verify: a bad or stale one is an `Err`, never a
/// quiet demotion to unsigned. See the module docs.
pub fn verify_pack(
    pack_bytes: &[u8],
    signature: Option<&PackSignature>,
    policy: &TrustPolicy,
    now: SystemTime,
) -> Result<PackTrust, SignatureError> {
    match signature {
        None => Ok(PackTrust::UnsignedLocal),
        Some(sig) => Ok(policy.classify(Some(sig.verify(pack_bytes, now)?))),
    }
}

/// The signature file that belongs to `pack_path`, or `None` if there is none.
///
/// A signature that is present but unreadable is an error, for the same reason
/// an invalid one is: "I could not read it" must not become "there wasn't one".
pub fn read_signature_beside(pack_path: &Path) -> Result<Option<PackSignature>, SigFileError> {
    let mut name = pack_path.as_os_str().to_os_string();
    name.push(SIGNATURE_SUFFIX);
    let sig_path = std::path::PathBuf::from(name);
    match std::fs::read_to_string(&sig_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SigFileError::Io(e.to_string())),
        Ok(src) => Ok(Some(PackSignature::parse(&src).map_err(SigFileError::Signature)?)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigFileError {
    Io(String),
    Signature(SignatureError),
}

impl core::fmt::Display for SigFileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "signature file could not be read: {e}"),
            Self::Signature(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SigFileError {}

#[cfg(test)]
mod tests {
    use super::*;

    const PACK: &[u8] = b"schema_version = 1\nid = \"example\"\n";

    fn secret(seed: u8) -> PrivateIdentityMaterial {
        PrivateIdentityMaterial::from_slice(&[seed; 64]).expect("64 bytes is a secret key")
    }

    fn identity(seed: u8) -> IdentityHash {
        secret(seed).public().identity_hash()
    }

    fn t0() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn signed(seed: u8, valid_for: Duration) -> PackSignature {
        PackSignature::sign(PACK, &secret(seed), t0(), valid_for).unwrap()
    }

    #[test]
    fn a_signature_verifies_over_the_pack_that_was_signed() {
        let sig = signed(1, Duration::from_secs(3600));
        assert_eq!(sig.verify(PACK, t0()).unwrap(), identity(1));
    }

    #[test]
    fn an_edited_pack_does_not_verify() {
        let sig = signed(1, Duration::from_secs(3600));
        let tampered = b"schema_version = 1\nid = \"example\"\nwritable_paths = [\"/\"]\n";
        assert_eq!(sig.verify(tampered, t0()), Err(SignatureError::BadSignature));
    }

    /// The window is signed, so extending it invalidates the signature. A
    /// holder who could edit `not_after` in the file would have a signature
    /// that never expires, which is §11.3 defeated in one line of text.
    #[test]
    fn the_validity_window_cannot_be_extended_by_editing_the_file() {
        let mut sig = signed(1, Duration::from_secs(3600));
        sig.not_after += 10 * 365 * 24 * 3600;
        assert_eq!(sig.verify(PACK, t0()), Err(SignatureError::BadSignature));
    }

    #[test]
    fn a_signature_goes_stale_on_its_own() {
        let sig = signed(1, Duration::from_secs(3600));
        let later = t0() + Duration::from_secs(3601);
        assert_eq!(
            sig.verify(PACK, later),
            Err(SignatureError::Expired { expired_at: sig.not_after })
        );
        assert_eq!(sig.remaining_secs(later), 0);
        assert_eq!(sig.remaining_secs(t0()), 3600);
    }

    #[test]
    fn a_signature_is_not_valid_before_its_window_opens() {
        let sig = PackSignature::sign(
            PACK,
            &secret(1),
            t0() + Duration::from_secs(600),
            Duration::from_secs(3600),
        )
        .unwrap();
        assert_eq!(
            sig.verify(PACK, t0()),
            Err(SignatureError::NotYetValid { valid_from: sig.not_before })
        );
    }

    /// `platform-auth` signs with the same key. Neither signature may verify as
    /// the other, or a login exchange becomes a pack-signing oracle.
    #[test]
    fn the_pack_domain_is_not_the_auth_domain() {
        assert_ne!(&PACK_SIGNING_DOMAIN[..], &b"gaming-platform-prns/auth/v1\0\0\0\0"[..]);
        let sig = signed(1, Duration::from_secs(3600));
        // Signing material without the domain must not verify either.
        let mut naive = Vec::new();
        naive.extend_from_slice(&sig.not_before.to_be_bytes());
        naive.extend_from_slice(&sig.not_after.to_be_bytes());
        naive.extend_from_slice(PACK);
        let forged = PackSignature { signature: hex::encode(secret(1).sign(&naive).0), ..sig };
        assert_eq!(forged.verify(PACK, t0()), Err(SignatureError::BadSignature));
    }

    /// The load-bearing one. An operator who allowed unsigned packs must still
    /// hear about a signature that failed — silence there turns a revoked or
    /// tampered pack into a deployable one.
    #[test]
    fn an_expired_signature_is_an_error_not_an_unsigned_pack() {
        let sig = signed(1, Duration::from_secs(3600));
        let policy = TrustPolicy::allowing_unsigned();
        let later = t0() + Duration::from_secs(7200);
        assert!(matches!(
            verify_pack(PACK, Some(&sig), &policy, later),
            Err(SignatureError::Expired { .. })
        ));
        // And the absent case is the only one that reads as unsigned.
        assert_eq!(verify_pack(PACK, None, &policy, later).unwrap(), PackTrust::UnsignedLocal);
    }

    #[test]
    fn tiers_come_from_the_operators_lists() {
        let sig = signed(1, Duration::from_secs(3600));
        let strict = TrustPolicy::strict();
        assert_eq!(
            verify_pack(PACK, Some(&sig), &strict, t0()).unwrap(),
            PackTrust::SignedUnknown { signer: identity(1) }
        );

        let community = strict.clone().trusting(identity(1));
        assert_eq!(
            verify_pack(PACK, Some(&sig), &community, t0()).unwrap(),
            PackTrust::SignedCommunity { signer: identity(1) }
        );

        let first_party = TrustPolicy::strict().with_first_party(identity(1)).trusting(identity(1));
        assert_eq!(
            verify_pack(PACK, Some(&sig), &first_party, t0()).unwrap(),
            PackTrust::FirstParty { signer: identity(1) }
        );
    }

    /// Tier 3 is refused for deploy unless the operator said otherwise, and a
    /// stranger's valid signature is refused on the same terms — a signature
    /// from a key nobody trusts is not a vouching.
    #[test]
    fn an_unvouched_pack_does_not_deploy_by_default() {
        let strict = TrustPolicy::strict();
        assert!(!strict.may_deploy(&PackTrust::UnsignedLocal));
        assert!(!strict.may_deploy(&PackTrust::SignedUnknown { signer: identity(9) }));
        assert!(strict.may_deploy(&PackTrust::FirstParty { signer: identity(1) }));
        assert!(strict.may_deploy(&PackTrust::SignedCommunity { signer: identity(2) }));

        let lax = TrustPolicy::allowing_unsigned();
        assert!(lax.may_deploy(&PackTrust::UnsignedLocal));
        assert!(lax.may_deploy(&PackTrust::SignedUnknown { signer: identity(9) }));
    }

    /// §11.4 wants the tier shown in these words.
    #[test]
    fn every_tier_has_the_words_the_plan_uses() {
        assert_eq!(PackTrust::FirstParty { signer: identity(1) }.label(), "first-party");
        assert_eq!(PackTrust::SignedCommunity { signer: identity(1) }.label(), "signed community");
        assert_eq!(PackTrust::UnsignedLocal.label(), "unsigned local");
        assert_eq!(PackTrust::UnsignedLocal.signer(), None);
    }

    #[test]
    fn a_signature_file_round_trips_through_toml() {
        let sig = signed(3, Duration::from_secs(86_400));
        let parsed = PackSignature::parse(&sig.to_toml()).unwrap();
        assert_eq!(parsed, sig);
        assert_eq!(parsed.verify(PACK, t0()).unwrap(), identity(3));
    }

    #[test]
    fn a_newer_signature_schema_is_refused_rather_than_half_read() {
        let src = signed(1, Duration::from_secs(3600))
            .to_toml()
            .replace("schema_version = 1", "schema_version = 2");
        assert_eq!(
            PackSignature::parse(&src),
            Err(SignatureError::UnsupportedSchema { found: 2, supported: 1 })
        );
    }

    #[test]
    fn an_empty_window_is_refused_at_parse() {
        let sig = signed(1, Duration::from_secs(3600));
        let src = sig.to_toml().replace(
            &format!("not_after = {}", sig.not_after),
            &format!("not_after = {}", sig.not_before),
        );
        assert_eq!(PackSignature::parse(&src), Err(SignatureError::EmptyWindow));
    }

    #[test]
    fn a_signature_file_with_an_unknown_field_is_refused() {
        let src = signed(1, Duration::from_secs(3600)).to_toml() + "revoked = false\n";
        assert!(matches!(PackSignature::parse(&src), Err(SignatureError::Parse(_))));
    }

    #[test]
    fn a_truncated_key_or_signature_is_reported_as_malformed() {
        let sig = signed(1, Duration::from_secs(3600));
        let short_key = PackSignature { public_key: "aabb".into(), ..sig.clone() };
        assert_eq!(short_key.verify(PACK, t0()), Err(SignatureError::MalformedPublicKey));
        let short_sig = PackSignature { signature: "aabb".into(), ..sig };
        assert_eq!(short_sig.verify(PACK, t0()), Err(SignatureError::MalformedSignature));
    }

    #[test]
    fn a_signature_beside_a_pack_is_found_by_name() {
        let dir = std::env::temp_dir().join(format!("pack-sig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pack = dir.join("example.toml");
        std::fs::write(&pack, PACK).unwrap();
        assert_eq!(read_signature_beside(&pack), Ok(None));

        let sig = signed(4, Duration::from_secs(3600));
        std::fs::write(dir.join("example.toml.sig"), sig.to_toml()).unwrap();
        assert_eq!(read_signature_beside(&pack).unwrap().as_ref(), Some(&sig));

        std::fs::write(dir.join("example.toml.sig"), "not toml at all = =\n").unwrap();
        assert!(matches!(
            read_signature_beside(&pack),
            Err(SigFileError::Signature(SignatureError::Parse(_)))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
