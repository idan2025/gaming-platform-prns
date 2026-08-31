//! Game-agnostic Reticulum bridge.
//!
//! Generalizes the single-game bridge in `idan2025/Svencoop-Prns` v0.1.10.
//! Extraction is one-directional (`PLAN.md` §5): this crate copies from that
//! repo and parametrizes; that repo never depends on this one.
//!
//! `PLAN.md` §8 phase 1 is complete here, and phase 2's browse core with it:
//! the engine is pinned to a patched Prns fork (`ENGINE.md`), the relay and
//! framing are parametrized by a game pack, servers announce the §3.3 record,
//! links can be allowlisted, transit is opt-in, and `BridgeSession::browse`
//! lists and filters what a node has heard with no index and no internet.

pub mod a2s;
pub mod announce;
pub mod browse;
pub mod config;
pub mod content;
pub mod details;
pub mod framing;
pub mod launch;
pub mod pack;
pub mod profile;
pub mod relay;
pub mod signing;
pub mod stream;

pub use announce::{AnnounceInfo, AnnounceRecord, DecodeError, EncodeError};
pub use browse::{browse, BrowseFilter, BrowseQuery, SortBy};
pub use pack::{GamePack, PackError};
pub use config::{BridgeConfig, BridgeRole, BrowserArgs, ClientArgs, RelayArgs, ServerArgs};
pub use profile::{GameProfile, GameTransport, ASPECT_CLIENT, ASPECT_SERVER};
pub use relay::{
    run_bridge, server_announce_app_data, server_announce_name_bytes, BridgeSession,
    ConnectedClient, DiscoveredServer,
};

use prns_core::engine::MAX_SEND_TO_LINK_PLAINTEXT_LEN;

/// The most plaintext one `SendToLink` call carries on the pinned engine.
///
/// `link_mdu(2048) = ((2048 - IFAC_MIN_LEN(1) - HEADER_MIN_LEN(19) -
/// TOKEN_OVERHEAD(48)) / 16) * 16 - 1 = 1967`
/// (`prns-core/src/routing/links/data.rs:35`).
///
/// **This is a fact about the fork, not about Prns.** Unpatched upstream sizes
/// the same constant off `BROADCAST_MTU = 500`, giving 431 — below a single
/// GoldSrc datagram. See `ENGINE.md`.
pub const LINK_PLAINTEXT_CAP: usize = 1967;

// If the pin ever moves to a rev without the link-MTU patch, every chunk-size
// assumption downstream (`framing::MAX_CHUNK = 1900`) silently becomes a
// per-datagram fragmentation storm. Fail at compile time instead.
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
