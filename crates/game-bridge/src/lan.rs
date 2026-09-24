//! Mode 3, virtual LAN: the room protocol (`PLAN.md` §14, step 1).
//!
//! A LAN room is a host and its members. Every member holds one Link, to the
//! host, and everything — the member table, broadcasts, and packets between two
//! members — rides those Links. This module is the protocol's pure half: the
//! member table and its addresses, the messages, which member a packet goes to,
//! and the gate that keeps a beaconing game from flooding the room. The session
//! that drives it over a real node is `lan_session.rs`; the virtual adapter
//! that produces the packets is step 2.
//!
//! # Why a star over Links, and not a GROUP
//!
//! `MODES.md` put broadcasts on a GROUP destination. The engine does not carry
//! one past the first hop: `maybe_forward` forwards only `Single` destinations
//! (`prns-core/src/routing/ingress/forward.rs`), and ingress drops GROUP data
//! that arrived with more than one hop
//! (`routing/ingress/dispatch/mod.rs:275`, `NON_TRANSPORTED_DATA_MAX_RECEIVED_HOPS
//! = 1` at `routing/ingress/outcome.rs:73`). Two players who reach each other
//! through a TCP hub are two hops apart, so a GROUP broadcast would simply
//! never arrive — on exactly the Internet setup this was built for. A GROUP key
//! can also only be registered when the node is built
//! (`prns-runtime/core/src/runtime/node/assembly.rs:370`), and a member learns
//! the key only after joining. The host fanning a broadcast out over Links has
//! neither problem, and a Link carries 1967 bytes where a GROUP carries 383.
//!
//! # Rules a later change could quietly break
//!
//! - **The host believes the IPv4 header's source only if it is the sender's
//!   own address.** [`route`] drops anything else. Without it one member could
//!   send as another, and every game's notion of who said what goes with it.
//! - **A member's address comes from its identity, not from its request.**
//!   There is nothing to ask for: [`MemberTable::admit`] derives it, so a
//!   member cannot claim somebody else's address by asking.
//! - **Unknown message types are ignored, never an error**, so a newer peer's
//!   message is not a reason to drop an older peer's room.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use prns_core::identity::IdentityHash;

/// The aspect a room's destination carries, beside the pack's `app_name`.
/// Distinct from `server`, so one identity can host a server and a room.
pub const ASPECT_LAN: &str = "lan";

/// The aspect a member's own destination carries. A member is never linked
/// to; the destination exists so its node holds the identity it identifies
/// with.
pub const ASPECT_LAN_MEMBER: &str = "lan-member";

/// The room's `MODES.md` mode, as the §3.3 announce record carries it.
pub const TRANSPORT_MODE_LAN: u8 = 3;

/// Protocol version carried by `Hello`.
pub const LAN_PROTOCOL_VERSION: u8 = 1;

/// The largest IP packet a room carries. Matches the virtual adapter's MTU
/// (`PLAN.md` §14.1): one IP packet inside one link packet, never fragmented
/// across two.
pub const LAN_MTU: usize = 1400;

/// Most members one room holds, the host included. The member table has to
/// fit one link packet; at 20 bytes an entry this is far inside it.
pub const MAX_MEMBERS: usize = 32;

/// The default room subnet: `198.19.0.0/16`.
///
/// Not `100.64.0.0/10`, which the first draft of `PLAN.md` §14 named:
/// Tailscale routes that whole range to its own adapter, so a player running
/// it would have the room's route shadowed. `198.18.0.0/15` is reserved for
/// benchmarking (RFC 2544) and is not a home LAN's range; its lower half is
/// taken by some proxy tools' fake-IP mode, so the upper half it is. The host
/// sends the prefix in every `Members`, so this is a default and not a wire
/// constant.
pub const DEFAULT_ROOM_PREFIX: Ipv4Addr = Ipv4Addr::new(198, 19, 0, 0);
pub const DEFAULT_ROOM_PREFIX_LEN: u8 = 16;

/// Every room subnet lies inside this range, whatever a host sends
/// ([`RoomSubnet::new`]).
pub const ALLOWED_RANGE: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 0);
pub const ALLOWED_RANGE_LEN: u8 = 15;

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/// A room's subnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomSubnet {
    pub prefix: Ipv4Addr,
    pub prefix_len: u8,
}

impl Default for RoomSubnet {
    fn default() -> Self {
        Self { prefix: DEFAULT_ROOM_PREFIX, prefix_len: DEFAULT_ROOM_PREFIX_LEN }
    }
}

impl RoomSubnet {
    /// A subnet with at least 8 host bits, so a room of [`MAX_MEMBERS`] always
    /// has room, and at most 16, so a derived address is two identity bytes —
    /// and always inside `198.18.0.0/15`.
    ///
    /// **The range is not the host's to choose.** The subnet arrives from the
    /// room's host and becomes a route on every member's machine. A host that
    /// could send `192.168.1.0/24` would put that route on a member's adapter
    /// and pull their real LAN — printer, router, NAS — into the room. So a
    /// subnet outside the benchmarking range is refused at decode, and the
    /// adapter checks again (`lan_adapter.rs`).
    pub fn new(prefix: Ipv4Addr, prefix_len: u8) -> Option<Self> {
        if !(16..=24).contains(&prefix_len) {
            return None;
        }
        let mask = Self::mask_for(prefix_len);
        let prefix = u32::from(prefix) & mask;
        if prefix & Self::mask_for(ALLOWED_RANGE_LEN) != u32::from(ALLOWED_RANGE) {
            return None;
        }
        Some(Self { prefix: Ipv4Addr::from(prefix), prefix_len })
    }

    fn mask_for(prefix_len: u8) -> u32 {
        u32::MAX << (32 - prefix_len as u32)
    }

    fn mask(&self) -> u32 {
        Self::mask_for(self.prefix_len)
    }

    pub fn contains(&self, addr: Ipv4Addr) -> bool {
        u32::from(addr) & self.mask() == u32::from(self.prefix)
    }

    /// The subnet's directed broadcast address.
    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.prefix) | !self.mask())
    }

    /// Whether `addr` reaches every member: the limited broadcast, the room's
    /// directed broadcast, or IPv4 multicast. LAN games use all three.
    pub fn is_broadcast(&self, addr: Ipv4Addr) -> bool {
        addr.is_broadcast() || addr == self.broadcast() || addr.is_multicast()
    }

    /// Whether `addr` is one a member may be given.
    ///
    /// Beyond the subnet's own network and broadcast addresses, a host part
    /// ending in `.0` or `.255` is refused too: plenty of old games compute a
    /// `/24` broadcast from their own address whatever the real mask is, and a
    /// member sitting on one would receive every beacon as if addressed to it.
    fn is_assignable(&self, addr: Ipv4Addr) -> bool {
        let last = addr.octets()[3];
        self.contains(addr) && last != 0 && last != 255
    }

    fn with_host(&self, host: u32) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.prefix) | (host & !self.mask()))
    }
}

/// The address `identity` asks for on its `attempt`-th try.
///
/// Two identity bytes per attempt, so the first eight tries need no hashing:
/// an identity hash is already uniformly distributed. Deterministic, so a
/// member that leaves and comes back is given the same address whenever it is
/// still free — which is what a game that remembered a peer by address needs.
pub fn derive_address(
    subnet: &RoomSubnet,
    identity: &IdentityHash,
    attempt: usize,
) -> Option<Ipv4Addr> {
    let bytes = identity.as_bytes();
    let i = attempt.checked_mul(2)?;
    let pair = bytes.get(i..i + 2)?;
    Some(subnet.with_host(u16::from_be_bytes([pair[0], pair[1]]) as u32))
}

// ---------------------------------------------------------------------------
// The member table
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub identity: IdentityHash,
    pub address: Ipv4Addr,
}

/// Why the host would not seat someone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The room is at its member limit.
    Full,
    /// The host's allowlist does not name this identity.
    NotAllowed,
    /// No free address. Unreachable below a few thousand members; kept so the
    /// table can never hand out a duplicate.
    NoAddress,
    /// A code this build does not know, from a newer host.
    Other(u8),
}

impl Refusal {
    fn to_byte(self) -> u8 {
        match self {
            Self::Full => 1,
            Self::NotAllowed => 2,
            Self::NoAddress => 3,
            Self::Other(b) => b,
        }
    }

    fn from_byte(b: u8) -> Self {
        match b {
            1 => Self::Full,
            2 => Self::NotAllowed,
            3 => Self::NoAddress,
            other => Self::Other(other),
        }
    }
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Full => write!(f, "the room is full"),
            Self::NotAllowed => write!(f, "the room's host does not allow this identity"),
            Self::NoAddress => write!(f, "the room has no free address"),
            Self::Other(b) => write!(f, "refused by the room's host (code {b})"),
        }
    }
}

/// The host's record of who is in the room. The host alone writes it; members
/// hold the copy the last `Members` gave them.
#[derive(Debug, Clone)]
pub struct MemberTable {
    subnet: RoomSubnet,
    capacity: usize,
    members: Vec<Member>,
    /// Bumped on every change, so a member can tell a stale copy from a new one.
    epoch: u32,
}

impl MemberTable {
    pub fn new(subnet: RoomSubnet, capacity: usize) -> Self {
        Self { subnet, capacity: capacity.clamp(1, MAX_MEMBERS), members: Vec::new(), epoch: 0 }
    }

    pub fn subnet(&self) -> RoomSubnet {
        self.subnet
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn members(&self) -> &[Member] {
        &self.members
    }

    pub fn address_of(&self, identity: &IdentityHash) -> Option<Ipv4Addr> {
        self.members.iter().find(|m| &m.identity == identity).map(|m| m.address)
    }

    pub fn identity_at(&self, address: Ipv4Addr) -> Option<IdentityHash> {
        self.members.iter().find(|m| m.address == address).map(|m| m.identity)
    }

    /// Seat `identity`, or say why not. Seating someone already seated is not
    /// a change: it returns their address and leaves the epoch alone, so a
    /// member reconnecting on a new link keeps its address.
    ///
    /// Collisions are arbitrated here and nowhere else: the first identity to
    /// hold an address keeps it, and the newcomer takes its next derivation,
    /// then the first free address in the subnet.
    pub fn admit(&mut self, identity: IdentityHash) -> Result<Ipv4Addr, Refusal> {
        if let Some(addr) = self.address_of(&identity) {
            return Ok(addr);
        }
        if self.members.len() >= self.capacity {
            return Err(Refusal::Full);
        }
        let taken = |a: Ipv4Addr| self.members.iter().any(|m| m.address == a);
        let derived = (0..8)
            .filter_map(|n| derive_address(&self.subnet, &identity, n))
            .find(|&a| self.subnet.is_assignable(a) && !taken(a));
        let address = match derived {
            Some(a) => a,
            None => {
                let hosts = !self.subnet.mask();
                (1..hosts)
                    .map(|h| self.subnet.with_host(h))
                    .find(|&a| self.subnet.is_assignable(a) && !taken(a))
                    .ok_or(Refusal::NoAddress)?
            }
        };
        self.members.push(Member { identity, address });
        self.epoch = self.epoch.wrapping_add(1);
        Ok(address)
    }

    /// Unseat `identity`. Returns whether anything changed.
    pub fn remove(&mut self, identity: &IdentityHash) -> bool {
        let before = self.members.len();
        self.members.retain(|m| &m.identity != identity);
        let changed = self.members.len() != before;
        if changed {
            self.epoch = self.epoch.wrapping_add(1);
        }
        changed
    }

    /// The `Members` message describing this table.
    pub fn snapshot(&self) -> LanMessage {
        LanMessage::Members {
            epoch: self.epoch,
            subnet: self.subnet,
            members: self.members.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

const MSG_HELLO: u8 = 0x01;
const MSG_MEMBERS: u8 = 0x02;
const MSG_REFUSED: u8 = 0x03;
const MSG_PACKET: u8 = 0x04;

/// One message on a member's link to its host. One per link packet.
///
/// Every message is safe to lose and safe to repeat. A member says `Hello`
/// until a `Members` naming it arrives; the host answers each `Hello` and
/// resends `Members` on a timer and on every change. Nothing waits for an
/// acknowledgement, because link data under `ProveNone` has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanMessage {
    /// Member → host: seat me. Sent after the link identifies.
    Hello { version: u8 },
    /// Host → member: who is in the room, and at which address.
    Members { epoch: u32, subnet: RoomSubnet, members: Vec<Member> },
    /// Host → member: not seated, and why. The host closes the link after.
    Refused(Refusal),
    /// Either way: one IPv4 packet. Its own header says where it is from and
    /// where it goes, so the message adds nothing that could disagree with it.
    Packet(Vec<u8>),
    /// A type this build does not know. Ignored, never an error.
    Unknown(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanDecodeError {
    Empty,
    Truncated,
    TooManyMembers(usize),
    BadSubnet,
    PacketTooLarge(usize),
}

impl core::fmt::Display for LanDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty LAN message"),
            Self::Truncated => write!(f, "LAN message is truncated"),
            Self::TooManyMembers(n) => {
                write!(f, "member table of {n} is over the {MAX_MEMBERS} limit")
            }
            Self::BadSubnet => write!(f, "room subnet is outside what this build accepts"),
            Self::PacketTooLarge(n) => {
                write!(f, "packet of {n} bytes is over the {LAN_MTU}-byte MTU")
            }
        }
    }
}

impl std::error::Error for LanDecodeError {}

const MEMBER_ENTRY_LEN: usize = 16 + 4;

impl LanMessage {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Hello { version } => vec![MSG_HELLO, *version],
            Self::Members { epoch, subnet, members } => {
                let members = &members[..members.len().min(MAX_MEMBERS)];
                let mut buf = Vec::with_capacity(11 + members.len() * MEMBER_ENTRY_LEN);
                buf.push(MSG_MEMBERS);
                buf.extend_from_slice(&epoch.to_be_bytes());
                buf.extend_from_slice(&subnet.prefix.octets());
                buf.push(subnet.prefix_len);
                buf.push(members.len() as u8);
                for m in members {
                    buf.extend_from_slice(m.identity.as_bytes());
                    buf.extend_from_slice(&m.address.octets());
                }
                buf
            }
            Self::Refused(r) => vec![MSG_REFUSED, r.to_byte()],
            Self::Packet(p) => {
                let mut buf = Vec::with_capacity(1 + p.len());
                buf.push(MSG_PACKET);
                buf.extend_from_slice(p);
                buf
            }
            Self::Unknown(t) => vec![*t],
        }
    }

    pub fn decode(buf: &[u8]) -> Result<Self, LanDecodeError> {
        let (&kind, rest) = buf.split_first().ok_or(LanDecodeError::Empty)?;
        match kind {
            MSG_HELLO => {
                Ok(Self::Hello { version: *rest.first().ok_or(LanDecodeError::Truncated)? })
            }
            MSG_MEMBERS => {
                if rest.len() < 10 {
                    return Err(LanDecodeError::Truncated);
                }
                let epoch = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                let prefix = Ipv4Addr::new(rest[4], rest[5], rest[6], rest[7]);
                let subnet = RoomSubnet::new(prefix, rest[8]).ok_or(LanDecodeError::BadSubnet)?;
                let count = rest[9] as usize;
                if count > MAX_MEMBERS {
                    return Err(LanDecodeError::TooManyMembers(count));
                }
                let entries = &rest[10..];
                if entries.len() < count * MEMBER_ENTRY_LEN {
                    return Err(LanDecodeError::Truncated);
                }
                let members = entries
                    .chunks_exact(MEMBER_ENTRY_LEN)
                    .take(count)
                    .map(|e| {
                        let mut id = [0u8; 16];
                        id.copy_from_slice(&e[..16]);
                        Member {
                            identity: IdentityHash::new(id),
                            address: Ipv4Addr::new(e[16], e[17], e[18], e[19]),
                        }
                    })
                    .collect();
                Ok(Self::Members { epoch, subnet, members })
            }
            MSG_REFUSED => Ok(Self::Refused(Refusal::from_byte(
                *rest.first().ok_or(LanDecodeError::Truncated)?,
            ))),
            MSG_PACKET => {
                if rest.len() > LAN_MTU {
                    return Err(LanDecodeError::PacketTooLarge(rest.len()));
                }
                Ok(Self::Packet(rest.to_vec()))
            }
            other => Ok(Self::Unknown(other)),
        }
    }
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Source and destination out of an IPv4 header, or `None` if `packet` is not
/// one. Nothing past the header is read.
pub fn ipv4_endpoints(packet: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr)> {
    if packet.len() < 20 || packet[0] >> 4 != 4 || (packet[0] & 0x0f) < 5 {
        return None;
    }
    let src = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
    let dst = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
    Some((src, dst))
}

/// Where the host sends a packet a member gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// To every member except the sender.
    Broadcast,
    /// To this one member.
    Unicast(IdentityHash),
    Drop(DropReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    NotIpv4,
    TooLarge,
    /// The sender is not seated.
    UnknownSender,
    /// The header claims a source that is not the sender's address.
    SpoofedSource,
    /// Addressed to nobody in the room. The adapter carries no default route,
    /// so this is a packet for somewhere the room does not go.
    UnknownDestination,
    /// Addressed to the sender itself.
    ToSelf,
}

/// Decide where `packet`, from the member `sender`, goes.
///
/// Pure, so the rules that keep one member from speaking as another are
/// tested without a mesh.
pub fn route(table: &MemberTable, sender: &IdentityHash, packet: &[u8]) -> Route {
    if packet.len() > LAN_MTU {
        return Route::Drop(DropReason::TooLarge);
    }
    let Some((src, dst)) = ipv4_endpoints(packet) else {
        return Route::Drop(DropReason::NotIpv4);
    };
    let Some(own) = table.address_of(sender) else {
        return Route::Drop(DropReason::UnknownSender);
    };
    if src != own {
        return Route::Drop(DropReason::SpoofedSource);
    }
    if table.subnet().is_broadcast(dst) {
        return Route::Broadcast;
    }
    match table.identity_at(dst) {
        Some(id) if &id == sender => Route::Drop(DropReason::ToSelf),
        Some(id) => Route::Unicast(id),
        None => Route::Drop(DropReason::UnknownDestination),
    }
}

// ---------------------------------------------------------------------------
// Storm control
// ---------------------------------------------------------------------------

/// Broadcasts one member may send per second, sustained.
pub const BROADCAST_RATE_PER_SEC: u32 = 20;
/// Broadcasts one member may send back to back.
pub const BROADCAST_BURST: u32 = 40;
/// A broadcast identical to the same member's previous one, sent this soon
/// after it, is a repeat nobody needs twice.
pub const BROADCAST_DEDUPE_WINDOW: Duration = Duration::from_millis(250);

/// The host's gate on broadcasts, per member (`PLAN.md` §14.2: storm control
/// ships in the first commit).
///
/// A broadcast costs the host one send per member, so it is what a beaconing
/// game makes expensive. Unicast is not gated: it costs one send, and a game's
/// real traffic must not be rationed by a rule written for its beacons.
#[derive(Debug, Default)]
pub struct BroadcastGate {
    // Keyed by the hash's bytes: `IdentityHash` does not implement `Hash`.
    per_member: HashMap<[u8; 16], GateState>,
}

#[derive(Debug)]
struct GateState {
    tokens: f64,
    refilled_at: Instant,
    last_packet: Vec<u8>,
    last_sent_at: Option<Instant>,
}

impl BroadcastGate {
    /// Whether `sender` may broadcast `packet` at `now`. Counts it if so.
    pub fn allow(&mut self, sender: IdentityHash, packet: &[u8], now: Instant) -> bool {
        let state = self.per_member.entry(*sender.as_bytes()).or_insert_with(|| GateState {
            tokens: BROADCAST_BURST as f64,
            refilled_at: now,
            last_packet: Vec::new(),
            last_sent_at: None,
        });
        let elapsed = now.saturating_duration_since(state.refilled_at).as_secs_f64();
        state.tokens =
            (state.tokens + elapsed * BROADCAST_RATE_PER_SEC as f64).min(BROADCAST_BURST as f64);
        state.refilled_at = now;

        let repeat = state.last_packet == packet
            && state
                .last_sent_at
                .is_some_and(|t| now.saturating_duration_since(t) < BROADCAST_DEDUPE_WINDOW);
        if repeat || state.tokens < 1.0 {
            return false;
        }
        state.tokens -= 1.0;
        state.last_packet.clear();
        state.last_packet.extend_from_slice(packet);
        state.last_sent_at = Some(now);
        true
    }

    /// Forget a member that left, so the gate does not grow with churn.
    pub fn forget(&mut self, member: &IdentityHash) {
        self.per_member.remove(member.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> IdentityHash {
        let mut b = [0u8; 16];
        for (i, x) in b.iter_mut().enumerate() {
            *x = n.wrapping_mul(31).wrapping_add(i as u8 * 7);
        }
        IdentityHash::new(b)
    }

    /// A minimal IPv4 header, which is all the room reads.
    fn packet(src: Ipv4Addr, dst: Ipv4Addr, body: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; 20];
        p[0] = 0x45;
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        p.extend_from_slice(body);
        p
    }

    #[test]
    fn an_address_is_derived_from_the_identity_and_stays_in_the_subnet() {
        let subnet = RoomSubnet::default();
        let a = derive_address(&subnet, &id(1), 0).unwrap();
        assert_eq!(a, derive_address(&subnet, &id(1), 0).unwrap(), "deterministic");
        assert!(subnet.contains(a));
        assert_ne!(a, derive_address(&subnet, &id(1), 1).unwrap(), "each attempt differs");
        assert!(derive_address(&subnet, &id(1), 8).is_none(), "16 bytes is eight attempts");
    }

    #[test]
    fn a_member_that_leaves_and_returns_gets_its_address_back() {
        let mut t = MemberTable::new(RoomSubnet::default(), 8);
        let first = t.admit(id(1)).unwrap();
        t.admit(id(2)).unwrap();
        assert!(t.remove(&id(1)));
        assert_eq!(t.admit(id(1)).unwrap(), first);
    }

    #[test]
    fn seating_someone_already_seated_changes_nothing() {
        let mut t = MemberTable::new(RoomSubnet::default(), 8);
        let a = t.admit(id(1)).unwrap();
        let epoch = t.epoch();
        assert_eq!(t.admit(id(1)).unwrap(), a);
        assert_eq!(t.epoch(), epoch, "a reconnect is not a membership change");
        assert_eq!(t.members().len(), 1);
    }

    /// Two identities whose first derivation collides: the one seated first
    /// keeps the address, the newcomer takes its next one. No duplicate, ever.
    #[test]
    fn a_collision_goes_to_whoever_was_seated_first() {
        let subnet = RoomSubnet::default();
        let mut a = [0x11u8; 16];
        a[0] = 0x12;
        a[1] = 0x34;
        let mut b = [0x22u8; 16];
        b[0] = 0x12;
        b[1] = 0x34;
        let (a, b) = (IdentityHash::new(a), IdentityHash::new(b));
        assert_eq!(derive_address(&subnet, &a, 0), derive_address(&subnet, &b, 0));

        let mut t = MemberTable::new(subnet, 8);
        let first = t.admit(a).unwrap();
        let second = t.admit(b).unwrap();
        assert_eq!(first, derive_address(&subnet, &a, 0).unwrap());
        assert_ne!(first, second);
        assert_eq!(second, derive_address(&subnet, &b, 1).unwrap());
    }

    #[test]
    fn no_member_is_given_a_dot_zero_or_dot_255_address() {
        let subnet = RoomSubnet::default();
        // Every derivation of this identity lands on .0 or .255.
        let mut bytes = [0u8; 16];
        for pair in bytes.chunks_exact_mut(2) {
            pair[1] = 0xff;
        }
        let mut t = MemberTable::new(subnet, 8);
        let a = t.admit(IdentityHash::new(bytes)).unwrap();
        assert!(subnet.contains(a));
        assert!(a.octets()[3] != 0 && a.octets()[3] != 255, "{a}");
    }

    #[test]
    fn a_full_room_refuses_rather_than_overfilling() {
        let mut t = MemberTable::new(RoomSubnet::default(), 2);
        t.admit(id(1)).unwrap();
        t.admit(id(2)).unwrap();
        assert_eq!(t.admit(id(3)), Err(Refusal::Full));
    }

    #[test]
    fn the_capacity_never_exceeds_what_one_members_message_carries() {
        let t = MemberTable::new(RoomSubnet::default(), 10_000);
        assert_eq!(t.capacity, MAX_MEMBERS);
        let full = LanMessage::Members {
            epoch: 1,
            subnet: RoomSubnet::default(),
            members: (0..MAX_MEMBERS as u8)
                .map(|n| Member { identity: id(n), address: Ipv4Addr::LOCALHOST })
                .collect(),
        };
        assert!(full.encode().len() <= crate::LINK_PLAINTEXT_CAP);
    }

    #[test]
    fn a_subnet_outside_16_to_24_bits_is_refused() {
        assert!(RoomSubnet::new(Ipv4Addr::new(198, 18, 0, 0), 8).is_none());
        assert!(RoomSubnet::new(Ipv4Addr::new(198, 18, 0, 0), 30).is_none());
        let s = RoomSubnet::new(Ipv4Addr::new(198, 18, 8, 7), 24).unwrap();
        assert_eq!(s.prefix, Ipv4Addr::new(198, 18, 8, 0), "host bits are masked off");
        assert_eq!(s.broadcast(), Ipv4Addr::new(198, 18, 8, 255));
    }

    /// A host must not be able to route a member's real LAN into the room.
    #[test]
    fn a_host_cannot_put_a_members_real_lan_into_the_room() {
        for (prefix, len) in [
            ((192, 168, 1, 0), 24),
            ((10, 0, 0, 0), 16),
            ((198, 20, 0, 0), 16),
            ((198, 16, 0, 0), 16),
        ] {
            let (a, b, c, d) = prefix;
            assert!(RoomSubnet::new(Ipv4Addr::new(a, b, c, d), len).is_none(), "{prefix:?}/{len}");
        }
        assert!(RoomSubnet::new(Ipv4Addr::new(198, 18, 0, 0), 16).is_some());
        let mut bytes = RoomSubnet::default().snapshot_for_test();
        bytes[5..9].copy_from_slice(&[192, 168, 1, 0]);
        bytes[9] = 24;
        assert_eq!(LanMessage::decode(&bytes), Err(LanDecodeError::BadSubnet));
    }

    #[test]
    fn every_message_round_trips() {
        let mut t = MemberTable::new(RoomSubnet::default(), 8);
        t.admit(id(1)).unwrap();
        t.admit(id(2)).unwrap();
        for m in [
            LanMessage::Hello { version: LAN_PROTOCOL_VERSION },
            t.snapshot(),
            LanMessage::Refused(Refusal::NotAllowed),
            LanMessage::Refused(Refusal::Other(77)),
            LanMessage::Packet(packet(Ipv4Addr::new(1, 2, 3, 4), Ipv4Addr::BROADCAST, b"hi")),
        ] {
            assert_eq!(LanMessage::decode(&m.encode()).unwrap(), m);
        }
    }

    #[test]
    fn an_unknown_message_type_is_ignored_not_an_error() {
        assert_eq!(LanMessage::decode(&[0x7f, 1, 2, 3]), Ok(LanMessage::Unknown(0x7f)));
    }

    #[test]
    fn a_truncated_message_never_panics() {
        let mut t = MemberTable::new(RoomSubnet::default(), 8);
        t.admit(id(1)).unwrap();
        let bytes = t.snapshot().encode();
        for n in 0..bytes.len() {
            let _ = LanMessage::decode(&bytes[..n]);
        }
        assert_eq!(LanMessage::decode(&bytes[..bytes.len() - 1]), Err(LanDecodeError::Truncated));
        assert_eq!(LanMessage::decode(&[]), Err(LanDecodeError::Empty));
    }

    #[test]
    fn a_packet_over_the_mtu_is_refused_at_decode() {
        let mut bytes = vec![MSG_PACKET];
        bytes.extend(std::iter::repeat_n(0u8, LAN_MTU + 1));
        assert_eq!(LanMessage::decode(&bytes), Err(LanDecodeError::PacketTooLarge(LAN_MTU + 1)));
    }

    impl RoomSubnet {
        fn snapshot_for_test(self) -> Vec<u8> {
            MemberTable::new(self, 8).snapshot().encode()
        }
    }

    fn room() -> (MemberTable, Ipv4Addr, Ipv4Addr) {
        let mut t = MemberTable::new(RoomSubnet::default(), 8);
        let a = t.admit(id(1)).unwrap();
        let b = t.admit(id(2)).unwrap();
        (t, a, b)
    }

    #[test]
    fn a_packet_to_another_member_goes_to_that_member() {
        let (t, a, b) = room();
        assert_eq!(route(&t, &id(1), &packet(a, b, b"x")), Route::Unicast(id(2)));
    }

    #[test]
    fn every_kind_of_broadcast_reaches_the_room() {
        let (t, a, _) = room();
        for dst in [Ipv4Addr::BROADCAST, t.subnet().broadcast(), Ipv4Addr::new(239, 255, 255, 250)]
        {
            assert_eq!(route(&t, &id(1), &packet(a, dst, b"x")), Route::Broadcast, "{dst}");
        }
    }

    /// The rule that matters most here: a member cannot speak as another.
    #[test]
    fn a_member_cannot_send_as_another_member() {
        let (t, _, b) = room();
        assert_eq!(
            route(&t, &id(1), &packet(b, Ipv4Addr::BROADCAST, b"x")),
            Route::Drop(DropReason::SpoofedSource)
        );
    }

    #[test]
    fn what_the_room_does_not_carry_is_dropped() {
        let (t, a, _) = room();
        let outside = Ipv4Addr::new(8, 8, 8, 8);
        assert_eq!(
            route(&t, &id(1), &packet(a, outside, b"")),
            Route::Drop(DropReason::UnknownDestination)
        );
        assert_eq!(route(&t, &id(1), &packet(a, a, b"")), Route::Drop(DropReason::ToSelf));
        assert_eq!(route(&t, &id(9), &packet(a, a, b"")), Route::Drop(DropReason::UnknownSender));
        assert_eq!(route(&t, &id(1), b"not an ip packet at all"), Route::Drop(DropReason::NotIpv4));
        let mut v6 = packet(a, a, b"");
        v6[0] = 0x60;
        assert_eq!(route(&t, &id(1), &v6), Route::Drop(DropReason::NotIpv4));
        let big = packet(a, Ipv4Addr::BROADCAST, &vec![0u8; LAN_MTU]);
        assert_eq!(route(&t, &id(1), &big), Route::Drop(DropReason::TooLarge));
    }

    #[test]
    fn the_gate_drops_a_repeated_beacon_inside_the_window() {
        let mut g = BroadcastGate::default();
        let now = Instant::now();
        assert!(g.allow(id(1), b"beacon", now));
        assert!(!g.allow(id(1), b"beacon", now + Duration::from_millis(100)));
        assert!(
            g.allow(id(1), b"other", now + Duration::from_millis(100)),
            "a different packet is not a repeat"
        );
        assert!(g.allow(id(1), b"beacon", now + BROADCAST_DEDUPE_WINDOW * 2));
        assert!(g.allow(id(2), b"beacon", now), "dedupe is per member");
    }

    #[test]
    fn the_gate_holds_a_member_to_its_rate_after_the_burst() {
        let mut g = BroadcastGate::default();
        let now = Instant::now();
        let sent = (0..1000u32).filter(|n| g.allow(id(1), &n.to_be_bytes(), now)).count();
        assert_eq!(sent, BROADCAST_BURST as usize);
        let later = now + Duration::from_secs(1);
        let refilled =
            (0..1000u32).filter(|n| g.allow(id(1), &(n + 5000).to_be_bytes(), later)).count();
        assert_eq!(refilled, BROADCAST_RATE_PER_SEC as usize);
    }
}
