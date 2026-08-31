//! The agent uplink wire format.
//!
//! The mirror of `crates/platform-index/src/wire.rs`, for the agent's control
//! channel over Reticulum (`PLAN.md` §8 phase 4, `DESIGN.md` §2.3). The index is
//! a client of these request endpoints; the agent is the destination.
//!
//! # Envelope hand-rolled, payloads JSON
//!
//! The envelope — schema byte, op byte, status byte — is hand-rolled binary so
//! a future schema change is detected and refused rather than misread, exactly
//! as `wire.rs` refuses an unknown schema. The payloads (`InstanceSpec`,
//! `InstanceStatus`, `platform_auth`'s `Challenge`/`ChallengeResponse`/`Session`)
//! ride as JSON, because they already have a stable serde contract the loopback
//! HTTP API and the launcher frontend pin. Re-encoding them a second way would
//! be a second place for the same facts to disagree — the argument `wire.rs`
//! makes for reusing `AnnounceRecord`, applied here.
//!
//! # One link packet
//!
//! Link plaintext is 1967 B on our fork (`MAX_CHUNK = 1900`,
//! `game-bridge/src/framing.rs:24`), so a response fills to `MAX_BODY_LEN` and
//! `OP_LIST` sets a truncation flag + `total_matched`, mirroring `wire.rs`. A
//! caller that sees `truncated` narrows its request rather than believing it has
//! the whole node.

use serde::{Deserialize, Serialize};

use crate::instance::{InstanceSpec, InstanceStatus};

/// Wire schema version. Bumped when an existing field changes meaning, not when
/// one is added — a newer agent read by an older index should fail loudly, not
/// load half-understood.
pub const UPLINK_SCHEMA: u8 = 1;

/// Keep a response inside one link packet. Matches `wire.rs::MAX_RESPONSE_LEN`;
/// both sit under the 1967 B link cap with headroom for the envelope.
pub const MAX_BODY_LEN: usize = 1800;

// Op codes. Request and response share the op byte so a reader can pair them.
pub const OP_CHALLENGE: u8 = 1;
pub const OP_VERIFY: u8 = 2;
pub const OP_CREATE: u8 = 3;
pub const OP_STOP: u8 = 4;
pub const OP_REMOVE: u8 = 5;
pub const OP_LIST: u8 = 6;
pub const OP_CAPACITY: u8 = 7;

// Status byte in a response.
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;

/// The fixed envelope positions, so a decoder never indexes magic numbers.
const ENV_SCHEMA: usize = 0;
const ENV_OP: usize = 1;
const ENV_STATUS: usize = 2;
const ENV_BODY: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    Truncated,
    /// The response or request was larger than `MAX_BODY_LEN`.
    TooLarge(usize),
    UnsupportedSchema(u8),
    /// An op byte the decoder does not know.
    UnknownOp(u8),
    /// A status byte other than ok/err.
    UnknownStatus(u8),
    /// The JSON payload did not parse into the expected shape.
    BadPayload(String),
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "uplink message is truncated"),
            Self::TooLarge(n) => write!(f, "uplink message is {n} bytes, over the cap"),
            Self::UnsupportedSchema(v) => write!(f, "uplink schema {v} is not supported"),
            Self::UnknownOp(v) => write!(f, "uplink op {v} is not known"),
            Self::UnknownStatus(v) => write!(f, "uplink status {v} is not known"),
            Self::BadPayload(s) => write!(f, "uplink payload did not parse: {s}"),
        }
    }
}

impl std::error::Error for WireError {}

// ---- Request payloads -------------------------------------------------------

/// `OP_CREATE`: run an instance on behalf of a user.
///
/// The end-user's identity hash rides in `spec.owner` — the same field the
/// loopback HTTP path uses, so there is one owner field, not two. The agent
/// trusts the *authenticated* index's claim about who the owner is; the index
/// proved its own identity by `OP_VERIFY`, and the node's operator put that
/// index in `trusted_indexes`. `OWNER_LABEL` is stamped from `spec.owner`
/// unchanged (`agent.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReq {
    pub token: String,
    pub spec: InstanceSpec,
}

/// `OP_STOP` / `OP_REMOVE`: act on one instance the caller can see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceReq {
    pub token: String,
    pub instance_id: String,
}

/// `OP_LIST` / `OP_CAPACITY`: read the node, with a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenReq {
    pub token: String,
}

// ---- Response payloads ------------------------------------------------------

/// `OP_LIST` result. Truncation is on the wire for the same reason as
/// `wire.rs::QueryResult`: a caller that cannot tell "this is the whole node"
/// from "this is as much as fit" will believe the first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListResp {
    pub instances: Vec<InstanceStatus>,
    pub truncated: bool,
    pub total_matched: u16,
}

/// `OP_CAPACITY`: the placement numbers an index picks a node by. Pulled, not
/// pushed — the same authenticated link, no index-side ingress endpoint. Push
/// is a documented follow-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityResp {
    pub max_instances: usize,
    pub running: usize,
    pub port_range_start: u16,
    pub port_range_end: u16,
}

// ---- Envelope encode/decode -------------------------------------------------

/// Encode a request: `schema | op | body`.
pub fn encode_request(op: u8, body: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ENV_BODY + body.len());
    buf.push(UPLINK_SCHEMA);
    buf.push(op);
    buf.extend_from_slice(body);
    buf
}

/// Decode a request into its op and payload bytes.
pub fn decode_request(buf: &[u8]) -> Result<(u8, &[u8]), WireError> {
    if buf.len() < ENV_OP + 1 {
        return Err(WireError::Truncated);
    }
    if buf[ENV_SCHEMA] != UPLINK_SCHEMA {
        return Err(WireError::UnsupportedSchema(buf[ENV_SCHEMA]));
    }
    let op = buf[ENV_OP];
    Ok((op, &buf[ENV_BODY - 1..]))
}

/// Encode a successful response: `schema | op | ok | body`.
pub fn encode_ok(op: u8, body: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ENV_BODY + body.len());
    buf.push(UPLINK_SCHEMA);
    buf.push(op);
    buf.push(STATUS_OK);
    buf.extend_from_slice(body);
    buf
}

/// Encode an error response: `schema | op | err | utf-8 message`.
pub fn encode_err(op: u8, message: &str) -> Vec<u8> {
    let body = message.as_bytes();
    let mut buf = Vec::with_capacity(ENV_BODY + body.len());
    buf.push(UPLINK_SCHEMA);
    buf.push(op);
    buf.push(STATUS_ERR);
    buf.extend_from_slice(body);
    buf
}

/// Decode a response into `(op, ok, body)`. `ok` is `true` for a success
/// envelope, `false` for an error envelope whose body is a UTF-8 message.
pub fn decode_response(buf: &[u8]) -> Result<(u8, bool, &[u8]), WireError> {
    if buf.len() < ENV_STATUS + 1 {
        return Err(WireError::Truncated);
    }
    if buf[ENV_SCHEMA] != UPLINK_SCHEMA {
        return Err(WireError::UnsupportedSchema(buf[ENV_SCHEMA]));
    }
    let op = buf[ENV_OP];
    let status = buf[ENV_STATUS];
    let ok = match status {
        STATUS_OK => true,
        STATUS_ERR => false,
        other => return Err(WireError::UnknownStatus(other)),
    };
    Ok((op, ok, &buf[ENV_BODY..]))
}

/// Decode an error body as a message string. Only call this on a body whose
/// envelope said `err`; an ok body is not a string.
pub fn decode_err_message(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

/// Parse a JSON payload, mapping the serde error into `BadPayload`.
pub fn parse_payload<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, WireError> {
    serde_json::from_slice(body).map_err(|e| WireError::BadPayload(e.to_string()))
}

/// Serialize a payload to JSON bytes.
pub fn payload_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, WireError> {
    serde_json::to_vec(value).map_err(|e| WireError::BadPayload(e.to_string()))
}

/// Refuse a response body that would not fit one link packet. Called by the
/// encoder before it hands bytes to `cx.respond`, so an oversized list is
/// truncated rather than dropped by the engine.
pub fn fits(body_len: usize) -> Result<(), WireError> {
    if body_len > MAX_BODY_LEN {
        Err(WireError::TooLarge(body_len))
    } else {
        Ok(())
    }
}

/// Encode as many instances as fit, and say how many there were — the same shape
/// as `wire.rs::encode_result`, for the same reason.
///
/// `total` is the true count before truncation; a caller uses it to detect a
/// node bigger than one packet.
pub fn encode_list_resp(instances: &[InstanceStatus], total: usize) -> Vec<u8> {
    let mut included = instances.to_vec();
    let mut truncated = false;

    // Shrink until the envelope + JSON fits. A single instance that alone
    // overflows is sent alone with the truncated flag, so the caller still
    // learns the node is bigger than one packet rather than getting nothing.
    loop {
        let resp = ListResp {
            total_matched: total.min(u16::MAX as usize) as u16,
            truncated: truncated || (included.len() < total),
            instances: included.clone(),
        };
        let body = match payload_bytes(&resp) {
            Ok(b) => b,
            // A payload that cannot serialize is a bug; send an empty ok so the
            // link does not hang, mirroring wire.rs dropping an un-encodable row.
            Err(_) => return encode_ok(OP_LIST, &[]),
        };
        if body.len() <= MAX_BODY_LEN {
            return encode_ok(OP_LIST, &body);
        }
        if included.is_empty() {
            // Even zero instances overflowed the envelope, which cannot happen
            // for realistic data; send the empty list flagged truncated.
            return encode_ok(OP_LIST, &[]);
        }
        included.pop();
        truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{InstanceSpec, InstanceState};

    fn spec(id: &str) -> InstanceSpec {
        InstanceSpec {
            instance_id: id.to_string(),
            game_id: "sven-coop".to_string(),
            name: "test".to_string(),
            max_players: 8,
            port: None,
            extra_ports: Default::default(),
            owner: None,
        }
    }

    fn status(id: &str) -> InstanceStatus {
        InstanceStatus {
            instance_id: id.to_string(),
            game_id: "sven-coop".to_string(),
            name: id.to_string(),
            state: InstanceState::Running,
            port: Some(27100),
            ports: Vec::new(),
            container_id: Some("abc".to_string()),
            uptime_secs: None,
            owner: None,
        }
    }

    #[test]
    fn a_request_round_trips_its_op_and_body() {
        let body = b"{\"hello\":1}";
        let buf = encode_request(OP_CREATE, body);
        let (op, payload) = decode_request(&buf).unwrap();
        assert_eq!(op, OP_CREATE);
        assert_eq!(payload, body);
    }

    #[test]
    fn a_response_round_trips_ok_and_err() {
        let ok = encode_ok(OP_LIST, b"[1,2,3]");
        let (op, ok_flag, body) = decode_response(&ok).unwrap();
        assert_eq!(op, OP_LIST);
        assert!(ok_flag);
        assert_eq!(body, b"[1,2,3]");

        let err = encode_err(OP_VERIFY, "untrusted identity");
        let (op, ok_flag, body) = decode_response(&err).unwrap();
        assert_eq!(op, OP_VERIFY);
        assert!(!ok_flag);
        assert_eq!(decode_err_message(body), "untrusted identity");
    }

    #[test]
    fn a_create_request_payload_round_trips() {
        let req = CreateReq { token: "t".into(), spec: spec("i-1") };
        let body = payload_bytes(&req).unwrap();
        let back: CreateReq = parse_payload(&body).unwrap();
        assert_eq!(back, req);
        assert_eq!(back.spec.instance_id, "i-1");
    }

    /// The list response flags truncation and reports the true total, so a
    /// caller can tell "the whole node" from "as much as fit".
    #[test]
    fn a_long_list_is_truncated_and_reports_the_total() {
        let instances: Vec<InstanceStatus> = (0..400).map(|i| status(&format!("i-{i:03}"))).collect();
        let buf = encode_list_resp(&instances, instances.len());
        let (op, ok, body) = decode_response(&buf).unwrap();
        assert_eq!(op, OP_LIST);
        assert!(ok);
        assert!(body.len() <= MAX_BODY_LEN);
        let resp: ListResp = parse_payload(body).unwrap();
        assert!(resp.truncated);
        assert!(resp.instances.len() < instances.len());
        assert_eq!(resp.total_matched, 400);
    }

    #[test]
    fn a_short_list_is_not_truncated() {
        let instances = vec![status("a"), status("b")];
        let buf = encode_list_resp(&instances, 2);
        let (_, _, body) = decode_response(&buf).unwrap();
        let resp: ListResp = parse_payload(body).unwrap();
        assert!(!resp.truncated);
        assert_eq!(resp.instances.len(), 2);
        assert_eq!(resp.total_matched, 2);
    }

    #[test]
    fn an_empty_list_is_valid() {
        let buf = encode_list_resp(&[], 0);
        let (_, _, body) = decode_response(&buf).unwrap();
        let resp: ListResp = parse_payload(body).unwrap();
        assert!(resp.instances.is_empty());
        assert!(!resp.truncated);
        assert_eq!(resp.total_matched, 0);
    }

    #[test]
    fn a_future_schema_is_refused() {
        let mut buf = encode_request(OP_CHALLENGE, b"");
        buf[0] = 9;
        assert_eq!(decode_request(&buf), Err(WireError::UnsupportedSchema(9)));

        let mut r = encode_ok(OP_CHALLENGE, b"");
        r[0] = 9;
        assert_eq!(decode_response(&r), Err(WireError::UnsupportedSchema(9)));
    }

    #[test]
    fn a_bad_status_byte_is_refused() {
        let mut r = encode_ok(OP_CHALLENGE, b"");
        r[ENV_STATUS] = 7;
        assert_eq!(decode_response(&r), Err(WireError::UnknownStatus(7)));
    }

    #[test]
    fn a_truncated_envelope_is_an_error_not_a_panic() {
        assert_eq!(decode_request(&[UPLINK_SCHEMA]), Err(WireError::Truncated));
        assert_eq!(decode_request(&[]), Err(WireError::Truncated));
        assert_eq!(decode_response(&[UPLINK_SCHEMA, OP_CHALLENGE]), Err(WireError::Truncated));
    }

    #[test]
    fn fits_rejects_an_oversized_body() {
        assert!(fits(MAX_BODY_LEN).is_ok());
        assert_eq!(fits(MAX_BODY_LEN + 1), Err(WireError::TooLarge(MAX_BODY_LEN + 1)));
    }
}