//! Mode 3, virtual LAN: what a member's adapter lets in and out
//! (`PLAN.md` §14.2, step 2).
//!
//! Hamachi's worst property is that joining a network puts every service on
//! your machine in reach of every other member. A room here is not that: the
//! pump between the adapter and the room runs every packet past a
//! [`LanFilter`], which admits inbound traffic only to the ports the game
//! declared and to replies on flows this side opened.
//!
//! It runs in the pump, in user space, rather than as firewall rules: it needs
//! no privilege, it is the same code on every platform, and it cannot be left
//! behind on a machine after the room is gone.
//!
//! The same port list gates what this side **broadcasts**. An operating
//! system beacons on its own — mDNS, NetBIOS, SSDP, LLMNR — and none of it is
//! the game. A broadcast to a port the game did not declare never reaches the
//! room, which is storm control and privacy at once.
//!
//! # Rules a later change could quietly break
//!
//! - **An undeclared inbound port is dropped, not merely unlisted.** A reply is
//!   let in only because this side sent the request: the flow is keyed on both
//!   ends' address *and* port, so a member cannot reach a local service by
//!   sending from a port somebody once talked to.
//! - **A non-first fragment carries no ports**, so it is admitted only if the
//!   first fragment of the same datagram was. Otherwise fragments would be a
//!   way past every other rule.
//! - **A refused packet is recorded, not just dropped** ([`RefusedLog`]). The
//!   first real NFSU2 race hung on joining with nothing to show why; a pack's
//!   ports come from somebody's captures and can be incomplete, and the only
//!   witness to a missing one is this filter. Operating systems' own chatter
//!   is left out, so what is listed is the game's.
//! - **`any` is the pack's to declare, loudly.** A game with unpredictable
//!   ports needs it; the launcher must say so before joining (`PLAN.md` §14.2).

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::lan::RoomSubnet;

const PROTO_ICMP: u8 = 1;
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;

/// How long a flow this side opened keeps admitting replies after its last
/// packet either way.
pub const FLOW_IDLE: Duration = Duration::from_secs(180);
/// How long a first fragment vouches for the rest of its datagram.
pub const FRAGMENT_WINDOW: Duration = Duration::from_secs(30);
/// Flows remembered at once. A game has a handful; past this, expired ones are
/// swept, and a flow that still does not fit simply gets no replies.
pub const MAX_FLOWS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanProto {
    Udp,
    Tcp,
}

/// One port a game uses on a LAN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LanPort {
    pub proto: LanProto,
    pub port: u16,
}

/// What a game declared about its LAN traffic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LanPolicy {
    /// Ports other members may reach, and the only ones this side broadcasts to.
    pub ports: Vec<LanPort>,
    /// Admit every inbound port and every broadcast. For a game whose ports are
    /// not predictable; the launcher warns before joining with it.
    pub any: bool,
}

impl LanPolicy {
    fn declares(&self, proto: LanProto, port: u16) -> bool {
        self.any || self.ports.iter().any(|p| p.proto == proto && p.port == port)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fragment {
    /// A whole datagram.
    Whole,
    /// The first piece of a fragmented one: ports present.
    First,
    /// A later piece: no transport header.
    Rest,
}

/// The fields of a packet the filter decides on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Parsed {
    src: Ipv4Addr,
    dst: Ipv4Addr,
    protocol: u8,
    id: u16,
    fragment: Fragment,
    /// `(source, destination)` for UDP and TCP, when the header is present.
    ports: Option<(u16, u16)>,
}

fn parse(packet: &[u8]) -> Option<Parsed> {
    let (src, dst) = crate::lan::ipv4_endpoints(packet)?;
    let ihl = (packet[0] & 0x0f) as usize * 4;
    if packet.len() < ihl {
        return None;
    }
    let id = u16::from_be_bytes([packet[4], packet[5]]);
    let flags_offset = u16::from_be_bytes([packet[6], packet[7]]);
    let more = flags_offset & 0x2000 != 0;
    let offset = flags_offset & 0x1fff;
    let fragment = match (offset, more) {
        (0, false) => Fragment::Whole,
        (0, true) => Fragment::First,
        _ => Fragment::Rest,
    };
    let protocol = packet[9];
    let ports = match (protocol, fragment) {
        (PROTO_UDP | PROTO_TCP, Fragment::Whole | Fragment::First) => {
            let t = packet.get(ihl..ihl + 4)?;
            Some((u16::from_be_bytes([t[0], t[1]]), u16::from_be_bytes([t[2], t[3]])))
        }
        _ => None,
    };
    Some(Parsed { src, dst, protocol, id, fragment, ports })
}

fn lan_proto(protocol: u8) -> Option<LanProto> {
    match protocol {
        PROTO_UDP => Some(LanProto::Udp),
        PROTO_TCP => Some(LanProto::Tcp),
        _ => None,
    }
}

/// A flow this side opened: protocol, its own port, the far address and port.
type FlowKey = (LanProto, u16, Ipv4Addr, u16);
/// The far address of a flow a broadcast opened: any member may answer it.
/// `0.0.0.0` is never a member's address, so it cannot collide with one.
const ANY_MEMBER: Ipv4Addr = Ipv4Addr::UNSPECIFIED;
/// A fragmented datagram: sender, IP id, protocol.
type FragmentKey = (Ipv4Addr, u16, u8);

/// Per-member packet filter, owned by the pump.
#[derive(Debug)]
pub struct LanFilter {
    /// The pack's own policy.
    base: LanPolicy,
    /// `base` plus the ports this machine's player allowed on top.
    policy: LanPolicy,
    /// Which [`ExtraPorts`] generation `policy` was built from.
    extra_generation: u64,
    /// The player's "still can't join" switch: every port from
    /// [`LOWEST_ALLOWABLE_PORT`] up, except operating systems' own chatter.
    any_high: bool,
    flows: HashMap<FlowKey, Instant>,
    fragments: HashMap<FragmentKey, Instant>,
}

impl LanFilter {
    pub fn new(policy: LanPolicy) -> Self {
        Self {
            base: policy.clone(),
            policy,
            extra_generation: 0,
            any_high: false,
            flows: HashMap::new(),
            fragments: HashMap::new(),
        }
    }

    /// Admit the ports `extra` holds on top of the pack's, if they changed
    /// since last asked. Cheap when nothing did: one atomic load.
    pub fn sync_extra(&mut self, extra: &ExtraPorts) {
        let generation = extra.generation();
        if generation == self.extra_generation {
            return;
        }
        let mut policy = self.base.clone();
        for port in extra.ports() {
            if !policy.ports.contains(&port) {
                policy.ports.push(port);
            }
        }
        self.policy = policy;
        self.any_high = extra.any_high();
        self.extra_generation = generation;
    }

    /// Whether `port` gets through: the pack's, the player's extras, or —
    /// with the switch on — any port a player may allow ([`is_allowable`]).
    fn admits(&self, proto: LanProto, port: u16) -> bool {
        self.policy.declares(proto, port) || (self.any_high && is_allowable(port))
    }

    /// Whether this side may send `packet` into the room. A unicast opens (or
    /// refreshes) a flow its replies are admitted on.
    pub fn outbound(&mut self, packet: &[u8], subnet: &RoomSubnet, now: Instant) -> bool {
        let Some(p) = parse(packet) else { return false };
        if subnet.is_broadcast(p.dst) {
            // Only the game's own beacons. A later fragment of a broadcast is
            // rare enough that sending it is not worth a table.
            return match (lan_proto(p.protocol), p.ports) {
                (Some(proto), Some((sport, dport))) => {
                    let allowed = self.admits(proto, dport);
                    if allowed {
                        // A search is a broadcast, and its answers are
                        // unicasts from whoever heard it — OpenTTD's LAN
                        // browser searches from a random port and is
                        // answered there. So the broadcast opens a flow whose
                        // far address is anyone in the room, still pinned to
                        // both ports. Linux's own firewall needs a helper for
                        // exactly this (`nf_conntrack_broadcast`).
                        self.remember_flow((proto, sport, ANY_MEMBER, dport), now);
                    }
                    allowed
                }
                (Some(_), None) => self.policy.any || p.fragment == Fragment::Rest,
                (None, _) => self.policy.any,
            };
        }
        if let (Some(proto), Some((sport, dport))) = (lan_proto(p.protocol), p.ports) {
            self.remember_flow((proto, sport, p.dst, dport), now);
        }
        true
    }

    /// Whether `packet`, delivered by the room, may reach this machine.
    pub fn inbound(&mut self, packet: &[u8], now: Instant) -> bool {
        let Some(p) = parse(packet) else { return false };
        if p.protocol == PROTO_ICMP {
            // Echo and the errors that make a failing connection fail fast.
            return true;
        }
        let key = (p.src, p.id, p.protocol);
        if p.fragment == Fragment::Rest {
            return self
                .fragments
                .get(&key)
                .is_some_and(|&t| now.duration_since(t) < FRAGMENT_WINDOW);
        }
        let Some(proto) = lan_proto(p.protocol) else { return self.policy.any };
        let Some((sport, dport)) = p.ports else { return false };
        let allowed = self.admits(proto, dport)
            || self.refresh_flow((proto, dport, p.src, sport), now)
            || self.refresh_flow((proto, dport, ANY_MEMBER, sport), now);
        if allowed && p.fragment == Fragment::First {
            self.fragments.retain(|_, t| now.duration_since(*t) < FRAGMENT_WINDOW);
            self.fragments.insert(key, now);
        }
        allowed
    }

    /// Whether `key` is a live flow, keeping it alive if so.
    fn refresh_flow(&mut self, key: FlowKey, now: Instant) -> bool {
        match self.flows.get_mut(&key) {
            Some(seen) if now.duration_since(*seen) < FLOW_IDLE => {
                *seen = now;
                true
            }
            _ => false,
        }
    }

    fn remember_flow(&mut self, key: FlowKey, now: Instant) {
        if !self.flows.contains_key(&key) && self.flows.len() >= MAX_FLOWS {
            self.flows.retain(|_, t| now.duration_since(*t) < FLOW_IDLE);
            if self.flows.len() >= MAX_FLOWS {
                return;
            }
        }
        self.flows.insert(key, now);
    }
}

// ---------------------------------------------------------------------------
// Ports a player allowed on top of the pack's.

/// The lowest port a player is offered to allow. Below it live a machine's
/// own services — SSH, file sharing, mail — which no LAN game uses, and which
/// a room member sending to them must not get opened by one careless click.
pub const LOWEST_ALLOWABLE_PORT: u16 = 1024;

/// Services above 1024 that hand whoever reaches them a machine or its data:
/// remote desktops, remote shells, databases, a container daemon. No LAN game
/// uses one, and the reason 1024 is the floor applies to them unchanged — a
/// member who sends to RDP would otherwise see it named as a "missing port"
/// one Allow click away, and the last-resort switch would open it outright.
const REMOTE_SERVICES: &[u16] = &[
    1433, 1434, // SQL Server
    1521, // Oracle
    2049, // NFS
    2375, 2376, // Docker's API
    3306, // MySQL / MariaDB
    3389, // Remote Desktop
    5432, // PostgreSQL
    5900, 5901, 5902, 5903, // VNC
    5985, 5986, // WinRM (PowerShell remoting)
    6379, // Redis
    9200, // Elasticsearch
    11211, // memcached
    27017, // MongoDB
];

/// Whether a player may let `port` through on top of a pack's, by Allow or by
/// the last-resort switch: at or above [`LOWEST_ALLOWABLE_PORT`], and neither
/// an operating system's own chatter nor a remote-access or database service.
/// A pack may still declare any of them; this gates only what a click opens.
pub fn is_allowable(port: u16) -> bool {
    port >= LOWEST_ALLOWABLE_PORT && !OS_CHATTER.contains(&port) && !REMOTE_SERVICES.contains(&port)
}

/// Ports this machine's player allowed for the room's game on top of its
/// pack's, after the room named them as missing (`RefusedLog`). Shared by the
/// session and the pump, which picks a change up on its next packet.
#[derive(Debug, Default)]
pub struct ExtraPorts {
    ports: std::sync::Mutex<Vec<LanPort>>,
    any_high: std::sync::atomic::AtomicBool,
    generation: std::sync::atomic::AtomicU64,
}

impl ExtraPorts {
    /// Replace the extra ports. Any port that is not [`is_allowable`] is
    /// refused here, whoever asks.
    pub fn set(&self, ports: Vec<LanPort>) -> Result<(), u16> {
        if let Some(p) = ports.iter().find(|p| !is_allowable(p.port)) {
            return Err(p.port);
        }
        let mut deduped: Vec<LanPort> = Vec::new();
        for p in ports {
            if !deduped.contains(&p) {
                deduped.push(p);
            }
        }
        *self.ports.lock().expect("extra ports lock") = deduped;
        self.generation.fetch_add(1, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    pub fn ports(&self) -> Vec<LanPort> {
        self.ports.lock().expect("extra ports lock").clone()
    }

    /// The last-resort switch: admit every [`is_allowable`] port for this
    /// game, for when the room cannot tell which one is missing.
    pub fn set_any_high(&self, on: bool) {
        self.any_high.store(on, std::sync::atomic::Ordering::Release);
        self.generation.fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    pub fn any_high(&self) -> bool {
        self.any_high.load(std::sync::atomic::Ordering::Acquire)
    }

    fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------
// What the filter refused, so a missing port names itself.

/// Ports operating systems chatter on by themselves — name lookup, network
/// browsing, discovery, time, DHCP. None is a game's, and listing them would
/// bury the one that is.
const OS_CHATTER: &[u16] = &[53, 67, 68, 123, 137, 138, 139, 445, 1900, 3702, 5353, 5355];

/// Ports at or above this are a stack's own ephemeral choice (Linux starts at
/// 32768, Windows at 49152), so the fixed side of such a packet is the
/// other end.
const EPHEMERAL_FROM: u16 = 32768;

/// Which way a refused packet was going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DropDirection {
    /// Another member sent it to this machine.
    Inbound,
    /// This machine broadcast it.
    OutboundBroadcast,
}

/// One kind of packet the filter refused, and how often.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub direction: DropDirection,
    pub proto: LanProto,
    /// This machine's port: the destination inbound, the source outbound.
    pub local_port: u16,
    /// The other end's address and port: the sender inbound, the broadcast
    /// address and destination port outbound.
    pub peer: Ipv4Addr,
    pub peer_port: u16,
    pub count: u64,
    pub last: Instant,
}

impl Refused {
    /// The port a pack would have to declare to let this through: the fixed
    /// end of it. Outbound, the broadcast's destination; inbound, this side's
    /// port unless that is an ephemeral one, when it is the sender's — a
    /// reply from a game's fixed port to a random one.
    pub fn suggested_port(&self) -> u16 {
        match self.direction {
            DropDirection::OutboundBroadcast => self.peer_port,
            DropDirection::Inbound if self.local_port >= EPHEMERAL_FROM => self.peer_port,
            DropDirection::Inbound => self.local_port,
        }
    }

    /// One line a player can paste into a bug report.
    pub fn describe(&self) -> String {
        let proto = match self.proto {
            LanProto::Udp => "UDP",
            LanProto::Tcp => "TCP",
        };
        match self.direction {
            DropDirection::Inbound => format!(
                "{proto} from {}:{} to this computer's port {} ({} packet{})",
                self.peer,
                self.peer_port,
                self.local_port,
                self.count,
                if self.count == 1 { "" } else { "s" }
            ),
            DropDirection::OutboundBroadcast => format!(
                "{proto} broadcast from this computer's port {} to port {} ({} packet{})",
                self.local_port,
                self.peer_port,
                self.count,
                if self.count == 1 { "" } else { "s" }
            ),
        }
    }
}

/// How many kinds of refused packet are remembered; a game has a handful,
/// and a flood of distinct ones is not worth more memory.
const REFUSED_CAP: usize = 64;
/// How long one is shown after its last packet.
pub const REFUSED_REMEMBER: Duration = Duration::from_secs(600);

type RefusedKey = (DropDirection, LanProto, u16, Ipv4Addr, u16);

/// What this member's filter refused, kept on the session for the room pane
/// and the room check. Written by the pump, read by anyone.
#[derive(Debug, Default)]
pub struct RefusedLog {
    inner: std::sync::Mutex<HashMap<RefusedKey, Refused>>,
}

impl RefusedLog {
    /// The filter refused `packet` going `direction`. Returns the record when
    /// this kind is new, so the caller can say so once.
    pub fn saw(&self, direction: DropDirection, packet: &[u8], now: Instant) -> Option<Refused> {
        let p = parse(packet)?;
        let proto = lan_proto(p.protocol)?;
        let (sport, dport) = p.ports?;
        let (local_port, peer, peer_port) = match direction {
            DropDirection::Inbound => (dport, p.src, sport),
            DropDirection::OutboundBroadcast => (sport, p.dst, dport),
        };
        if OS_CHATTER.contains(&dport) || OS_CHATTER.contains(&sport) {
            return None;
        }
        let key = (direction, proto, local_port, peer, peer_port);
        let mut log = self.inner.lock().expect("refused log lock");
        log.retain(|_, r| now.duration_since(r.last) < REFUSED_REMEMBER);
        if let Some(r) = log.get_mut(&key) {
            r.count += 1;
            r.last = now;
            return None;
        }
        if log.len() >= REFUSED_CAP {
            return None;
        }
        let r = Refused { direction, proto, local_port, peer, peer_port, count: 1, last: now };
        log.insert(key, r.clone());
        Some(r)
    }

    /// Forget what was refused on `ports`: the player just allowed them.
    pub fn forget(&self, ports: &[LanPort]) {
        let mut log = self.inner.lock().expect("refused log lock");
        log.retain(|_, r| {
            !ports.iter().any(|p| p.proto == r.proto && p.port == r.suggested_port())
        });
    }

    /// Everything refused in the last [`REFUSED_REMEMBER`], most packets first.
    pub fn refused(&self, now: Instant) -> Vec<Refused> {
        let mut log = self.inner.lock().expect("refused log lock");
        log.retain(|_, r| now.duration_since(r.last) < REFUSED_REMEMBER);
        let mut out: Vec<Refused> = log.values().cloned().collect();
        out.sort_by(|a, b| b.count.cmp(&a.count).then(a.local_port.cmp(&b.local_port)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: Ipv4Addr = Ipv4Addr::new(198, 19, 1, 1);
    const PEER: Ipv4Addr = Ipv4Addr::new(198, 19, 2, 2);

    fn ip(
        src: Ipv4Addr,
        dst: Ipv4Addr,
        protocol: u8,
        id: u16,
        frag: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut p = vec![0u8; 20];
        p[0] = 0x45;
        p[4..6].copy_from_slice(&id.to_be_bytes());
        p[6..8].copy_from_slice(&frag.to_be_bytes());
        p[9] = protocol;
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        p.extend_from_slice(payload);
        p
    }

    fn udp(src: Ipv4Addr, sport: u16, dst: Ipv4Addr, dport: u16) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(&sport.to_be_bytes());
        t.extend_from_slice(&dport.to_be_bytes());
        t.extend_from_slice(&[0, 8, 0, 0]);
        ip(src, dst, PROTO_UDP, 1, 0, &t)
    }

    fn tcp(src: Ipv4Addr, sport: u16, dst: Ipv4Addr, dport: u16) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(&sport.to_be_bytes());
        t.extend_from_slice(&dport.to_be_bytes());
        t.extend_from_slice(&[0u8; 16]);
        ip(src, dst, PROTO_TCP, 1, 0, &t)
    }

    fn game() -> LanFilter {
        LanFilter::new(LanPolicy {
            ports: vec![LanPort { proto: LanProto::Udp, port: 9999 }],
            any: false,
        })
    }

    #[test]
    fn a_declared_port_is_reachable() {
        let mut f = game();
        assert!(f.inbound(&udp(PEER, 5555, ME, 9999), Instant::now()));
    }

    /// The Hamachi property this exists to refuse: a member reaching a
    /// service on this machine the game never asked to expose.
    #[test]
    fn an_undeclared_port_is_not_reachable() {
        let mut f = game();
        let now = Instant::now();
        assert!(!f.inbound(&tcp(PEER, 40000, ME, 22), now), "ssh");
        assert!(!f.inbound(&tcp(PEER, 40000, ME, 445), now), "smb");
        assert!(!f.inbound(&udp(PEER, 40000, ME, 9998), now), "the port next to the game's");
        assert!(
            !f.inbound(&tcp(PEER, 40000, ME, 9999), now),
            "the game's number, the wrong protocol"
        );
    }

    #[test]
    fn a_reply_to_a_flow_this_side_opened_is_admitted() {
        let mut f = game();
        let now = Instant::now();
        assert!(f.outbound(&tcp(ME, 50000, PEER, 7000), &RoomSubnet::default(), now));
        assert!(f.inbound(&tcp(PEER, 7000, ME, 50000), now));
        assert!(!f.inbound(&tcp(PEER, 7000, ME, 50001), now), "another local port is not the flow");
        assert!(
            !f.inbound(&tcp(PEER, 7001, ME, 50000), now),
            "another remote port is not the flow"
        );
        let other = Ipv4Addr::new(198, 19, 3, 3);
        assert!(!f.inbound(&tcp(other, 7000, ME, 50000), now), "another member is not the flow");
    }

    /// OpenTTD's LAN search, found by `tests/lan_openttd.rs` against the real
    /// game: a broadcast from a random port to the game's port, answered by a
    /// unicast back to that random port.
    #[test]
    fn a_reply_to_a_broadcast_this_side_sent_is_admitted() {
        let mut f = game();
        let s = RoomSubnet::default();
        let now = Instant::now();
        assert!(f.outbound(&udp(ME, 41234, s.broadcast(), 9999), &s, now));
        assert!(f.inbound(&udp(PEER, 9999, ME, 41234), now), "the answer to the search");
        let other = Ipv4Addr::new(198, 19, 3, 3);
        assert!(f.inbound(&udp(other, 9999, ME, 41234), now), "any member may answer a broadcast");
        assert!(
            !f.inbound(&udp(PEER, 9998, ME, 41234), now),
            "but only from the port it was sent to"
        );
        assert!(!f.inbound(&udp(PEER, 9999, ME, 41235), now), "and only to the port it came from");
        // An undeclared broadcast was never sent, so it opens nothing.
        assert!(!f.outbound(&udp(ME, 5353, s.broadcast(), 5353), &s, now));
        assert!(!f.inbound(&udp(PEER, 5353, ME, 5353), now));
    }

    #[test]
    fn a_flow_expires_when_idle() {
        let mut f = game();
        let now = Instant::now();
        f.outbound(&udp(ME, 50000, PEER, 7000), &RoomSubnet::default(), now);
        assert!(!f.inbound(&udp(PEER, 7000, ME, 50000), now + FLOW_IDLE + Duration::from_secs(1)));
    }

    #[test]
    fn only_the_games_own_broadcasts_leave_this_machine() {
        let mut f = game();
        let s = RoomSubnet::default();
        let now = Instant::now();
        assert!(
            f.outbound(&udp(ME, 5555, Ipv4Addr::BROADCAST, 9999), &s, now),
            "the game's beacon"
        );
        assert!(!f.outbound(&udp(ME, 5353, Ipv4Addr::new(224, 0, 0, 251), 5353), &s, now), "mDNS");
        assert!(!f.outbound(&udp(ME, 137, s.broadcast(), 137), &s, now), "NetBIOS");
        assert!(
            !f.outbound(&udp(ME, 1900, Ipv4Addr::new(239, 255, 255, 250), 1900), &s, now),
            "SSDP"
        );
        assert!(
            f.outbound(&udp(ME, 5555, PEER, 1234), &s, now),
            "unicast is never gated on the way out"
        );
    }

    #[test]
    fn any_admits_everything_the_room_carries() {
        let mut f = LanFilter::new(LanPolicy { ports: Vec::new(), any: true });
        let now = Instant::now();
        assert!(f.inbound(&tcp(PEER, 1, ME, 22), now));
        assert!(f.outbound(&udp(ME, 1, Ipv4Addr::BROADCAST, 2), &RoomSubnet::default(), now));
    }

    #[test]
    fn a_later_fragment_rides_only_on_an_admitted_first_one() {
        let mut f = game();
        let now = Instant::now();
        // A first fragment to the game's port, then the rest of that datagram.
        let mut first = udp(PEER, 5555, ME, 9999);
        first[4..6].copy_from_slice(&7u16.to_be_bytes());
        first[6..8].copy_from_slice(&0x2000u16.to_be_bytes());
        assert!(f.inbound(&first, now));
        assert!(f.inbound(&ip(PEER, ME, PROTO_UDP, 7, 185, b"rest"), now));
        // A later fragment of a datagram nobody admitted: no ports to check,
        // so it must not be a way in.
        assert!(!f.inbound(&ip(PEER, ME, PROTO_UDP, 8, 185, b"rest"), now));
    }

    #[test]
    fn icmp_is_admitted_and_garbage_is_not() {
        let mut f = game();
        let now = Instant::now();
        assert!(f.inbound(&ip(PEER, ME, PROTO_ICMP, 1, 0, &[8, 0, 0, 0]), now));
        assert!(!f.inbound(b"not a packet", now));
        assert!(!f.inbound(&ip(PEER, ME, PROTO_UDP, 1, 0, &[0, 1]), now), "a UDP header cut short");
    }

    /// The NFSU2 case this exists for: after discovery the game answers from
    /// ports it never declared, the filter drops it, and nothing said so.
    #[test]
    fn a_refused_packet_is_recorded_and_says_which_port_the_pack_lacks() {
        let log = RefusedLog::default();
        let now = Instant::now();
        let first = log.saw(DropDirection::Inbound, &udp(PEER, 3660, ME, 3660), now).unwrap();
        assert_eq!(first.suggested_port(), 3660);
        assert!(log.saw(DropDirection::Inbound, &udp(PEER, 3660, ME, 3660), now).is_none(), "said once");
        let r = log.refused(now);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].count, 2);
        assert_eq!(r[0].describe(), "UDP from 198.19.2.2:3660 to this computer's port 3660 (2 packets)");
        assert!(log.refused(now + REFUSED_REMEMBER).is_empty(), "and forgotten later");
    }

    /// A reply from a game's fixed port to a random one: the fixed one is what
    /// a pack can declare.
    #[test]
    fn a_reply_to_an_ephemeral_port_suggests_the_senders_port() {
        let log = RefusedLog::default();
        let r = log.saw(DropDirection::Inbound, &tcp(PEER, 3290, ME, 51000), Instant::now()).unwrap();
        assert_eq!((r.proto, r.suggested_port()), (LanProto::Tcp, 3290));
        let s = RoomSubnet::default();
        let b = log
            .saw(DropDirection::OutboundBroadcast, &udp(ME, 51001, s.broadcast(), 7777), Instant::now())
            .unwrap();
        assert_eq!(b.suggested_port(), 7777);
        assert!(b.describe().contains("broadcast"), "{}", b.describe());
    }

    #[test]
    fn an_operating_systems_own_chatter_is_not_blamed_on_the_pack() {
        let log = RefusedLog::default();
        let now = Instant::now();
        let s = RoomSubnet::default();
        for port in [137, 138, 1900, 5353, 5355, 445] {
            assert!(log.saw(DropDirection::OutboundBroadcast, &udp(ME, port, s.broadcast(), port), now).is_none());
            assert!(log.saw(DropDirection::Inbound, &tcp(PEER, 50000, ME, port), now).is_none());
        }
        assert!(log.saw(DropDirection::Inbound, &ip(PEER, ME, PROTO_ICMP, 1, 0, &[8, 0, 0, 0]), now).is_none());
        assert!(log.refused(now).is_empty());
    }

    /// One click on "Allow" opens exactly the port the room named, and only
    /// for the game — and Undo shuts it again.
    #[test]
    fn a_port_the_player_allowed_is_admitted_and_undo_shuts_it() {
        let mut f = game();
        let extra = ExtraPorts::default();
        let now = Instant::now();
        assert!(!f.inbound(&udp(PEER, 3660, ME, 3660), now));
        extra.set(vec![LanPort { proto: LanProto::Udp, port: 3660 }]).unwrap();
        f.sync_extra(&extra);
        assert!(f.inbound(&udp(PEER, 3660, ME, 3660), now));
        assert!(f.inbound(&udp(PEER, 5555, ME, 9999), now), "the pack's own port still is");
        assert!(!f.inbound(&tcp(PEER, 3660, ME, 3660), now), "the other protocol is not");
        extra.set(Vec::new()).unwrap();
        f.sync_extra(&extra);
        assert!(!f.inbound(&udp(PEER, 3660, ME, 3660), now));
        assert!(f.inbound(&udp(PEER, 5555, ME, 9999), now), "undo never touches the pack's");
    }

    #[test]
    fn a_system_port_can_never_be_allowed() {
        let extra = ExtraPorts::default();
        assert_eq!(extra.set(vec![LanPort { proto: LanProto::Tcp, port: 22 }]), Err(22));
        assert_eq!(extra.set(vec![LanPort { proto: LanProto::Tcp, port: 445 }]), Err(445));
        assert_eq!(extra.set(vec![LanPort { proto: LanProto::Tcp, port: 3389 }]), Err(3389), "RDP");
        assert_eq!(extra.set(vec![LanPort { proto: LanProto::Tcp, port: 5900 }]), Err(5900), "VNC");
        assert!(extra.ports().is_empty());
    }

    /// The switch for when the room cannot name the missing port: every high
    /// port, but never a system service's and never the OS's own chatter.
    #[test]
    fn the_last_resort_switch_opens_high_ports_only() {
        let mut f = game();
        let extra = ExtraPorts::default();
        let now = Instant::now();
        let s = RoomSubnet::default();
        extra.set_any_high(true);
        f.sync_extra(&extra);
        assert!(f.inbound(&udp(PEER, 51000, ME, 52000), now), "an ephemeral pair");
        assert!(f.inbound(&tcp(PEER, 40000, ME, 3290), now));
        assert!(!f.inbound(&tcp(PEER, 40000, ME, 22), now), "ssh stays shut");
        assert!(!f.inbound(&tcp(PEER, 40000, ME, 445), now), "smb stays shut");
        assert!(!f.inbound(&tcp(PEER, 40000, ME, 3389), now), "RDP is above 1024 and stays shut");
        assert!(!f.inbound(&tcp(PEER, 40000, ME, 5985), now), "WinRM stays shut");
        assert!(!f.inbound(&tcp(PEER, 40000, ME, 3306), now), "a database stays shut");
        assert!(!f.outbound(&udp(ME, 5353, Ipv4Addr::new(224, 0, 0, 251), 5353), &s, now), "mDNS stays home");
        assert!(!f.outbound(&udp(ME, 1900, s.broadcast(), 1900), &s, now), "SSDP stays home");
        assert!(f.outbound(&udp(ME, 51000, s.broadcast(), 7777), &s, now), "a game's own broadcast goes");
        extra.set_any_high(false);
        f.sync_extra(&extra);
        assert!(!f.inbound(&udp(PEER, 51000, ME, 52000), now));
    }
}
