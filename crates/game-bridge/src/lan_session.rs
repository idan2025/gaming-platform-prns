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
    self, BroadcastGate, ChatGate, LanMessage, Member, MemberTable, Refusal, RoomSubnet, Route, ASPECT_LAN,
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
/// Chat lines a session keeps for its UI. Older ones fall off.
const CHAT_KEPT: usize = 200;

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
    /// What the host is called in its own room's chat and member list.
    pub player_name: Option<String>,
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
            player_name: None,
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
    /// What this member asks to be called in the room.
    pub player_name: Option<String>,
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
            player_name: None,
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
    /// The names members gave, by identity, as the host last said. Data a
    /// member chose: never how members are told apart.
    pub names: Vec<(IdentityHash, String)>,
    /// Whether the room has chat: always on a host, and on a member once the
    /// host has sent `Names` (a host older than chat never does).
    pub chat: bool,
}

impl RoomView {
    /// The name `identity` gave, if it gave one.
    pub fn name_of(&self, identity: &IdentityHash) -> Option<&str> {
        self.names.iter().find(|(id, _)| id == identity).map(|(_, n)| n.as_str())
    }
}

/// One line of room chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatLine {
    /// Counts up from 1 for the session, so a UI asks for what is newer.
    pub seq: u64,
    /// Who sent it, as the host's link proved it.
    pub from: IdentityHash,
    pub text: String,
    pub at_unix: u64,
}

#[derive(Debug, Default)]
struct ChatLog {
    lines: std::collections::VecDeque<ChatLine>,
    next: u64,
}

impl ChatLog {
    fn push(&mut self, from: IdentityHash, text: String) {
        self.next += 1;
        let at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.lines.push_back(ChatLine { seq: self.next, from, text, at_unix });
        while self.lines.len() > CHAT_KEPT {
            self.lines.pop_front();
        }
    }
}

/// What the caller asks of the session's loop.
enum Control {
    SetName(String),
    Chat(String),
}

/// Why a chat line or name was not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatError {
    /// Empty, too long, or not one line of plain text.
    NotPlainText,
    /// The host's build has no chat.
    NoChat,
    Stopped,
}

impl core::fmt::Display for ChatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotPlainText => write!(
                f,
                "one line of text, up to {} characters (a name: up to {})",
                lan::MAX_CHAT_BYTES,
                lan::MAX_NAME_BYTES
            ),
            Self::NoChat => write!(f, "the room's host runs a Lanthorn without chat"),
            Self::Stopped => write!(f, "the room session has stopped"),
        }
    }
}

impl std::error::Error for ChatError {}

impl RoomView {
    fn new(own_identity: IdentityHash, subnet: RoomSubnet) -> Self {
        Self {
            own_identity,
            own_address: None,
            subnet,
            members: Vec::new(),
            epoch: 0,
            refused: None,
            names: Vec::new(),
            chat: false,
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
    probes: crate::lan_check::ProbeLog,
    connects: crate::lan_firewall::ConnectWatch,
    refused: crate::lan_filter::RefusedLog,
    extra_ports: crate::lan_filter::ExtraPorts,
    rebound: Mutex<Vec<crate::lan_rebind::Rebound>>,
    /// Wakes a host's announcer early (`announce_now`). `None` on a member,
    /// which announces nothing anyone browses.
    announce: Option<Arc<tokio::sync::Notify>>,
    control: mpsc::UnboundedSender<Control>,
    chat: Arc<Mutex<ChatLog>>,
}

impl LanSession {
    /// Say one line in the room's chat. It shows in [`chat_since`] once the
    /// host sends it back, which is the only receipt there is.
    ///
    /// [`chat_since`]: LanSession::chat_since
    pub fn send_chat(&self, text: &str) -> Result<(), ChatError> {
        let text = text.trim();
        if !lan::valid_chat(text) {
            return Err(ChatError::NotPlainText);
        }
        if !self.view.lock().expect("room view lock").chat {
            return Err(ChatError::NoChat);
        }
        self.control.send(Control::Chat(text.to_string())).map_err(|_| ChatError::Stopped)
    }

    /// Change what this side is called in the room.
    pub fn set_name(&self, name: &str) -> Result<(), ChatError> {
        let name = name.trim();
        if !lan::valid_name(name) {
            return Err(ChatError::NotPlainText);
        }
        self.control.send(Control::SetName(name.to_string())).map_err(|_| ChatError::Stopped)
    }

    /// Chat lines newer than `seq` (0 for all that are kept), oldest first.
    pub fn chat_since(&self, seq: u64) -> Vec<ChatLine> {
        let log = self.chat.lock().expect("chat lock");
        log.lines.iter().filter(|l| l.seq > seq).cloned().collect()
    }

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

    /// The room's subnet, without copying the member table.
    pub fn subnet(&self) -> RoomSubnet {
        self.view.lock().expect("room view lock").subnet
    }

    pub fn own_address(&self) -> Option<Ipv4Addr> {
        // Read on every packet: the field, not a copy of the member table.
        self.view.lock().expect("room view lock").own_address
    }

    /// The room's destination: this node's own on a host, the one it joined
    /// (once known) on a member.
    pub fn room_hash(&self) -> Option<DestinationHash> {
        *self.room_hash.lock().expect("room hash lock")
    }

    /// What this member's pump saw of a room check (`lan_check.rs`).
    pub fn probe_log(&self) -> &crate::lan_check::ProbeLog {
        &self.probes
    }

    /// Connections the room delivered here that this machine never answered
    /// (`lan_firewall.rs`).
    pub fn connect_watch(&self) -> &crate::lan_firewall::ConnectWatch {
        &self.connects
    }

    /// What this member's filter refused (`lan_filter.rs`): the ports a game
    /// used that its pack does not list.
    pub fn refused_log(&self) -> &crate::lan_filter::RefusedLog {
        &self.refused
    }

    /// Ports this machine's player allowed on top of the pack's; the pump
    /// picks a change up on its next packet.
    pub fn extra_ports(&self) -> &crate::lan_filter::ExtraPorts {
        &self.extra_ports
    }

    /// The game's ports this machine's pump is translating to another of its
    /// addresses, because the game listens there and not on the room's
    /// (`lan_rebind.rs`). Written by the pump.
    pub fn rebound(&self) -> Vec<crate::lan_rebind::Rebound> {
        self.rebound.lock().expect("rebound lock").clone()
    }

    pub fn set_rebound(&self, rebound: Vec<crate::lan_rebind::Rebound>) {
        *self.rebound.lock().expect("rebound lock") = rebound;
    }

    /// Announce the room now rather than at the announcer's next tick, so a
    /// player who just connected a new interface does not wait out the
    /// interval. The announcer keeps its own floor between announces, so
    /// pressing this repeatedly cannot become a storm on a slow link. Returns
    /// false on a member: only a host has a room to announce.
    pub fn announce_now(&self) -> bool {
        match &self.announce {
            Some(n) => {
                n.notify_one();
                true
            }
            None => false,
        }
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

        let announce = Arc::new(tokio::sync::Notify::new());
        let announce_wake = announce.clone();
        let mut table = MemberTable::new(args.subnet, args.max_members);
        let own_address = table.admit(own_identity).expect("an empty room seats its host");
        let view = Arc::new(Mutex::new(RoomView::new(own_identity, args.subnet)));
        let own_name = args.player_name.as_deref().map(str::trim).filter(|n| lan::valid_name(n));
        {
            let mut v = view.lock().expect("room view lock");
            v.chat = true;
            if let Some(n) = own_name {
                v.names = vec![(own_identity, n.to_string())];
            }
        }
        let own_name = own_name.map(str::to_string);
        let (control_tx, control_rx) = mpsc::unbounded_channel::<Control>();
        let chat = Arc::new(Mutex::new(ChatLog::default()));
        let thread_chat = chat.clone();
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
                    remote_control: personal_rns::runtime::RemoteControlNodeSetup::new(
                        personal_rns::remote_control::RemoteControlService::Unavailable,
                    ),
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
                        // A room's chat is the room's own (`lan.rs`).
                        chat: false,
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
                    let mut last: Option<tokio::time::Instant> = None;
                    loop {
                        let wait = early.next().unwrap_or(interval);
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                            _ = announce_wake.notified() => {
                                // Asked for early: still never closer together
                                // than the floor.
                                if let Some(at) = last {
                                    let due = at + crate::relay::MIN_ANNOUNCE_GAP;
                                    tokio::time::sleep_until(due).await;
                                }
                            }
                        }
                        last = Some(tokio::time::Instant::now());
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
                    names: own_name.into_iter().map(|n| (*own_identity.as_bytes(), n)).collect(),
                    chat_gate: ChatGate::default(),
                    chat: thread_chat,
                };
                tokio::spawn(host.run(event_rx, out_rx, control_rx, discovered));
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
            probes: Default::default(),
            connects: Default::default(),
            refused: Default::default(),
            extra_ports: Default::default(),
            rebound: Default::default(),
            announce: Some(announce),
            control: control_tx,
            chat,
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
        let (control_tx, control_rx) = mpsc::unbounded_channel::<Control>();
        let chat = Arc::new(Mutex::new(ChatLog::default()));
        let thread_chat = chat.clone();
        let own_name = args
            .player_name
            .as_deref()
            .map(str::trim)
            .filter(|n| lan::valid_name(n))
            .map(str::to_string);

        let bridge = spawn_bridge_node(
            BridgeRole::Client,
            Some(own_hash),
            args.relay_transit,
            move |discovered, _| async move {
                let (event_tx, event_rx) = mpsc::unbounded_channel::<BridgeEvent>();
                let node = PrnsNode::new(PrnsNodeRecipe {
                    transport_identity: args.relay_transit.then_some(secret.clone()),
                    remote_control: personal_rns::runtime::RemoteControlNodeSetup::new(
                        personal_rns::remote_control::RemoteControlService::Unavailable,
                    ),
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
                    name: own_name,
                    chat: thread_chat,
                };
                tokio::spawn(member.run(event_rx, out_rx, control_rx, discovered));
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
            probes: Default::default(),
            connects: Default::default(),
            refused: Default::default(),
            extra_ports: Default::default(),
            rebound: Default::default(),
            announce: None,
            control: control_tx,
            chat,
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
    /// Names by identity, the host's own included. Only seated members'.
    names: HashMap<[u8; 16], String>,
    chat_gate: ChatGate,
    chat: Arc<Mutex<ChatLog>>,
}

impl Host {
    async fn run(
        mut self,
        mut events: mpsc::UnboundedReceiver<BridgeEvent>,
        mut outbound: mpsc::UnboundedReceiver<Vec<u8>>,
        mut control: mpsc::UnboundedReceiver<Control>,
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
                Some(c) = control.recv() => match c {
                    Control::SetName(name) => {
                        self.names.insert(*self.own_identity.as_bytes(), name);
                        self.publish_names();
                    }
                    Control::Chat(text) => {
                        let own = self.own_identity;
                        self.say(own, text);
                    }
                },
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
            // Only a game server's chat reader makes these; a room has none.
            BridgeEvent::ChatHello { .. } => {}
            BridgeEvent::LinkClosed { link_id } => {
                if let Some(Some(identity)) = self.links.remove(&link_id) {
                    if self.seated.get(identity.as_bytes()) == Some(&link_id) {
                        self.seated.remove(identity.as_bytes());
                        self.gate.forget(&identity);
                        self.chat_gate.forget(&identity);
                        self.names.remove(identity.as_bytes());
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
                        send_message(&self.handle, link_id, &self.names_message());
                    }
                    Ok(LanMessage::Packet(packet)) => self.forward(identity, Some(link_id), packet),
                    Ok(LanMessage::Name(name)) => {
                        if self.names.get(identity.as_bytes()) != Some(&name) {
                            self.names.insert(*identity.as_bytes(), name);
                            self.publish_names();
                        }
                    }
                    Ok(LanMessage::Chat(text)) => {
                        if self.chat_gate.allow(identity, Instant::now()) {
                            self.say(identity, text);
                        } else {
                            debug!(sender = ?identity.as_bytes(), "chat over the member's rate; dropped");
                        }
                    }
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
                        send_message(&self.handle, link_id, &self.names_message());
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
        self.publish_names();
    }

    /// The names of members seated now, in table order.
    fn names_message(&self) -> LanMessage {
        LanMessage::Names(
            self.table
                .members()
                .iter()
                .filter_map(|m| {
                    self.names.get(m.identity.as_bytes()).map(|n| (m.identity, n.clone()))
                })
                .collect(),
        )
    }

    fn publish_names(&self) {
        let message = self.names_message();
        for &link_id in self.seated.values() {
            send_message(&self.handle, link_id, &message);
        }
        if let LanMessage::Names(names) = message {
            self.view.lock().expect("room view lock").names = names;
        }
    }

    /// One chat line from `from` — a seated member, or the host — to everyone,
    /// the sender included: getting it back is the sender's receipt.
    fn say(&mut self, from: IdentityHash, text: String) {
        let message = LanMessage::ChatFrom { from, text: text.clone() };
        for &link_id in self.seated.values() {
            send_message(&self.handle, link_id, &message);
        }
        self.chat.lock().expect("chat lock").push(from, text);
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
    /// What this member asks to be called.
    name: Option<String>,
    chat: Arc<Mutex<ChatLog>>,
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
        mut control: mpsc::UnboundedReceiver<Control>,
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
                Some(c) = control.recv() => match c {
                    Control::SetName(name) => {
                        self.name = Some(name);
                        self.send_name();
                    }
                    Control::Chat(text) => {
                        if let Some(link_id) = self.link {
                            send_message(&self.handle, link_id, &LanMessage::Chat(text));
                        }
                    }
                },
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
                let mut view = self.view.lock().expect("room view lock");
                view.own_address = None;
                view.chat = false;
            }
            BridgeEvent::LinkData { link_id, bytes } if self.link == Some(link_id) => {
                match LanMessage::decode(&bytes) {
                    Ok(LanMessage::Members { epoch, subnet, members }) => {
                        let mut view = self.view.lock().expect("room view lock");
                        let was = view.own_address;
                        view.apply(epoch, subnet, members);
                        let seated_now = was.is_none() && view.own_address.is_some();
                        if seated_now {
                            if let Some(address) = view.own_address {
                                info!(%address, "seated in the LAN room");
                            }
                        }
                        drop(view);
                        if seated_now {
                            self.send_name();
                        }
                    }
                    Ok(LanMessage::Names(names)) => {
                        let missing = {
                            let mut view = self.view.lock().expect("room view lock");
                            view.chat = true;
                            view.names = names;
                            self.name.as_deref().is_some_and(|n| {
                                view.own_address.is_some()
                                    && view.name_of(&self.own_identity) != Some(n)
                            })
                        };
                        // Lost, or the host restarted: say it again. The
                        // host's table is the receipt.
                        if missing {
                            self.send_name();
                        }
                    }
                    Ok(LanMessage::ChatFrom { from, text }) => {
                        self.chat.lock().expect("chat lock").push(from, text);
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

    fn send_name(&self) {
        let seated = self.view.lock().expect("room view lock").own_address.is_some();
        if let (Some(link_id), Some(name), true) = (self.link, &self.name, seated) {
            send_message(&self.handle, link_id, &LanMessage::Name(name.clone()));
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
