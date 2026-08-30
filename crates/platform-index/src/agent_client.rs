//! Driving a remote agent over Reticulum, from the index's side.
//!
//! The mirror of `crates/platform-agent/src/uplink.rs` and the sibling of
//! `client.rs`. An index that has a node (`node.rs`) can drive an agent on
//! another host with no inbound port on either side: it opens a Link to the
//! agent's `platform-agent.control` destination, authenticates by
//! challenge/response (`platform_auth`, audience = the agent's identity), and
//! issues create/stop/remove/list requests. This is what unblocks multi-node
//! hosting (`PLAN.md` §8 phase 4, `DESIGN.md` §2.3).
//!
//! # One key
//!
//! The index authenticates to the agent with the **same** identity it presents
//! to users — the one users bind their signatures to (`node.rs`). An operator
//! who trusts this index puts that identity hash in the agent's
//! `trusted_indexes`; the index is then the agent's authorized caller.
//!
//! # Sessions are cached per agent
//!
//! Challenge + verify are two round trips; an op is a third. A token lives 12 h
//! (`platform_auth::DEFAULT_SESSION_TTL`), so the client caches one per agent
//! destination and re-authenticates only when a token is absent or the agent
//! refuses it. A refusal mid-session (the agent's operator removed this index
//! from `trusted_indexes`) drops the cached token and surfaces the error — the
//! agent's `authorize` re-checks the allowlist on every op, so this side cannot
//! paper over a revocation.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use personal_rns::prelude::{DestinationHash, PrnsNodeHandle, Zeroizing, IDENTITY_SECRET_KEY_LEN};
use prns_core::identity::PrivateIdentityMaterial;
use prns_core::routing::request_handlers::RequestPathHash;
use tokio::sync::Mutex;
use tracing::debug;

use platform_agent::instance::{InstanceSpec, InstanceStatus};
use platform_agent::uplink::CONTROL_ENDPOINT_ID;
use platform_agent::uplink_wire::{
    decode_response, encode_request, parse_payload, payload_bytes, CapacityResp, CreateReq,
    InstanceReq, ListResp, TokenReq, OP_CAPACITY, OP_CHALLENGE, OP_CREATE, OP_LIST, OP_REMOVE,
    OP_STOP, OP_VERIFY,
};
use platform_auth::{answer_challenge, Challenge, ChallengeResponse, Session};

/// The endpoint path hash, computed once.
fn endpoint_hash() -> RequestPathHash {
    RequestPathHash::of(CONTROL_ENDPOINT_ID)
}

/// A client of one or more remote agents, backed by the index's Reticulum node.
///
/// Cloneable through `Arc`; the session cache is shared.
pub struct AgentClient {
    handle: PrnsNodeHandle,
    secret: Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>,
    /// Cached session token per agent destination hash.
    sessions: Mutex<HashMap<[u8; 16], String>>,
}

impl AgentClient {
    pub fn new(handle: PrnsNodeHandle, secret: Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>) -> Arc<Self> {
        Arc::new(Self {
            handle,
            secret,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// The index's own identity hash, for logging / matching against an agent's
    /// `trusted_indexes`.
    pub fn identity_hash(&self) -> Result<prns_core::identity::IdentityHash> {
        let id = PrivateIdentityMaterial::from_slice(&self.secret[..])
            .map_err(|e| anyhow!("index identity: {e:?}"))?;
        Ok(id.identity_hash())
    }

    /// Create an instance on a remote agent. `owner` is the end-user's identity
    /// hash, stamped onto the container via `spec.owner`.
    pub async fn create(
        &self,
        agent: DestinationHash,
        mut spec: InstanceSpec,
        owner: Option<String>,
    ) -> Result<InstanceStatus> {
        spec.owner = owner;
        let req = CreateReq { token: String::new(), spec };
        let body = self
            .signed_op(agent, OP_CREATE, &payload_bytes(&req)?)
            .await?;
        parse_ok(&body, OP_CREATE)
    }

    pub async fn stop(&self, agent: DestinationHash, instance_id: &str) -> Result<()> {
        let req = InstanceReq { token: String::new(), instance_id: instance_id.to_string() };
        let body = self.signed_op(agent, OP_STOP, &payload_bytes(&req)?).await?;
        expect_empty(&body, OP_STOP)
    }

    pub async fn remove(&self, agent: DestinationHash, instance_id: &str) -> Result<()> {
        let req = InstanceReq { token: String::new(), instance_id: instance_id.to_string() };
        let body = self.signed_op(agent, OP_REMOVE, &payload_bytes(&req)?).await?;
        expect_empty(&body, OP_REMOVE)
    }

    pub async fn list(&self, agent: DestinationHash) -> Result<Vec<InstanceStatus>> {
        let req = TokenReq { token: String::new() };
        let body = self.signed_op(agent, OP_LIST, &payload_bytes(&req)?).await?;
        let resp: ListResp = parse_ok(&body, OP_LIST)?;
        Ok(resp.instances)
    }

    pub async fn capacity(&self, agent: DestinationHash) -> Result<CapacityResp> {
        let req = TokenReq { token: String::new() };
        let body = self.signed_op(agent, OP_CAPACITY, &payload_bytes(&req)?).await?;
        parse_ok(&body, OP_CAPACITY)
    }

    /// Send `op` with a session token, authenticating first if needed. The
    /// request payloads carry an empty token at the call site; `inject_token`
    /// parses the body, fills the real token, and re-serializes, so there is one
    /// wire shape rather than a special unauthed one.
    async fn signed_op(
        &self,
        agent: DestinationHash,
        op: u8,
        body_without_token: &[u8],
    ) -> Result<Vec<u8>> {
        // The body's `token` field was empty; resolve a real token, then re-emit
        // the request with the token filled. Re-encoding keeps the wire path
        // uniform — there is no special "unauthed" request shape.
        let token = self.resolve_token(agent).await?;
        let body = inject_token(op, body_without_token, &token)?;
        let raw = self.call(agent, op, &body).await?;
        // An auth failure means the cached token is stale or revoked: drop it
        // and retry once with a fresh session.
        if is_auth_error(&raw) {
            self.sessions.lock().await.remove(&agent_bytes(agent));
            let token = self.resolve_token(agent).await?;
            let body = inject_token(op, body_without_token, &token)?;
            return self.call(agent, op, &body).await;
        }
        Ok(raw)
    }

    /// Ensure a cached session token for `agent`, authenticating if there is
    /// none.
    async fn resolve_token(&self, agent: DestinationHash) -> Result<String> {
        if let Some(t) = self.sessions.lock().await.get(&agent_bytes(agent)).cloned() {
            return Ok(t);
        }
        let session = self.authenticate(agent).await?;
        self.sessions
            .lock()
            .await
            .insert(agent_bytes(agent), session.token.clone());
        Ok(session.token)
    }

    /// Challenge + verify over a fresh Link, returning the session.
    async fn authenticate(&self, agent: DestinationHash) -> Result<Session> {
        let link = self.link_to(agent).await?;

        // Challenge.
        let challenge_raw = self
            .handle
            .request(link, endpoint_hash(), &encode_request(OP_CHALLENGE, &[]))
            .await
            .map_err(|e| anyhow!("challenge request failed: {e:?}"))?
            .0;
        let (_, ok, body) = decode_response(&challenge_raw)
            .map_err(|e| anyhow!("agent sent a challenge we cannot read: {e}"))?;
        if !ok {
            return Err(anyhow!(
                "agent refused the challenge: {}",
                String::from_utf8_lossy(body)
            ));
        }
        let challenge: Challenge = parse_payload(body).map_err(|e| anyhow!("{e}"))?;

        // The agent's audience is its own identity hash; sign for that.
        let audience = prns_core::identity::IdentityHash::new(
            hex::decode(&challenge.audience)
                .map_err(|_| anyhow!("agent challenge audience is not hex"))?
                .as_slice()
                .try_into()
                .map_err(|_| anyhow!("agent challenge audience is not 16 bytes"))?,
        );
        let identity = PrivateIdentityMaterial::from_slice(&self.secret[..])
            .map_err(|e| anyhow!("index identity: {e:?}"))?;
        let response: ChallengeResponse = answer_challenge(&challenge, &audience, &identity)
            .map_err(|e| anyhow!("signing the challenge: {e}"))?;

        // Verify.
        let verify_raw = self
            .handle
            .request(link, endpoint_hash(), &encode_request(OP_VERIFY, &payload_bytes(&response)?))
            .await
            .map_err(|e| anyhow!("verify request failed: {e:?}"))?
            .0;
        let _ = self.handle.close_link(link);
        let (_, ok, body) = decode_response(&verify_raw)
            .map_err(|e| anyhow!("agent sent a verify reply we cannot read: {e}"))?;
        if !ok {
            return Err(anyhow!(
                "agent refused to verify this index: {}",
                String::from_utf8_lossy(body)
            ));
        }
        parse_payload::<Session>(body).map_err(|e| anyhow!("{e}"))
    }

    /// Open a Link to the agent, requesting a path first if there is no route —
    /// the same fallback `client.rs` uses for an index query.
    async fn link_to(&self, agent: DestinationHash) -> Result<prns_core::routing::links::LinkId> {
        match self.handle.establish_link(agent).await {
            Ok(id) => Ok(id),
            Err(e) => {
                debug!(error = ?e, "no route to the agent; requesting a path first");
                self.handle
                    .request_path(agent)
                    .await
                    .map_err(|pe| anyhow!("no path to the agent: {pe:?}"))?;
                self.handle
                    .establish_link(agent)
                    .await
                    .map_err(|e2| anyhow!("link to the agent failed: {e2:?}"))
            }
        }
    }

    /// Send one op request over a fresh Link and return the response body bytes
    /// (the bytes after the envelope: status + payload).
    async fn call(&self, agent: DestinationHash, op: u8, body: &[u8]) -> Result<Vec<u8>> {
        let link = self.link_to(agent).await?;
        let raw = self
            .handle
            .request(link, endpoint_hash(), &encode_request(op, body))
            .await
            .map_err(|e| anyhow!("agent {op} request failed: {e:?}"))?
            .0;
        let _ = self.handle.close_link(link);
        Ok(raw)
    }
}

/// The cached-token map is keyed by the destination hash's 16 bytes.
fn agent_bytes(agent: DestinationHash) -> [u8; 16] {
    *agent.as_bytes()
}

/// Re-encode a request body with the resolved token. The request payloads all
/// carry a `token` field; rather than parse-and-edit each shape, we exploit that
/// each is JSON with a `"token":"..."` field whose value was the empty string.
/// That is fragile to formatting, so we parse into the typed struct, set the
/// token, and re-serialize — one path per op, explicit.
fn inject_token(op: u8, body: &[u8], token: &str) -> Result<Vec<u8>> {
    match op {
        OP_CREATE => {
            let mut r: CreateReq = parse_payload(body).map_err(|e| anyhow!("{e}"))?;
            r.token = token.to_string();
            payload_bytes(&r).map_err(|e| anyhow!("{e}"))
        }
        OP_STOP | OP_REMOVE => {
            let mut r: InstanceReq = parse_payload(body).map_err(|e| anyhow!("{e}"))?;
            r.token = token.to_string();
            payload_bytes(&r).map_err(|e| anyhow!("{e}"))
        }
        OP_LIST | OP_CAPACITY => {
            let mut r: TokenReq = parse_payload(body).map_err(|e| anyhow!("{e}"))?;
            r.token = token.to_string();
            payload_bytes(&r).map_err(|e| anyhow!("{e}"))
        }
        other => Err(anyhow!("cannot inject a token into op {other}")),
    }
}

/// A response body is an auth error if it decodes as an `err` envelope whose
/// message names the auth path — "untrusted identity", "no such session",
/// "session expired". The agent's `authorize`/`verify` produce these strings.
fn is_auth_error(raw: &[u8]) -> bool {
    let Ok((_, ok, body)) = decode_response(raw) else {
        return false;
    };
    if ok {
        return false;
    }
    let msg = String::from_utf8_lossy(body).to_ascii_lowercase();
    msg.contains("untrusted identity")
        || msg.contains("no such session")
        || msg.contains("session expired")
}

/// Decode a response envelope, require ok, and parse the payload.
fn parse_ok<T: for<'de> serde::Deserialize<'de>>(raw: &[u8], op: u8) -> Result<T> {
    let (resp_op, ok, body) =
        decode_response(raw).map_err(|e| anyhow!("agent reply we cannot read: {e}"))?;
    if resp_op != op {
        return Err(anyhow!("agent replied with op {resp_op}, expected {op}"));
    }
    if !ok {
        return Err(anyhow!(
            "the agent refused: {}",
            String::from_utf8_lossy(body)
        ));
    }
    parse_payload(body).map_err(|e| anyhow!("agent reply payload did not parse: {e}"))
}

fn expect_empty(raw: &[u8], op: u8) -> Result<()> {
    let (resp_op, ok, body) =
        decode_response(raw).map_err(|e| anyhow!("agent reply we cannot read: {e}"))?;
    if resp_op != op {
        return Err(anyhow!("agent replied with op {resp_op}, expected {op}"));
    }
    if !ok {
        return Err(anyhow!(
            "the agent refused: {}",
            String::from_utf8_lossy(body)
        ));
    }
    Ok(())
}