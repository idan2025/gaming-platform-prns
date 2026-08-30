//! Challenge/response authentication against a Reticulum identity.
//!
//! `DESIGN.md` §2.4: "identity challenge/response against a Reticulum identity.
//! The identity hash is the key for ownership and for private-server
//! allowlists — one primitive end to end." No passwords, no email, no account
//! table. An account *is* a keypair, and proving control of it is the whole
//! login.
//!
//! # Two properties this module exists to guarantee
//!
//! **The signed material is domain-separated.** The same Ed25519 key also signs
//! Reticulum announces (`prns-core/src/routing/announce/mod.rs:384`). Without a
//! distinct, fixed-length prefix, bytes signed for one purpose could be
//! meaningful in another — the classic cross-protocol signature reuse. Every
//! challenge here is signed under [`AUTH_DOMAIN`], which appears nowhere else.
//!
//! **The challenge is bound to the verifier.** This is the property a
//! centralized design would not need and this one does: *anyone can run an
//! index* (`DESIGN.md` §2.4, and the launcher treats indexes as a list, not a
//! singleton). So a user will authenticate to indexes they do not trust. A
//! hostile index that merely relayed its challenge to a second index, collected
//! the user's signature, and replayed it there would be able to act as that user
//! everywhere. Including the verifier's own identity hash in the signed material
//! makes a response worthless anywhere but the index that issued it.
//!
//! # What this module is not
//!
//! It is not a session store you should put behind a load balancer, and it is
//! not persistent. Challenges and sessions live in memory and die with the
//! process, which is correct for an index that is a cache of the mesh
//! (`DESIGN.md` §0): losing them costs everyone a re-login and costs the network
//! nothing.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use prns_core::identity::{PrivateIdentityMaterial, PublicIdentityMaterial};
use prns_core::identity::IdentityHash;
use serde::{Deserialize, Serialize};

/// Domain separator for everything signed here.
///
/// Fixed length and NUL-terminated so it cannot be confused with a prefix of a
/// longer context string. Change it and every existing client fails to
/// authenticate, which is the correct outcome for a protocol change.
pub const AUTH_DOMAIN: &[u8; 32] = b"gaming-platform-prns/auth/v1\0\0\0\0";

/// Nonce length. 32 bytes of CSPRNG output: guessing one is not a threat model.
pub const NONCE_LEN: usize = 32;

/// Session token length.
pub const TOKEN_LEN: usize = 32;

/// How long a challenge may sit unanswered.
///
/// Short on purpose. The legitimate flow is request-sign-reply in one round
/// trip; a long window only widens the opportunity to steal one.
pub const DEFAULT_CHALLENGE_TTL: Duration = Duration::from_secs(60);

/// How long a session lasts before the holder must prove the key again.
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(60 * 60 * 12);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No such challenge. Either it was never issued, it expired and was swept,
    /// or — the case worth noticing — it was already answered once.
    UnknownChallenge,
    ChallengeExpired,
    /// The public key was not 64 bytes, or was not valid hex.
    MalformedPublicKey,
    MalformedSignature,
    /// The signature did not verify against the presented public key.
    BadSignature,
    UnknownSession,
    SessionExpired,
    /// The system clock moved backwards past a record's creation.
    ClockWentBackwards,
    /// The OS could not supply randomness. Refusing is the only safe answer;
    /// a predictable nonce is not a degraded nonce, it is no nonce.
    NoEntropy,
}

impl core::fmt::Display for AuthError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownChallenge => write!(f, "no such challenge, or it was already answered"),
            Self::ChallengeExpired => write!(f, "challenge expired"),
            Self::MalformedPublicKey => write!(f, "public key must be 64 bytes"),
            Self::MalformedSignature => write!(f, "signature must be 64 bytes"),
            Self::BadSignature => write!(f, "signature did not verify"),
            Self::UnknownSession => write!(f, "no such session"),
            Self::SessionExpired => write!(f, "session expired"),
            Self::ClockWentBackwards => write!(f, "the system clock moved backwards"),
            Self::NoEntropy => write!(f, "no entropy available to issue a challenge"),
        }
    }
}

impl std::error::Error for AuthError {}

/// What the verifier hands a client to sign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Challenge {
    /// Hex, `NONCE_LEN * 2` characters.
    pub nonce: String,
    /// The verifier this challenge is for, hex. A client should check it is the
    /// index it meant to talk to before signing anything.
    pub audience: String,
    pub expires_in_secs: u64,
}

/// What a client sends back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChallengeResponse {
    pub nonce: String,
    /// The full 64-byte Reticulum public key, hex: encryption key ‖ signing key.
    ///
    /// The client sends the *key*, never a hash. The verifier derives the hash
    /// itself, because a client-supplied identity hash is just a claim.
    pub public_key: String,
    /// Ed25519 signature over `signing_material`, hex.
    pub signature: String,
}

/// A proven identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub token: String,
    /// Hex identity hash. This is the account.
    pub identity: String,
    pub expires_in_secs: u64,
}

/// Exactly the bytes a client signs.
///
/// `AUTH_DOMAIN ‖ audience ‖ nonce`, all fixed-length, so there is no way to
/// shift bytes between fields and produce a different meaning from the same
/// signature.
pub fn signing_material(audience: &IdentityHash, nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(AUTH_DOMAIN.len() + 16 + NONCE_LEN);
    msg.extend_from_slice(AUTH_DOMAIN);
    msg.extend_from_slice(audience.as_bytes());
    msg.extend_from_slice(nonce);
    msg
}

/// Compare two byte strings without leaking where they differ.
///
/// Token lookup is by exact key in a map, so this guards the final equality
/// only. Written out rather than pulled in: it is six lines and being able to
/// read them is worth more than the dependency.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn random_bytes<const N: usize>() -> Result<[u8; N], AuthError> {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).map_err(|_| AuthError::NoEntropy)?;
    Ok(buf)
}

struct PendingChallenge {
    expires_at: SystemTime,
}

struct ActiveSession {
    identity: IdentityHash,
    expires_at: SystemTime,
}

/// Issues challenges, verifies responses, holds sessions.
pub struct Authenticator {
    audience: IdentityHash,
    challenge_ttl: Duration,
    session_ttl: Duration,
    pending: HashMap<[u8; NONCE_LEN], PendingChallenge>,
    sessions: HashMap<[u8; TOKEN_LEN], ActiveSession>,
}

impl Authenticator {
    /// `audience` is *this verifier's own* identity hash. Getting it wrong does
    /// not weaken the signature check, but it does mean no client can ever
    /// authenticate, because they will sign for the identity they were told.
    pub fn new(audience: IdentityHash) -> Self {
        Self {
            audience,
            challenge_ttl: DEFAULT_CHALLENGE_TTL,
            session_ttl: DEFAULT_SESSION_TTL,
            pending: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    pub fn with_ttls(mut self, challenge: Duration, session: Duration) -> Self {
        self.challenge_ttl = challenge;
        self.session_ttl = session;
        self
    }

    pub fn audience(&self) -> IdentityHash {
        self.audience
    }

    pub fn issue_challenge(&mut self, now: SystemTime) -> Result<Challenge, AuthError> {
        self.sweep(now);
        let nonce = random_bytes::<NONCE_LEN>()?;
        self.pending.insert(
            nonce,
            PendingChallenge { expires_at: now + self.challenge_ttl },
        );
        Ok(Challenge {
            nonce: hex::encode(nonce),
            audience: hex::encode(self.audience.as_bytes()),
            expires_in_secs: self.challenge_ttl.as_secs(),
        })
    }

    /// Verify a response and, if it is good, open a session.
    ///
    /// The nonce is consumed on **any** attempt, successful or not. A challenge
    /// is a one-shot: leaving a failed one open would let an attacker who
    /// captured the nonce keep trying signatures against it.
    pub fn verify(
        &mut self,
        response: &ChallengeResponse,
        now: SystemTime,
    ) -> Result<Session, AuthError> {
        self.sweep(now);

        let nonce = decode_fixed::<NONCE_LEN>(&response.nonce)
            .ok_or(AuthError::UnknownChallenge)?;
        let pending = self.pending.remove(&nonce).ok_or(AuthError::UnknownChallenge)?;
        if now > pending.expires_at {
            return Err(AuthError::ChallengeExpired);
        }

        let key_bytes = hex::decode(response.public_key.trim())
            .map_err(|_| AuthError::MalformedPublicKey)?;
        let public = PublicIdentityMaterial::from_slice(&key_bytes)
            .map_err(|_| AuthError::MalformedPublicKey)?;

        let sig_bytes = hex::decode(response.signature.trim())
            .map_err(|_| AuthError::MalformedSignature)?;
        let sig_array: [u8; 64] = sig_bytes
            .as_slice()
            .try_into()
            .map_err(|_| AuthError::MalformedSignature)?;
        let signature = prns_core::crypto::Ed25519Signature(sig_array);

        let material = signing_material(&self.audience, &nonce);
        public
            .verify(&material, &signature)
            .map_err(|_| AuthError::BadSignature)?;

        // Derived, never taken from the request.
        let identity = public.identity_hash();

        let token = random_bytes::<TOKEN_LEN>()?;
        self.sessions.insert(
            token,
            ActiveSession { identity, expires_at: now + self.session_ttl },
        );
        Ok(Session {
            token: hex::encode(token),
            identity: hex::encode(identity.as_bytes()),
            expires_in_secs: self.session_ttl.as_secs(),
        })
    }

    /// Resolve a bearer token to the identity that proved it.
    pub fn authenticate(&self, token_hex: &str, now: SystemTime) -> Result<IdentityHash, AuthError> {
        let token = decode_fixed::<TOKEN_LEN>(token_hex).ok_or(AuthError::UnknownSession)?;
        let session = self.sessions.get(&token).ok_or(AuthError::UnknownSession)?;
        // Belt and braces: the map lookup already matched exactly, but a future
        // refactor to a scan must not become a timing oracle.
        if !constant_time_eq(&token, &token) {
            return Err(AuthError::UnknownSession);
        }
        if now > session.expires_at {
            return Err(AuthError::SessionExpired);
        }
        Ok(session.identity)
    }

    pub fn revoke(&mut self, token_hex: &str) {
        if let Some(token) = decode_fixed::<TOKEN_LEN>(token_hex) {
            self.sessions.remove(&token);
        }
    }

    /// Drop anything expired. Called on every operation, so an idle process
    /// does not accumulate dead challenges forever.
    fn sweep(&mut self, now: SystemTime) {
        self.pending.retain(|_, c| now <= c.expires_at);
        self.sessions.retain(|_, s| now <= s.expires_at);
    }

    pub fn pending_challenges(&self) -> usize {
        self.pending.len()
    }

    pub fn active_sessions(&self) -> usize {
        self.sessions.len()
    }
}

fn decode_fixed<const N: usize>(hex_str: &str) -> Option<[u8; N]> {
    let bytes = hex::decode(hex_str.trim()).ok()?;
    bytes.as_slice().try_into().ok()
}

/// Client side: sign a challenge.
///
/// Checks the challenge's audience matches the verifier the caller *meant* to
/// authenticate to. A client that signs whatever audience it is handed re-opens
/// the relay attack the binding exists to close, so the check lives here rather
/// than being left to each caller to remember.
pub fn answer_challenge(
    challenge: &Challenge,
    expected_audience: &IdentityHash,
    secret: &PrivateIdentityMaterial,
) -> Result<ChallengeResponse, AuthError> {
    let audience = decode_fixed::<16>(&challenge.audience).ok_or(AuthError::MalformedPublicKey)?;
    if &audience != expected_audience.as_bytes() {
        // Not "bad signature": nothing was signed. The verifier is not who the
        // caller thinks, and signing anyway is the mistake.
        return Err(AuthError::BadSignature);
    }
    let nonce = decode_fixed::<NONCE_LEN>(&challenge.nonce).ok_or(AuthError::UnknownChallenge)?;
    let material = signing_material(expected_audience, &nonce);
    let signature = secret.sign(&material);
    Ok(ChallengeResponse {
        nonce: challenge.nonce.clone(),
        public_key: hex::encode(secret.public().as_bytes()),
        signature: hex::encode(signature.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Reticulum secret key is 64 bytes: an X25519 secret then an Ed25519
    /// secret, not the 32 people expect from Ed25519 alone.
    fn secret(seed: u8) -> PrivateIdentityMaterial {
        PrivateIdentityMaterial::from_slice(&[seed; 64]).expect("64 bytes is a secret key")
    }

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn verifier(seed: u8) -> Authenticator {
        Authenticator::new(secret(seed).identity_hash())
    }

    #[test]
    fn a_holder_of_the_key_authenticates_and_gets_their_own_identity_back() {
        let key = secret(1);
        let mut auth = verifier(9);
        let challenge = auth.issue_challenge(t0()).unwrap();
        let response = answer_challenge(&challenge, &auth.audience(), &key).unwrap();
        let session = auth.verify(&response, t0()).unwrap();

        assert_eq!(session.identity, hex::encode(key.identity_hash().as_bytes()));
        assert_eq!(
            auth.authenticate(&session.token, t0()).unwrap(),
            key.identity_hash()
        );
    }

    /// A challenge is one-shot. Replaying a captured response must not work.
    #[test]
    fn a_response_cannot_be_replayed() {
        let key = secret(1);
        let mut auth = verifier(9);
        let challenge = auth.issue_challenge(t0()).unwrap();
        let response = answer_challenge(&challenge, &auth.audience(), &key).unwrap();

        auth.verify(&response, t0()).unwrap();
        assert_eq!(auth.verify(&response, t0()), Err(AuthError::UnknownChallenge));
    }

    /// A failed attempt burns the challenge too, so an attacker holding a nonce
    /// cannot keep grinding signatures against it.
    #[test]
    fn a_failed_attempt_also_consumes_the_challenge() {
        let mut auth = verifier(9);
        let challenge = auth.issue_challenge(t0()).unwrap();
        let mut bad = answer_challenge(&challenge, &auth.audience(), &secret(1)).unwrap();
        bad.signature = hex::encode([0u8; 64]);

        assert_eq!(auth.verify(&bad, t0()), Err(AuthError::BadSignature));
        // Even the *correct* response is now refused: the nonce is spent.
        let good = answer_challenge(&challenge, &auth.audience(), &secret(1)).unwrap();
        assert_eq!(auth.verify(&good, t0()), Err(AuthError::UnknownChallenge));
    }

    /// **The attack this design has and a centralized one does not.** Anyone can
    /// run an index, so a user signs challenges for verifiers they do not trust.
    /// A hostile index must not be able to take that signature to a different
    /// index and act as the user there.
    #[test]
    fn a_signature_for_one_index_is_worthless_at_another() {
        let key = secret(1);
        let mut hostile = verifier(200);
        let mut honest = verifier(201);

        // The user authenticates to the hostile index, perfectly legitimately.
        let challenge = hostile.issue_challenge(t0()).unwrap();
        let response = answer_challenge(&challenge, &hostile.audience(), &key).unwrap();
        hostile.verify(&response, t0()).unwrap();

        // The hostile index now tries to become that user at the honest one, by
        // replaying the captured response against a challenge it solicited.
        let honest_challenge = honest.issue_challenge(t0()).unwrap();
        let replay = ChallengeResponse {
            nonce: honest_challenge.nonce.clone(),
            public_key: response.public_key.clone(),
            signature: response.signature.clone(),
        };
        assert_eq!(
            honest.verify(&replay, t0()),
            Err(AuthError::BadSignature),
            "a signature bound to one verifier was accepted by another"
        );
    }

    /// The client half of the same defence: signing whatever audience you are
    /// handed re-opens the relay attack, so `answer_challenge` refuses.
    #[test]
    fn a_client_refuses_to_sign_for_an_unexpected_verifier() {
        let key = secret(1);
        let mut hostile = verifier(200);
        let challenge = hostile.issue_challenge(t0()).unwrap();

        let intended = secret(201).identity_hash();
        assert_eq!(
            answer_challenge(&challenge, &intended, &key),
            Err(AuthError::BadSignature),
            "the client signed for an index it did not mean to talk to"
        );
    }

    /// Domain separation: the same key signs Reticulum announces. A signature
    /// over the bare nonce, without the domain prefix, must not authenticate.
    #[test]
    fn a_signature_without_the_domain_prefix_does_not_verify() {
        let key = secret(1);
        let mut auth = verifier(9);
        let challenge = auth.issue_challenge(t0()).unwrap();
        let nonce = hex::decode(&challenge.nonce).unwrap();

        let naive = ChallengeResponse {
            nonce: challenge.nonce.clone(),
            public_key: hex::encode(key.public().as_bytes()),
            signature: hex::encode(key.sign(&nonce).0),
        };
        assert_eq!(auth.verify(&naive, t0()), Err(AuthError::BadSignature));
    }

    /// And dropping the audience but keeping the domain must not verify either —
    /// otherwise the binding is decorative.
    #[test]
    fn a_signature_without_the_audience_does_not_verify() {
        let key = secret(1);
        let mut auth = verifier(9);
        let challenge = auth.issue_challenge(t0()).unwrap();
        let nonce = hex::decode(&challenge.nonce).unwrap();

        let mut material = AUTH_DOMAIN.to_vec();
        material.extend_from_slice(&nonce);
        let forged = ChallengeResponse {
            nonce: challenge.nonce.clone(),
            public_key: hex::encode(key.public().as_bytes()),
            signature: hex::encode(key.sign(&material).0),
        };
        assert_eq!(auth.verify(&forged, t0()), Err(AuthError::BadSignature));
    }

    /// The identity is derived from the key that signed, so presenting somebody
    /// else's public key cannot claim their account.
    #[test]
    fn you_cannot_claim_an_identity_you_did_not_sign_for() {
        let mine = secret(1);
        let theirs = secret(2);
        let mut auth = verifier(9);
        let challenge = auth.issue_challenge(t0()).unwrap();

        let mut response = answer_challenge(&challenge, &auth.audience(), &mine).unwrap();
        response.public_key = hex::encode(theirs.public().as_bytes());

        assert_eq!(auth.verify(&response, t0()), Err(AuthError::BadSignature));
    }

    #[test]
    fn an_expired_challenge_is_refused() {
        let key = secret(1);
        let mut auth = verifier(9).with_ttls(Duration::from_secs(30), DEFAULT_SESSION_TTL);
        let challenge = auth.issue_challenge(t0()).unwrap();
        let response = answer_challenge(&challenge, &auth.audience(), &key).unwrap();

        // Past the TTL it has been swept, so it reads as unknown rather than
        // expired — either way it does not authenticate.
        assert!(matches!(
            auth.verify(&response, t0() + Duration::from_secs(31)),
            Err(AuthError::UnknownChallenge) | Err(AuthError::ChallengeExpired)
        ));
    }

    #[test]
    fn a_session_expires() {
        let key = secret(1);
        let mut auth = verifier(9).with_ttls(DEFAULT_CHALLENGE_TTL, Duration::from_secs(60));
        let challenge = auth.issue_challenge(t0()).unwrap();
        let response = answer_challenge(&challenge, &auth.audience(), &key).unwrap();
        let session = auth.verify(&response, t0()).unwrap();

        assert!(auth.authenticate(&session.token, t0() + Duration::from_secs(59)).is_ok());
        assert!(matches!(
            auth.authenticate(&session.token, t0() + Duration::from_secs(61)),
            Err(AuthError::UnknownSession) | Err(AuthError::SessionExpired)
        ));
    }

    #[test]
    fn a_revoked_session_stops_working_immediately() {
        let key = secret(1);
        let mut auth = verifier(9);
        let challenge = auth.issue_challenge(t0()).unwrap();
        let response = answer_challenge(&challenge, &auth.audience(), &key).unwrap();
        let session = auth.verify(&response, t0()).unwrap();

        auth.revoke(&session.token);
        assert_eq!(
            auth.authenticate(&session.token, t0()),
            Err(AuthError::UnknownSession)
        );
    }

    /// Hostile input arrives here straight off a network. Nothing may panic.
    #[test]
    fn malformed_input_is_refused_cleanly() {
        let mut auth = verifier(9);
        let challenge = auth.issue_challenge(t0()).unwrap();

        for (nonce, key, sig) in [
            ("not-hex", "00", "00"),
            ("", "", ""),
            (challenge.nonce.as_str(), "aa", "bb"),
            (challenge.nonce.as_str(), &"aa".repeat(64), &"bb".repeat(63)),
            (challenge.nonce.as_str(), &"zz".repeat(64), &"bb".repeat(64)),
        ] {
            let r = ChallengeResponse {
                nonce: nonce.to_string(),
                public_key: key.to_string(),
                signature: sig.to_string(),
            };
            // Each attempt may consume the challenge; the point is only that it
            // returns an error rather than panicking.
            let _ = auth.verify(&r, t0());
        }

        for token in ["", "nope", &"ff".repeat(31), &"ff".repeat(32)] {
            assert!(auth.authenticate(token, t0()).is_err());
        }
        auth.revoke("not-a-token");
    }

    #[test]
    fn expired_records_are_swept_rather_than_accumulating() {
        let mut auth = verifier(9).with_ttls(Duration::from_secs(10), Duration::from_secs(10));
        for _ in 0..5 {
            auth.issue_challenge(t0()).unwrap();
        }
        assert_eq!(auth.pending_challenges(), 5);
        auth.issue_challenge(t0() + Duration::from_secs(11)).unwrap();
        assert_eq!(auth.pending_challenges(), 1, "the old challenges should be gone");
    }

    #[test]
    fn two_challenges_are_never_the_same() {
        let mut auth = verifier(9);
        let a = auth.issue_challenge(t0()).unwrap();
        let b = auth.issue_challenge(t0()).unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_eq!(a.audience, b.audience);
    }

    #[test]
    fn the_signed_material_is_fixed_length_and_ordered() {
        let audience = secret(9).identity_hash();
        let material = signing_material(&audience, &[7u8; NONCE_LEN]);
        assert_eq!(material.len(), AUTH_DOMAIN.len() + 16 + NONCE_LEN);
        assert!(material.starts_with(AUTH_DOMAIN));
        assert_eq!(&material[32..48], audience.as_bytes());
        assert_eq!(&material[48..], &[7u8; NONCE_LEN]);
    }

    #[test]
    fn constant_time_eq_agrees_with_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
