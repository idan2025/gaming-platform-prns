//! The agent as a Reticulum destination: the control uplink.
//!
//! The mirror of `crates/platform-index/src/node.rs`, on the other end of the
//! wire. An index that wants to deploy on this node opens a Link to this
//! destination and issues authenticated create/stop/remove/list requests, so the
//! agent needs **no inbound port and no public IP** — the same property the
//! platform sells to its users (`DESIGN.md` §2.3, `PLAN.md` §8 phase 4). The
//! loopback HTTP API (`api.rs`) stays for local use; this is for remote use.
//!
//! # Authentication is challenge/response; authorization is the allowlist
//!
//! The agent is the verifier; the index is the client. `platform_auth` does the
//! cryptography verbatim: the agent holds an `Authenticator` keyed to **its own**
//! identity hash, the index calls `answer_challenge` with that hash as audience,
//! and a signature bound to one agent is worthless at another (already pinned in
//! `platform_auth::tests::a_signature_for_one_index_is_worthless_at_another`).
//!
//! Authorization is separate and lives here: the operator's `trusted_indexes`
//! list. `Authenticator::verify` derives the caller's identity from the signing
//! key — never from a client-supplied hash — and the agent admits the session
//! only if that identity is in the list. **The check is repeated on every op,
//! not just at verify:** a session token alone is never enough, so an index
//! removed from the allowlist is refused on its next request, not its next
//! login. Empty `trusted_indexes` refuses every caller — hosting off, the same
//! shape as `HostingConfig.games` empty meaning hosting off.
//!
//! # Why not reject at the link
//!
//! `LinkRequestPolicy` is `AcceptAll`/`AcceptNone` only; `RequestEndpointPolicy`
//! has an `AllowList` variant but it wants a `&'static [IdentityHash]` we do not
//! have at compile time, and `cx.requester` is `Option` — only `Some` if the
//! peer *volunteered* its identity, which a stranger will not. So the link is
//! accepted and every mutating request is rejected without a valid session
//! token, the same pattern `DetailsEndpoint` uses (`game-bridge/src/relay.rs`).
//!
//! # Trust direction, stated plainly
//!
//! From the user's side, agents are untrusted — user-contributed nodes
//! (`DESIGN.md` §5). From the agent's side, the index it talks to is trusted:
//! the operator put it in `trusted_indexes`. The index authenticates to the
//! agent with its own key, and it passes the end-user's identity hash as
//! `spec.owner`, which the agent stamps onto the container as `OWNER_LABEL`
//! unchanged (`agent.rs`). Quota enforcement stays the index's job.

use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use personal_rns::prelude::*;
use personal_rns::load_or_create_identity_secret;
use prns_core::identity::{IdentityHash, PrivateIdentityMaterial};
use tokio::sync::{oneshot, Mutex};
use tokio::task::LocalSet;
use tracing::{debug, info, warn};

use platform_auth::{Authenticator, ChallengeResponse, Session};

use crate::agent::Agent;
use crate::config::UplinkConfig;
use crate::uplink_wire::*;

/// How often the agent re-announces its control destination.
const ANNOUNCE_INTERVAL_SECS: u64 = 30;

/// Reticulum app name + aspect the agent announces under. An index discovers
/// agents the same way it discovers game servers — by hearing the announce —
/// though this increment wires nodes statically (`PLAN.md` §8 phase 4).
pub const AGENT_APP_NAME: &str = "platform-agent";
pub const AGENT_ASPECT: &str = "control";

/// The single request endpoint every op rides on. One endpoint, op byte inside.
pub const CONTROL_ENDPOINT_ID: &str = "/platform-agent/control/1";

/// State the request endpoint handler sees.
struct UplinkState {
    agent: Arc<Agent>,
    auth: Mutex<Authenticator>,
    trusted: Vec<IdentityHash>,
}

/// A running agent uplink node. Dropping the handle stops it.
pub struct AgentUplinkNode {
    destination: DestinationHash,
    handle: PrnsNodeHandle,
    stop_tx: Option<oneshot::Sender<()>>,
    done: Option<oneshot::Receiver<()>>,
}

impl AgentUplinkNode {
    /// The destination an index links to. Also what the node announces.
    pub fn destination(&self) -> DestinationHash {
        self.destination
    }

    /// The node's command handle (`Send + Sync + Clone`, see `relay.rs`), so the
    /// local API can add, remove, rename and list mesh interfaces on the live
    /// node after it started (`interfaces.rs`, `PLAN.md` §13.5). The uplink was
    /// the only place a node was created and its interfaces were fixed at start;
    /// handing the handle out is what lets the web UI configure them live.
    pub fn handle(&self) -> PrnsNodeHandle {
        self.handle.clone()
    }

    pub async fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.done.take() {
            let _ = rx.await;
        }
    }
}

// ---- Authorization, factored so it tests without a Docker daemon ----------

/// Resolve a session token to a trusted identity, or refuse.
///
/// The trusted check runs on **every** op, not only at verify, so an index
/// removed from the allowlist is refused on its next request — the load-bearing
/// invariant `an_untrusted_index_cannot_create_an_instance_even_with_a_valid_signature`
/// and `a_trusted_index_removed_from_the_allowlist_is_killed_mid_session`.
async fn authorize(
    auth: &Mutex<Authenticator>,
    trusted: &[IdentityHash],
    token: &str,
    now: SystemTime,
) -> Result<IdentityHash, String> {
    let identity = auth.lock().await.authenticate(token, now).map_err(|e| e.to_string())?;
    if !trusted.iter().any(|h| h == &identity) {
        return Err("untrusted identity".to_string());
    }
    Ok(identity)
}

/// Verify a challenge response, then authorize the resulting session.
///
/// The single choke point: a valid signature from an identity the operator did
/// not list is refused and the session is burned, so it cannot be replayed. The
/// auth binding (audience, domain separation, one-shot nonce) is `platform_auth`'s
/// job; this is only the allowlist gate on top of it.
async fn verify_and_authorize(
    auth: &Mutex<Authenticator>,
    trusted: &[IdentityHash],
    response: ChallengeResponse,
    now: SystemTime,
) -> Result<Session, String> {
    let session = auth.lock().await.verify(&response, now).map_err(|e| e.to_string())?;
    // The signature verified. Re-authenticate to get the typed identity back,
    // then check the allowlist. An untrusted identity gets its just-minted
    // session revoked, not merely declined.
    let identity = auth.lock().await.authenticate(&session.token, now).map_err(|e| e.to_string())?;
    if !trusted.iter().any(|h| h == &identity) {
        auth.lock().await.revoke(&session.token);
        return Err("untrusted identity".to_string());
    }
    Ok(session)
}

// ---- The request endpoint ---------------------------------------------------

struct ControlEndpoint;

impl RequestEndpoint<Arc<UplinkState>> for ControlEndpoint {
    const ENDPOINT_ID: &'static str = CONTROL_ENDPOINT_ID;
    // Gating is in the handler: `POLICY` is a const and cannot see the runtime
    // allowlist, and `AllowList` wants a `&'static [IdentityHash]` we do not
    // have. Same constraint and same resolution as `DetailsEndpoint`.
    const POLICY: RequestEndpointPolicy = RequestEndpointPolicy::AllowAll;

    async fn handle(mut cx: RequestContext<'_, Arc<UplinkState>>) -> Result<(), Decline> {
        let now = SystemTime::now();
        let (op, body) = match decode_request(cx.data) {
            Ok(v) => v,
            Err(e) => {
                // We do not know the op, so the client sees an unknown op back
                // rather than a mis-paired response. Malformed input never
                // panics.
                let _ = cx.respond(encode_err(0, &e.to_string()));
                return Ok(());
            }
        };

        let resp = dispatch(cx.state, op, body, now).await;
        let _ = cx.respond(resp);
        Ok(())
    }
}

/// One op. Returns the full response envelope so the handler can hand it to
/// `cx.respond` unchanged. Every path answers — a decline would leave the client
/// waiting on a timeout, and the client can render an error as easily as a result.
async fn dispatch(state: &Arc<UplinkState>, op: u8, body: &[u8], now: SystemTime) -> Vec<u8> {
    match op {
        OP_CHALLENGE => match state.auth.lock().await.issue_challenge(now) {
            Ok(challenge) => match payload_bytes(&challenge) {
                Ok(bytes) => encode_ok(OP_CHALLENGE, &bytes),
                Err(e) => encode_err(OP_CHALLENGE, &e.to_string()),
            },
            Err(e) => encode_err(OP_CHALLENGE, &e.to_string()),
        },

        OP_VERIFY => {
            let response: ChallengeResponse = match parse_payload(body) {
                Ok(r) => r,
                Err(e) => return encode_err(OP_VERIFY, &e.to_string()),
            };
            match verify_and_authorize(&state.auth, &state.trusted, response, now).await {
                Ok(session) => match payload_bytes(&session) {
                    Ok(bytes) => encode_ok(OP_VERIFY, &bytes),
                    Err(e) => encode_err(OP_VERIFY, &e.to_string()),
                },
                Err(msg) => encode_err(OP_VERIFY, &msg),
            }
        }

        OP_CREATE => {
            let req: CreateReq = match parse_payload(body) {
                Ok(r) => r,
                Err(e) => return encode_err(OP_CREATE, &e.to_string()),
            };
            if let Err(msg) = authorize(&state.auth, &state.trusted, &req.token, now).await {
                return encode_err(OP_CREATE, &msg);
            }
            // `spec.owner` already carries the end-user's identity hash; the
            // agent stamps `OWNER_LABEL` from it (`agent.rs`). The authenticated
            // index asserted it, and the operator trusted that index.
            match state.agent.create(req.spec).await {
                Ok(status) => match payload_bytes(&status) {
                    Ok(bytes) if fits(bytes.len()).is_ok() => encode_ok(OP_CREATE, &bytes),
                    Ok(bytes) => encode_err(OP_CREATE, &WireError::TooLarge(bytes.len()).to_string()),
                    Err(e) => encode_err(OP_CREATE, &e.to_string()),
                },
                Err(e) => encode_err(OP_CREATE, &format!("{e:#}")),
            }
        }

        OP_STOP => {
            let req: InstanceReq = match parse_payload(body) {
                Ok(r) => r,
                Err(e) => return encode_err(OP_STOP, &e.to_string()),
            };
            if let Err(msg) = authorize(&state.auth, &state.trusted, &req.token, now).await {
                return encode_err(OP_STOP, &msg);
            }
            match state.agent.stop(&req.instance_id).await {
                Ok(()) => encode_ok(OP_STOP, &[]),
                Err(e) => encode_err(OP_STOP, &format!("{e:#}")),
            }
        }

        OP_REMOVE => {
            let req: InstanceReq = match parse_payload(body) {
                Ok(r) => r,
                Err(e) => return encode_err(OP_REMOVE, &e.to_string()),
            };
            if let Err(msg) = authorize(&state.auth, &state.trusted, &req.token, now).await {
                return encode_err(OP_REMOVE, &msg);
            }
            match state.agent.remove(&req.instance_id).await {
                Ok(()) => encode_ok(OP_REMOVE, &[]),
                Err(e) => encode_err(OP_REMOVE, &format!("{e:#}")),
            }
        }

        OP_LIST => {
            let req: TokenReq = match parse_payload(body) {
                Ok(r) => r,
                Err(e) => return encode_err(OP_LIST, &e.to_string()),
            };
            if let Err(msg) = authorize(&state.auth, &state.trusted, &req.token, now).await {
                return encode_err(OP_LIST, &msg);
            }
            // `list_detailed`, so an index deciding what to reap sees real
            // player counts rather than a field it would have to guess at.
            match state.agent.list_detailed().await {
                Ok(instances) => encode_list_resp(&instances, instances.len()),
                Err(e) => encode_err(OP_LIST, &format!("{e:#}")),
            }
        }

        OP_CAPACITY => {
            let req: TokenReq = match parse_payload(body) {
                Ok(r) => r,
                Err(e) => return encode_err(OP_CAPACITY, &e.to_string()),
            };
            if let Err(msg) = authorize(&state.auth, &state.trusted, &req.token, now).await {
                return encode_err(OP_CAPACITY, &msg);
            }
            // Capacity is pulled, not pushed: the same authenticated link, no
            // index-side ingress endpoint. `Agent::capacity` is shared with the
            // loopback API so the two surfaces cannot answer differently.
            let resp = state.agent.capacity().await;
            match payload_bytes(&resp) {
                Ok(bytes) => encode_ok(OP_CAPACITY, &bytes),
                Err(e) => encode_err(OP_CAPACITY, &e.to_string()),
            }
        }

        other => encode_err(other, "unknown op"),
    }
}

// ---- Starting the node ------------------------------------------------------

/// Parse a 32-char hex identity hash (validated by `config`) into the typed form.
fn parse_identity_hash(hex_str: &str) -> Result<IdentityHash> {
    let bytes = hex::decode(hex_str).map_err(|e| anyhow!("bad identity hash: {e}"))?;
    let arr: [u8; 16] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("identity hash is not 16 bytes"))?;
    Ok(IdentityHash::new(arr))
}

/// Start the Reticulum side of the agent.
///
/// `identity_secret_path` is loaded (or created on first run, like the bridge)
/// from `UplinkConfig`. The same key is the announce identity, the auth
/// audience, and the transport identity — one key, consistent.
pub async fn start(agent: Arc<Agent>, config: UplinkConfig) -> Result<AgentUplinkNode> {
    let secret = load_or_create_identity_secret(&config.identity_secret_path)
        .map_err(|e| anyhow!("loading agent identity: {e}"))?;
    let identity = PrivateIdentityMaterial::from_slice(&secret[..])
        .map_err(|e| anyhow!("agent identity key: {e:?}"))?;
    let identity_hash = identity.identity_hash();

    let trusted: Vec<IdentityHash> = config
        .trusted_indexes
        .iter()
        .map(|h| parse_identity_hash(h))
        .collect::<Result<_>>()?;

    let state = Arc::new(UplinkState {
        agent,
        auth: Mutex::new(Authenticator::new(identity_hash)),
        trusted,
    });

    let destination_hash = PreConfiguredDestination::Single {
        app_name: AGENT_APP_NAME,
        aspects: &[AGENT_ASPECT],
        identity: secret.clone(),
        announce_app_data: b"",
        proof: ProofStrategy::ProveAll,
        link_requests: LinkRequestPolicy::AcceptAll,
        ratchet: RatchetPolicy::NoRatchets,
        resource_strategy: ResourceStrategy::AcceptNone,
        maximum_request_bytes: Default::default(),
        request_endpoints: ServeMyRequestEndpoints::Yes,
    }
    .destination_hash()
    .map_err(|e| anyhow!("invalid agent destination: {e:?}"))?;

    // The init handshake carries the node handle out of the dedicated uplink
    // thread as well as the stop channel, so the local API can drive interfaces
    // on the live node (`interfaces.rs`). The handle is `Send + Sync`
    // (`relay.rs`), so moving it across the thread boundary is sound.
    let (init_tx, init_rx) =
        oneshot::channel::<Result<(oneshot::Sender<()>, PrnsNodeHandle)>>();
    let (done_tx, done_rx) = oneshot::channel::<()>();

    let tcp = config.tcp;
    let auto = config.auto;

    std::thread::Builder::new()
        .name("platform-agent-uplink".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    let _ = init_tx.send(Err(anyhow!("build runtime: {e}")));
                    return;
                }
            };
            let local = LocalSet::new();
            rt.block_on(local.run_until(async move {
                let destination = PreConfiguredDestination::Single {
                    app_name: AGENT_APP_NAME,
                    aspects: &[AGENT_ASPECT],
                    identity: secret.clone(),
                    announce_app_data: b"",
                    proof: ProofStrategy::ProveAll,
                    link_requests: LinkRequestPolicy::AcceptAll,
                    ratchet: RatchetPolicy::NoRatchets,
                    resource_strategy: ResourceStrategy::AcceptNone,
                    maximum_request_bytes: Default::default(),
                    request_endpoints: ServeMyRequestEndpoints::Yes,
                };

                let node = PrnsNode::new(PrnsNodeRecipe {
                    // An agent is infrastructure: it carries transit for others,
                    // like the index node, unlike a player's client.
                    transport_identity: Some(secret),
                    pre_configured_destinations: [destination],
                    app_state: state,
                    storage: GrowableHeap,
                    request_endpoints: request_endpoints![ControlEndpoint],
                    interfaces: |node: &PrnsNodeHandle| {
                        game_bridge::relay::attach_interfaces(node, tcp.as_deref(), auto);
                    },
                    persistence: NoPersistence,
                    on_event: |_event, _state| {},
                });
                let handle = node.handle();

                let announcer = handle.clone();
                let _announce_task = tokio::spawn(async move {
                    let mut ticker =
                        tokio::time::interval(std::time::Duration::from_secs(ANNOUNCE_INTERVAL_SECS));
                    ticker.tick().await;
                    loop {
                        ticker.tick().await;
                        if announcer
                            .issue(PrnsCommand::AnnounceNow(AnnounceNow {
                                destination: destination_hash,
                                target: AnnounceTarget::AllInterfaces,
                                app_data: AnnounceAppData::Registered,
                            }))
                            .is_none()
                        {
                            return;
                        }
                    }
                });

                let (stop_tx, stop_rx) = oneshot::channel::<()>();
                if init_tx.send(Ok((stop_tx, handle.clone()))).is_err() {
                    return;
                }
                debug!("agent uplink node running");
                tokio::select! {
                    _ = node.run() => {}
                    _ = stop_rx => {}
                }
            }));
            drop(rt);
            let _ = done_tx.send(());
        })
        .map_err(|e| anyhow!("spawning the agent uplink thread: {e}"))?;

    let (stop_tx, handle) = init_rx
        .await
        .map_err(|_| anyhow!("agent uplink thread died before starting"))??;

    info!(
        destination = %hex::encode(destination_hash.as_bytes()),
        trusted = config.trusted_indexes.len(),
        "agent uplink answering over Reticulum"
    );
    if config.trusted_indexes.is_empty() {
        warn!("agent uplink has no trusted indexes; every caller is refused");
    }
    Ok(AgentUplinkNode {
        destination: destination_hash,
        handle,
        stop_tx: Some(stop_tx),
        done: Some(done_rx),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_auth::answer_challenge;
    use prns_core::identity::PrivateIdentityMaterial;
    use std::time::{Duration, SystemTime};

    /// A 64-byte Reticulum secret: X25519 secret ‖ Ed25519 secret.
    fn secret(seed: u8) -> PrivateIdentityMaterial {
        PrivateIdentityMaterial::from_slice(&[seed; 64]).unwrap()
    }

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn verifier(seed: u8) -> Mutex<Authenticator> {
        Mutex::new(Authenticator::new(secret(seed).identity_hash()))
    }

    fn audience_of(seed: u8) -> IdentityHash {
        secret(seed).identity_hash()
    }

    /// The whole authorization in one test: a valid signature from an identity
    /// the operator did not list is refused, and the session is burned.
    #[tokio::test]
    async fn an_untrusted_identity_cannot_verify_even_with_a_valid_signature() {
        let auth = verifier(9);
        // The operator trusts somebody else, not the caller.
        let trusted: Vec<IdentityHash> = vec![secret(42).identity_hash()];

        let caller = secret(1);
        let challenge = auth.lock().await.issue_challenge(t0()).unwrap();
        let response = answer_challenge(&challenge, &audience_of(9), &caller).unwrap();

        let result = verify_and_authorize(&auth, &trusted, response, t0()).await;
        assert!(result.is_err());
        assert_eq!(
            auth.lock().await.active_sessions(),
            0,
            "an untrusted identity's session was not burned"
        );
    }

    /// A trusted identity verifies and gets a session that authorizes follow-up
    /// ops.
    #[tokio::test]
    async fn a_trusted_identity_verifies_and_authorizes() {
        let caller = secret(1);
        let auth = verifier(9);
        let trusted = vec![caller.identity_hash()];

        let challenge = auth.lock().await.issue_challenge(t0()).unwrap();
        let response = answer_challenge(&challenge, &audience_of(9), &caller).unwrap();
        let session = verify_and_authorize(&auth, &trusted, response, t0()).await.unwrap();
        assert_eq!(session.identity, hex::encode(caller.identity_hash().as_bytes()));

        // The token authorizes a follow-up op against the same allowlist.
        assert!(authorize(&auth, &trusted, &session.token, t0()).await.is_ok());
    }

    /// The trusted check runs on every op, so the same token stops authorizing
    /// once the identity is no longer in the list. This is the
    /// "removed-from-allowlist mid-session" invariant: nothing caches trust.
    #[tokio::test]
    async fn a_token_stops_authorizing_when_the_identity_leaves_the_allowlist() {
        let caller = secret(1);
        let auth = verifier(9);
        let trusted_with = vec![caller.identity_hash()];
        let trusted_without: Vec<IdentityHash> = vec![];

        let challenge = auth.lock().await.issue_challenge(t0()).unwrap();
        let response = answer_challenge(&challenge, &audience_of(9), &caller).unwrap();
        let session = verify_and_authorize(&auth, &trusted_with, response, t0()).await.unwrap();

        assert!(authorize(&auth, &trusted_with, &session.token, t0()).await.is_ok());
        assert!(
            authorize(&auth, &trusted_without, &session.token, t0()).await.is_err(),
            "a token whose identity left the allowlist still authorized"
        );
    }

    /// A request with no session token is refused, not treated as anonymous.
    #[tokio::test]
    async fn a_request_without_a_session_token_is_refused() {
        let auth = verifier(9);
        let trusted = vec![secret(1).identity_hash()];
        assert!(authorize(&auth, &trusted, "", t0()).await.is_err());
        assert!(authorize(&auth, &trusted, "not-a-token", t0()).await.is_err());
    }

    /// A signature bound to a different agent's audience does not verify here —
    /// re-pinned at the uplink layer, on top of `platform_auth`'s own test.
    #[tokio::test]
    async fn a_signature_for_another_agent_does_not_verify_here() {
        let caller = secret(1);
        let other = verifier(200);
        let challenge = other.lock().await.issue_challenge(t0()).unwrap();
        // The caller signs for the other agent's audience.
        let response = answer_challenge(&challenge, &audience_of(200), &caller).unwrap();

        let this_agent = verifier(201);
        let trusted = vec![caller.identity_hash()];
        let result = verify_and_authorize(&this_agent, &trusted, response, t0()).await;
        assert!(result.is_err());
    }

    /// `parse_identity_hash` accepts the shape `config` validates and rejects
    /// the rest, so a config typo cannot become a silent allowlist miss.
    #[test]
    fn parse_identity_hash_round_trips_a_config_entry() {
        let h = secret(1).identity_hash();
        let hex = hex::encode(h.as_bytes());
        assert_eq!(parse_identity_hash(&hex).unwrap(), h);
        assert!(parse_identity_hash("not-hex").is_err());
        assert!(parse_identity_hash(&hex::encode([0u8; 15])).is_err());
    }
}