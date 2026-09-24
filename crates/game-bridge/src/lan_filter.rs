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
    policy: LanPolicy,
    flows: HashMap<FlowKey, Instant>,
    fragments: HashMap<FragmentKey, Instant>,
}

impl LanFilter {
    pub fn new(policy: LanPolicy) -> Self {
        Self { policy, flows: HashMap::new(), fragments: HashMap::new() }
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
                    let allowed = self.policy.declares(proto, dport);
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
        let allowed = self.policy.declares(proto, dport)
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
}
