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
            if complete.len() > 65_535 {
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
