//! The index's Reticulum wire format.
//!
//! `DESIGN.md` §2.4 asks for the index over **both** HTTPS and a Reticulum
//! destination. The HTTP half is a convenience for people with internet. This
//! half is the one that matters to the architecture: a client on a mesh with no
//! internet at all must still be able to ask an index for a list, or "an index
//! is a convenience, never a dependency" quietly becomes "an index is a
//! convenience if you have internet".
//!
//! # Rows reuse the announce record
//!
//! A row is a destination hash, a hop count, and then **the same
//! `AnnounceRecord` encoding the server itself announced**
//! (`game_bridge::announce`). Not a parallel format: a launcher already has that
//! decoder, it is already tested, and a second encoding of the same facts is a
//! second place for them to disagree. A legacy row — a deployed v0.1.x peer with
//! nothing but a name — gets its own short variant, because it genuinely has no
//! record to send.
//!
//! # Everything is bounded and truncation is visible
//!
//! A response rides one link packet. A mesh query that needed reassembly could
//! half-arrive, and a listing is exactly the kind of thing a caller retries. So
//! the encoder fills to a cap, sets a flag, and reports how many rows matched in
//! total — a client that sees `truncated` knows to narrow its filter rather than
//! believing it has seen the mesh.

use game_bridge::announce::{decode as decode_announce, AnnounceInfo, AnnounceRecord};
use game_bridge::DiscoveredServer;

/// Endpoint an index registers and a client requests.
pub const QUERY_ENDPOINT_ID: &str = "/platform-index/servers/1";

/// Reticulum app name for an index destination. Indexes announce under this, so
/// **finding an index is itself an announce**, with no bootstrap list anywhere.
pub const INDEX_APP_NAME: &str = "platform-index";

/// Aspect of an index destination that answers queries.
pub const INDEX_ASPECT: &str = "query";

pub const QUERY_SCHEMA: u8 = 1;

/// Keep a response inside one link packet.
pub const MAX_RESPONSE_LEN: usize = 1800;

const ROW_LEGACY: u8 = 0;
const ROW_RECORD: u8 = 1;

/// A query, as it travels over a link.
///
/// Deliberately a small subset of `BrowseFilter`: a mesh query costs a round
/// trip to somebody else's node, so it carries the filters that meaningfully cut
/// the result and leaves the cosmetic ones to the client, which has the rows
/// anyway once they arrive.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexQuery {
    /// Empty means any game.
    pub game_id: String,
    pub max_hops: Option<u8>,
    pub has_players: bool,
    pub include_legacy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    Truncated,
    InvalidUtf8,
    UnsupportedSchema(u8),
    BadRow,
    TooLarge(usize),
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "index message is truncated"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 in an index message"),
            Self::UnsupportedSchema(v) => write!(f, "index schema {v} is not supported"),
            Self::BadRow => write!(f, "a row did not decode"),
            Self::TooLarge(n) => write!(f, "index message is {n} bytes, over the cap"),
        }
    }
}

impl std::error::Error for WireError {}

/// One row as a client receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRow {
    pub destination_hash: [u8; 16],
    pub hops: u8,
    pub info: AnnounceInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResult {
    pub rows: Vec<WireRow>,
    /// More matched than fit. Narrow the filter.
    pub truncated: bool,
    /// How many matched in total, before truncation. Saturates at u16::MAX,
    /// which is a bigger mesh than this format is for.
    pub total_matched: u16,
}

fn take<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], WireError> {
    let end = pos.checked_add(n).ok_or(WireError::Truncated)?;
    if end > buf.len() {
        return Err(WireError::Truncated);
    }
    let out = &buf[*pos..end];
    *pos = end;
    Ok(out)
}

fn take_byte(buf: &[u8], pos: &mut usize) -> Result<u8, WireError> {
    Ok(take(buf, pos, 1)?[0])
}

fn take_str(buf: &[u8], pos: &mut usize) -> Result<String, WireError> {
    let len = take_byte(buf, pos)? as usize;
    let bytes = take(buf, pos, len)?;
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| WireError::InvalidUtf8)
}

const FLAG_HAS_PLAYERS: u8 = 0x01;
const FLAG_INCLUDE_LEGACY: u8 = 0x02;
const FLAG_HAS_MAX_HOPS: u8 = 0x04;

impl IndexQuery {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(32);
        buf.push(QUERY_SCHEMA);
        let mut flags = 0u8;
        if self.has_players {
            flags |= FLAG_HAS_PLAYERS;
        }
        if self.include_legacy {
            flags |= FLAG_INCLUDE_LEGACY;
        }
        if self.max_hops.is_some() {
            flags |= FLAG_HAS_MAX_HOPS;
        }
        buf.push(flags);
        buf.push(self.max_hops.unwrap_or(0));
        let id = self.game_id.as_bytes();
        let id = &id[..id.len().min(24)];
        buf.push(id.len() as u8);
        buf.extend_from_slice(id);
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self, WireError> {
        let mut pos = 0;
        let schema = take_byte(buf, &mut pos)?;
        if schema != QUERY_SCHEMA {
            return Err(WireError::UnsupportedSchema(schema));
        }
        let flags = take_byte(buf, &mut pos)?;
        let hops_byte = take_byte(buf, &mut pos)?;
        let game_id = take_str(buf, &mut pos)?;
        Ok(Self {
            game_id,
            // Only meaningful when the flag says so, so a zero cannot be
            // mistaken for "at most zero hops", which would match nothing.
            max_hops: (flags & FLAG_HAS_MAX_HOPS != 0).then_some(hops_byte),
            has_players: flags & FLAG_HAS_PLAYERS != 0,
            include_legacy: flags & FLAG_INCLUDE_LEGACY != 0,
        })
    }
}

/// Encode as many rows as fit, and say how many there were.
pub fn encode_result(rows: &[DiscoveredServer], total_matched: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.push(QUERY_SCHEMA);
    let flags_at = buf.len();
    buf.push(0);
    buf.extend_from_slice(&(total_matched.min(u16::MAX as usize) as u16).to_be_bytes());
    let count_at = buf.len();
    buf.push(0);

    let mut written: u8 = 0;
    let mut truncated = false;
    for row in rows {
        if written == u8::MAX {
            truncated = true;
            break;
        }
        let encoded = match &row.info {
            AnnounceInfo::Record(r) => match r.encode() {
                Ok(bytes) => {
                    let mut out = vec![ROW_RECORD];
                    out.push(bytes.len() as u8);
                    out.extend_from_slice(&bytes);
                    out
                }
                // A row this index cannot re-encode is dropped rather than
                // corrupting the stream. It arrived decodable, so this means the
                // record grew past what encode accepts — a bug worth not
                // amplifying into a broken response.
                Err(_) => continue,
            },
            AnnounceInfo::Legacy { name } => {
                let name = name.as_deref().unwrap_or("");
                let name = &name.as_bytes()[..name.len().min(48)];
                let mut out = vec![ROW_LEGACY];
                out.push(name.len() as u8);
                out.extend_from_slice(name);
                out
            }
        };
        if buf.len() + 17 + encoded.len() > MAX_RESPONSE_LEN {
            truncated = true;
            break;
        }
        buf.extend_from_slice(row.destination_hash.as_bytes());
        buf.push(row.hops);
        buf.extend_from_slice(&encoded);
        written += 1;
    }
    buf[count_at] = written;
    if truncated || (written as usize) < total_matched {
        buf[flags_at] |= 0x01;
    }
    buf
}

pub fn decode_result(buf: &[u8]) -> Result<QueryResult, WireError> {
    if buf.len() > MAX_RESPONSE_LEN {
        return Err(WireError::TooLarge(buf.len()));
    }
    let mut pos = 0;
    let schema = take_byte(buf, &mut pos)?;
    if schema != QUERY_SCHEMA {
        return Err(WireError::UnsupportedSchema(schema));
    }
    let flags = take_byte(buf, &mut pos)?;
    let total_matched = u16::from_be_bytes(
        take(buf, &mut pos, 2)?.try_into().map_err(|_| WireError::Truncated)?,
    );
    let count = take_byte(buf, &mut pos)? as usize;

    let mut rows = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let hash: [u8; 16] = take(buf, &mut pos, 16)?
            .try_into()
            .map_err(|_| WireError::Truncated)?;
        let hops = take_byte(buf, &mut pos)?;
        let kind = take_byte(buf, &mut pos)?;
        let len = take_byte(buf, &mut pos)? as usize;
        let body = take(buf, &mut pos, len)?;
        let info = match kind {
            ROW_RECORD => match decode_announce(body).map_err(|_| WireError::BadRow)? {
                AnnounceInfo::Record(r) => AnnounceInfo::Record(r),
                // The row claimed to be a record and decoded as something else.
                AnnounceInfo::Legacy { .. } => return Err(WireError::BadRow),
            },
            ROW_LEGACY => {
                let name = std::str::from_utf8(body).map_err(|_| WireError::InvalidUtf8)?;
                AnnounceInfo::Legacy {
                    name: (!name.is_empty()).then(|| name.to_string()),
                }
            }
            _ => return Err(WireError::BadRow),
        };
        rows.push(WireRow { destination_hash: hash, hops, info });
    }
    Ok(QueryResult { rows, truncated: flags & 0x01 != 0, total_matched })
}

/// Re-exported so a caller can build a row without depending on game-bridge's
/// internals directly.
pub type Record = AnnounceRecord;

#[cfg(test)]
mod tests {
    use super::*;
    use game_bridge::announce::AnnounceFlags;
    use personal_rns::prelude::DestinationHash;
    use prns_core::interfaces::InterfaceId;
    use std::time::Instant;

    fn record_row(n: u8, name: &str) -> DiscoveredServer {
        DiscoveredServer {
            destination_hash: DestinationHash::from_slice(&[n; 16]).unwrap(),
            last_seen: Instant::now(),
            hops: n,
            source_interface: InterfaceId::new([n; 8]),
            info: AnnounceInfo::Record(AnnounceRecord {
                protocol_version: 1,
                flags: AnnounceFlags::default(),
                min_link_class: 1,
                players: 3,
                max_players: 8,
                game_id: "sven-coop".to_string(),
                name: name.to_string(),
                map: "svencoop1".to_string(),
                tlvs: Vec::new(),
            }),
        }
    }

    fn legacy_row(n: u8, name: Option<&str>) -> DiscoveredServer {
        DiscoveredServer {
            destination_hash: DestinationHash::from_slice(&[n; 16]).unwrap(),
            last_seen: Instant::now(),
            hops: n,
            source_interface: InterfaceId::new([n; 8]),
            info: AnnounceInfo::Legacy { name: name.map(str::to_string) },
        }
    }

    #[test]
    fn a_query_round_trips() {
        for q in [
            IndexQuery::default(),
            IndexQuery {
                game_id: "sven-coop".into(),
                max_hops: Some(3),
                has_players: true,
                include_legacy: true,
            },
        ] {
            assert_eq!(IndexQuery::decode(&q.encode()).unwrap(), q);
        }
    }

    /// `max_hops: None` and `max_hops: Some(0)` mean opposite things — "any
    /// distance" and "match nothing" — so the flag is not decoration.
    #[test]
    fn no_hop_limit_is_distinct_from_a_zero_hop_limit() {
        let none = IndexQuery { max_hops: None, ..Default::default() };
        let zero = IndexQuery { max_hops: Some(0), ..Default::default() };
        assert_eq!(IndexQuery::decode(&none.encode()).unwrap().max_hops, None);
        assert_eq!(IndexQuery::decode(&zero.encode()).unwrap().max_hops, Some(0));
    }

    #[test]
    fn rows_round_trip_including_legacy_ones() {
        let rows = vec![record_row(1, "alpha"), legacy_row(2, Some("old")), legacy_row(3, None)];
        let result = decode_result(&encode_result(&rows, rows.len())).unwrap();
        assert_eq!(result.rows.len(), 3);
        assert!(!result.truncated);
        assert_eq!(result.total_matched, 3);

        assert_eq!(result.rows[0].destination_hash, [1u8; 16]);
        assert_eq!(result.rows[0].hops, 1);
        match &result.rows[0].info {
            AnnounceInfo::Record(r) => assert_eq!(r.name, "alpha"),
            _ => panic!("expected a record"),
        }
        assert_eq!(
            result.rows[1].info,
            AnnounceInfo::Legacy { name: Some("old".into()) }
        );
        assert_eq!(result.rows[2].info, AnnounceInfo::Legacy { name: None });
    }

    /// A client that cannot tell "this is the whole mesh" from "this is as much
    /// as fit" will believe the first. So truncation is on the wire.
    #[test]
    fn an_oversized_result_is_truncated_and_says_so() {
        let rows: Vec<DiscoveredServer> = (0..200)
            .map(|i| record_row((i % 255) as u8, &format!("server-number-{i:04}")))
            .collect();
        let encoded = encode_result(&rows, rows.len());
        assert!(encoded.len() <= MAX_RESPONSE_LEN);

        let result = decode_result(&encoded).unwrap();
        assert!(result.truncated, "truncation must be visible to the client");
        assert!(result.rows.len() < rows.len());
        assert_eq!(result.total_matched, 200, "the client is told what it is missing");
    }

    /// A count that fits but a total that does not still counts as truncated.
    #[test]
    fn a_short_page_of_a_long_match_is_flagged() {
        let rows = vec![record_row(1, "only-one-sent")];
        let result = decode_result(&encode_result(&rows, 50)).unwrap();
        assert!(result.truncated);
        assert_eq!(result.total_matched, 50);
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn truncated_input_never_panics() {
        let rows = vec![record_row(1, "alpha"), legacy_row(2, Some("old"))];
        let bytes = encode_result(&rows, 2);
        for n in 0..bytes.len() {
            assert!(
                decode_result(&bytes[..n]).is_err(),
                "a {n}-byte prefix decoded as a whole result"
            );
        }
        let q = IndexQuery { game_id: "sven-coop".into(), ..Default::default() }.encode();
        for n in 0..q.len() {
            assert!(IndexQuery::decode(&q[..n]).is_err());
        }
    }

    #[test]
    fn a_future_schema_is_refused_rather_than_misread() {
        let mut bytes = encode_result(&[record_row(1, "a")], 1);
        bytes[0] = 9;
        assert_eq!(decode_result(&bytes), Err(WireError::UnsupportedSchema(9)));

        let mut q = IndexQuery::default().encode();
        q[0] = 9;
        assert_eq!(IndexQuery::decode(&q), Err(WireError::UnsupportedSchema(9)));
    }

    #[test]
    fn an_unknown_row_kind_is_an_error_not_a_guess() {
        let mut bytes = encode_result(&[record_row(1, "a")], 1);
        // schema, flags, total(2), count(1), hash(16), hops(1) -> kind at 22
        bytes[22] = 7;
        assert_eq!(decode_result(&bytes), Err(WireError::BadRow));
    }

    #[test]
    fn an_oversized_response_is_refused() {
        assert!(matches!(
            decode_result(&vec![1u8; MAX_RESPONSE_LEN + 1]),
            Err(WireError::TooLarge(_))
        ));
    }

    #[test]
    fn an_empty_result_is_valid_and_says_nothing_matched() {
        let result = decode_result(&encode_result(&[], 0)).unwrap();
        assert!(result.rows.is_empty());
        assert!(!result.truncated);
        assert_eq!(result.total_matched, 0);
    }
}
