//! The index as a Reticulum destination.
//!
//! This is the half of `DESIGN.md` §2.4 that the architecture depends on. HTTP
//! is for people with internet; this is for a client on a mesh that has none.
//! Without it, "an index is a convenience, never a dependency" quietly becomes
//! "an index is a convenience if you are online", and the offline mesh case —
//! the thing this platform is for — loses its directory.
//!
//! # Finding an index is itself an announce
//!
//! The node announces `platform-index.query`. A launcher discovers indexes the
//! same way it discovers game servers: by listening. There is no bootstrap list,
//! no well-known address, and nothing to hard-code — which is what stops one
//! index from becoming load-bearing by being the only one anybody can find.
//!
//! # It answers anyone
//!
//! The query endpoint takes no authentication. Listing is the index's public
//! good; requiring a session to read a list of servers anyone could hear for
//! themselves would be theatre with a login attached.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use game_bridge::browse::{BrowseFilter, BrowseQuery};
use personal_rns::prelude::*;
use tokio::sync::oneshot;
use tokio::task::LocalSet;
use tracing::{debug, info};

use crate::http::IndexState;
use crate::wire::{
    encode_result, IndexQuery, INDEX_APP_NAME, INDEX_ASPECT, QUERY_ENDPOINT_ID,
};

/// How often the index re-announces itself.
const ANNOUNCE_INTERVAL_SECS: u64 = 30;

/// The query endpoint, server side.
struct QueryEndpoint;

impl RequestEndpoint<Arc<IndexState>> for QueryEndpoint {
    const ENDPOINT_ID: &'static str = QUERY_ENDPOINT_ID;
    const POLICY: RequestEndpointPolicy = RequestEndpointPolicy::AllowAll;

    async fn handle(mut cx: RequestContext<'_, Arc<IndexState>>) -> Result<(), Decline> {
        // A malformed query is answered with an empty result rather than a
        // decline. A client that sent nonsense gets a well-formed "nothing",
        // which it can render; a decline it would have to special-case.
        let query = IndexQuery::decode(cx.data).unwrap_or_default();

        let browse = BrowseQuery {
            filter: BrowseFilter {
                game_id: (!query.game_id.is_empty()).then(|| query.game_id.clone()),
                max_hops: query.max_hops,
                has_players: query.has_players,
                include_legacy: query.include_legacy,
                ..Default::default()
            },
            ..Default::default()
        };

        let registry = cx.state.registry.lock().await;
        let matched = registry.query(&browse, Instant::now());
        let body = encode_result(&matched, matched.len());
        drop(registry);

        cx.respond(body)
    }
}

/// A running index node. Dropping the handle stops it.
pub struct IndexNode {
    destination: DestinationHash,
    handle: Option<PrnsNodeHandle>,
    stop_tx: Option<oneshot::Sender<()>>,
    done: Option<oneshot::Receiver<()>>,
}

impl IndexNode {
    /// The destination clients query. Also what the node announces.
    pub fn destination(&self) -> DestinationHash {
        self.destination
    }

    /// The node's command handle, so other parts of the index (the hosted-deploy
    /// path driving remote agents) can open Links and issue requests on the same
    /// Reticulum stack. `PrnsNodeHandle` is `Send`, so it can leave the node's
    /// `!Send` runtime thread.
    pub fn handle(&self) -> PrnsNodeHandle {
        self.handle
            .clone()
            .expect("handle is present until stop() consumes it")
    }

    pub async fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        self.handle = None;
        if let Some(rx) = self.done.take() {
            let _ = rx.await;
        }
    }
}

/// Start the Reticulum side of an index.
///
/// `identity` is the index's own key — the same one clients bind their auth
/// signatures to, so the destination they query and the audience they sign for
/// are provably the same party.
pub async fn start(
    state: Arc<IndexState>,
    identity: Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>,
    tcp: Option<String>,
    auto: bool,
) -> Result<IndexNode> {
    let destination_hash = PreConfiguredDestination::Single {
        app_name: INDEX_APP_NAME,
        aspects: &[INDEX_ASPECT],
        identity: identity.clone(),
        announce_app_data: b"",
        proof: ProofStrategy::ProveAll,
        link_requests: LinkRequestPolicy::AcceptAll,
        ratchet: RatchetPolicy::NoRatchets,
        resource_strategy: ResourceStrategy::AcceptNone,
        maximum_request_bytes: Default::default(),
        request_endpoints: ServeMyRequestEndpoints::Yes,
    }
    .destination_hash()
    .map_err(|e| anyhow!("invalid index destination: {e:?}"))?;

    let (init_tx, init_rx) = oneshot::channel::<Result<(oneshot::Sender<()>, PrnsNodeHandle)>>();
    let (done_tx, done_rx) = oneshot::channel::<()>();

    // The node future is !Send, so it gets its own thread and LocalSet — the
    // same arrangement game-bridge uses, and for the same reason.
    std::thread::Builder::new()
        .name("platform-index-node".into())
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
                    app_name: INDEX_APP_NAME,
                    aspects: &[INDEX_ASPECT],
                    identity: identity.clone(),
                    announce_app_data: b"",
                    proof: ProofStrategy::ProveAll,
                    link_requests: LinkRequestPolicy::AcceptAll,
                    ratchet: RatchetPolicy::NoRatchets,
                    resource_strategy: ResourceStrategy::AcceptNone,
                    maximum_request_bytes: Default::default(),
                    request_endpoints: ServeMyRequestEndpoints::Yes,
                };

                let node = PrnsNode::new(PrnsNodeRecipe {
                    // An index is a service that wants to be reachable, so it
                    // carries transit for others too. Unlike a player's client,
                    // it opted into being infrastructure.
                    transport_identity: Some(identity),
                    pre_configured_destinations: [destination],
                    app_state: state,
                    storage: GrowableHeap,
                    request_endpoints: request_endpoints![QueryEndpoint],
                    on_event: |_event, _state| {},
                    interfaces: |node: &PrnsNodeHandle| {
                        game_bridge::relay::attach_interfaces(node, tcp.as_deref(), auto);
                    },
                    persistence: NoPersistence,
                });
                let handle = node.handle();

                let announcer = handle.clone();
                let _announce_task = tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
                        ANNOUNCE_INTERVAL_SECS,
                    ));
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
                debug!("index node running");
                tokio::select! {
                    _ = node.run() => {}
                    _ = stop_rx => {}
                }
            }));
            drop(rt);
            let _ = done_tx.send(());
        })
        .map_err(|e| anyhow!("spawning the index node thread: {e}"))?;

    let (stop_tx, handle) = init_rx
        .await
        .map_err(|_| anyhow!("index node thread died before starting"))??;

    info!(
        destination = %hex::encode(destination_hash.as_bytes()),
        "index answering queries over Reticulum"
    );
    Ok(IndexNode {
        destination: destination_hash,
        handle: Some(handle),
        stop_tx: Some(stop_tx),
        done: Some(done_rx),
    })
}
