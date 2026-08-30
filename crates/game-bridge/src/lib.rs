//! Game-agnostic Reticulum bridge.
//!
//! Phase 1 step 1 (`PLAN.md` §8): this crate exists so far only to pin the
//! engine and to fail the build if the pinned fork ever loses a patch the
//! platform depends on. Steps 2-6 fill it in with the parametrized copy of
//! `relay.rs` + `framing.rs` from `idan2025/Svencoop-Prns`.

use prns_core::engine::MAX_SEND_TO_LINK_PLAINTEXT_LEN;

/// The most plaintext one `SendToLink` call carries on the pinned engine.
///
/// `link_mdu(2048) = ((2048 - IFAC_MIN_LEN(1) - HEADER_MIN_LEN(19) -
/// TOKEN_OVERHEAD(48)) / 16) * 16 - 1 = 1967`
/// (`prns-core/src/routing/links/data.rs:35`).
///
/// **This is a fact about the fork, not about Prns.** Unpatched upstream sizes
/// the same constant off `BROADCAST_MTU = 500`, giving 431 — below a single
/// GoldSrc datagram. See ENGINE.md.
pub const LINK_PLAINTEXT_CAP: usize = 1967;

// If the pin ever moves to a rev without the link-MTU patch, every chunk-size
// assumption downstream (`MAX_CHUNK = 1900`) silently becomes a per-datagram
// fragmentation storm. Fail at compile time instead.
const _: () = assert!(
    MAX_SEND_TO_LINK_PLAINTEXT_LEN == LINK_PLAINTEXT_CAP,
    "pinned engine lost the link-MTU patch: MAX_SEND_TO_LINK_PLAINTEXT_LEN is not 1967 (upstream default is 431). See ENGINE.md."
);

#[cfg(test)]
mod tests {
    use personal_rns::prelude::Diagnostic;

    /// The whole server browser rests on `app_data` being readable off an
    /// announce: `AnnounceHeard` carries no aspect and no identity, and the
    /// destination hash is one-way, so the game id cannot be recovered from it
    /// (`PLAN.md` §3.1). If the pin loses this patch, Browse is unbuildable.
    #[test]
    fn announce_heard_exposes_app_data() {
        fn assert_field(d: &Diagnostic) -> Option<usize> {
            match d {
                Diagnostic::AnnounceHeard { app_data, .. } => Some(app_data.len()),
                _ => None,
            }
        }
        let _ = assert_field;
    }
}
