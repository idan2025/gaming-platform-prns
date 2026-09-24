//! Mode 3, virtual LAN: a room over a real node (`PLAN.md` §14, step 1).
//!
//! [`LanSession::host`] opens a room: it announces `<app_name>.lan` with the
//! §3.3 record at `transport_mode = 3`, seats whoever links and identifies, and
//! fans packets out between them. [`LanSession::join`] links to a room, is
//! seated, and exchanges packets with it. Both give the caller the same
//! surface — [`send`](LanSession::send) an IPv4 packet, [`recv`](LanSession::recv)
//! one — because the host is a member of its own room. Step 2's virtual adapter
//! sits on exactly that surface.
//!
//! Everything rides member↔host Links; `lan.rs` says why a GROUP cannot carry
//! it. Unicast between two members also passes through the host for now: it is
//! correct on any mesh, and a direct member↔member link is a latency
//! optimization that can come after there is a game to measure it with.
//!
//! The host is the one place the room's rules are enforced — the member table,
//! the source-address check in [`lan::route`], the broadcast gate. A member
//! checks what it sends and what it is given too, but only as a courtesy: a
//! member is somebody else's code.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use personal_rns::prelude::*;
use prns_core::engine::{SendToLink, SendToLinkPayload};
use prns_core::identity::in_memory::InMemoryNodeIdentity;
use prns_core::identity::{IdentityHash, IdentitySigner};
use prns_core::routing::announce::emit::AnnounceAppDataBytes;
use prns_core::routing::links::LinkId;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::announce::{AnnounceFlags, AnnounceInfo, AnnounceRecord};
use crate::config::BridgeRole;
use crate::lan::{
    self, BroadcastGate, LanMessage, Member, MemberTable, Refusal, RoomSubnet, Route, ASPECT_LAN,
    ASPECT_LAN_MEMBER, LAN_MTU, LAN_PROTOCOL_VERSION, TRANSPORT_MODE_LAN,
};
use crate::profile::GameProfile;
use crate::relay::{
    attach_interfaces, establish_link_with_path_retry, funnel_event, load_identity,
    parse_allowlist, parse_destination_hash, remember_server, spawn_bridge_node, BridgeEvent,
    DiscoveredServer, EARLY_ANNOUNCE_DELAYS_SECS,
};
use crate::BridgeSession;

/// How often a member repeats `Hello` until it is seated.
const HELLO_RETRY: Duration = Duration::from_secs(1);
/// How often the host resends the member table unprompted. It also sends it on
/// every change; this is what repairs a lost one.
const MEMBERS_REFRESH: Duration = Duration::from_secs(10);
/// How long a member waits before linking to the room again after losing it.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);
/// How long the host keeps a refused link open, so the `Refused` it just sent
/// can arrive before the link's teardown does.
const REFUSED_CLOSE_DELAY: Duration = Duration::from_secs(1);
/// Packets waiting for the caller to [`recv`](LanSession::recv) them. Past
/// this, new ones are dropped, the way a full NIC ring drops them.
const INBOUND_QUEUE: usize = 1024;

/// Open a room.
#[derive(Debug, Clone)]
pub struct LanHostArgs {
    /// Which game the room is for. Its `app_name` names the room's destination.
    pub profile: GameProfile,
    pub identity: std::path::PathBuf,
    /// As `ServerArgs::tcp`.
    pub tcp: Option<String>,
    pub auto: bool,
    pub announce_interval: u64,
    /// The room's name in the browser.
    pub name: Option<String>,
    /// Most members, the host included. Clamped to [`lan::MAX_MEMBERS`].
    pub max_members: usize,
    /// Identity hashes (hex) that may be seated. Empty seats anyone.
    pub allowlist: Vec<String>,
    /// How long a link may stay up without identifying. Every room needs one:
    /// a member's address is derived from its identity, so a link that never
    /// identifies can never be seated.
    pub identify_timeout_secs: u64,
    pub relay_transit: bool,
    pub subnet: RoomSubnet,
}

impl LanHostArgs {
    pub fn new(profile: GameProfile) -> Self {
        Self {
            profile,
            identity: std::path::PathBuf::from("./game-bridge-lan.identity"),
            tcp: None,
            auto: false,
            announce_interval: 15,
            name: None,
            max_members: 8,
            allowlist: Vec::new(),
            identify_timeout_secs: 10,
            relay_transit: true,
            subnet: RoomSubnet::default(),
        }
    }
}

/// Join a room.
#[derive(Debug, Clone)]
pub struct LanMemberArgs {
    pub profile: GameProfile,
    pub identity: std::path::PathBuf,
    pub tcp: Option<String>,
    pub auto: bool,
    /// The room's destination hash, hex. `None` joins the first room for this
    /// game that announces.
    pub room_hash: Option<String>,
    /// Off by default, as for a client (`PLAN.md` §4).
    pub relay_transit: bool,
}

impl LanMemberArgs {
    pub fn new(profile: GameProfile) -> Self {
        Self {
            profile,
            identity: std::path::PathBuf::from("./game-bridge-lan.identity"),
            tcp: None,
            auto: false,
            room_hash: None,
            relay_transit: false,
        }
    }
}

/// What this side knows about its room right now.
#[derive(Debug, Clone)]
pub struct RoomView {
    pub own_identity: IdentityHash,
    /// `None` until seated.
    pub own_address: Option<Ipv4Addr>,
    pub subnet: RoomSubnet,
    pub members: Vec<Member>,
    pub epoch: u32,
    /// Set when the host refused this member. A refused member stops trying.
    pub refused: Option<Refusal>,
}

impl RoomView {
    fn new(own_identity: IdentityHash, subnet: RoomSubnet) -> Self {
        Self {
            own_identity,
            own_address: None,
            subnet,
            members: Vec::new(),
            epoch: 0,
            refused: None,
        }
    }

    /// Take a host's table. The host is the only writer, so its newest word
    /// wins; an older copy arriving late is ignored.
    fn apply(&mut self, epoch: u32, subnet: RoomSubnet, members: Vec<Member>) {
        let newer = self.own_address.is_none() || epoch.wrapping_sub(self.epoch) as i32 > 0;
        if !newer && epoch != self.epoch {
            return;
        }
        self.own_address =
            members.iter().find(|m| m.identity == self.own_identity).map(|m| m.address);
        self.epoch = epoch;
        self.subnet = subnet;
        self.members = members;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanSendError {
    NotIpv4,
    TooLarge(usize),
    Stopped,
}

impl core::fmt::Display for LanSendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotIpv4 => write!(f, "not an IPv4 packet"),
            Self::TooLarge(n) => write!(f, "packet of {n} bytes is over the {LAN_MTU}-byte MTU"),
            Self::Stopped => write!(f, "the room session has stopped"),
        }
    }
}

impl std::error::Error for LanSendError {}

/// A running room, from either side.
pub struct LanSession {
    bridge: BridgeSession,
    view: Arc<Mutex<RoomView>>,
    outbound: mpsc::UnboundedSender<Vec<u8>>,
    inbound: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    room_hash: Arc<Mutex<Option<DestinationHash>>>,
}

impl LanSession {
    /// Hand one IPv4 packet to the room. Returns once it is queued; like a
    /// LAN, delivery is not promised.
    pub fn send(&self, packet: Vec<u8>) -> Result<(), LanSendError> {
        if packet.len() > LAN_MTU {
            return Err(LanSendError::TooLarge(packet.len()));
        }
        if lan::ipv4_endpoints(&packet).is_none() {
            return Err(LanSendError::NotIpv4);
        }
        self.outbound.send(packet).map_err(|_| LanSendError::Stopped)
    }

    /// The next packet the room delivered to this member. `None` once the
    /// session has stopped.
    pub async fn recv(&self) -> Option<Vec<u8>> {
        self.inbound.lock().await.recv().await
    }

    pub fn view(&self) -> RoomView {
        self.view.lock().expect("room view lock").clone()
    }

    pub fn own_address(&self) -> Option<Ipv4Addr> {
        self.view().own_address
    }

    /// The room's destination: this node's own on a host, the one it joined
    /// (once known) on a member.
    pub fn room_hash(&self) -> Option<DestinationHash> {
        *self.room_hash.lock().expect("room hash lock")
    }

    /// Rooms and servers this node has heard announce.
    pub async fn discovered(&self) -> Vec<DiscoveredServer> {
        self.bridge.discovered().await
    }

    pub async fn stop(&mut self) {
        self.bridge.stop().await;
    }

    /// Open a room.
    pub async fn host(args: LanHostArgs) -> Result<Self> {
        args.profile.validate().map_err(|e| anyhow!("invalid game profile: {e}"))?;
        let allowlist = parse_allowlist(&args.allowlist)?;
        let secret = load_identity(&args.identity)?;
        let own_identity = InMemoryNodeIdentity::from_secret_key_bytes(&secret).identity_hash();
        let room_hash = room_destination(&args.profile, secret.clone(), &[])?;

        let mut table = MemberTable::new(args.subnet, args.max_members);
        let own_address = table.admit(own_identity).expect("an empty room seats its host");
        let view = Arc::new(Mutex::new(RoomView::new(own_identity, args.subnet)));
        view.lock().expect("room view lock").apply(
            table.epoch(),
            table.subnet(),
            table.members().to_vec(),
        );
        info!(game = %args.profile.id, address = %own_address, "opening a LAN room");

        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(INBOUND_QUEUE);
        let thread_view = view.clone();

        let bridge = spawn_bridge_node(
            BridgeRole::Server,
            Some(room_hash),
            args.relay_transit,
            move |discovered, _| async move {
                let (event_tx, event_rx) = mpsc::unbounded_channel::<BridgeEvent>();
                let node = PrnsNode::new(PrnsNodeRecipe {
                    transport_identity: args.relay_transit.then_some(secret.clone()),
                    remote_control: personal_rns::remote_control::RemoteControlService::Unavailable,
                    pre_configured_destinations: [room_config(
                        &args.profile.app_name,
                        &[ASPECT_LAN],
                        secret,
                    )],
                    app_state: (),
                    storage: GrowableHeap,
                    request_endpoints: request_endpoints![],
                    on_event: {
                        let event_tx = event_tx.clone();
                        move |event, _state| funnel_event(event, &event_tx)
                    },
                    interfaces: |node: &PrnsNodeHandle| {
                        attach_interfaces(node, args.tcp.as_deref(), args.auto)
                    },
                    persistence: NoPersistence,
                });
                let handle = node.handle();

                // The announcer. The row is rebuilt every tick so the member count
                // in the browser follows the room.
                let announcer = handle.clone();
                let announce_view = thread_view.clone();
                let base = AnnounceRecord {
                    protocol_version: LAN_PROTOCOL_VERSION,
                    flags: AnnounceFlags {
                        passworded: false,
                        allowlisted: !allowlist.is_empty(),
                        dedicated: false,
                        transport_mode: TRANSPORT_MODE_LAN,
                    },
                    min_link_class: args.profile.min_link_class,
                    players: 0,
                    max_players: args.max_members.clamp(1, lan::MAX_MEMBERS) as u8,
                    game_id: args.profile.id.clone(),
                    name: crate::relay::truncate(
                        args.name
                            .as_deref()
                            .map(str::trim)
                            .filter(|n| !n.is_empty())
                            .unwrap_or("LAN room"),
                        crate::announce::MAX_NAME_LEN,
                    ),
                    map: String::new(),
                    tlvs: Vec::new(),
                };
                let interval = args.announce_interval.max(1);
                tokio::spawn(async move {
                    let mut early = EARLY_ANNOUNCE_DELAYS_SECS.iter().copied();
                    loop {
                        let wait = early.next().unwrap_or(interval);
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        let mut record = base.clone();
                        record.players =
                            announce_view.lock().expect("room view lock").members.len() as u8;
                        let Ok(bytes) = record.encode() else {
                            warn!("a LAN room record did not encode; not announcing");
                            return;
                        };
                        let app_data = AnnounceAppData::Data(
                            AnnounceAppDataBytes::from_slice(&bytes).unwrap_or_default(),
                        );
                        let sent = announcer.issue(PrnsCommand::AnnounceNow(AnnounceNow {
                            destination: room_hash,
                            target: AnnounceTarget::AllInterfaces,
                            app_data,
                        }));
                        if sent.is_none() {
                            return;
                        }
                    }
                });

                let host = Host {
                    handle: handle.clone(),
                    own_identity,
                    table,
                    allowlist,
                    gate: BroadcastGate::default(),
                    links: HashMap::new(),
                    seated: HashMap::new(),
                    view: thread_view,
                    inbound: in_tx,
                    event_tx: event_tx.clone(),
                    identify_timeout: Duration::from_secs(args.identify_timeout_secs.max(1)),
                };
                tokio::spawn(host.run(event_rx, out_rx, discovered));
                Ok((handle, async move {
                    let _ = node.run().await;
                }))
            },
        )
        .await?;

        Ok(Self {
            bridge,
            view,
            outbound: out_tx,
            inbound: tokio::sync::Mutex::new(in_rx),
            room_hash: Arc::new(Mutex::new(Some(room_hash))),
        })
    }

    /// Join a room.
    pub async fn join(args: LanMemberArgs) -> Result<Self> {
        args.profile.validate().map_err(|e| anyhow!("invalid game profile: {e}"))?;
        let explicit = match args.room_hash.as_deref() {
            Some(hex) => Some(parse_destination_hash(hex).context("invalid room hash")?),
            None => None,
        };
        let secret = load_identity(&args.identity)?;
        let own_identity = InMemoryNodeIdentity::from_secret_key_bytes(&secret).identity_hash();
        let own_hash = room_destination(&args.profile, secret.clone(), &[ASPECT_LAN_MEMBER])?;

        let view = Arc::new(Mutex::new(RoomView::new(own_identity, RoomSubnet::default())));
        let room_hash = Arc::new(Mutex::new(explicit));
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(INBOUND_QUEUE);
        let thread_view = view.clone();
        let thread_room = room_hash.clone();

        let bridge = spawn_bridge_node(
            BridgeRole::Client,
            Some(own_hash),
            args.relay_transit,
            move |discovered, _| async move {
                let (event_tx, event_rx) = mpsc::unbounded_channel::<BridgeEvent>();
                let node = PrnsNode::new(PrnsNodeRecipe {
                    transport_identity: args.relay_transit.then_some(secret.clone()),
                    remote_control: personal_rns::remote_control::RemoteControlService::Unavailable,
                    pre_configured_destinations: [room_config(
                        &args.profile.app_name,
                        &[ASPECT_LAN_MEMBER],
                        secret,
                    )],
                    app_state: (),
                    storage: GrowableHeap,
                    request_endpoints: request_endpoints![],
                    on_event: {
                        let event_tx = event_tx.clone();
                        move |event, _state| funnel_event(event, &event_tx)
                    },
                    interfaces: |node: &PrnsNodeHandle| {
                        attach_interfaces(node, args.tcp.as_deref(), args.auto)
                    },
                    persistence: NoPersistence,
                });
                let handle = node.handle();
                let member = MemberSide {
                    handle: handle.clone(),
                    game_id: args.profile.id.clone(),
                    own_identity,
                    room: thread_room,
                    link: None,
                    view: thread_view,
                    inbound: in_tx,
                };
                tokio::spawn(member.run(event_rx, out_rx, discovered));
                Ok((handle, async move {
                    let _ = node.run().await;
                }))
            },
        )
        .await?;

        Ok(Self {
            bridge,
            view,
            outbound: out_tx,
            inbound: tokio::sync::Mutex::new(in_rx),
            room_hash,
        })
    }
}

/// A room's (or a member's) destination: `ProveNone`, like a game server's,
/// because a packet on a LAN is never retried and a proof for one buys nothing.
fn room_config<'a>(
    app_name: &'a str,
    aspects: &'a [&'a str],
    identity: crate::relay::ZeroizingIdentity,
) -> PreConfiguredDestination<'a> {
    PreConfiguredDestination::Single {
        app_name,
        aspects,
        identity,
        announce_app_data: b"",
        proof: ProofStrategy::ProveNone,
        link_requests: LinkRequestPolicy::AcceptAll,
        ratchet: RatchetPolicy::NoRatchets,
        resource_strategy: ResourceStrategy::AcceptNone,
        maximum_request_bytes: Default::default(),
        request_endpoints: ServeMyRequestEndpoints::No,
    }
}

fn room_destination(
    profile: &GameProfile,
    identity: crate::relay::ZeroizingIdentity,
    aspects: &[&str],
) -> Result<DestinationHash> {
    let aspects: &[&str] = if aspects.is_empty() { &[ASPECT_LAN] } else { aspects };
    room_config(&profile.app_name, aspects, identity)
        .destination_hash()
        .map_err(|e| anyhow!("invalid destination name: {e:?}"))
}

fn send_message(handle: &PrnsNodeHandle, link_id: LinkId, message: &LanMessage) -> bool {
    let bytes = message.encode();
    match SendToLinkPayload::from_slice(&bytes) {
        Ok(payload) => {
            handle.issue(PrnsCommand::SendToLink(SendToLink { link_id, payload })).is_some()
        }
        Err(_) => {
            // Bounded by construction (`LAN_MTU`, `MAX_MEMBERS`), so this is
            // our bug, not a peer's.
            warn!(len = bytes.len(), "a LAN message is over the link cap; dropping it");
            false
        }
    }
}

fn deliver(inbound: &mpsc::Sender<Vec<u8>>, packet: Vec<u8>) {
    if let Err(mpsc::error::TrySendError::Full(_)) = inbound.try_send(packet) {
        debug!("LAN inbound queue full; dropping a packet");
    }
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

struct Host {
    handle: PrnsNodeHandle,
    own_identity: IdentityHash,
    table: MemberTable,
    allowlist: Vec<IdentityHash>,
    gate: BroadcastGate,
    /// Every accepted link, with the identity it gave once it gave one.
    links: HashMap<LinkId, Option<IdentityHash>>,
    /// Seated identity → the link it is seated on. A member that reconnects
    /// is moved to its new link; the old one closing then unseats no one.
    seated: HashMap<[u8; 16], LinkId>,
    view: Arc<Mutex<RoomView>>,
    inbound: mpsc::Sender<Vec<u8>>,
    event_tx: mpsc::UnboundedSender<BridgeEvent>,
    identify_timeout: Duration,
}

impl Host {
    async fn run(
        mut self,
        mut events: mpsc::UnboundedReceiver<BridgeEvent>,
        mut outbound: mpsc::UnboundedReceiver<Vec<u8>>,
        discovered: Arc<tokio::sync::RwLock<Vec<DiscoveredServer>>>,
    ) {
        let mut refresh = tokio::time::interval(MEMBERS_REFRESH);
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { return };
                    self.on_event(event, &discovered).await;
                }
                packet = outbound.recv() => {
                    let Some(packet) = packet else { return };
                    let own = self.own_identity;
                    self.forward(own, None, packet);
                }
                _ = refresh.tick() => self.publish_members(),
            }
        }
    }

    async fn on_event(
        &mut self,
        event: BridgeEvent,
        discovered: &Arc<tokio::sync::RwLock<Vec<DiscoveredServer>>>,
    ) {
        match event {
            BridgeEvent::AnnounceHeard { destination, hops, source_interface, info } => {
                remember_server(discovered, destination, hops, source_interface, info).await;
            }
            BridgeEvent::LinkEstablished { link_id } => {
                self.links.insert(link_id, None);
                let tx = self.event_tx.clone();
                let timeout = self.identify_timeout;
                tokio::spawn(async move {
                    tokio::time::sleep(timeout).await;
                    let _ = tx.send(BridgeEvent::IdentifyTimeout { link_id });
                });
            }
            BridgeEvent::PeerIdentified { link_id, identity } => self.seat(link_id, identity),
            BridgeEvent::IdentifyTimeout { link_id } => {
                if let Some(None) = self.links.get(&link_id) {
                    debug!(link = ?link_id, "a room link never identified; closing it");
                    self.links.remove(&link_id);
                    let _ = self.handle.close_link(link_id);
                }
            }
            BridgeEvent::LinkClosed { link_id } => {
                if let Some(Some(identity)) = self.links.remove(&link_id) {
                    if self.seated.get(identity.as_bytes()) == Some(&link_id) {
                        self.seated.remove(identity.as_bytes());
                        self.gate.forget(&identity);
                        if self.table.remove(&identity) {
                            info!(identity = ?identity.as_bytes(), "a member left the room");
                            self.publish_members();
                        }
                    }
                }
            }
            BridgeEvent::LinkData { link_id, bytes } => {
                let Some(Some(identity)) = self.links.get(&link_id).copied() else {
                    return;
                };
                if self.seated.get(identity.as_bytes()) != Some(&link_id) {
                    return;
                }
                match LanMessage::decode(&bytes) {
                    Ok(LanMessage::Hello { .. }) => {
                        send_message(&self.handle, link_id, &self.table.snapshot());
                    }
                    Ok(LanMessage::Packet(packet)) => self.forward(identity, Some(link_id), packet),
                    // Members and Refused are the host's own words; a member
                    // sending them is ignored like any unknown message.
                    Ok(_) => {}
                    Err(e) => debug!(link = ?link_id, error = %e, "undecodable LAN message"),
                }
            }
        }
    }

    fn seat(&mut self, link_id: LinkId, identity: IdentityHash) {
        if !self.links.contains_key(&link_id) {
            return;
        }
        self.links.insert(link_id, Some(identity));
        let refusal = if identity == self.own_identity {
            // Someone linking with the host's own identity would otherwise be
            // handed the host's seat, and its traffic.
            Some(Refusal::NotAllowed)
        } else if !self.allowlist.is_empty() && !self.allowlist.contains(&identity) {
            Some(Refusal::NotAllowed)
        } else {
            let before = self.table.epoch();
            match self.table.admit(identity) {
                Ok(address) => {
                    self.seated.insert(*identity.as_bytes(), link_id);
                    info!(identity = ?identity.as_bytes(), %address, "seated a member");
                    if self.table.epoch() != before {
                        self.publish_members();
                    } else {
                        send_message(&self.handle, link_id, &self.table.snapshot());
                    }
                    None
                }
                Err(r) => Some(r),
            }
        };
        if let Some(refusal) = refusal {
            warn!(identity = ?identity.as_bytes(), %refusal, "refused a member");
            send_message(&self.handle, link_id, &LanMessage::Refused(refusal));
            let handle = self.handle.clone();
            tokio::spawn(async move {
                tokio::time::sleep(REFUSED_CLOSE_DELAY).await;
                let _ = handle.close_link(link_id);
            });
        }
    }

    /// Route a packet from `sender` — a member on `from`, or the host itself.
    fn forward(&mut self, sender: IdentityHash, from: Option<LinkId>, packet: Vec<u8>) {
        match lan::route(&self.table, &sender, &packet) {
            Route::Broadcast => {
                if !self.gate.allow(sender, &packet, Instant::now()) {
                    debug!(sender = ?sender.as_bytes(), "broadcast over the member's rate; dropped");
                    return;
                }
                if sender != self.own_identity {
                    deliver(&self.inbound, packet.clone());
                }
                let message = LanMessage::Packet(packet);
                for &link_id in self.seated.values() {
                    if Some(link_id) != from {
                        send_message(&self.handle, link_id, &message);
                    }
                }
            }
            Route::Unicast(to) if to == self.own_identity => deliver(&self.inbound, packet),
            Route::Unicast(to) => {
                if let Some(&link_id) = self.seated.get(to.as_bytes()) {
                    send_message(&self.handle, link_id, &LanMessage::Packet(packet));
                }
            }
            Route::Drop(reason) => {
                debug!(sender = ?sender.as_bytes(), ?reason, "dropped a LAN packet")
            }
        }
    }

    fn publish_members(&self) {
        let snapshot = self.table.snapshot();
        for &link_id in self.seated.values() {
            send_message(&self.handle, link_id, &snapshot);
        }
        self.view.lock().expect("room view lock").apply(
            self.table.epoch(),
            self.table.subnet(),
            self.table.members().to_vec(),
        );
    }
}

// ---------------------------------------------------------------------------
// Member
// ---------------------------------------------------------------------------

struct MemberSide {
    handle: PrnsNodeHandle,
    game_id: String,
    own_identity: IdentityHash,
    room: Arc<Mutex<Option<DestinationHash>>>,
    /// The link to the host, while there is one.
    link: Option<LinkId>,
    view: Arc<Mutex<RoomView>>,
    inbound: mpsc::Sender<Vec<u8>>,
}

/// What the connector tells the member's loop.
enum Connection {
    Up(LinkId),
    Failed,
}

impl MemberSide {
    async fn run(
        mut self,
        mut events: mpsc::UnboundedReceiver<BridgeEvent>,
        mut outbound: mpsc::UnboundedReceiver<Vec<u8>>,
        discovered: Arc<tokio::sync::RwLock<Vec<DiscoveredServer>>>,
    ) {
        let (conn_tx, mut conn_rx) = mpsc::unbounded_channel::<Connection>();
        let mut connecting = false;
        let mut hello = tokio::time::interval(HELLO_RETRY);
        loop {
            // Link to the room whenever there is a room to link to and no link.
            let room = *self.room.lock().expect("room hash lock");
            let refused = self.view.lock().expect("room view lock").refused.is_some();
            if let (Some(room), None, false, false) = (room, self.link, connecting, refused) {
                connecting = true;
                let handle = self.handle.clone();
                let own = self.own_identity;
                let tx = conn_tx.clone();
                tokio::spawn(async move {
                    let outcome = match establish_link_with_path_retry(&handle, room).await {
                        Some(link_id) => match handle.identify(link_id, own).await {
                            Ok(()) => Connection::Up(link_id),
                            Err(e) => {
                                warn!(error = ?e, "could not identify to the room");
                                let _ = handle.close_link(link_id);
                                Connection::Failed
                            }
                        },
                        None => Connection::Failed,
                    };
                    if matches!(outcome, Connection::Failed) {
                        tokio::time::sleep(RECONNECT_DELAY).await;
                    }
                    let _ = tx.send(outcome);
                });
            }

            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { return };
                    self.on_event(event, &discovered).await;
                }
                conn = conn_rx.recv() => {
                    connecting = false;
                    if let Some(Connection::Up(link_id)) = conn {
                        info!(link = ?link_id, "linked to the LAN room");
                        self.link = Some(link_id);
                        send_message(&self.handle, link_id, &LanMessage::Hello { version: LAN_PROTOCOL_VERSION });
                    }
                }
                packet = outbound.recv() => {
                    let Some(packet) = packet else { return };
                    self.send(packet);
                }
                _ = hello.tick() => {
                    let seated = self.view.lock().expect("room view lock").own_address.is_some();
                    if let (Some(link_id), false) = (self.link, seated) {
                        send_message(&self.handle, link_id, &LanMessage::Hello { version: LAN_PROTOCOL_VERSION });
                    }
                }
            }
        }
    }

    async fn on_event(
        &mut self,
        event: BridgeEvent,
        discovered: &Arc<tokio::sync::RwLock<Vec<DiscoveredServer>>>,
    ) {
        match event {
            BridgeEvent::AnnounceHeard { destination, hops, source_interface, info } => {
                let is_room = matches!(
                    &info,
                    AnnounceInfo::Record(r) if r.flags.transport_mode == TRANSPORT_MODE_LAN && r.game_id == self.game_id
                );
                remember_server(discovered, destination, hops, source_interface, info).await;
                let mut room = self.room.lock().expect("room hash lock");
                if is_room && room.is_none() {
                    info!(room = ?destination.as_bytes(), "found a LAN room");
                    *room = Some(destination);
                }
            }
            BridgeEvent::LinkClosed { link_id } if self.link == Some(link_id) => {
                warn!(link = ?link_id, "lost the LAN room; linking again");
                self.link = None;
                self.view.lock().expect("room view lock").own_address = None;
            }
            BridgeEvent::LinkData { link_id, bytes } if self.link == Some(link_id) => {
                match LanMessage::decode(&bytes) {
                    Ok(LanMessage::Members { epoch, subnet, members }) => {
                        let mut view = self.view.lock().expect("room view lock");
                        let was = view.own_address;
                        view.apply(epoch, subnet, members);
                        if was.is_none() {
                            if let Some(address) = view.own_address {
                                info!(%address, "seated in the LAN room");
                            }
                        }
                    }
                    Ok(LanMessage::Refused(refusal)) => {
                        warn!(%refusal, "the LAN room refused this member");
                        self.view.lock().expect("room view lock").refused = Some(refusal);
                    }
                    Ok(LanMessage::Packet(packet)) => self.receive(packet),
                    Ok(_) => {}
                    Err(e) => debug!(error = %e, "undecodable LAN message"),
                }
            }
            _ => {}
        }
    }

    /// Accept a packet the host delivered, if it is addressed to this member.
    fn receive(&self, packet: Vec<u8>) {
        let Some((_, dst)) = lan::ipv4_endpoints(&packet) else { return };
        let view = self.view.lock().expect("room view lock");
        if Some(dst) == view.own_address || view.subnet.is_broadcast(dst) {
            deliver(&self.inbound, packet);
        }
    }

    fn send(&self, packet: Vec<u8>) {
        let Some(link_id) = self.link else { return };
        let own = self.view.lock().expect("room view lock").own_address;
        match (lan::ipv4_endpoints(&packet), own) {
            (Some((src, _)), Some(own)) if src == own => {
                send_message(&self.handle, link_id, &LanMessage::Packet(packet));
            }
            // Not seated yet, or a source that is not this member's. The host
            // would drop it anyway; not sending it is cheaper.
            _ => debug!("not sending a LAN packet this member cannot originate"),
        }
    }
}
