//! The bridge itself: builds the Prns node, wires interfaces, and pumps game
//! datagrams in and out of Reticulum Links.
//!
//! Copied from `idan2025/Svencoop-Prns` `src/relay.rs` at v0.1.10 and
//! parametrized by `GameProfile` (`PLAN.md` §8 step 2). The extraction is
//! one-directional (`PLAN.md` §5): nothing here may become a dependency of the
//! standalone app, and a fix landed here is a separate, deliberate decision to
//! port there.
//!
//! What was Sven-specific and is now a profile field:
//!   - the app name, hardcoded `sven-coop` at the original's `src/relay.rs:189,
//!     207,397,434`, which *is* the destination hash and therefore the wire
//!     contract;
//!   - the local port, hardcoded 27015;
//!   - every log line that named the game.
//!
//! What deliberately did not change: the aspects stay `server`/`client` (they
//! describe the role, not the game), and the framing stays generation 1.
//!
//! Wire contract: one raw game datagram per Reticulum link packet,
//! bytes-in-bytes-out. Reticulum supplies encryption, ordering, routing.
//!
//! Server side
//! -----------
//!  - Announces `<app_name>.server` regularly.
//!  - For each accepted link, opens a UDP socket to the local game server and
//!    pumps link<->UDP both ways until the link closes.
//!
//! Client side
//! -----------
//!  - Binds `127.0.0.1:<listen_port>`.
//!  - Discovers `<app_name>.server` via announce (or uses an explicit hash).
//!  - For each distinct game-client source addr, opens a link and pumps
//!    UDP<->link both ways until the link closes.
//!
//! Both sides accumulate every `<app_name>.server` announce heard into a
//! discovered-server list (`BridgeSession::discovered`), which is the raw
//! material for the Browse role (`PLAN.md` §3).
//!
//! The node's `run()` future is `!Send` (it holds a non-Send guard across an
//! await), so it cannot be `tokio::spawn`'d on a multi-thread runtime. Each
//! `BridgeSession` therefore drives the node on a dedicated current-thread
//! runtime + `LocalSet` in its own OS thread, and hands the caller a
//! `PrnsNodeHandle` (which is `Send + Sync`) for live control plus a stop
//! channel.

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use personal_rns::prelude::*;
use personal_rns::{load_or_create_identity_secret, IdentitySecretFileError};
use prns_core::engine::{SendToLink, SendToLinkPayload};
use prns_core::identity::in_memory::InMemoryNodeIdentity;
use prns_core::identity::{IdentityHash, IdentitySigner};
use prns_core::routing::announce::emit::{AnnounceAppDataBytes, MAX_ANNOUNCE_APP_DATA_LEN};
use prns_core::routing::delivery::Delivery;
use prns_core::interfaces::InterfaceId;
use prns_core::routing::links::LinkId;
use prns_core::routing::request_handlers::RequestPathHash;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::task::LocalSet;
use tracing::{debug, error, info, warn};

use crate::announce::{self, AnnounceFlags, AnnounceInfo, AnnounceRecord};
use crate::details::{ServerDetails, StatsSource, DETAILS_ENDPOINT_ID};
use crate::config::{
    AnnounceFormat, BridgeConfig, BridgeRole, BrowserArgs, ClientArgs, RelayArgs, ServerArgs,
};
use crate::framing::{frame, Reassembler};
use crate::profile::{ASPECT_CLIENT, ASPECT_SERVER};

const UDP_READ_BUF: usize = 8192;

/// How often the server re-reads live stats out of its game server.
///
/// Polled on a timer rather than queried inside the request handler: an A2S
/// query takes up to two seconds, and the handler runs on the node's own
/// single-threaded runtime, so a probe that blocked on it would stall every
/// other peer's traffic for that long. The cost is that "live" means "read
/// this recently", which `ServerDetails::stats_age_secs` reports rather than
/// hides.
const LIVE_STATS_INTERVAL: Duration = Duration::from_secs(10);

/// Live figures read out of the game server, and when.
struct LiveStats {
    players: u8,
    max_players: u8,
    map: String,
    player_names: Vec<String>,
    read_at: Instant,
}

/// What the server knows about itself, shared with the detail-probe endpoint.
///
/// Held by the node as its `app_state`, so a request handler gets `&ServerState`
/// and never has to reach back into the relay's tokio structures.
#[derive(Clone)]
pub struct ServerState {
    inner: Arc<ServerStateInner>,
}

struct ServerStateInner {
    game_id: String,
    name: String,
    /// The map as configured. A live read supersedes it when one is available.
    announced_map: String,
    announced_players: u8,
    announced_max_players: u8,
    started: Instant,
    bridge_clients: std::sync::atomic::AtomicU16,
    live: std::sync::Mutex<Option<LiveStats>>,
    /// Empty means "open"; non-empty means only these identities may ask.
    allowlist: Vec<IdentityHash>,
}

impl ServerState {
    fn new(args: &ServerArgs, allowlist: Vec<IdentityHash>) -> Self {
        let record = server_announce_record(args);
        Self {
            inner: Arc::new(ServerStateInner {
                game_id: record.game_id,
                name: record.name,
                announced_map: record.map,
                announced_players: record.players,
                announced_max_players: record.max_players,
                started: Instant::now(),
                bridge_clients: std::sync::atomic::AtomicU16::new(0),
                live: std::sync::Mutex::new(None),
                allowlist,
            }),
        }
    }

    fn set_bridge_clients(&self, n: usize) {
        self.inner
            .bridge_clients
            .store(n.min(u16::MAX as usize) as u16, std::sync::atomic::Ordering::Relaxed);
    }

    fn store_live(&self, stats: LiveStats) {
        if let Ok(mut slot) = self.inner.live.lock() {
            *slot = Some(stats);
        }
    }

    /// Whether `requester` may ask this server about itself.
    ///
    /// An allowlisted server does not hand its roster to strangers. The
    /// allowlist already decides who may *play*; who may see who is playing is
    /// the same question, and answering it more freely would leak exactly what
    /// the allowlist exists to keep private.
    fn may_answer(&self, requester: Option<IdentityHash>) -> bool {
        if self.inner.allowlist.is_empty() {
            return true;
        }
        match requester {
            Some(id) => self.inner.allowlist.contains(&id),
            None => false,
        }
    }

    fn details(&self) -> ServerDetails {
        let i = &self.inner;
        let uptime_secs = i.started.elapsed().as_secs().min(u32::MAX as u64) as u32;
        let bridge_clients = i.bridge_clients.load(std::sync::atomic::Ordering::Relaxed);

        let live = i.live.lock().ok().and_then(|slot| {
            slot.as_ref().map(|s| {
                (
                    s.players,
                    s.max_players,
                    s.map.clone(),
                    s.player_names.clone(),
                    s.read_at.elapsed().as_secs().min(u16::MAX as u64) as u16,
                )
            })
        });

        match live {
            Some((players, max_players, map, player_names, age)) => ServerDetails {
                game_id: i.game_id.clone(),
                name: i.name.clone(),
                map,
                players,
                max_players,
                stats_source: StatsSource::Live,
                uptime_secs,
                bridge_clients,
                stats_age_secs: age,
                player_names,
                roster_truncated: false,
            },
            None => ServerDetails {
                game_id: i.game_id.clone(),
                name: i.name.clone(),
                map: i.announced_map.clone(),
                players: i.announced_players,
                max_players: i.announced_max_players,
                stats_source: StatsSource::Announced,
                uptime_secs,
                bridge_clients,
                stats_age_secs: 0,
                player_names: Vec::new(),
                roster_truncated: false,
            },
        }
    }
}

/// The `PLAN.md` §3.4 detail probe, server side.
struct DetailsEndpoint;

impl RequestEndpoint<ServerState> for DetailsEndpoint {
    const ENDPOINT_ID: &'static str = DETAILS_ENDPOINT_ID;
    // Gating happens in the handler instead, against the configured allowlist:
    // `POLICY` is a const and cannot see runtime config, and `AllowList` wants
    // a `&'static [IdentityHash]` we do not have.
    const POLICY: RequestEndpointPolicy = RequestEndpointPolicy::AllowAll;

    async fn handle(mut cx: RequestContext<'_, ServerState>) -> Result<(), Decline> {
        if !cx.state.may_answer(cx.requester) {
            return Err(Decline::Ignore);
        }
        // The request body advertises the requester's newest schema. We answer
        // in ours regardless; a requester too old to read it gets a clean
        // `UnsupportedSchema` from its own decoder rather than a mis-parse.
        let bytes = cx.state.details().encode();
        cx.respond(bytes)
    }
}

pub async fn run_bridge(cfg: BridgeConfig) -> Result<()> {
    let session = match cfg {
        BridgeConfig::Server(args) => BridgeSession::start_server(args).await?,
        BridgeConfig::Client(args) => BridgeSession::start_client(args).await?,
        BridgeConfig::Relay(args) => BridgeSession::start_relay(args).await?,
        BridgeConfig::Browse(args) => BridgeSession::start_browser(args).await?,
    };
    session.await_completion().await
}

/// A running bridge: owns a `PrnsNodeHandle` (for live interface control and
/// introspection) and the discovered-server list, and can stop the node.
pub struct BridgeSession {
    handle: PrnsNodeHandle,
    discovered: Arc<RwLock<Vec<DiscoveredServer>>>,
    connected_clients: ConnectedClients,
    own_hash: Option<DestinationHash>,
    role: BridgeRole,
    relay_transit: bool,
    stop_tx: Option<oneshot::Sender<()>>,
    done: Option<oneshot::Receiver<()>>,
}

/// A discovered `<app_name>.server` destination heard via announce.
#[derive(Debug, Clone)]
pub struct DiscoveredServer {
    pub destination_hash: DestinationHash,
    pub last_seen: Instant,
    /// Mesh distance, straight off the announce.
    ///
    /// `PLAN.md` §3.4 sorts the browser by this and shows it instead of a
    /// ping, because it is the one distance figure that is both free and
    /// honest. A ping would have to be measured by opening a Link to every
    /// server in the list, which is exactly the traffic a decentralized
    /// browser must not generate.
    pub hops: u8,
    /// Which of this node's interfaces heard the announce. Shown, not
    /// filtered on by default: it is how a user tells a LAN neighbour from
    /// something eight hops away over a TCP relay.
    pub source_interface: InterfaceId,
    /// What the announce said about itself: a §3.3 record, or a bare display
    /// name from a deployed v0.1.x peer.
    pub info: AnnounceInfo,
}

impl DiscoveredServer {
    /// Display name, whichever announce shape it came in.
    pub fn name(&self) -> Option<&str> {
        match &self.info {
            AnnounceInfo::Record(r) if !r.name.is_empty() => Some(&r.name),
            AnnounceInfo::Record(_) => None,
            AnnounceInfo::Legacy { name } => name.as_deref(),
        }
    }

    /// Which game this server runs, when it says.
    ///
    /// A legacy announce carries no game id and the destination hash is
    /// one-way (`PLAN.md` §3.1), so this is `None` for every deployed v0.1.x
    /// peer. A browser must show those as unattributed rather than guess.
    pub fn game_id(&self) -> Option<&str> {
        match &self.info {
            AnnounceInfo::Record(r) => Some(&r.game_id),
            AnnounceInfo::Legacy { .. } => None,
        }
    }

    pub fn record(&self) -> Option<&AnnounceRecord> {
        match &self.info {
            AnnounceInfo::Record(r) => Some(r),
            AnnounceInfo::Legacy { .. } => None,
        }
    }
}

/// A client identity learned by the server via `identify()` on link
/// establishment (client-initiated).
#[derive(Debug, Clone, Copy)]
pub struct ConnectedClient {
    pub identity_hash: IdentityHash,
}

impl BridgeSession {
    /// The engine handle — live interface add/remove/rename, introspection
    /// (routes, destination identities, link count), and link/path/announce
    /// commands. See `personal_rns::prelude::PrnsNodeHandle`.
    pub fn handle(&self) -> &PrnsNodeHandle {
        &self.handle
    }

    pub fn role(&self) -> BridgeRole {
        self.role
    }

    /// This node's own destination hash.
    pub fn own_hash(&self) -> Option<DestinationHash> {
        self.own_hash
    }

    /// Snapshot of discovered server destinations (the browser list).
    pub async fn discovered(&self) -> Vec<DiscoveredServer> {
        self.discovered.read().await.clone()
    }

    /// Snapshot of clients that have `identify()`d themselves over an
    /// established link (server side only — the client role never accepts
    /// links, so its map stays empty).
    pub async fn connected_clients(&self) -> Vec<ConnectedClient> {
        self.connected_clients
            .read()
            .await
            .values()
            .map(|&identity_hash| ConnectedClient { identity_hash })
            .collect()
    }

    /// Re-announce this node's own destination now, instead of waiting for the
    /// periodic announcer's next tick.
    pub async fn announce_now(&self, app_data: AnnounceAppData) -> Result<()> {
        let destination = self
            .own_hash
            .ok_or_else(|| anyhow!("this session has no destination to announce"))?;
        self.handle
            .announce_now(AnnounceNow {
                destination,
                target: AnnounceTarget::AllInterfaces,
                app_data,
            })
            .await
            .map_err(|e| anyhow!("announce failed: {e:?}"))
    }

    /// Stop the node and wait for it to fully tear down — including releasing
    /// bound sockets — before returning. Callers that immediately start a new
    /// session on the same port depend on this: returning before teardown
    /// finished previously caused an intermittent "address already in use".
    pub async fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.done.take() {
            let _ = rx.await;
        }
    }

    /// Wait until the node stops on its own (error or Ctrl-C). Consumes self.
    pub async fn await_completion(mut self) -> Result<()> {
        match self.done.take() {
            Some(rx) => {
                let _ = rx.await;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn start_server(args: ServerArgs) -> Result<Self> {
        args.profile
            .validate()
            .map_err(|e| anyhow!("invalid game profile: {e}"))?;
        // Parsed here rather than in the node thread so a typo'd hash is a
        // startup error the caller sees, not a warning in a log nobody reads.
        let allowlist = parse_allowlist(&args.allowlist)?;
        if !allowlist.is_empty() {
            info!(entries = allowlist.len(), "link allowlist active");
        }

        // Computed up front, with identical inputs to the destination built
        // again inside the node thread, so the server's own hash is available
        // to the caller before the node finishes starting.
        let precomputed_hash = {
            let identity = load_identity(&args.identity)?;
            PreConfiguredDestination::Single {
                app_name: &args.profile.app_name,
                aspects: &[ASPECT_SERVER],
                identity,
                announce_app_data: b"",
                proof: ProofStrategy::ProveAll,
                link_requests: LinkRequestPolicy::AcceptAll,
                ratchet: RatchetPolicy::NoRatchets,
                resource_strategy: ResourceStrategy::AcceptNone,
                maximum_request_bytes: Default::default(),
                request_endpoints: ServeMyRequestEndpoints::Yes,
            }
            .destination_hash()
            .map_err(|e| anyhow!("invalid destination name: {e:?}"))?
        };

        spawn_bridge_node(BridgeRole::Server, Some(precomputed_hash), args.relay_transit, move |discovered, connected_clients| async move {
            let identity = load_identity(&args.identity)?;
            let app_name = args.profile.app_name.clone();
            let game_id = args.profile.id.clone();
            let name_bytes = server_announce_bytes(&args);
            let relay_transit = args.relay_transit;
            info!(game = %args.profile.id, relay_transit, "transit relaying");
            let state = ServerState::new(&args, allowlist.clone());
            let destination = PreConfiguredDestination::Single {
                app_name: &app_name,
                aspects: &[ASPECT_SERVER],
                identity: identity.clone(),
                announce_app_data: &name_bytes,
                proof: ProofStrategy::ProveAll,
                link_requests: LinkRequestPolicy::AcceptAll,
                ratchet: RatchetPolicy::NoRatchets,
                resource_strategy: ResourceStrategy::AcceptNone,
                maximum_request_bytes: Default::default(),
                // Serves the §3.4 detail probe.
                request_endpoints: ServeMyRequestEndpoints::Yes,
            };
            let server_hash = destination
                .destination_hash()
                .map_err(|e| anyhow!("invalid destination name: {e:?}"))?;
            info!(game = %game_id, server_hash = ?server_hash.as_bytes(), "bridge server starting");

            let game_addr: SocketAddr = format!("{}:{}", args.game_host, args.game_port)
                .parse()
                .with_context(|| {
                    format!("invalid game host/port: {}:{}", args.game_host, args.game_port)
                })?;
            info!(game = %game_id, game_addr = %game_addr, "bridging to game server");

            let (event_tx, event_rx) = mpsc::unbounded_channel::<BridgeEvent>();
            let link_senders: LinkSenders = Arc::new(RwLock::new(std::collections::HashMap::new()));

            let node = PrnsNode::new(PrnsNodeRecipe {
                // Config-driven, not unconditional (`PLAN.md` §4). `None`
                // leaves the engine's TransportState::Unidentified, so nothing
                // is forwarded for anyone else.
                transport_identity: relay_transit.then_some(identity),
                pre_configured_destinations: [destination],
                app_state: state.clone(),
                storage: GrowableHeap,
                request_endpoints: request_endpoints![DetailsEndpoint],
                on_event: {
                    let event_tx = event_tx.clone();
                    move |event, _state| funnel_event(event, &event_tx)
                },
                interfaces: |node: &PrnsNodeHandle| {
                    attach_interfaces(node, args.tcp.as_deref(), args.auto);
                },
                persistence: NoPersistence,
            });
            let handle = node.handle();

            // Live-stats poller. Only for games that answer a query at all;
            // everything else serves announced numbers, flagged as announced.
            if let Some(crate::profile::QueryProtocol::A2s) = args.profile.query {
                let stats_state = state.clone();
                let _stats_task = tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(LIVE_STATS_INTERVAL);
                    loop {
                        ticker.tick().await;
                        match crate::a2s::query(game_addr).await {
                            Ok(stats) => stats_state.store_live(LiveStats {
                                players: stats.info.players,
                                max_players: stats.info.max_players,
                                map: stats.info.map.clone(),
                                player_names: stats
                                    .players_list
                                    .iter()
                                    .map(|p| p.name.clone())
                                    .collect(),
                                read_at: Instant::now(),
                            }),
                            // Expected whenever the game server is not running.
                            // The last good read stays, and its age says so.
                            Err(e) => debug!(error = %e, "live stats query failed"),
                        }
                    }
                });
            }

            // Announcer.
            let announcer = handle.clone();
            let interval = args.announce_interval;
            let announce_app_data = server_announce_app_data(&args);
            let _announce_task = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(interval.max(1)));
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    if announcer
                        .issue(PrnsCommand::AnnounceNow(AnnounceNow {
                            destination: server_hash,
                            target: AnnounceTarget::AllInterfaces,
                            app_data: announce_app_data.clone(),
                        }))
                        .is_none()
                    {
                        return;
                    }
                }
            });

            // Event router.
            let router_handle = handle.clone();
            let router_senders = link_senders.clone();
            let router_discovered = discovered.clone();
            let router_connected_clients = connected_clients.clone();
            let identify_timeout_tx = event_tx.clone();
            let router_state = state.clone();
            let identify_timeout_secs = args.identify_timeout_secs.max(1);
            let _router_task = tokio::spawn(async move {
                let mut event_rx = event_rx;
                // Links accepted but not yet identified, held with their
                // buffered data. Only ever non-empty when an allowlist is
                // configured. Local to this task, which is the only owner.
                let mut pending_identify: std::collections::HashMap<LinkId, mpsc::Receiver<Vec<u8>>> =
                    std::collections::HashMap::new();
                while let Some(event) = event_rx.recv().await {
                    match event {
                        BridgeEvent::AnnounceHeard { destination, hops, source_interface, info } => {
                            remember_server(&router_discovered, destination, hops, source_interface, info).await;
                        }
                        BridgeEvent::PeerIdentified { link_id, identity } => {
                            router_connected_clients.write().await.insert(link_id, identity);
                            if allowlist.is_empty() {
                                continue;
                            }
                            if allowlist.contains(&identity) {
                                if let Some(rx) = pending_identify.remove(&link_id) {
                                    info!(
                                        link = ?link_id,
                                        identity = ?identity.as_bytes(),
                                        "peer is on the allowlist; starting relay"
                                    );
                                    spawn_server_link_relay(
                                        link_id,
                                        rx,
                                        router_senders.clone(),
                                        router_handle.clone(),
                                        game_addr,
                                    );
                                }
                            } else {
                                warn!(
                                    link = ?link_id,
                                    identity = ?identity.as_bytes(),
                                    "peer is not on the allowlist; closing link"
                                );
                                pending_identify.remove(&link_id);
                                router_senders.write().await.remove(&link_id);
                                router_connected_clients.write().await.remove(&link_id);
                                let _ = router_handle.close_link(link_id);
                            }
                        }
                        BridgeEvent::IdentifyTimeout { link_id } => {
                            // Only fires for a link still waiting: an allowed
                            // peer's entry was removed when its relay started.
                            if pending_identify.remove(&link_id).is_some() {
                                warn!(
                                    link = ?link_id,
                                    "link did not identify within the allowlist timeout; closing"
                                );
                                router_senders.write().await.remove(&link_id);
                                let _ = router_handle.close_link(link_id);
                            }
                        }
                        BridgeEvent::LinkEstablished { link_id } => {
                            if router_senders.read().await.contains_key(&link_id) {
                                continue;
                            }
                            let (tx, rx) = mpsc::channel::<Vec<u8>>(256);
                            router_senders.write().await.insert(link_id, tx);

                            if allowlist.is_empty() {
                                spawn_server_link_relay(
                                    link_id,
                                    rx,
                                    router_senders.clone(),
                                    router_handle.clone(),
                                    game_addr,
                                );
                                router_state.set_bridge_clients(router_senders.read().await.len());
                                continue;
                            }

                            // Allowlisted: hold the link until it identifies.
                            // The sender is already in the map, so anything
                            // the peer sends meanwhile buffers in the channel
                            // instead of being dropped -- a peer that turns
                            // out to be allowed loses no datagram, including
                            // the first one.
                            router_state.set_bridge_clients(router_senders.read().await.len());
                            pending_identify.insert(link_id, rx);
                            let timeout_tx = identify_timeout_tx.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_secs(identify_timeout_secs)).await;
                                let _ = timeout_tx.send(BridgeEvent::IdentifyTimeout { link_id });
                            });
                        }
                        BridgeEvent::LinkClosed { link_id } => {
                            pending_identify.remove(&link_id);
                            if let Some(tx) = router_senders.write().await.remove(&link_id) {
                                let _ = tx.send(Vec::new()).await;
                            }
                            router_connected_clients.write().await.remove(&link_id);
                            router_state.set_bridge_clients(router_senders.read().await.len());
                        }
                        BridgeEvent::LinkData { link_id, bytes } => {
                            if let Some(tx) = router_senders.read().await.get(&link_id).cloned() {
                                if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(bytes) {
                                    debug!(link = ?link_id, "server relay channel full; dropping chunk");
                                }
                            } else {
                                debug!(link = ?link_id, "server LinkData for unknown link");
                            }
                        }
                    }
                }
            });

            info!("server node running");
            Ok((handle, async move { let _ = node.run().await; }))
        })
        .await
    }

    pub async fn start_client(args: ClientArgs) -> Result<Self> {
        args.profile
            .validate()
            .map_err(|e| anyhow!("invalid game profile: {e}"))?;

        let precomputed_hash = {
            let identity = load_identity(&args.identity)?;
            PreConfiguredDestination::Single {
                app_name: &args.profile.app_name,
                aspects: &[ASPECT_CLIENT],
                identity,
                announce_app_data: b"",
                proof: ProofStrategy::ProveAll,
                link_requests: LinkRequestPolicy::AcceptAll,
                ratchet: RatchetPolicy::NoRatchets,
                resource_strategy: ResourceStrategy::AcceptNone,
                maximum_request_bytes: Default::default(),
                request_endpoints: ServeMyRequestEndpoints::No,
            }
            .destination_hash()
            .map_err(|e| anyhow!("invalid destination name: {e:?}"))?
        };

        spawn_bridge_node(BridgeRole::Client, Some(precomputed_hash), args.relay_transit, move |discovered, _connected_clients| async move {
            let identity = load_identity(&args.identity)?;
            let app_name = args.profile.app_name.clone();
            let game_id = args.profile.id.clone();
            // Sent to the server via `identify()` once a link is up, so the
            // server can show this client in its connected list.
            let client_identity_hash = InMemoryNodeIdentity::from_secret_key_bytes(&identity).identity_hash();
            let listen_addr: SocketAddr = format!("127.0.0.1:{}", args.listen_port)
                .parse()
                .with_context(|| format!("invalid listen port: {}", args.listen_port))?;
            info!(game = %game_id, listen = %listen_addr, "bridge client starting");

            let target_hash = match args.server_hash.as_deref() {
                Some(hex) => Some(parse_destination_hash(hex).context("invalid server hash")?),
                None => None,
            };

            let udp = UdpSocket::bind(listen_addr)
                .await
                .with_context(|| format!("binding UDP listener on {listen_addr}"))?;
            info!(listen = %listen_addr, "point the game client at this address");

            let (event_tx, event_rx) = mpsc::unbounded_channel::<BridgeEvent>();
            let destination = PreConfiguredDestination::Single {
                app_name: &app_name,
                aspects: &[ASPECT_CLIENT],
                identity: identity.clone(),
                announce_app_data: b"",
                proof: ProofStrategy::ProveAll,
                link_requests: LinkRequestPolicy::AcceptAll,
                ratchet: RatchetPolicy::NoRatchets,
                resource_strategy: ResourceStrategy::AcceptNone,
                maximum_request_bytes: Default::default(),
                request_endpoints: ServeMyRequestEndpoints::No,
            };

            let node = PrnsNode::new(PrnsNodeRecipe {
                // Off by default for a client (`PLAN.md` §4): a player who
                // installed this to join one server should not be forwarding
                // strangers' traffic on a metered connection without knowing.
                transport_identity: args.relay_transit.then_some(identity),
                pre_configured_destinations: [destination],
                app_state: (),
                storage: GrowableHeap,
                request_endpoints: request_endpoints![],
                on_event: {
                    let event_tx = event_tx.clone();
                    move |event, _state| funnel_event(event, &event_tx)
                },
                interfaces: |node: &PrnsNodeHandle| {
                    attach_interfaces(node, args.tcp.as_deref(), args.auto);
                },
                persistence: NoPersistence,
            });
            let handle = node.handle();

            let server_target: Arc<RwLock<Option<DestinationHash>>> = Arc::new(RwLock::new(target_hash));
            if let Some(h) = target_hash {
                info!(server_hash = ?h.as_bytes(), "will connect to explicit server hash");
            } else {
                info!(game = %game_id, "no server hash given; waiting to discover one via announce");
            }

            // Proactively request a path to an explicit server hash so the
            // first game packet isn't dropped with NoRouteToDestination
            // (announces may be slow or never rebroadcast between
            // same-interface peers).
            if let Some(h) = target_hash {
                let probe_handle = handle.clone();
                let _path_probe_task = tokio::spawn(async move {
                    for attempt in 1..=12u32 {
                        match probe_handle.request_path(h).await {
                            Ok(_) => {
                                info!(server_hash = ?h.as_bytes(), attempt, "path to server resolved via path request");
                                return;
                            }
                            Err(e) => {
                                debug!(attempt, error = ?e, "path request pending; retrying in 5s");
                                tokio::time::sleep(Duration::from_secs(5)).await;
                            }
                        }
                    }
                    warn!(server_hash = ?h.as_bytes(), "could not resolve path to server after retries");
                });
            }

            let link_data: LinkSenders = Arc::new(RwLock::new(std::collections::HashMap::new()));
            let udp_links: Arc<RwLock<std::collections::HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>> =
                Arc::new(RwLock::new(std::collections::HashMap::new()));

            let router_target = server_target.clone();
            let router_link_data = link_data.clone();
            let router_discovered = discovered.clone();
            let _router_task = tokio::spawn(async move {
                let mut event_rx = event_rx;
                while let Some(event) = event_rx.recv().await {
                    match event {
                        BridgeEvent::AnnounceHeard { destination, hops, source_interface, info } => {
                            remember_server(&router_discovered, destination, hops, source_interface, info).await;
                            let mut t = router_target.write().await;
                            if t.is_none() {
                                info!(server_hash = ?destination.as_bytes(), "discovered a server announce");
                                *t = Some(destination);
                            }
                        }
                        BridgeEvent::LinkEstablished { .. } => {}
                        // The client is always the link initiator, so it never
                        // becomes the identified peer of a link it accepted,
                        // and it never allowlists anyone.
                        BridgeEvent::PeerIdentified { .. } => {}
                        BridgeEvent::IdentifyTimeout { .. } => {}
                        BridgeEvent::LinkClosed { link_id } => {
                            router_link_data.write().await.remove(&link_id);
                        }
                        BridgeEvent::LinkData { link_id, bytes } => {
                            if let Some(tx) = router_link_data.read().await.get(&link_id).cloned() {
                                if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(bytes) {
                                    debug!(link = ?link_id, "client relay channel full; dropping chunk");
                                }
                            } else {
                                debug!(link = ?link_id, "client LinkData for unknown link");
                            }
                        }
                    }
                }
            });

            let udp: Arc<UdpSocket> = Arc::new(udp);
            let udp_handle = handle.clone();
            let udp_target = server_target.clone();
            let udp_link_data = link_data.clone();
            let _udp_task = tokio::spawn(async move {
                let mut buf = vec![0u8; UDP_READ_BUF];
                loop {
                    let (n, src) = match udp.recv_from(&mut buf).await {
                        Ok(p) => p,
                        Err(e) => {
                            error!(error = %e, "client UDP listener died");
                            return;
                        }
                    };
                    let pkt = buf[..n].to_vec();

                    if let Some(tx) = udp_links.read().await.get(&src).cloned() {
                        if tx.send(pkt).await.is_err() {
                            udp_links.write().await.remove(&src);
                        }
                        continue;
                    }

                    let Some(target) = *udp_target.read().await else {
                        warn!(src = %src, "first packet seen but no server discovered yet; dropping");
                        continue;
                    };

                    let handle = udp_handle.clone();
                    let udp = udp.clone();
                    let links = udp_links.clone();
                    let link_data_map = udp_link_data.clone();
                    tokio::spawn(async move {
                        debug!(src = %src, target = ?target.as_bytes(), "establishing link to server");
                        let link_id = match handle.establish_link(target).await {
                            Ok(id) => id,
                            Err(e) => {
                                info!(src = %src, error = ?e, "no route to server; requesting path then retrying");
                                match handle.request_path(target).await {
                                    Ok(_) => {}
                                    Err(pe) => {
                                        error!(src = %src, error = ?pe, "path request to server failed");
                                        return;
                                    }
                                }
                                match handle.establish_link(target).await {
                                    Ok(id) => id,
                                    Err(e2) => {
                                        error!(src = %src, error = ?e2, "establish link failed after path request");
                                        return;
                                    }
                                }
                            }
                        };
                        debug!(src = %src, link = ?link_id, "link established");
                        // Best-effort: lets the server list this client. Does
                        // not block the relay if it fails (e.g. an older peer
                        // without identify support).
                        if let Err(e) = handle.identify(link_id, client_identity_hash).await {
                            debug!(src = %src, link = ?link_id, error = ?e, "identify failed");
                        }

                        let (udp_to_link_tx, mut udp_to_link_rx) = mpsc::channel::<Vec<u8>>(256);
                        links.write().await.insert(src, udp_to_link_tx);

                        for chunk in frame(&pkt) {
                            let Some(payload) = link_payload(&chunk, link_id) else {
                                links.write().await.remove(&src);
                                let _ = handle.close_link(link_id);
                                return;
                            };
                            if handle
                                .issue(PrnsCommand::SendToLink(SendToLink { link_id, payload }))
                                .is_none()
                            {
                                error!(src = %src, link = ?link_id, "first send failed: node stopped");
                                links.write().await.remove(&src);
                                let _ = handle.close_link(link_id);
                                return;
                            }
                        }

                        let (link_to_udp_tx, mut link_to_udp_rx) = mpsc::channel::<Vec<u8>>(256);
                        link_data_map.write().await.insert(link_id, link_to_udp_tx);
                        debug!(src = %src, link = ?link_id, "client relay registered");

                        let h1 = handle.clone();
                        let send_task = tokio::spawn(async move {
                            while let Some(bytes) = udp_to_link_rx.recv().await {
                                if bytes.is_empty() {
                                    break;
                                }
                                for chunk in frame(&bytes) {
                                    let Some(payload) = link_payload(&chunk, link_id) else {
                                        return;
                                    };
                                    if h1
                                        .issue(PrnsCommand::SendToLink(SendToLink { link_id, payload }))
                                        .is_none()
                                    {
                                        return;
                                    }
                                }
                            }
                        });

                        let udp_back = udp.clone();
                        let recv_task = tokio::spawn(async move {
                            let mut reassembler = Reassembler::default();
                            while let Some(chunk) = link_to_udp_rx.recv().await {
                                if let Some(datagram) = reassembler.push(&chunk) {
                                    if udp_back.send_to(&datagram, src).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        });

                        let _ = tokio::join!(send_task, recv_task);
                        link_data_map.write().await.remove(&link_id);
                        links.write().await.remove(&src);
                        let _ = handle.close_link(link_id);
                        debug!(src = %src, link = ?link_id, "client relay ended");
                    });
                }
            });

            info!("client node running");
            Ok((handle, async move { let _ = node.run().await; }))
        })
        .await
    }
}

impl BridgeSession {
    /// Start a node that carries other people's traffic and nothing else.
    ///
    /// No destination is registered and nothing is announced: a transport node
    /// is not a service anyone links to, it is a hop on the way to one. That
    /// is why this role has no `own_hash` and no discovered-server list of its
    /// own worth speaking of — though it still hears announces, because every
    /// node on the interface does.
    pub async fn start_relay(args: RelayArgs) -> Result<Self> {
        spawn_bridge_node(BridgeRole::Relay, None, true, move |discovered, _connected| async move {
            let identity = load_identity(&args.identity)?;
            let identity_hash = InMemoryNodeIdentity::from_secret_key_bytes(&identity).identity_hash();
            info!(
                identity = ?identity_hash.as_bytes(),
                "relay node starting; it forwards ciphertext it cannot read"
            );

            let (event_tx, event_rx) = mpsc::unbounded_channel::<BridgeEvent>();
            let node = PrnsNode::new(PrnsNodeRecipe {
                transport_identity: Some(identity),
                // The whole point of the role: no game, no destination.
                pre_configured_destinations: [] as [PreConfiguredDestination; 0],
                app_state: (),
                storage: GrowableHeap,
                request_endpoints: request_endpoints![],
                on_event: {
                    let event_tx = event_tx.clone();
                    move |event, _state| funnel_event(event, &event_tx)
                },
                interfaces: |node: &PrnsNodeHandle| {
                    attach_interfaces(node, args.tcp.as_deref(), args.auto);
                },
                persistence: NoPersistence,
            });
            let handle = node.handle();

            // A relay still hears announces, and keeping them costs nothing —
            // a relay operator has as much right to a server list as anyone.
            let router_discovered = discovered.clone();
            let _router_task = tokio::spawn(async move {
                let mut event_rx = event_rx;
                while let Some(event) = event_rx.recv().await {
                    if let BridgeEvent::AnnounceHeard { destination, hops, source_interface, info } = event {
                        remember_server(&router_discovered, destination, hops, source_interface, info).await;
                    }
                }
            });

            info!("relay node running");
            Ok((handle, async move { let _ = node.run().await; }))
        })
        .await
    }

    /// Start a node that only listens for announces.
    ///
    /// The zero-infrastructure baseline of `PLAN.md` §8 phase 2: no index, no
    /// account, no internet, no bound game port, nothing announced, nothing
    /// forwarded unless asked. Two launchers on a shared interface find each
    /// other with this and nothing else, which is the property the whole
    /// design exists to protect (`DESIGN.md` §0).
    ///
    /// Call `browse()` for a filtered, sorted view of what it has heard.
    pub async fn start_browser(args: BrowserArgs) -> Result<Self> {
        spawn_bridge_node(BridgeRole::Browse, None, false, move |discovered, _connected| async move {
            let (event_tx, event_rx) = mpsc::unbounded_channel::<BridgeEvent>();
            let node = PrnsNode::new(PrnsNodeRecipe {
                // No transport identity, so this node forwards nothing for
                // anyone. That is structural, not a setting — see BrowserArgs.
                transport_identity: None,
                pre_configured_destinations: [] as [PreConfiguredDestination; 0],
                app_state: (),
                storage: GrowableHeap,
                request_endpoints: request_endpoints![],
                on_event: {
                    let event_tx = event_tx.clone();
                    move |event, _state| funnel_event(event, &event_tx)
                },
                interfaces: |node: &PrnsNodeHandle| {
                    attach_interfaces(node, args.tcp.as_deref(), args.auto);
                },
                persistence: NoPersistence,
            });
            let handle = node.handle();

            let router_discovered = discovered.clone();
            let _router_task = tokio::spawn(async move {
                let mut event_rx = event_rx;
                while let Some(event) = event_rx.recv().await {
                    if let BridgeEvent::AnnounceHeard { destination, hops, source_interface, info } = event {
                        remember_server(&router_discovered, destination, hops, source_interface, info).await;
                    }
                }
            });

            info!("browse node running; listening for announces");
            Ok((handle, async move { let _ = node.run().await; }))
        })
        .await
    }

    /// A filtered, sorted view of everything this node has heard.
    ///
    /// Takes the snapshot and runs `browse::browse` over it. `PLAN.md` §3.4's
    /// default is ascending hops, which is what `BrowseQuery::default` gives.
    pub async fn browse(&self, query: &crate::browse::BrowseQuery) -> Vec<DiscoveredServer> {
        let rows = self.discovered.read().await;
        crate::browse::browse(&rows, query, Instant::now())
            .into_iter()
            .cloned()
            .collect()
    }

    /// Ask one server about itself over a Link (`PLAN.md` §3.4).
    ///
    /// **One server, because a person opened it.** Never call this across a
    /// list, not even lazily and not even for the visible rows: an announce is
    /// free to listen to, but a probe is a connection to somebody else's
    /// machine, and a browser that opened one per row would be a scanner. The
    /// announce record is what fills the list; this fills a detail pane.
    ///
    /// Returns the details and the link's measured round trip. That RTT is the
    /// only latency figure in the whole product, and it exists precisely
    /// because it was paid for — the list still sorts by hops.
    pub async fn probe_details(
        &self,
        destination: DestinationHash,
    ) -> Result<(ServerDetails, u32)> {
        let link_id = match self.handle.establish_link(destination).await {
            Ok(id) => id,
            Err(e) => {
                // A server may need a path resolved before a link will open —
                // announces are not always enough between same-interface peers.
                debug!(error = ?e, "no route for probe; requesting a path first");
                self.handle
                    .request_path(destination)
                    .await
                    .map_err(|pe| anyhow!("no path to server: {pe:?}"))?;
                self.handle
                    .establish_link(destination)
                    .await
                    .map_err(|e2| anyhow!("link to server failed: {e2:?}"))?
            }
        };

        let outcome = self
            .handle
            .request(
                link_id,
                RequestPathHash::of(DETAILS_ENDPOINT_ID),
                &crate::details::request_body(),
            )
            .await;
        // Close the link either way: a probe is a question, not a session.
        let _ = self.handle.close_link(link_id);

        let (response, rtt) = outcome.map_err(|e| anyhow!("detail probe failed: {e:?}"))?;
        let details = ServerDetails::decode(&response)
            .map_err(|e| anyhow!("server sent a details response we cannot read: {e}"))?;
        Ok((details, rtt.millis().min(u32::MAX as u64) as u32))
    }

    /// What this node is carrying, per interface.
    ///
    /// **Read the caveat before showing these numbers to a user.**
    /// `transported_links` is honest: it counts links this node carries *for
    /// other people*. `rx_bytes`/`tx_bytes` are not — they count everything on
    /// the interface, this node's own game traffic included. There is no
    /// engine-level split: `Diagnostic` has no forwarded-packet variant
    /// (`prns-runtime/core/src/runtime/event.rs`), so bytes attributable to
    /// transit alone cannot be reported without patching the engine. Label
    /// them as total interface throughput, not as "bandwidth you donated".
    pub fn transit_stats(&self) -> Vec<TransitStats> {
        self.handle
            .interfaces()
            .into_iter()
            .map(|s| TransitStats {
                interface: s.id,
                transported_links: s.transported_links,
                links: s.links,
                interface_rx_bytes: s.rx_bytes,
                interface_tx_bytes: s.tx_bytes,
            })
            .collect()
    }

    /// Whether this node is forwarding for others at all.
    pub fn relays_transit(&self) -> bool {
        self.role == BridgeRole::Relay || self.relay_transit
    }
}

/// Per-interface transit visibility. See `BridgeSession::transit_stats` for
/// which of these numbers are attributable to transit and which are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitStats {
    pub interface: InterfaceId,
    /// Links this node carries on behalf of other nodes. Transit, exactly.
    pub transported_links: u32,
    /// Links terminating at this node.
    pub links: u32,
    /// Total bytes on the interface, own traffic included. Not transit alone.
    pub interface_rx_bytes: u64,
    /// Total bytes on the interface, own traffic included. Not transit alone.
    pub interface_tx_bytes: u64,
}

/// Pump one accepted link against the local game server until either side
/// ends. Split out of the server's `LinkEstablished` arm so the allowlist can
/// defer the call until the peer has identified itself (`PLAN.md` §8 step 4)
/// without duplicating the relay.
fn spawn_server_link_relay(
    link_id: LinkId,
    mut rx: mpsc::Receiver<Vec<u8>>,
    senders: LinkSenders,
    handle: PrnsNodeHandle,
    game_addr: SocketAddr,
) {
    tokio::spawn(async move {
        debug!(link = ?link_id, game_addr = %game_addr, "server relay started");
        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                error!(link = ?link_id, error = %e, "failed to bind relay UDP socket");
                senders.write().await.remove(&link_id);
                let _ = handle.close_link(link_id);
                return;
            }
        };
        if let Err(e) = sock.connect(game_addr).await {
            error!(link = ?link_id, error = %e, "failed to connect relay UDP socket to game server");
            senders.write().await.remove(&link_id);
            let _ = handle.close_link(link_id);
            return;
        }
        let sock_send = sock.clone();
        let to_game = tokio::spawn(async move {
            let mut reassembler = Reassembler::default();
            while let Some(chunk) = rx.recv().await {
                if chunk.is_empty() {
                    break;
                }
                if let Some(datagram) = reassembler.push(&chunk) {
                    if sock_send.send(&datagram).await.is_err() {
                        break;
                    }
                }
            }
        });
        let sock_recv = sock.clone();
        let from_game = {
            let handle = handle.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; UDP_READ_BUF];
                loop {
                    match sock_recv.recv(&mut buf).await {
                        Ok(n) if n > 0 => {
                            let datagram = &buf[..n];
                            for chunk in frame(datagram) {
                                let Some(payload) = link_payload(&chunk, link_id) else {
                                    return;
                                };
                                if handle
                                    .issue(PrnsCommand::SendToLink(SendToLink {
                                        link_id,
                                        payload,
                                    }))
                                    .is_none()
                                {
                                    return;
                                }
                            }
                        }
                        Ok(_) => continue,
                        Err(e) => {
                            debug!(link = ?link_id, error = ?e, "UDP recv from game server ended");
                            break;
                        }
                    }
                }
            })
        };
        let _ = tokio::join!(to_game, from_game);
        senders.write().await.remove(&link_id);
        let _ = handle.close_link(link_id);
        debug!(link = ?link_id, "server relay ended");
    });
}

/// Wrap a framed chunk for `SendToLink`.
///
/// `frame` never emits more than `MAX_CHUNK + 1` bytes and the pinned engine's
/// cap is 1967 (`ENGINE.md`), so this cannot fail — but the original panicked
/// here via `.expect()`, which turns a future `MAX_CHUNK` or engine-pin change
/// into a crash inside a relay task. Log and drop the link instead.
fn link_payload(chunk: &[u8], link_id: LinkId) -> Option<SendToLinkPayload> {
    match SendToLinkPayload::from_slice(chunk) {
        Ok(p) => Some(p),
        Err(_) => {
            error!(
                link = ?link_id,
                chunk_len = chunk.len(),
                "framed chunk exceeds the engine's link payload cap; dropping relay"
            );
            None
        }
    }
}

/// Run a bridge node on a dedicated current-thread runtime + `LocalSet` (the
/// node's `run()` future is `!Send`). `build` wires the node + relay tasks and
/// returns the handle plus the node's `run()` future; this helper drives that
/// future, hands the caller the handle + a stop channel, and signals `done`
/// when the node exits.
async fn spawn_bridge_node<B, Fut, NodeRun>(
    role: BridgeRole,
    own_hash: Option<DestinationHash>,
    relay_transit: bool,
    build: B,
) -> Result<BridgeSession>
where
    B: FnOnce(Arc<RwLock<Vec<DiscoveredServer>>>, ConnectedClients) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(PrnsNodeHandle, NodeRun)>> + 'static,
    NodeRun: Future<Output = ()> + 'static,
{
    let discovered: Arc<RwLock<Vec<DiscoveredServer>>> = Arc::new(RwLock::new(Vec::new()));
    let connected_clients: ConnectedClients = Arc::new(RwLock::new(std::collections::HashMap::new()));
    let disc_for_thread = discovered.clone();
    let cc_for_thread = connected_clients.clone();
    let (init_tx, init_rx) = oneshot::channel::<Result<(PrnsNodeHandle, oneshot::Sender<()>)>>();
    let (done_tx, done_rx) = oneshot::channel::<()>();

    std::thread::Builder::new()
        .name("game-bridge-node".into())
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
                let (handle, node_run) = match build(disc_for_thread, cc_for_thread).await {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };
                let (stop_tx, stop_rx) = oneshot::channel::<()>();
                if init_tx.send(Ok((handle, stop_tx))).is_err() {
                    return;
                }
                tokio::select! {
                    _ = node_run => {}
                    _ = stop_rx => {}
                }
            }));
            // Drop the runtime explicitly, then signal `done` — not from
            // inside the async block above, which resolves as soon as
            // `select!` picks a branch, *before* the runtime tears down.
            // `Runtime::drop` synchronously aborts every task spawned on it
            // and waits for their resources — including bound sockets — to be
            // released. Signaling `done` only after that is what makes
            // `BridgeSession::stop()`'s await a real guarantee: a restart can
            // rebind the same port instead of racing the old socket's
            // teardown. Previously a real bug ("address already in use").
            drop(rt);
            let _ = done_tx.send(());
        })
        .context("failed to spawn bridge node thread")?;

    let (handle, stop_tx) = init_rx
        .await
        .map_err(|_| anyhow!("bridge node thread died before starting"))??;
    Ok(BridgeSession {
        handle,
        discovered,
        connected_clients,
        own_hash,
        role,
        relay_transit,
        stop_tx: Some(stop_tx),
        done: Some(done_rx),
    })
}

/// Insert or refresh a discovered server in the browser list (dedup by hash).
/// `name` always overwrites — a name change, or a peer that stops sending one,
/// should show up on the next hear.
async fn remember_server(
    list: &Arc<RwLock<Vec<DiscoveredServer>>>,
    destination: DestinationHash,
    hops: u8,
    source_interface: InterfaceId,
    info: AnnounceInfo,
) {
    let mut l = list.write().await;
    if let Some(s) = l.iter_mut().find(|s| s.destination_hash == destination) {
        s.last_seen = Instant::now();
        // Every field is overwritten, not merged. A server that moved closer,
        // changed interface, renamed itself or emptied out should read as it
        // is now; the freshest announce is the truth about it.
        s.hops = hops;
        s.source_interface = source_interface;
        s.info = info;
    } else {
        l.push(DiscoveredServer {
            destination_hash: destination,
            last_seen: Instant::now(),
            hops,
            source_interface,
            info,
        });
    }
}

// =========================================================================
// Shared event routing
// =========================================================================

#[derive(Debug)]
enum BridgeEvent {
    AnnounceHeard {
        destination: DestinationHash,
        hops: u8,
        source_interface: InterfaceId,
        info: AnnounceInfo,
    },
    LinkEstablished { link_id: LinkId },
    PeerIdentified { link_id: LinkId, identity: IdentityHash },
    /// Synthesized by the server's own timer, not by the engine: an accepted
    /// link never identified itself while an allowlist was in force.
    IdentifyTimeout { link_id: LinkId },
    LinkClosed { link_id: LinkId },
    LinkData { link_id: LinkId, bytes: Vec<u8> },
}

fn funnel_event(event: PrnsEvent<'_>, tx: &mpsc::UnboundedSender<BridgeEvent>) {
    match event {
        PrnsEvent::Diagnostic(Diagnostic::AnnounceHeard {
            destination,
            hops,
            source_interface,
            app_data,
        }) => {
            // A malformed announce is dropped, not listed: it is bytes from an
            // unauthenticated stranger, and a row we cannot parse is a row we
            // cannot honestly show.
            match crate::announce::decode(&app_data) {
                Ok(info) => {
                    let _ = tx.send(BridgeEvent::AnnounceHeard {
                        destination,
                        hops,
                        source_interface,
                        info,
                    });
                }
                Err(e) => {
                    debug!(
                        destination = ?destination.as_bytes(),
                        error = %e,
                        "ignoring an announce whose app_data did not decode"
                    );
                }
            }
        }
        PrnsEvent::Diagnostic(Diagnostic::LinkEstablished(established)) => {
            let _ = tx.send(BridgeEvent::LinkEstablished {
                link_id: established.link_id,
            });
        }
        PrnsEvent::Diagnostic(Diagnostic::PeerIdentified { link_id, identity }) => {
            let _ = tx.send(BridgeEvent::PeerIdentified { link_id, identity });
        }
        PrnsEvent::Diagnostic(Diagnostic::LinkClosed { link_id, .. }) => {
            let _ = tx.send(BridgeEvent::LinkClosed { link_id });
        }
        PrnsEvent::Message(Message::Delivered(Delivery::Link(link_delivery))) => {
            let _ = tx.send(BridgeEvent::LinkData {
                link_id: link_delivery.link_id,
                bytes: link_delivery.plaintext.to_vec(),
            });
        }
        _ => {}
    }
}

type LinkSenders = Arc<RwLock<std::collections::HashMap<LinkId, mpsc::Sender<Vec<u8>>>>>;
type ConnectedClients = Arc<RwLock<std::collections::HashMap<LinkId, IdentityHash>>>;

/// Announce app_data used when no display name is configured.
///
/// Frozen: deployed v0.1.10 peers announce exactly these bytes, and the
/// launcher's browser recognises them. Do not "improve" it.
const DEFAULT_SERVER_ANNOUNCE_NAME: &[u8] = b"sc-rns-bridge";
const DEFAULT_SERVER_ANNOUNCE_NAME_STR: &str = "sc-rns-bridge";

/// The framing generation this build speaks, advertised in the record so a
/// future channel-id format can be negotiated instead of assumed. Generation 1
/// is `framing.rs` as frozen by `PLAN.md` §5.
const FRAMING_GENERATION: u8 = 1;

/// The legacy announce payload: the configured name (trimmed, UTF-8, truncated
/// to the wire budget), or the fixed default. Byte-identical to v0.1.10.
pub fn server_announce_name_bytes(name: &Option<String>) -> Vec<u8> {
    let trimmed = name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let bytes = trimmed.map(str::as_bytes).unwrap_or(DEFAULT_SERVER_ANNOUNCE_NAME);
    bytes[..bytes.len().min(MAX_ANNOUNCE_APP_DATA_LEN)].to_vec()
}

/// Build the §3.3 record this server advertises.
///
/// Over-long fields are truncated rather than refused: a server should still
/// appear in the browser under a shortened name, and refusing to announce over
/// a 49th character would make the server invisible instead.
pub fn server_announce_record(args: &ServerArgs) -> AnnounceRecord {
    fn truncate(s: &str, max: usize) -> String {
        // Truncate on a char boundary so the field stays valid UTF-8.
        let mut end = s.len().min(max);
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }

    let name = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_SERVER_ANNOUNCE_NAME_STR);

    AnnounceRecord {
        protocol_version: FRAMING_GENERATION,
        flags: AnnounceFlags {
            passworded: args.passworded,
            allowlisted: !args.allowlist.is_empty(),
            dedicated: args.dedicated,
            transport_mode: args.transport_mode,
        },
        min_link_class: args.profile.min_link_class,
        players: args.players,
        max_players: args.max_players,
        game_id: truncate(&args.profile.id, announce::MAX_GAME_ID_LEN),
        name: truncate(name, announce::MAX_NAME_LEN),
        map: truncate(args.map.as_deref().unwrap_or(""), announce::MAX_MAP_LEN),
        tlvs: Vec::new(),
    }
}

/// The bytes this server puts in `app_data`, honouring `announce_format`.
///
/// A record that fails to encode falls back to the legacy name rather than
/// announcing nothing: an unlistable server is worse than an unfilterable one.
/// `ServerArgs` validation should make that unreachable, so it is logged.
pub fn server_announce_bytes(args: &ServerArgs) -> Vec<u8> {
    match args.announce_format {
        AnnounceFormat::Legacy => server_announce_name_bytes(&args.name),
        AnnounceFormat::Record => match server_announce_record(args).encode() {
            Ok(bytes) => bytes,
            Err(e) => {
                error!(error = %e, "announce record did not encode; falling back to a bare name");
                server_announce_name_bytes(&args.name)
            }
        },
    }
}

/// The `AnnounceAppData` for an `AnnounceNow` command, built from the same
/// bytes as the destination's registered app_data.
pub fn server_announce_app_data(args: &ServerArgs) -> AnnounceAppData {
    let bytes = server_announce_bytes(args);
    AnnounceAppData::Data(AnnounceAppDataBytes::from_slice(&bytes).unwrap_or_default())
}

pub fn attach_interfaces(node: &PrnsNodeHandle, tcp: Option<&str>, auto: bool) {
    info!(tcp = ?tcp, auto, "attach_interfaces called");
    if let Some(addr) = tcp {
        if let Some(colon) = addr.rfind(':') {
            let host = &addr[..colon];
            let port: u16 = addr[colon + 1..].parse().unwrap_or(0);
            // 0.0.0.0 (or empty host) means "bind a TCP server here";
            // any other host means "connect to a TCP server there".
            if host == "0.0.0.0" || host.is_empty() {
                let node = node.clone();
                let addr = addr.to_string();
                tokio::spawn(async move {
                    match TcpServer::bind(&addr).await {
                        Ok(srv) => {
                            node.supervise(srv);
                            info!(tcp = %addr, "attached TCP server interface");
                        }
                        Err(e) => error!(tcp = %addr, error = ?e, "failed to bind TCP server"),
                    }
                });
            } else if port > 0 {
                let client = TcpClientInterface::new(addr.to_string());
                node.attach(client);
                info!(tcp = %addr, "attached TCP client interface");
            }
        } else {
            warn!(tcp = ?addr, "ignoring tcp interface without a port");
        }
    }
    if auto {
        node.attach(AutoWifi::default());
        info!("attached Wi-Fi/LAN auto-discovery interface");
    }
    if tcp.is_none() && !auto {
        warn!("no interfaces attached; this node cannot talk to anything");
    }
}

/// Parse hex identity hashes into the allowlist.
///
/// # Why the allowlist cannot be enforced at accept time
///
/// `PLAN.md` §6 assumed gating was "a rejection before that insert, not new
/// plumbing", on the strength of v0.1.9 capturing the peer identity at accept.
/// The engine does not support that. `LinkRequestPolicy`
/// (`prns-core/src/routing/upstream_app_destinations/core.rs:24`) has exactly
/// two values, `AcceptAll` and `AcceptNone` — there is no per-request callback,
/// and no identity is available when the link request arrives. The identity
/// only becomes known if the peer *volunteers* it by calling `identify()`
/// after the link is already up, which surfaces as
/// `Diagnostic::PeerIdentified`.
///
/// So enforcement is: accept the link, relay nothing, and hold the peer's data
/// in its channel until it identifies. Allowed peers get the relay started and
/// lose no datagram; peers that identify as someone else are closed; and peers
/// that never identify at all are closed on a timer — without that last case
/// an allowlist would be bypassed by simply staying silent, which is the
/// obvious attack and the reason a timeout is not optional.
fn parse_allowlist(entries: &[String]) -> Result<Vec<IdentityHash>> {
    entries
        .iter()
        .map(|entry| {
            let hex_str = entry.trim();
            let bytes = hex::decode(hex_str)
                .map_err(|e| anyhow!("invalid hex in allowlist entry {hex_str:?}: {e}"))?;
            let arr: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
                anyhow!(
                    "allowlist entry {hex_str:?} is {} bytes; an identity hash is 16 (32 hex chars)",
                    bytes.len()
                )
            })?;
            Ok(IdentityHash::new(arr))
        })
        .collect()
}

fn parse_destination_hash(hex: &str) -> Result<DestinationHash> {
    let hex = hex.trim();
    let bytes = hex::decode(hex).map_err(|e| anyhow!("invalid hex in server hash: {e}"))?;
    DestinationHash::from_slice(&bytes)
        .ok_or_else(|| anyhow!("server hash must be 16 bytes (32 hex chars)"))
}

fn load_identity(path: &Path) -> Result<ZeroizingIdentity> {
    load_or_create_identity_secret(path)
        .map_err(|e: IdentitySecretFileError| anyhow::Error::from(e))
        .with_context(|| format!("loading identity at {}", path.display()))
}

type ZeroizingIdentity = Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_announce_name_bytes_uses_default_when_unset() {
        assert_eq!(server_announce_name_bytes(&None), DEFAULT_SERVER_ANNOUNCE_NAME);
        assert_eq!(
            server_announce_name_bytes(&Some("   ".to_string())),
            DEFAULT_SERVER_ANNOUNCE_NAME
        );
    }

    #[test]
    fn server_announce_name_bytes_trims_and_uses_configured_name() {
        assert_eq!(
            server_announce_name_bytes(&Some("  My Server  ".to_string())),
            b"My Server"
        );
    }

    #[test]
    fn server_announce_name_bytes_truncates_to_wire_budget() {
        let long_name = "x".repeat(MAX_ANNOUNCE_APP_DATA_LEN + 100);
        assert_eq!(
            server_announce_name_bytes(&Some(long_name)).len(),
            MAX_ANNOUNCE_APP_DATA_LEN
        );
    }

    fn sven_server_args() -> ServerArgs {
        let mut args = ServerArgs::new(crate::profile::GameProfile::sven_coop());
        args.name = Some("Idan's Server".to_string());
        args.players = 4;
        args.max_players = 32;
        args.map = Some("svencoop1".to_string());
        args
    }

    /// What the server announces must be what a browser reads back.
    #[test]
    fn announced_record_round_trips_through_decode() {
        let args = sven_server_args();
        let AnnounceAppData::Data(bytes) = server_announce_app_data(&args) else {
            panic!("expected AnnounceAppData::Data");
        };
        let AnnounceInfo::Record(r) = crate::announce::decode(&bytes).unwrap() else {
            panic!("expected a record");
        };
        assert_eq!(r.game_id, "sven-coop");
        assert_eq!(r.name, "Idan's Server");
        assert_eq!(r.map, "svencoop1");
        assert_eq!((r.players, r.max_players), (4, 32));
        assert_eq!(r.min_link_class, 1);
        assert_eq!(r.protocol_version, FRAMING_GENERATION);
    }

    /// The legacy format must stay byte-identical to v0.1.10, or deployed Sven
    /// clients stop showing the server's name (`PLAN.md` §5).
    #[test]
    fn legacy_format_is_byte_identical_to_v0_1_10() {
        let mut args = sven_server_args();
        args.announce_format = AnnounceFormat::Legacy;
        assert_eq!(server_announce_bytes(&args), b"Idan's Server");

        args.name = None;
        assert_eq!(server_announce_bytes(&args), DEFAULT_SERVER_ANNOUNCE_NAME);
    }

    /// An allowlisted server must say so, so a browser can show the padlock
    /// rather than letting a player discover it on a failed join.
    #[test]
    fn allowlist_is_advertised_in_the_record() {
        let mut args = sven_server_args();
        assert!(!server_announce_record(&args).flags.allowlisted);
        args.allowlist = vec!["0102030405060708090a0b0c0d0e0f10".to_string()];
        assert!(server_announce_record(&args).flags.allowlisted);
    }

    /// An over-long name shortens the row; it never suppresses the announce.
    #[test]
    fn over_long_fields_truncate_rather_than_refuse() {
        let mut args = sven_server_args();
        args.name = Some("q".repeat(200));
        let record = server_announce_record(&args);
        assert_eq!(record.name.len(), crate::announce::MAX_NAME_LEN);
        record.encode().expect("a truncated record still encodes");
    }

    /// Truncation must not split a multi-byte character.
    #[test]
    fn truncation_respects_char_boundaries() {
        let mut args = sven_server_args();
        // 'é' is two bytes, so a 48-byte cut lands mid-character at 24 of them.
        args.name = Some("é".repeat(40));
        let record = server_announce_record(&args);
        assert!(record.name.len() <= crate::announce::MAX_NAME_LEN);
        assert_eq!(record.name.chars().count(), 24);
    }

    #[test]
    fn parse_allowlist_accepts_valid_hashes_and_rejects_junk() {
        assert!(parse_allowlist(&[]).unwrap().is_empty());

        let hex = "0102030405060708090a0b0c0d0e0f10";
        let parsed = parse_allowlist(&[format!("  {hex}  ")]).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0], IdentityHash::new([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]));

        assert!(parse_allowlist(&["nothex".to_string()]).is_err());
        // 15 bytes: right alphabet, wrong length.
        assert!(parse_allowlist(&["0102030405060708090a0b0c0d0e0f".to_string()]).is_err());
    }

    /// `frame` must never produce a chunk the engine refuses. If `MAX_CHUNK`
    /// or the engine pin ever moves the wrong way this fails here rather than
    /// at runtime inside a relay task.
    #[test]
    fn every_framed_chunk_fits_the_engine_cap() {
        let datagram = vec![0xabu8; crate::framing::MAX_CHUNK * 3 + 1];
        for chunk in frame(&datagram) {
            assert!(
                SendToLinkPayload::from_slice(&chunk).is_ok(),
                "chunk of {} bytes exceeds the engine cap",
                chunk.len()
            );
        }
    }
}
