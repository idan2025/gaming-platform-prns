//! Length-prefix framing for game datagrams over Reticulum links.
//!
//! Copied from `idan2025/Svencoop-Prns` `src/framing.rs` at v0.1.10 and kept
//! **byte-for-byte wire-compatible with it**. The extraction is one-directional
//! (`PLAN.md` §5): fixes here do not flow back automatically, and a change to
//! the wire format here breaks joining a deployed standalone Sven server.
//!
//! Reticulum's link payload ceiling (the MDU) is smaller than a game datagram
//! can be — a GoldSrc signon/world-state packet runs to ~1400 bytes. A single
//! `SendToLink` call cannot carry an arbitrary datagram, so the bridge
//! fragments each one into MDU-sized chunks and reassembles on the far side.
//!
//! Wire format per chunk: one header byte + up to `MAX_CHUNK` payload bytes.
//!   header bit 0 (0x01): final chunk of this datagram
//! Every other header bit is reserved and MUST be written as zero.
//!
//! **The reserved bits are not free.** `Reassembler::push` masks only
//! `FLAG_FINAL` and ignores the rest, so a deployed peer silently treats a
//! header carrying, say, a channel id in bits 1-3 as an ordinary chunk and
//! corrupts the stream instead of rejecting it. `PLAN.md` §3.3/§5 therefore
//! freeze this format as framing generation 1 / channel 0: multi-port games
//! get channel ids only behind announce-advertised version negotiation, never
//! by starting to set a reserved bit.
//!
//! Reassembly is per-link, per-direction state; the relay owns one buffer for
//! each direction it cares about.
//!
//! # Generation 2: channel ids in bits 1-3
//!
//! `GAMES.md` §3: a Source-engine server wants more than one port reachable
//! (game, RCON, SourceTV), and a destination fronts one. Bits 1-3 of the header
//! become a channel id, so one destination carries up to eight ports.
//!
//! **The two generations live in two types on purpose.** `Reassembler` is
//! generation 1 and stays exactly as deployed peers implement it — it is the
//! model of what a v0.1.10 peer does with a header it does not understand, and
//! `reserved_header_bits_are_ignored_not_rejected` pins that. `ChannelReassembler`
//! is generation 2. Teaching the v1 type about channels would delete the only
//! executable record of the hazard.
//!
//! Two properties make generation 2 safe to deploy into a network of v1 peers:
//!
//!  - **Channel 0 is byte-identical to generation 1.** `frame_on_channel(d, 0)`
//!    emits exactly what `frame(d)` emits, so a v2 sender talking to a v1 peer
//!    on channel 0 is indistinguishable from a v1 sender.
//!  - **A v1 sender decodes as channel 0 on a v2 receiver**, because its
//!    reserved bits are zero.
//!
//! What is *not* safe, and is what the gate exists for: a non-zero channel sent
//! to a v1 peer. That peer merges the channels into one corrupt stream rather
//! than rejecting them. So a non-zero channel may only be sent to a peer whose
//! announce advertised `FRAMING_V2` (`announce.rs`'s `protocol_version`), and a
//! server never *initiates* one — it answers on the channel a chunk arrived on.
//! `PLAN.md` §3.3, §5.

/// Largest datagram reassembly will hand back. A UDP datagram cannot exceed
/// this, so anything longer is a malformed or hostile stream rather than a big
/// packet.
const MAX_DATAGRAM_LEN: usize = 65_535;

/// The largest payload handed to `SendToLink` in one chunk.
///
/// The pinned engine's `MAX_SEND_TO_LINK_PLAINTEXT_LEN` is `link_mdu(2048)` =
/// **1967** bytes (`ENGINE.md`; upstream Prns computes 431 for the same
/// constant). 1900 stays under that with room for the one-byte header, so a
/// typical ~1400-byte game datagram still rides in a single chunk with no
/// application-layer fragmentation.
///
/// This value is part of the wire contract only in the weak sense that both
/// ends must accept chunks of at least this size; a sender may use less.
pub const MAX_CHUNK: usize = 1900;

const FLAG_FINAL: u8 = 0x01;

/// Header bits carrying the channel id in framing generation 2.
const CHANNEL_MASK: u8 = 0x0E;
const CHANNEL_SHIFT: u32 = 1;

/// Highest channel id the three header bits can express.
pub const MAX_CHANNEL: u8 = 7;

/// The channel every peer speaks, and the one frozen to generation 1's wire
/// format forever: the game's own port.
pub const CHANNEL_GAME: u8 = 0;

/// `protocol_version` in the announce record for the original framing.
pub const FRAMING_V1: u8 = 1;

/// `protocol_version` announcing that this peer understands channel ids.
pub const FRAMING_V2: u8 = 2;

/// Whether a peer advertising this `protocol_version` can be sent a non-zero
/// channel without corrupting it.
///
/// The comparison is `>=` rather than `==` so a future generation 3 does not
/// silently lose the ability to carry channels.
pub const fn supports_channels(protocol_version: u8) -> bool {
    protocol_version >= FRAMING_V2
}

/// A channel id was outside what three header bits can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelTooLarge(pub u8);

impl core::fmt::Display for ChannelTooLarge {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "channel {} is over the maximum of {MAX_CHANNEL}", self.0)
    }
}

impl std::error::Error for ChannelTooLarge {}

/// Frame one game datagram into a sequence of link-packet payloads (each
/// `<= MAX_CHUNK + 1`, ready for `SendToLink`). Returns the chunks in order.
pub fn frame(datagram: &[u8]) -> Vec<Vec<u8>> {
    if datagram.is_empty() {
        // A zero-length datagram is degenerate; emit a single final chunk with
        // no payload so the reassembler still produces an empty datagram.
        return vec![vec![FLAG_FINAL]];
    }
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < datagram.len() {
        let end = (offset + MAX_CHUNK).min(datagram.len());
        let mut chunk = Vec::with_capacity(1 + (end - offset));
        let is_final = end == datagram.len();
        chunk.push(if is_final { FLAG_FINAL } else { 0u8 });
        chunk.extend_from_slice(&datagram[offset..end]);
        out.push(chunk);
        offset = end;
    }
    out
}

/// Frame one datagram onto a channel (framing generation 2).
///
/// `channel == CHANNEL_GAME` produces bytes identical to `frame`, which is what
/// keeps a v2 sender safe to point at a v1 peer. Any other channel **must not**
/// be sent to a peer that has not advertised `FRAMING_V2` — see the module docs
/// for what that peer does with it.
pub fn frame_on_channel(datagram: &[u8], channel: u8) -> Result<Vec<Vec<u8>>, ChannelTooLarge> {
    if channel > MAX_CHANNEL {
        return Err(ChannelTooLarge(channel));
    }
    let tag = (channel << CHANNEL_SHIFT) & CHANNEL_MASK;
    let mut chunks = frame(datagram);
    for chunk in &mut chunks {
        chunk[0] |= tag;
    }
    Ok(chunks)
}

/// Read the channel id out of a chunk header.
pub fn channel_of(chunk: &[u8]) -> Option<u8> {
    chunk
        .first()
        .map(|header| (header & CHANNEL_MASK) >> CHANNEL_SHIFT)
}

/// Generation 2's reassembly buffer: one buffer per channel.
///
/// A v1 peer's chunks carry zero in the channel bits, so they reassemble on
/// `CHANNEL_GAME` with no special case — a v2 receiver needs no knowledge of
/// which generation the sender is.
#[derive(Default)]
pub struct ChannelReassembler {
    buffers: std::collections::BTreeMap<u8, Vec<u8>>,
}

impl ChannelReassembler {
    /// Feed one chunk. Returns the channel and the datagram when a final chunk
    /// completes one.
    pub fn push(&mut self, chunk: &[u8]) -> Option<(u8, Vec<u8>)> {
        if chunk.is_empty() {
            return None;
        }
        let header = chunk[0];
        let channel = (header & CHANNEL_MASK) >> CHANNEL_SHIFT;
        let buf = self.buffers.entry(channel).or_default();
        buf.extend_from_slice(&chunk[1..]);
        if header & FLAG_FINAL == 0 {
            return None;
        }
        let complete = std::mem::take(buf);
        // Same 64 KiB guard as generation 1, per channel: a peer that never
        // sends a final chunk must not grow a buffer without bound, and it can
        // now do that eight times over.
        if complete.len() > MAX_DATAGRAM_LEN {
            return None;
        }
        Some((channel, complete))
    }

    pub fn reset(&mut self) {
        self.buffers.clear();
    }
}

/// A per-direction reassembly buffer. Feed it chunks from `frame`; `push`
/// returns `Some(complete_datagram)` when the final chunk arrives.
#[derive(Default)]
pub struct Reassembler {
    buf: Vec<u8>,
}

impl Reassembler {
    pub fn push(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        if chunk.is_empty() {
            return None;
        }
        let header = chunk[0];
        let body = &chunk[1..];
        self.buf.extend_from_slice(body);
        if header & FLAG_FINAL != 0 {
            let complete = std::mem::take(&mut self.buf);
            // Guard: a UDP datagram is at most ~64 KiB. If reassembly somehow
            // runs past that, drop the buffer to avoid unbounded growth on a
            // malformed stream.
            if complete.len() > MAX_DATAGRAM_LEN {
                self.buf.clear();
                return None;
            }
            Some(complete)
        } else {
            None
        }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_datagram_is_one_final_chunk() {
        let chunks = frame(b"hello");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0][0], FLAG_FINAL);
        assert_eq!(&chunks[0][1..], b"hello");
    }

    #[test]
    fn empty_datagram_round_trips_as_empty() {
        let chunks = frame(b"");
        assert_eq!(chunks, vec![vec![FLAG_FINAL]]);
        let mut r = Reassembler::default();
        assert_eq!(r.push(&chunks[0]), Some(Vec::new()));
    }

    #[test]
    fn large_datagram_splits_and_reassembles() {
        let datagram: Vec<u8> = (0..(MAX_CHUNK * 2 + 7)).map(|i| (i % 251) as u8).collect();
        let chunks = frame(&datagram);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0][0], 0);
        assert_eq!(chunks[1][0], 0);
        assert_eq!(chunks[2][0], FLAG_FINAL);
        assert!(chunks.iter().all(|c| c.len() <= MAX_CHUNK + 1));

        let mut r = Reassembler::default();
        assert_eq!(r.push(&chunks[0]), None);
        assert_eq!(r.push(&chunks[1]), None);
        assert_eq!(r.push(&chunks[2]), Some(datagram));
    }

    /// The freeze in the module docs, as an executable statement: a deployed
    /// v0.1.10 peer ignores every header bit but `FLAG_FINAL`. If a future
    /// generation ever sets one ungated, this is the behaviour it inherits.
    #[test]
    fn reserved_header_bits_are_ignored_not_rejected() {
        let mut r = Reassembler::default();
        assert_eq!(r.push(&[0xfe, b'a']), None, "non-final with every reserved bit set");
        assert_eq!(r.push(&[0xff, b'b']), Some(b"ab".to_vec()), "final with every reserved bit set");
    }

    /// The property that makes generation 2 deployable: on channel 0 it emits
    /// generation 1's exact bytes. If this ever fails, every deployed peer is
    /// receiving a format it will merge into garbage.
    #[test]
    fn channel_zero_is_byte_identical_to_generation_one() {
        for datagram in [b"".as_slice(), b"hello", &vec![7u8; MAX_CHUNK * 2 + 3]] {
            assert_eq!(
                frame_on_channel(datagram, CHANNEL_GAME).unwrap(),
                frame(datagram),
                "channel 0 must be frozen to the v1 wire format"
            );
        }
    }

    /// The other half: a v1 sender's chunks land on channel 0 at a v2 receiver,
    /// with no version negotiation and no special case.
    #[test]
    fn a_generation_one_sender_reassembles_as_channel_zero() {
        let mut r = ChannelReassembler::default();
        let chunks = frame(b"legacy peer");
        let mut out = None;
        for chunk in &chunks {
            out = r.push(chunk);
        }
        assert_eq!(out, Some((CHANNEL_GAME, b"legacy peer".to_vec())));
    }

    #[test]
    fn channels_do_not_braid_together() {
        let mut r = ChannelReassembler::default();
        let game: Vec<u8> = (0..(MAX_CHUNK + 40)).map(|i| (i % 251) as u8).collect();
        let rcon = b"rcon: status".to_vec();

        let game_chunks = frame_on_channel(&game, CHANNEL_GAME).unwrap();
        let rcon_chunks = frame_on_channel(&rcon, 3).unwrap();
        assert_eq!(game_chunks.len(), 2);

        // Interleaved on the wire, which is exactly what one link carrying two
        // ports produces.
        assert_eq!(r.push(&game_chunks[0]), None);
        assert_eq!(r.push(&rcon_chunks[0]), Some((3, rcon)));
        assert_eq!(r.push(&game_chunks[1]), Some((CHANNEL_GAME, game)));
    }

    #[test]
    fn a_channel_id_survives_the_header_round_trip() {
        for channel in 0..=MAX_CHANNEL {
            let chunks = frame_on_channel(b"x", channel).unwrap();
            assert_eq!(channel_of(&chunks[0]), Some(channel));
            assert_eq!(chunks[0][0] & FLAG_FINAL, FLAG_FINAL, "the final bit still means final");
        }
        assert_eq!(frame_on_channel(b"x", 8), Err(ChannelTooLarge(8)));
    }

    /// The gate the deployed network depends on: `protocol_version` decides
    /// whether a peer may be sent a non-zero channel at all.
    #[test]
    fn only_a_v2_peer_may_be_sent_a_channel() {
        assert!(!supports_channels(FRAMING_V1));
        assert!(supports_channels(FRAMING_V2));
        assert!(supports_channels(3), "a later generation keeps the capability");
        assert!(!supports_channels(0), "an unversioned peer is a v1 peer");
    }

    #[test]
    fn an_unfinished_channel_cannot_grow_without_bound() {
        let mut r = ChannelReassembler::default();
        let body = vec![0u8; 40_000];
        let mut chunk = vec![(2 << CHANNEL_SHIFT) & CHANNEL_MASK];
        chunk.extend_from_slice(&body);
        assert_eq!(r.push(&chunk), None);
        let mut fin = vec![((2 << CHANNEL_SHIFT) & CHANNEL_MASK) | FLAG_FINAL];
        fin.extend_from_slice(&body);
        assert_eq!(r.push(&fin), None, "80 KB exceeds the 64 KiB guard");
    }

    #[test]
    fn oversized_reassembly_is_dropped() {
        let mut r = Reassembler::default();
        let body = vec![0u8; 40_000];
        let mut chunk = vec![0u8];
        chunk.extend_from_slice(&body);
        assert_eq!(r.push(&chunk), None);
        let mut fin = vec![FLAG_FINAL];
        fin.extend_from_slice(&body);
        assert_eq!(r.push(&fin), None, "80 KB exceeds the 64 KiB guard");
        assert_eq!(r.push(&[FLAG_FINAL, b'x']), Some(b"x".to_vec()), "buffer was cleared");
    }
}
