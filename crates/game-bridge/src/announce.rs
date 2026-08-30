//! The server-browser announce record (`PLAN.md` §3.3).
//!
//! One announce carries one row of the server browser. That row has to fit in
//! `app_data`, which is **316 bytes** (284 if the identity is ratcheted), so
//! everything expensive — player lists, mods, full config — is a Link query to
//! the server instead (`PLAN.md` §3.4). Budget against 284, not 316.
//!
//! The announce's own Ed25519 signature already covers `app_data`
//! (`prns-core/src/routing/announce/mod.rs:384`), so a server cannot lie about
//! another server's row and an index cannot alter what it relays — it can only
//! decline to list it. There is deliberately no second signature and no
//! checksum here.
//!
//! # Three rules, none of them cosmetic
//!
//! **The bare-UTF-8 fallback is mandatory.** Deployed `svencoop-prns` v0.1.x
//! servers put a bare UTF-8 display name in `app_data` with no structure at
//! all. If byte 0 is not a record version this build knows, the whole payload
//! decodes as a display name. This is why the version byte is low: a bare name
//! never starts with a byte under 0x20. Dropping this fallback makes every
//! deployed Sven server vanish from the browser, which `PLAN.md` §5 forbids.
//!
//! **Unknown TLV types are skipped, never an error**, and are preserved
//! verbatim so a decode/encode round-trip does not silently discard a field a
//! newer peer sent.
//!
//! **Decode is permissive about values and strict about lengths.** This is the
//! rule most easily got backwards. `min_link_class` and the transport mode are
//! enumerations that will grow — `GAMES.md` §4 already anticipates more tiers.
//! A decoder that errors on a value it does not recognise would make every
//! deployed client drop those servers from its list the day the enumeration
//! grows: silent, total, and unfixable in the field. So any value decodes, and
//! `is_min_link_class_known` / `is_transport_mode_known` say whether this build
//! understands it. Lengths and bounds stay hard errors, because a field length
//! outside the record's own definition is a malformed record, not a newer one.
//! Encode is strict in both directions: writing a value this build does not
//! know is our bug, not a peer's.
//!
//! Raising `MAX_NAME_LEN` and friends is therefore a **record version bump**,
//! not a constant edit — older peers reject the longer field by design.
//!
//! Drafted with GLM 5.2 against the §3.3 spec, then reviewed and corrected:
//! decode originally rejected unknown `min_link_class`/mode values (the
//! forward-compatibility trap above), accepted any field length including a
//! zero-length `game_id`, and had a `Default` for the flags that `encode`
//! refused.

pub const MAX_ANNOUNCE_APP_DATA_LEN: usize = 316;
pub const MAX_RATCHETED_ANNOUNCE_APP_DATA_LEN: usize = 284;
pub const RECORD_VERSION: u8 = 1;
pub const MAX_GAME_ID_LEN: usize = 24;
pub const MAX_NAME_LEN: usize = 48;
pub const MAX_MAP_LEN: usize = 32;

const FLAG_PASSWORDED: u8 = 0x01;
const FLAG_ALLOWLISTED: u8 = 0x02;
const FLAG_DEDICATED: u8 = 0x04;
const FLAG_TRANSPORT_MODE_MASK: u8 = 0x18;
const FLAG_TRANSPORT_MODE_SHIFT: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnounceFlags {
    pub passworded: bool,
    pub allowlisted: bool,
    pub dedicated: bool,
    pub transport_mode: u8,
}

impl AnnounceFlags {
    pub fn from_byte(b: u8) -> Self {
        AnnounceFlags {
            passworded: b & FLAG_PASSWORDED != 0,
            allowlisted: b & FLAG_ALLOWLISTED != 0,
            dedicated: b & FLAG_DEDICATED != 0,
            transport_mode: (b & FLAG_TRANSPORT_MODE_MASK) >> FLAG_TRANSPORT_MODE_SHIFT,
        }
    }

    pub fn to_byte(&self) -> u8 {
        let mut b: u8 = 0;
        if self.passworded {
            b |= FLAG_PASSWORDED;
        }
        if self.allowlisted {
            b |= FLAG_ALLOWLISTED;
        }
        if self.dedicated {
            b |= FLAG_DEDICATED;
        }
        b |= self.transport_mode.wrapping_shl(FLAG_TRANSPORT_MODE_SHIFT) & FLAG_TRANSPORT_MODE_MASK;
        b
    }

    pub fn is_transport_mode_known(&self) -> bool {
        self.transport_mode >= 1 && self.transport_mode <= 3
    }
}

impl Default for AnnounceFlags {
    fn default() -> Self {
        AnnounceFlags {
            passworded: false,
            allowlisted: false,
            dedicated: false,
            // Mode 1, not 0: `encode` refuses an unknown mode, so a default
            // that cannot be encoded is a trap.
            transport_mode: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tlv {
    pub kind: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceRecord {
    pub protocol_version: u8,
    pub flags: AnnounceFlags,
    pub min_link_class: u8,
    pub players: u8,
    pub max_players: u8,
    pub game_id: String,
    pub name: String,
    pub map: String,
    pub tlvs: Vec<Tlv>,
}

impl AnnounceRecord {
    pub fn is_min_link_class_known(&self) -> bool {
        self.min_link_class >= 1 && self.min_link_class <= 3
    }

    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        if self.game_id.is_empty() {
            return Err(EncodeError::GameIdEmpty);
        }
        if self.game_id.len() > MAX_GAME_ID_LEN {
            return Err(EncodeError::GameIdTooLong);
        }
        if !self.game_id.is_ascii() {
            return Err(EncodeError::GameIdNotAscii);
        }
        if self.name.len() > MAX_NAME_LEN {
            return Err(EncodeError::NameTooLong);
        }
        if self.map.len() > MAX_MAP_LEN {
            return Err(EncodeError::MapTooLong);
        }
        if self.min_link_class < 1 || self.min_link_class > 3 {
            return Err(EncodeError::UnknownMinLinkClass);
        }
        if self.flags.transport_mode < 1 || self.flags.transport_mode > 3 {
            return Err(EncodeError::UnknownTransportMode);
        }
        for tlv in &self.tlvs {
            if tlv.data.len() > 255 {
                return Err(EncodeError::TlvTooLong);
            }
        }

        let tlv_size: usize = self.tlvs.iter().map(|t| 2usize + t.data.len()).sum();
        let total = 6usize
            .checked_add(1 + self.game_id.len())
            .and_then(|v| v.checked_add(1 + self.name.len()))
            .and_then(|v| v.checked_add(1 + self.map.len()))
            .and_then(|v| v.checked_add(tlv_size))
            .ok_or(EncodeError::OutputTooLong)?;

        if total > MAX_RATCHETED_ANNOUNCE_APP_DATA_LEN {
            return Err(EncodeError::OutputTooLong);
        }

        let mut buf = Vec::with_capacity(total);
        buf.push(RECORD_VERSION);
        buf.push(self.protocol_version);
        buf.push(self.flags.to_byte());
        buf.push(self.min_link_class);
        buf.push(self.players);
        buf.push(self.max_players);
        buf.push(self.game_id.len() as u8);
        buf.extend_from_slice(self.game_id.as_bytes());
        buf.push(self.name.len() as u8);
        buf.extend_from_slice(self.name.as_bytes());
        buf.push(self.map.len() as u8);
        buf.extend_from_slice(self.map.as_bytes());
        for tlv in &self.tlvs {
            buf.push(tlv.kind);
            buf.push(tlv.data.len() as u8);
            buf.extend_from_slice(&tlv.data);
        }

        Ok(buf)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnounceInfo {
    Record(AnnounceRecord),
    Legacy { name: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    InvalidUtf8,
    GameIdNotAscii,
    TlvTruncated,
    /// A declared field length outside the range this record version defines.
    /// A *length* violation, so it is a hard error even though unrecognised
    /// *values* are not — see the module docs.
    InvalidGameIdLen(usize),
    InvalidNameLen(usize),
    InvalidMapLen(usize),
    /// Longer than an announce can physically carry.
    OverBudget(usize),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "record is truncated"),
            DecodeError::InvalidUtf8 => write!(f, "invalid UTF-8 in payload"),
            DecodeError::GameIdNotAscii => write!(f, "game_id contains non-ASCII bytes"),
            DecodeError::TlvTruncated => write!(f, "TLV extends past end of buffer"),
            DecodeError::InvalidGameIdLen(n) => {
                write!(f, "game_id length {n} is outside 1..={MAX_GAME_ID_LEN}")
            }
            DecodeError::InvalidNameLen(n) => {
                write!(f, "name length {n} is over the {MAX_NAME_LEN}-byte maximum")
            }
            DecodeError::InvalidMapLen(n) => {
                write!(f, "map length {n} is over the {MAX_MAP_LEN}-byte maximum")
            }
            DecodeError::OverBudget(n) => write!(
                f,
                "app_data is {n} bytes, over the {MAX_ANNOUNCE_APP_DATA_LEN}-byte announce cap"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    GameIdEmpty,
    GameIdNotAscii,
    GameIdTooLong,
    NameTooLong,
    MapTooLong,
    UnknownMinLinkClass,
    UnknownTransportMode,
    TlvTooLong,
    OutputTooLong,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::GameIdEmpty => write!(f, "game_id must not be empty"),
            EncodeError::GameIdNotAscii => write!(f, "game_id must be ASCII"),
            EncodeError::GameIdTooLong => write!(f, "game_id exceeds maximum length"),
            EncodeError::NameTooLong => write!(f, "name exceeds maximum length"),
            EncodeError::MapTooLong => write!(f, "map exceeds maximum length"),
            EncodeError::UnknownMinLinkClass => {
                write!(f, "min_link_class value not known to this build")
            }
            EncodeError::UnknownTransportMode => {
                write!(f, "transport mode value not known to this build")
            }
            EncodeError::TlvTooLong => write!(f, "TLV data exceeds 255 bytes"),
            EncodeError::OutputTooLong => {
                write!(
                    f,
                    "encoded record exceeds maximum announce app_data length"
                )
            }
        }
    }
}

impl std::error::Error for EncodeError {}

pub fn decode(app_data: &[u8]) -> Result<AnnounceInfo, DecodeError> {
    // A real announce cannot carry more than this — the engine caps app_data
    // at MAX_ANNOUNCE_APP_DATA_LEN before it ever reaches us. Anything longer
    // did not come off the wire, so refuse it rather than parse it.
    if app_data.len() > MAX_ANNOUNCE_APP_DATA_LEN {
        return Err(DecodeError::OverBudget(app_data.len()));
    }
    // Empty payload -> legacy with no name.
    if app_data.is_empty() {
        return Ok(AnnounceInfo::Legacy { name: None });
    }

    // Unknown record version → fall back to legacy: decode the WHOLE payload as UTF-8.
    if app_data[0] != RECORD_VERSION {
        return match std::str::from_utf8(app_data) {
            Ok(s) => Ok(AnnounceInfo::Legacy {
                name: Some(s.to_string()),
            }),
            Err(_) => Err(DecodeError::InvalidUtf8),
        };
    }

    // Record version 1 — need at least 7 bytes (6-byte header + game_id length byte).
    if app_data.len() < 7 {
        return Err(DecodeError::Truncated);
    }

    let protocol_version = app_data[1];
    let flags = AnnounceFlags::from_byte(app_data[2]);
    let min_link_class = app_data[3];
    let players = app_data[4];
    let max_players = app_data[5];

    let mut pos: usize = 6;

    // game_id: length-prefixed, ASCII.
    let game_id_len = app_data[pos] as usize;
    if game_id_len == 0 || game_id_len > MAX_GAME_ID_LEN {
        return Err(DecodeError::InvalidGameIdLen(game_id_len));
    }
    pos = pos.checked_add(1).ok_or(DecodeError::Truncated)?;
    if pos.checked_add(game_id_len).is_none_or(|end| end > app_data.len()) {
        return Err(DecodeError::Truncated);
    }
    let game_id_bytes = &app_data[pos..pos + game_id_len];
    if !game_id_bytes.iter().all(|&b| b.is_ascii()) {
        return Err(DecodeError::GameIdNotAscii);
    }
    let game_id = std::str::from_utf8(game_id_bytes)
        .map_err(|_| DecodeError::GameIdNotAscii)?
        .to_string();
    pos += game_id_len;

    // name: length-prefixed, UTF-8.
    if pos >= app_data.len() {
        return Err(DecodeError::Truncated);
    }
    let name_len = app_data[pos] as usize;
    if name_len > MAX_NAME_LEN {
        return Err(DecodeError::InvalidNameLen(name_len));
    }
    pos += 1;
    if pos.checked_add(name_len).is_none_or(|end| end > app_data.len()) {
        return Err(DecodeError::Truncated);
    }
    let name_bytes = &app_data[pos..pos + name_len];
    let name = std::str::from_utf8(name_bytes)
        .map_err(|_| DecodeError::InvalidUtf8)?
        .to_string();
    pos += name_len;

    // map: length-prefixed, UTF-8, empty allowed.
    if pos >= app_data.len() {
        return Err(DecodeError::Truncated);
    }
    let map_len = app_data[pos] as usize;
    if map_len > MAX_MAP_LEN {
        return Err(DecodeError::InvalidMapLen(map_len));
    }
    pos += 1;
    if pos.checked_add(map_len).is_none_or(|end| end > app_data.len()) {
        return Err(DecodeError::Truncated);
    }
    let map_bytes = &app_data[pos..pos + map_len];
    let map = std::str::from_utf8(map_bytes)
        .map_err(|_| DecodeError::InvalidUtf8)?
        .to_string();
    pos += map_len;

    // TLVs: zero or more, each u8 type + u8 length + data.
    let mut tlvs = Vec::new();
    while pos < app_data.len() {
        if pos.checked_add(2).is_none_or(|end| end > app_data.len()) {
            return Err(DecodeError::Truncated);
        }
        let kind = app_data[pos];
        let len = app_data[pos + 1] as usize;
        pos += 2;
        if pos.checked_add(len).is_none_or(|end| end > app_data.len()) {
            return Err(DecodeError::TlvTruncated);
        }
        let data = app_data[pos..pos + len].to_vec();
        pos += len;
        // Unknown TLV types are preserved verbatim, never an error.
        tlvs.push(Tlv { kind, data });
    }

    Ok(AnnounceInfo::Record(AnnounceRecord {
        protocol_version,
        flags,
        min_link_class,
        players,
        max_players,
        game_id,
        name,
        map,
        tlvs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> AnnounceRecord {
        AnnounceRecord {
            protocol_version: 1,
            flags: AnnounceFlags {
                passworded: true,
                allowlisted: true,
                dedicated: false,
                transport_mode: 3,
            },
            min_link_class: 2,
            players: 5,
            max_players: 20,
            game_id: "mygame".to_string(),
            name: "My Awesome Server".to_string(),
            map: "de_dust2".to_string(),
            tlvs: vec![
                Tlv {
                    kind: 0xFE,
                    data: vec![1, 2, 3],
                },
                Tlv {
                    kind: 0xFF,
                    data: vec![],
                },
            ],
        }
    }

    #[test]
    fn round_trip_with_unknown_tlvs() {
        let record = sample_record();
        let encoded = record.encode().unwrap();
        let decoded = decode(&encoded).unwrap();
        match decoded {
            AnnounceInfo::Record(r) => {
                assert_eq!(r.protocol_version, 1);
                assert_eq!(r.flags, record.flags);
                assert_eq!(r.min_link_class, 2);
                assert_eq!(r.players, 5);
                assert_eq!(r.max_players, 20);
                assert_eq!(r.game_id, "mygame");
                assert_eq!(r.name, "My Awesome Server");
                assert_eq!(r.map, "de_dust2");
                assert_eq!(r.tlvs.len(), 2);
                assert_eq!(r.tlvs[0].kind, 0xFE);
                assert_eq!(r.tlvs[0].data, vec![1, 2, 3]);
                assert_eq!(r.tlvs[1].kind, 0xFF);
                assert_eq!(r.tlvs[1].data, vec![]);
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn bare_utf8_name_decodes_as_legacy() {
        let info = decode(b"My Server").unwrap();
        match info {
            AnnounceInfo::Legacy { name } => {
                assert_eq!(name, Some("My Server".to_string()));
            }
            _ => panic!("expected Legacy"),
        }
    }

    #[test]
    fn empty_payload_decodes_as_legacy_no_name() {
        let info = decode(&[]).unwrap();
        match info {
            AnnounceInfo::Legacy { name } => {
                assert_eq!(name, None);
            }
            _ => panic!("expected Legacy"),
        }
    }

    #[test]
    fn unknown_version_falls_back_to_legacy() {
        // Version 2 with valid UTF-8 → legacy with a name.
        let info = decode(&[2, b'H', b'i']).unwrap();
        match info {
            AnnounceInfo::Legacy { name: Some(_) } => {}
            _ => panic!("expected Legacy with name"),
        }

        // Version 2 with invalid UTF-8 → error.
        let result = decode(&[2, 0xFF, 0xFE]);
        assert!(result.is_err());
    }

    #[test]
    fn rule3_unknown_values_decode_successfully() {
        // Manually construct bytes: min_link_class=7, transport_mode=0.
        let bytes = [
            RECORD_VERSION, // byte 0
            1,              // protocol version
            0x00,           // flags: mode=0, no flags
            7,              // min_link_class = 7
            4,              // players
            16,             // max_players
            1,              // game_id len
            b'x',           // game_id
            0,              // name len
            0,              // map len
        ];
        let info = decode(&bytes).unwrap();
        match info {
            AnnounceInfo::Record(r) => {
                assert_eq!(r.min_link_class, 7);
                assert!(!r.is_min_link_class_known());
                assert_eq!(r.flags.transport_mode, 0);
                assert!(!r.flags.is_transport_mode_known());
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn rule3_encode_rejects_unknown_values() {
        let mut record = sample_record();
        record.min_link_class = 7;
        assert!(record.encode().is_err());

        let mut record = sample_record();
        record.flags.transport_mode = 0;
        assert!(record.encode().is_err());
    }

    #[test]
    fn truncated_input_never_panics() {
        let record = AnnounceRecord {
            protocol_version: 1,
            flags: AnnounceFlags {
                passworded: false,
                allowlisted: false,
                dedicated: false,
                transport_mode: 1,
            },
            min_link_class: 1,
            players: 0,
            max_players: 0,
            game_id: "a".repeat(24),
            name: "a".repeat(48),
            map: "a".repeat(32),
            tlvs: vec![],
        };
        let bytes = record.encode().unwrap();
        assert_eq!(bytes.len(), 113);

        for n in 0..bytes.len() {
            let result = std::panic::catch_unwind(|| decode(&bytes[..n]));
            assert!(result.is_ok(), "decode panicked at n={}", n);
            let decoded = decode(&bytes[..n]);
            if n == 0 {
                assert!(
                    matches!(decoded, Ok(AnnounceInfo::Legacy { name: None })),
                    "expected Legacy at n=0, got {:?}",
                    decoded
                );
            } else {
                assert!(
                    decoded.is_err(),
                    "expected Err at n={}, got {:?}",
                    n,
                    decoded
                );
            }
        }
    }

    #[test]
    fn tlv_length_past_end_is_error() {
        let mut record = sample_record();
        record.tlvs.clear();
        let mut bytes = record.encode().unwrap();
        // Append a TLV header claiming 16 bytes but provide only 2.
        bytes.push(0x42);
        bytes.push(16);
        bytes.push(0x01);
        bytes.push(0x02);
        let result = decode(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn encode_rejects_overlength_fields() {
        let mut record = sample_record();
        record.game_id = "a".repeat(25);
        assert!(record.encode().is_err());

        let mut record = sample_record();
        record.name = "a".repeat(49);
        assert!(record.encode().is_err());

        let mut record = sample_record();
        record.map = "a".repeat(33);
        assert!(record.encode().is_err());
    }

    #[test]
    fn encode_rejects_record_past_284() {
        let record = AnnounceRecord {
            protocol_version: 1,
            flags: AnnounceFlags {
                passworded: false,
                allowlisted: false,
                dedicated: false,
                transport_mode: 1,
            },
            min_link_class: 1,
            players: 0,
            max_players: 0,
            game_id: "a".repeat(24),
            name: "a".repeat(48),
            map: "a".repeat(32),
            tlvs: vec![Tlv {
                kind: 0x42,
                data: vec![0u8; 170],
            }],
        };
        // 113 + 2 + 170 = 285 > 284
        assert!(record.encode().is_err());
    }

    #[test]
    fn worst_case_encodes_to_113() {
        let record = AnnounceRecord {
            protocol_version: 1,
            flags: AnnounceFlags {
                passworded: false,
                allowlisted: false,
                dedicated: false,
                transport_mode: 1,
            },
            min_link_class: 1,
            players: 0,
            max_players: 0,
            game_id: "a".repeat(24),
            name: "a".repeat(48),
            map: "a".repeat(32),
            tlvs: vec![],
        };
        let encoded = record.encode().unwrap();
        assert_eq!(encoded.len(), 113);
    }

    #[test]
    fn flags_round_trip_and_reserved_bits_ignored() {
        let flags = AnnounceFlags {
            passworded: true,
            allowlisted: false,
            dedicated: true,
            transport_mode: 2,
        };
        let byte = flags.to_byte();
        let decoded = AnnounceFlags::from_byte(byte);
        assert_eq!(decoded, flags);

        // Set all reserved bits (0xE0) — they must be ignored on decode.
        let byte_with_reserved = byte | 0xE0;
        let decoded2 = AnnounceFlags::from_byte(byte_with_reserved);
        assert_eq!(decoded2, flags);
    }

    // ---- added in review; the cases the draft did not cover ----

    /// The §5 test. These are the exact bytes a deployed `svencoop-prns`
    /// v0.1.10 server announces (`relay.rs`'s `DEFAULT_SERVER_ANNOUNCE_NAME`),
    /// and a named one. If either stops decoding as a legacy row, every
    /// deployed Sven server disappears from the browser.
    #[test]
    fn deployed_v0_1_10_announces_still_decode() {
        assert_eq!(
            decode(b"sc-rns-bridge").unwrap(),
            AnnounceInfo::Legacy { name: Some("sc-rns-bridge".to_string()) }
        );
        assert_eq!(
            decode("Idan's Server".as_bytes()).unwrap(),
            AnnounceInfo::Legacy { name: Some("Idan's Server".to_string()) }
        );
    }

    /// A field length outside the record's own definition is malformed, not
    /// merely newer — unlike an unrecognised *value*, which must decode.
    #[test]
    fn out_of_range_field_lengths_are_rejected() {
        let mut bytes = sample_record().encode().unwrap();

        // game_id length lives at byte 6.
        let mut zero_id = bytes.clone();
        zero_id[6] = 0;
        assert_eq!(decode(&zero_id), Err(DecodeError::InvalidGameIdLen(0)));

        let mut long_id = bytes.clone();
        long_id[6] = (MAX_GAME_ID_LEN + 1) as u8;
        assert_eq!(
            decode(&long_id),
            Err(DecodeError::InvalidGameIdLen(MAX_GAME_ID_LEN + 1))
        );

        // The name length byte follows game_id.
        let name_len_at = 7 + sample_record().game_id.len();
        bytes[name_len_at] = (MAX_NAME_LEN + 1) as u8;
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::InvalidNameLen(MAX_NAME_LEN + 1))
        );
    }

    #[test]
    fn map_length_over_maximum_is_rejected() {
        let r = sample_record();
        let bytes = r.encode().unwrap();
        let map_len_at = 7 + r.game_id.len() + 1 + r.name.len();
        let mut bad = bytes.clone();
        bad[map_len_at] = (MAX_MAP_LEN + 1) as u8;
        assert_eq!(decode(&bad), Err(DecodeError::InvalidMapLen(MAX_MAP_LEN + 1)));
    }

    #[test]
    fn payload_over_the_announce_cap_is_rejected() {
        let too_big = vec![b'x'; MAX_ANNOUNCE_APP_DATA_LEN + 1];
        assert_eq!(
            decode(&too_big),
            Err(DecodeError::OverBudget(MAX_ANNOUNCE_APP_DATA_LEN + 1))
        );
        // Exactly at the cap is fine (decodes as a legacy name).
        let at_cap = vec![b'x'; MAX_ANNOUNCE_APP_DATA_LEN];
        assert!(decode(&at_cap).is_ok());
    }

    /// `encode` refuses an unknown transport mode, so the default flags must
    /// not be one it refuses.
    #[test]
    fn default_flags_are_encodable() {
        let mut r = sample_record();
        r.flags = AnnounceFlags::default();
        r.encode().expect("default flags must encode");
    }

    /// Every legal record fits the ratcheted budget with room for the TLVs
    /// §3.3 reserves.
    #[test]
    fn worst_case_leaves_tlv_headroom() {
        let worst = AnnounceRecord {
            protocol_version: 1,
            flags: AnnounceFlags::default(),
            min_link_class: 3,
            players: 255,
            max_players: 255,
            game_id: "x".repeat(MAX_GAME_ID_LEN),
            name: "y".repeat(MAX_NAME_LEN),
            map: "z".repeat(MAX_MAP_LEN),
            tlvs: Vec::new(),
        };
        let encoded = worst.encode().unwrap();
        assert_eq!(encoded.len(), 113);
        assert!(
            MAX_RATCHETED_ANNOUNCE_APP_DATA_LEN - encoded.len() >= 170,
            "less TLV headroom than PLAN.md §3.3 claims"
        );
    }
}
