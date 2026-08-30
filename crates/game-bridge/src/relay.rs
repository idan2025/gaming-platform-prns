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
use prns_core::routing::links::LinkId;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::task::LocalSet;
use tracing::{debug, error, info, warn};

use crate::config::{BridgeConfig, BridgeRole, ClientArgs, ServerArgs};
use crate::framing::{frame, Reassembler};
use crate::profile::{ASPECT_CLIENT, ASPECT_SERVER};

const UDP_READ_BUF: usize = 8192;

pub async fn run_bridge(cfg: BridgeConfig) -> Result<()> {
    let session = match cfg {
        BridgeConfig::Server(args) => BridgeSession::start_server(args).await?,
        BridgeConfig::Client(args) => BridgeSession::start_client(args).await?,
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
    stop_tx: Option<oneshot::Sender<()>>,
    done: Option<oneshot::Receiver<()>>,
}

/// A discovered `<app_name>.server` destination heard via announce.
#[derive(Debug, Clone)]
pub struct DiscoveredServer {
    pub destination_hash: DestinationHash,
    pub last_seen: Instant,
    /// The server's self-chosen display name, decoded from its announce
    /// app_data (valid UTF-8 only). `None` if the announcer didn't send a
    /// name-shaped payload.
    ///
    /// Step 3 (`PLAN.md` §3.3) replaces this single field with the decoded
    /// announce record, keeping this bare-UTF-8 path as the mandatory fallback
    /// for deployed v0.1.x peers.
    pub name: Option<String>,
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
                request_endpoints: ServeMyRequestEndpoints::No,
            }
            .destination_hash()
            .map_err(|e| anyhow!("invalid destination name: {e:?}"))?
        };

        spawn_bridge_node(BridgeRole::Server, Some(precomputed_hash), move |discovered, connected_clients| async move {
            let identity = load_identity(&args.identity)?;
            let app_name = args.profile.app_name.clone();
            let game_id = args.profile.id.clone();
            let name_bytes = server_announce_name_bytes(&args.name);
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
                request_endpoints: ServeMyRequestEndpoints::No,
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
                transport_identity: Some(identity),
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

            // Announcer.
            let announcer = handle.clone();
            let interval = args.announce_interval;
            let announce_app_data = server_announce_app_data(&args.name);
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
            let _router_task = tokio::spawn(async move {
                let mut event_rx = event_rx;
                while let Some(event) = event_rx.recv().await {
                    match event {
                        BridgeEvent::AnnounceHeard { destination, name } => {
                            remember_server(&router_discovered, destination, name).await;
                        }
                        BridgeEvent::PeerIdentified { link_id, identity } => {
                            router_connected_clients.write().await.insert(link_id, identity);
                        }
                        BridgeEvent::LinkEstablished { link_id } => {
                            if router_senders.read().await.contains_key(&link_id) {
                                continue;
                            }
                            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
                            router_senders.write().await.insert(link_id, tx);

                            let senders = router_senders.clone();
                            let handle = router_handle.clone();
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
                        BridgeEvent::LinkClosed { link_id } => {
                            if let Some(tx) = router_senders.write().await.remove(&link_id) {
                                let _ = tx.send(Vec::new()).await;
                            }
                            router_connected_clients.write().await.remove(&link_id);
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

        spawn_bridge_node(BridgeRole::Client, Some(precomputed_hash), move |discovered, _connected_clients| async move {
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
                transport_identity: Some(identity),
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
                        BridgeEvent::AnnounceHeard { destination, name } => {
                            remember_server(&router_discovered, destination, name).await;
                            let mut t = router_target.write().await;
                            if t.is_none() {
                                info!(server_hash = ?destination.as_bytes(), "discovered a server announce");
                                *t = Some(destination);
                            }
                        }
                        BridgeEvent::LinkEstablished { .. } => {}
                        // The client is always the link initiator, so it never
                        // becomes the identified peer of a link it accepted.
                        BridgeEvent::PeerIdentified { .. } => {}
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
    name: Option<String>,
) {
    let mut l = list.write().await;
    if let Some(s) = l.iter_mut().find(|s| s.destination_hash == destination) {
        s.last_seen = Instant::now();
        s.name = name;
    } else {
        l.push(DiscoveredServer { destination_hash: destination, last_seen: Instant::now(), name });
    }
}

// =========================================================================
// Shared event routing
// =========================================================================

#[derive(Debug)]
enum BridgeEvent {
    AnnounceHeard { destination: DestinationHash, name: Option<String> },
    LinkEstablished { link_id: LinkId },
    PeerIdentified { link_id: LinkId, identity: IdentityHash },
    LinkClosed { link_id: LinkId },
    LinkData { link_id: LinkId, bytes: Vec<u8> },
}

fn funnel_event(event: PrnsEvent<'_>, tx: &mpsc::UnboundedSender<BridgeEvent>) {
    match event {
        PrnsEvent::Diagnostic(Diagnostic::AnnounceHeard { destination, app_data, .. }) => {
            let name = announce_app_data_to_name(&app_data);
            let _ = tx.send(BridgeEvent::AnnounceHeard { destination, name });
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

/// Decode an announce's app_data as a display name: valid, non-empty UTF-8
/// only — anything else yields `None` rather than mojibake.
///
/// This is the **fallback path** that `PLAN.md` §3.3/§5 make mandatory: it is
/// how deployed v0.1.x servers, which announce a bare UTF-8 name and nothing
/// else, stay listable. Step 3 puts the structured record in front of it; this
/// function does not go away.
fn announce_app_data_to_name(app_data: &AnnounceAppDataBytes) -> Option<String> {
    if app_data.is_empty() {
        return None;
    }
    std::str::from_utf8(app_data).ok().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

type LinkSenders = Arc<RwLock<std::collections::HashMap<LinkId, mpsc::Sender<Vec<u8>>>>>;
type ConnectedClients = Arc<RwLock<std::collections::HashMap<LinkId, IdentityHash>>>;

/// Announce app_data used when no display name is configured.
///
/// Frozen: deployed v0.1.10 peers announce exactly these bytes, and the
/// launcher's browser recognises them. Do not "improve" it.
const DEFAULT_SERVER_ANNOUNCE_NAME: &[u8] = b"sc-rns-bridge";

/// Build the bytes a server announces as its app_data: the configured name
/// (trimmed, UTF-8, truncated to the wire budget), or the fixed default.
pub fn server_announce_name_bytes(name: &Option<String>) -> Vec<u8> {
    let trimmed = name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let bytes = trimmed.map(str::as_bytes).unwrap_or(DEFAULT_SERVER_ANNOUNCE_NAME);
    bytes[..bytes.len().min(MAX_ANNOUNCE_APP_DATA_LEN)].to_vec()
}

/// The `AnnounceAppData` for an `AnnounceNow` command, built from the same
/// name bytes as the destination's registered app_data.
pub fn server_announce_app_data(name: &Option<String>) -> AnnounceAppData {
    let bytes = server_announce_name_bytes(name);
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

    #[test]
    fn server_announce_app_data_round_trips_through_announce_app_data_to_name() {
        let name = Some("Idan's Server".to_string());
        let AnnounceAppData::Data(bytes) = server_announce_app_data(&name) else {
            panic!("expected AnnounceAppData::Data");
        };
        assert_eq!(announce_app_data_to_name(&bytes), Some("Idan's Server".to_string()));
    }

    #[test]
    fn announce_app_data_to_name_rejects_empty_and_non_utf8() {
        assert_eq!(announce_app_data_to_name(&AnnounceAppDataBytes::new()), None);
        let invalid_utf8 = AnnounceAppDataBytes::from_slice(&[0xff, 0xfe]).unwrap();
        assert_eq!(announce_app_data_to_name(&invalid_utf8), None);
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
