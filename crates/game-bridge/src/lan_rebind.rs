//! Mode 3, virtual LAN: reaching a game that listens on the wrong address.
//!
//! Wine binds a game's sockets to what it believes is the machine's own
//! address, and that is the adapter carrying the default route — the Wi-Fi or
//! the Ethernet, never the room's. Measured with Need for Speed: Underground 2
//! under Proton: its discovery socket was on `0.0.0.0:9999`, so the race
//! showed up in every member's list, and its game socket was on
//! `192.168.32.203:9900`, so every join to the room address was refused.
//!
//! The pump already holds every room packet in user space, so it translates:
//! a packet this machine sends into the room from one of its other addresses
//! leaves with the room address as source, and a packet the room delivers
//! for the room address goes to the address the game is actually on. Rules:
//!
//! - **Only what the game is on is translated.** An inbound packet is
//!   rewritten only for a declared port some local socket holds on another
//!   address while nothing holds it on the room address ([`survey`]), or for
//!   a flow this machine opened from another address. A program bound to the
//!   room address, or to every address, is never touched.
//! - **This machine's addresses only, never another host's.** A source is
//!   translated only if it is one of this machine's own addresses. A machine
//!   that forwards (any Docker host does) also routes other hosts' packets into
//!   the room adapter, and translating those would bridge the room onto the
//!   home network.
//! - **Behind the filter on the way in, before it on the way out.** The filter,
//!   the probe log and the connection watch see room addresses only, and the
//!   translation never widens what a member may reach: only ports the room
//!   already admits ever arrive here.
//! - **Wine or native, Linux or Windows.** Nothing here knows what a game
//!   is: it looks at where a socket is bound. Linux delivers a packet for any
//!   of its addresses on any interface; Windows only does once the room
//!   adapter is weak-host, which `lan_adapter/windows.rs` sets on that
//!   adapter alone. The pump drops any room packet not addressed to this
//!   member, so neither lets the room reach another address directly.
//! - **The survey never listens.** It *binds* a socket, without listening, to
//!   learn whether an address and port are taken, and binds the room address
//!   only once another address is known to be taken — so it cannot steal a
//!   port from a game starting at the same moment unless that game binds the
//!   room address while already holding another.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::lan_filter::{LanPort, LanProto};

const PROTO_ICMP: u8 = 1;
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;

/// How long a translated flow is remembered after its last packet.
const FLOW_IDLE: Duration = Duration::from_secs(300);
/// The most translated flows remembered at once.
const MAX_FLOWS: usize = 4096;
/// How long the later pieces of a fragmented datagram may follow the first.
const FRAGMENT_WINDOW: Duration = Duration::from_secs(5);

/// A flow this machine opened from another address: protocol, its own port,
/// the far address and port.
type FlowKey = (LanProto, u16, Ipv4Addr, u16);

/// One of the game's ports, held on an address other than the room's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rebound {
    pub port: LanPort,
    /// The address the game is actually listening on.
    pub address: Ipv4Addr,
}

/// What [`survey`] found: this machine's other addresses, and which of the
/// game's ports are held on one of them instead of on the room's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Survey {
    pub locals: Vec<Ipv4Addr>,
    pub rebound: Vec<Rebound>,
}

/// The pump's translation state.
#[derive(Debug, Default)]
pub struct Rebinder {
    survey: Survey,
    flows: HashMap<FlowKey, (Ipv4Addr, Instant)>,
    fragments: HashMap<(Ipv4Addr, u16, u8), (Ipv4Addr, Instant)>,
}

impl Rebinder {
    /// Take a new survey. Returns whether what is translated changed.
    pub fn update(&mut self, survey: Survey) -> bool {
        let changed = survey.rebound != self.survey.rebound;
        self.survey = survey;
        changed
    }

    pub fn rebound(&self) -> &[Rebound] {
        &self.survey.rebound
    }

    /// A packet this machine sent into the room adapter. If it left from
    /// another of this machine's addresses, give it the room address and
    /// remember the flow, so its replies find their way back.
    pub fn outbound(&mut self, packet: &mut [u8], room: Ipv4Addr, now: Instant) -> bool {
        let Some((src, dst)) = crate::lan::ipv4_endpoints(packet) else { return false };
        if src == room || !self.survey.locals.contains(&src) {
            return false;
        }
        if let Some((proto, sport, dport)) = transport(packet) {
            if !self.flows.contains_key(&(proto, sport, dst, dport))
                && self.flows.len() >= MAX_FLOWS
            {
                self.flows.retain(|_, (_, t)| now.duration_since(*t) < FLOW_IDLE);
                if self.flows.len() >= MAX_FLOWS {
                    return rewrite(packet, Field::Src, room);
                }
            }
            self.flows.insert((proto, sport, dst, dport), (src, now));
        }
        rewrite(packet, Field::Src, room)
    }

    /// A packet the room delivered, already admitted by the filter. Returns
    /// the packet to deliver instead, if the game it is for listens on another
    /// address.
    pub fn inbound(&mut self, packet: &[u8], room: Ipv4Addr, now: Instant) -> Option<Vec<u8>> {
        let (src, dst) = crate::lan::ipv4_endpoints(packet)?;
        if dst != room {
            return None;
        }
        let protocol = packet[9];
        let id = u16::from_be_bytes([packet[4], packet[5]]);
        let flags_offset = u16::from_be_bytes([packet[6], packet[7]]);
        let (more, offset) = (flags_offset & 0x2000 != 0, flags_offset & 0x1fff);
        let to = if offset != 0 {
            let &(to, t) = self.fragments.get(&(src, id, protocol))?;
            (now.duration_since(t) < FRAGMENT_WINDOW).then_some(to)?
        } else if protocol == PROTO_ICMP {
            // An error about a packet this machine sent: what it quotes is that
            // packet, from the room address, which is the flow to look up.
            let (proto, sport, inner_dst, dport) = quoted_transport(packet)?;
            self.target(proto, sport, inner_dst, dport, now)?
        } else {
            let (proto, sport, dport) = transport(packet)?;
            self.target(proto, dport, src, sport, now)?
        };
        if offset == 0 && more {
            self.fragments.retain(|_, (_, t)| now.duration_since(*t) < FRAGMENT_WINDOW);
            self.fragments.insert((src, id, protocol), (to, now));
        }
        let mut out = packet.to_vec();
        rewrite(&mut out, Field::Dst, to).then_some(out)
    }

    /// Where a packet for `local_port` from `peer:peer_port` goes: back to the
    /// address that opened the flow, else to the address the game listens on.
    fn target(
        &mut self,
        proto: LanProto,
        local_port: u16,
        peer: Ipv4Addr,
        peer_port: u16,
        now: Instant,
    ) -> Option<Ipv4Addr> {
        if let Some((to, seen)) = self.flows.get_mut(&(proto, local_port, peer, peer_port)) {
            if now.duration_since(*seen) < FLOW_IDLE {
                *seen = now;
                return Some(*to);
            }
        }
        self.survey
            .rebound
            .iter()
            .find(|r| r.port.proto == proto && r.port.port == local_port)
            .map(|r| r.address)
    }
}

/// `(protocol, source port, destination port)` of a whole datagram or its
/// first piece.
fn transport(packet: &[u8]) -> Option<(LanProto, u16, u16)> {
    let ihl = (packet[0] & 0x0f) as usize * 4;
    if u16::from_be_bytes([packet[6], packet[7]]) & 0x1fff != 0 {
        return None;
    }
    let proto = match packet[9] {
        PROTO_TCP => LanProto::Tcp,
        PROTO_UDP => LanProto::Udp,
        _ => return None,
    };
    let t = packet.get(ihl..ihl + 4)?;
    Some((proto, u16::from_be_bytes([t[0], t[1]]), u16::from_be_bytes([t[2], t[3]])))
}

/// What an ICMP error quotes: `(protocol, source port, destination address,
/// destination port)` of the packet it is about.
fn quoted_transport(packet: &[u8]) -> Option<(LanProto, u16, Ipv4Addr, u16)> {
    let ihl = (packet[0] & 0x0f) as usize * 4;
    let icmp = packet.get(ihl..)?;
    if icmp.len() < 8 || !is_icmp_error(icmp[0]) {
        return None;
    }
    let inner = &icmp[8..];
    let (_, inner_dst) = crate::lan::ipv4_endpoints(inner)?;
    let (proto, sport, dport) = transport(inner)?;
    Some((proto, sport, inner_dst, dport))
}

/// Destination unreachable, source quench, redirect, time exceeded,
/// parameter problem: the ICMP messages that quote a packet.
fn is_icmp_error(kind: u8) -> bool {
    matches!(kind, 3 | 4 | 5 | 11 | 12)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Src,
    Dst,
}

impl Field {
    fn offset(self) -> usize {
        match self {
            Field::Src => 12,
            Field::Dst => 16,
        }
    }

    fn opposite(self) -> Field {
        match self {
            Field::Src => Field::Dst,
            Field::Dst => Field::Src,
        }
    }
}

/// Put `to` in `packet`'s source or destination address, and keep every
/// checksum that covers it right: the IP header's, TCP's and UDP's (both
/// cover the addresses through their pseudo-header), and for an ICMP error
/// the packet it quotes, whose addresses are this packet's swapped.
fn rewrite(packet: &mut [u8], field: Field, to: Ipv4Addr) -> bool {
    let ihl = (packet[0] & 0x0f) as usize * 4;
    if packet.len() < ihl.max(20) {
        return false;
    }
    let at = field.offset();
    let old: [u8; 4] = packet[at..at + 4].try_into().expect("four bytes");
    let new = to.octets();
    if old == new {
        return false;
    }
    packet[at..at + 4].copy_from_slice(&new);
    adjust_at(packet, 10, &old, &new);
    if u16::from_be_bytes([packet[6], packet[7]]) & 0x1fff != 0 {
        // A later piece: the transport header, and its checksum, are in the
        // first, which was rewritten on its own.
        return true;
    }
    match packet[9] {
        PROTO_TCP if packet.len() >= ihl + 18 => adjust_at(packet, ihl + 16, &old, &new),
        PROTO_UDP if packet.len() >= ihl + 8 => adjust_udp_at(packet, ihl + 6, &old, &new),
        PROTO_ICMP if packet.len() >= ihl + 8 + 20 && is_icmp_error(packet[ihl]) => {
            rewrite_quoted(packet, ihl, field.opposite(), &old, &new)
        }
        _ => {}
    }
    true
}

/// The packet an ICMP error quotes, starting eight bytes into the ICMP
/// message at `ihl`: its `field` holds the address this one's other field did.
/// Every change inside it is also a change to the ICMP checksum.
fn rewrite_quoted(packet: &mut [u8], ihl: usize, field: Field, old: &[u8; 4], new: &[u8; 4]) {
    let inner = ihl + 8;
    let at = inner + field.offset();
    if packet[at..at + 4] != old[..] || packet[inner] >> 4 != 4 {
        return;
    }
    let icmp_sum = ihl + 2;
    let change = |packet: &mut [u8], at: usize, bytes: &[u8]| {
        let before = packet[at..at + bytes.len()].to_vec();
        packet[at..at + bytes.len()].copy_from_slice(bytes);
        adjust_at(packet, icmp_sum, &before, bytes);
    };
    change(packet, at, new);
    let sum = checksum_adjust(read16(packet, inner + 10), old, new);
    change(packet, inner + 10, &sum.to_be_bytes());
    let inner_ihl = (packet[inner] & 0x0f) as usize * 4;
    let transport = inner + inner_ihl;
    if u16::from_be_bytes([packet[inner + 6], packet[inner + 7]]) & 0x1fff != 0 {
        return;
    }
    // Usually only the first eight bytes of the transport header are quoted:
    // UDP's checksum is among them, TCP's is not.
    match packet[inner + 9] {
        PROTO_UDP if packet.len() >= transport + 8 && read16(packet, transport + 6) != 0 => {
            let mut sum = checksum_adjust(read16(packet, transport + 6), old, new);
            if sum == 0 {
                sum = 0xffff;
            }
            change(packet, transport + 6, &sum.to_be_bytes());
        }
        PROTO_TCP if packet.len() >= transport + 18 => {
            let sum = checksum_adjust(read16(packet, transport + 16), old, new);
            change(packet, transport + 16, &sum.to_be_bytes());
        }
        _ => {}
    }
}

fn read16(packet: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([packet[at], packet[at + 1]])
}

fn adjust_at(packet: &mut [u8], at: usize, old: &[u8], new: &[u8]) {
    let sum = checksum_adjust(read16(packet, at), old, new);
    packet[at..at + 2].copy_from_slice(&sum.to_be_bytes());
}

/// UDP's checksum: zero means the sender computed none, and stays zero; a
/// computed zero is sent as all ones.
fn adjust_udp_at(packet: &mut [u8], at: usize, old: &[u8], new: &[u8]) {
    let sum = read16(packet, at);
    if sum == 0 {
        return;
    }
    let sum = match checksum_adjust(sum, old, new) {
        0 => 0xffff,
        s => s,
    };
    packet[at..at + 2].copy_from_slice(&sum.to_be_bytes());
}

/// RFC 1624's incremental update, `HC' = ~(~HC + ~m + m')`, for 16-bit words
/// `old` replaced by `new` at an even offset of whatever `sum` covers.
fn checksum_adjust(sum: u16, old: &[u8], new: &[u8]) -> u16 {
    let mut acc = u32::from(!sum);
    for (o, n) in old.chunks(2).zip(new.chunks(2)) {
        acc += u32::from(!u16::from_be_bytes([o[0], o[1]]));
        acc += u32::from(u16::from_be_bytes([n[0], n[1]]));
    }
    while acc >> 16 != 0 {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    !(acc as u16)
}

// ---------------------------------------------------------------------------
// Finding where the game is.

/// Which of `ports` some program on this machine holds on one of its other
/// addresses while nothing holds it on `room`.
pub fn survey(room: Ipv4Addr, ports: &[LanPort]) -> Survey {
    let locals: Vec<Ipv4Addr> =
        local_addresses().into_iter().filter(|a| *a != room && is_translatable(*a)).collect();
    let mut rebound = Vec::new();
    for &port in ports {
        let Some(&address) = locals.iter().find(|a| taken(port, **a)) else { continue };
        // Taken on another address; if the room address is taken too, the
        // game is on every address (or on the room's as well) and reaches the
        // room without help.
        if !taken(port, room) {
            rebound.push(Rebound { port, address });
        }
    }
    Survey { locals, rebound }
}

/// An address a packet from the room may be translated to: not loopback
/// (Linux drops a packet for `127/8` arriving on another interface), not
/// link-local, not a room's own range.
fn is_translatable(a: Ipv4Addr) -> bool {
    !a.is_loopback()
        && !a.is_unspecified()
        && !a.is_link_local()
        && !a.is_broadcast()
        && !a.is_multicast()
        && !in_room_range(a)
}

/// Whether `a` is inside the range every room's subnet comes from.
fn in_room_range(a: Ipv4Addr) -> bool {
    let mask = u32::MAX << (32 - crate::lan::ALLOWED_RANGE_LEN);
    u32::from(a) & mask == u32::from(crate::lan::ALLOWED_RANGE) & mask
}

/// Whether something on this machine already holds `port` on `address`.
/// Binds, never listens, and lets go at once.
fn taken(port: LanPort, address: Ipv4Addr) -> bool {
    let at = std::net::SocketAddr::from((address, port.port));
    let result = match port.proto {
        LanProto::Udp => std::net::UdpSocket::bind(at).map(drop),
        LanProto::Tcp => tokio::net::TcpSocket::new_v4().and_then(|s| s.bind(at)),
    };
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => true,
        // Windows answers a port held with SO_EXCLUSIVEADDRUSE (or reserved
        // by the system) with WSAEACCES. Read alike on both addresses, it
        // never starts a translation by itself.
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => true,
        _ => false,
    }
}

/// This machine's IPv4 addresses, every interface's.
#[cfg(target_os = "linux")]
pub fn local_addresses() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list freed below by freeifaddrs;
    // every node is read only while the list is alive.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut node = head;
        while !node.is_null() {
            let addr = (*node).ifa_addr;
            if !addr.is_null() && i32::from((*addr).sa_family) == libc::AF_INET {
                let sin = &*(addr as *const libc::sockaddr_in);
                let a = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !out.contains(&a) {
                    out.push(a);
                }
            }
            node = (*node).ifa_next;
        }
        libc::freeifaddrs(head);
    }
    out
}

/// This machine's IPv4 addresses, every interface's.
#[cfg(windows)]
pub fn local_addresses() -> Vec<Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetUnicastIpAddressTable, MIB_UNICASTIPADDRESS_TABLE,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;
    let mut out = Vec::new();
    let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = std::ptr::null_mut();
    // SAFETY: the table is allocated by GetUnicastIpAddressTable, read only
    // within its NumEntries while alive, and freed by FreeMibTable.
    unsafe {
        if GetUnicastIpAddressTable(AF_INET, &mut table) != 0 || table.is_null() {
            return out;
        }
        let rows =
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        for row in rows {
            let a = Ipv4Addr::from(u32::from_be(row.Address.Ipv4.sin_addr.S_un.S_addr));
            if !out.contains(&a) {
                out.push(a);
            }
        }
        FreeMibTable(table.cast());
    }
    out
}

/// No room adapter on any other system.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn local_addresses() -> Vec<Ipv4Addr> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOM: Ipv4Addr = Ipv4Addr::new(198, 19, 45, 221);
    const WIFI: Ipv4Addr = Ipv4Addr::new(192, 168, 32, 203);
    const PEER: Ipv4Addr = Ipv4Addr::new(198, 19, 2, 2);

    fn sum(bytes: &[u8]) -> u16 {
        let mut acc: u32 = 0;
        for c in bytes.chunks(2) {
            acc += u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]));
        }
        while acc >> 16 != 0 {
            acc = (acc & 0xffff) + (acc >> 16);
        }
        !(acc as u16)
    }

    fn ip_header(src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, payload: usize) -> Vec<u8> {
        let mut p = vec![0u8; 20];
        p[0] = 0x45;
        p[2..4].copy_from_slice(&((20 + payload) as u16).to_be_bytes());
        p[4..6].copy_from_slice(&0x1234u16.to_be_bytes());
        p[8] = 64;
        p[9] = protocol;
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        let s = sum(&p);
        p[10..12].copy_from_slice(&s.to_be_bytes());
        p
    }

    fn pseudo(p: &[u8], protocol: u8, len: usize) -> Vec<u8> {
        let mut v = p[12..20].to_vec();
        v.extend_from_slice(&[0, protocol]);
        v.extend_from_slice(&(len as u16).to_be_bytes());
        v
    }

    fn with_transport(
        src: Ipv4Addr,
        dst: Ipv4Addr,
        protocol: u8,
        sport: u16,
        dport: u16,
    ) -> Vec<u8> {
        let mut t = match protocol {
            PROTO_TCP => vec![0u8; 20],
            _ => vec![0u8; 8],
        };
        t[0..2].copy_from_slice(&sport.to_be_bytes());
        t[2..4].copy_from_slice(&dport.to_be_bytes());
        t.extend_from_slice(b"game data!");
        if protocol == PROTO_TCP {
            t[12] = 0x50;
        } else {
            let len = t.len() as u16;
            t[4..6].copy_from_slice(&len.to_be_bytes());
        }
        let mut p = ip_header(src, dst, protocol, t.len());
        let mut covered = pseudo(&p, protocol, t.len());
        covered.extend_from_slice(&t);
        let s = sum(&covered);
        let at = if protocol == PROTO_TCP { 16 } else { 6 };
        t[at..at + 2].copy_from_slice(&s.to_be_bytes());
        p.extend_from_slice(&t);
        p
    }

    /// Every checksum in `p` verifies from scratch.
    fn verifies(p: &[u8]) -> bool {
        if sum(&p[..20]) != 0 {
            return false;
        }
        let protocol = p[9];
        let t = &p[20..];
        match protocol {
            PROTO_TCP | PROTO_UDP => {
                if protocol == PROTO_UDP && t[6..8] == [0, 0] {
                    return true;
                }
                let mut covered = pseudo(p, protocol, t.len());
                covered.extend_from_slice(t);
                sum(&covered) == 0
            }
            PROTO_ICMP => sum(t) == 0 && verifies(&t[8..]),
            _ => true,
        }
    }

    fn icmp_unreachable_about(quoted: &[u8], from: Ipv4Addr, to: Ipv4Addr) -> Vec<u8> {
        let mut icmp = vec![3u8, 3, 0, 0, 0, 0, 0, 0];
        icmp.extend_from_slice(quoted);
        let s = sum(&icmp);
        icmp[2..4].copy_from_slice(&s.to_be_bytes());
        let mut p = ip_header(from, to, PROTO_ICMP, icmp.len());
        p.extend_from_slice(&icmp);
        p
    }

    fn rebinder() -> Rebinder {
        let mut r = Rebinder::default();
        r.update(Survey {
            locals: vec![WIFI],
            rebound: vec![Rebound {
                port: LanPort { proto: LanProto::Tcp, port: 9900 },
                address: WIFI,
            }],
        });
        r
    }

    #[test]
    fn the_incremental_checksum_matches_one_computed_from_scratch() {
        for protocol in [PROTO_TCP, PROTO_UDP] {
            let mut p = with_transport(WIFI, PEER, protocol, 9900, 51000);
            assert!(verifies(&p));
            assert!(rewrite(&mut p, Field::Src, ROOM));
            assert_eq!(crate::lan::ipv4_endpoints(&p), Some((ROOM, PEER)));
            assert!(verifies(&p), "protocol {protocol} after rewriting the source");
            assert!(rewrite(&mut p, Field::Dst, WIFI));
            assert!(verifies(&p), "protocol {protocol} after rewriting the destination");
        }
    }

    #[test]
    fn a_udp_datagram_without_a_checksum_keeps_none() {
        let mut p = with_transport(WIFI, PEER, PROTO_UDP, 9999, 9999);
        p[26..28].copy_from_slice(&[0, 0]);
        assert!(rewrite(&mut p, Field::Src, ROOM));
        assert_eq!(&p[26..28], &[0, 0]);
        assert_eq!(sum(&p[..20]), 0);
    }

    /// The case this exists for: a member connects to the room address, the
    /// game listens on the Wi-Fi address, and its answer leaves as the room's.
    #[test]
    fn a_join_to_the_room_address_reaches_a_game_on_the_wifi_address_and_back() {
        let mut r = rebinder();
        let now = Instant::now();
        let syn = with_transport(PEER, ROOM, PROTO_TCP, 51000, 9900);
        let delivered = r.inbound(&syn, ROOM, now).expect("rewritten for the game");
        assert_eq!(crate::lan::ipv4_endpoints(&delivered), Some((PEER, WIFI)));
        assert!(verifies(&delivered));

        let mut answer = with_transport(WIFI, PEER, PROTO_TCP, 9900, 51000);
        assert!(r.outbound(&mut answer, ROOM, now));
        assert_eq!(crate::lan::ipv4_endpoints(&answer), Some((ROOM, PEER)));
        assert!(verifies(&answer));
    }

    /// A game that also calls out from the Wi-Fi address (a client's callback
    /// ports): the replies to it come back to that address, port declared or
    /// not.
    #[test]
    fn a_flow_opened_from_another_address_gets_its_replies() {
        let mut r = rebinder();
        let now = Instant::now();
        let mut out = with_transport(WIFI, PEER, PROTO_UDP, 3658, 3658);
        assert!(r.outbound(&mut out, ROOM, now));
        let reply = with_transport(PEER, ROOM, PROTO_UDP, 3658, 3658);
        let delivered = r.inbound(&reply, ROOM, now).expect("back to the address that asked");
        assert_eq!(crate::lan::ipv4_endpoints(&delivered), Some((PEER, WIFI)));
        assert!(r.inbound(&reply, ROOM, now + FLOW_IDLE).is_none(), "and forgotten when idle");
    }

    #[test]
    fn a_program_on_the_room_address_is_never_touched() {
        let mut r = rebinder();
        let now = Instant::now();
        let mut out = with_transport(ROOM, PEER, PROTO_UDP, 9999, 9999);
        let before = out.clone();
        assert!(!r.outbound(&mut out, ROOM, now));
        assert_eq!(out, before);
        let other_port = with_transport(PEER, ROOM, PROTO_TCP, 51000, 3290);
        assert!(
            r.inbound(&other_port, ROOM, now).is_none(),
            "an undeclared listener stays on the room"
        );
    }

    /// A forwarding machine routes other hosts' packets into the room adapter
    /// too. Translating those would put the home network in the room.
    #[test]
    fn another_hosts_address_is_never_translated() {
        let mut r = rebinder();
        let mut forwarded =
            with_transport(Ipv4Addr::new(172, 17, 0, 2), PEER, PROTO_TCP, 9900, 51000);
        let before = forwarded.clone();
        assert!(!r.outbound(&mut forwarded, ROOM, Instant::now()));
        assert_eq!(forwarded, before);
    }

    /// A refused connection must fail fast, not hang: the error quotes the
    /// packet as it was sent, and both it and the quote are translated.
    #[test]
    fn an_icmp_error_and_the_packet_it_quotes_are_translated_both_ways() {
        let mut r = rebinder();
        let now = Instant::now();
        // This machine's stack refusing a packet translated to the Wi-Fi
        // address: it quotes it as delivered.
        let delivered = with_transport(PEER, WIFI, PROTO_UDP, 50000, 9900);
        let mut err = icmp_unreachable_about(&delivered, WIFI, PEER);
        assert!(verifies(&err));
        assert!(r.outbound(&mut err, ROOM, now));
        assert_eq!(crate::lan::ipv4_endpoints(&err), Some((ROOM, PEER)));
        assert_eq!(crate::lan::ipv4_endpoints(&err[28..]), Some((PEER, ROOM)), "the quote too");
        assert!(verifies(&err));

        // A member refusing a packet this machine sent from the Wi-Fi address.
        let mut sent = with_transport(WIFI, PEER, PROTO_UDP, 3659, 3659);
        assert!(r.outbound(&mut sent, ROOM, now));
        let err = icmp_unreachable_about(&sent, PEER, ROOM);
        let back = r.inbound(&err, ROOM, now).expect("routed to the flow's address");
        assert_eq!(crate::lan::ipv4_endpoints(&back), Some((PEER, WIFI)));
        assert_eq!(crate::lan::ipv4_endpoints(&back[28..]), Some((WIFI, PEER)));
        assert!(verifies(&back));
    }

    #[test]
    fn the_later_pieces_of_a_fragmented_datagram_follow_the_first() {
        let mut r = rebinder();
        r.survey
            .rebound
            .push(Rebound { port: LanPort { proto: LanProto::Udp, port: 9900 }, address: WIFI });
        let now = Instant::now();
        let mut first = with_transport(PEER, ROOM, PROTO_UDP, 50000, 9900);
        first[6] = 0x20; // more fragments
        let mut rest = ip_header(PEER, ROOM, PROTO_UDP, 8);
        rest[6..8].copy_from_slice(&0x0003u16.to_be_bytes());
        let s = {
            rest[10..12].copy_from_slice(&[0, 0]);
            sum(&rest[..20])
        };
        rest[10..12].copy_from_slice(&s.to_be_bytes());
        rest.extend_from_slice(&[0u8; 8]);
        assert!(r.inbound(&first, ROOM, now).is_some());
        let rest = r.inbound(&rest, ROOM, now).expect("the same address as the first");
        assert_eq!(crate::lan::ipv4_endpoints(&rest), Some((PEER, WIFI)));
        assert_eq!(sum(&rest[..20]), 0);
    }

    #[test]
    fn only_an_address_a_packet_could_be_delivered_to_is_a_target() {
        assert!(is_translatable(WIFI));
        assert!(is_translatable(Ipv4Addr::new(10, 0, 0, 5)));
        assert!(!is_translatable(Ipv4Addr::LOCALHOST));
        assert!(!is_translatable(Ipv4Addr::new(169, 254, 1, 1)));
        assert!(!is_translatable(Ipv4Addr::new(198, 19, 3, 3)), "another room");
    }

    /// The survey on this machine: a socket held on one of its addresses and
    /// not on the room's is found; one on every address is not.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_survey_finds_a_port_held_on_another_address_only() {
        let Some(&lan) = local_addresses().iter().find(|a| is_translatable(**a)) else {
            eprintln!("skipping: no non-loopback address here");
            return;
        };
        // Stands in for the room address: local and translatable-looking is
        // not needed, only bindable.
        let room = Ipv4Addr::LOCALHOST;
        let held = std::net::UdpSocket::bind((lan, 0)).unwrap();
        let port = LanPort { proto: LanProto::Udp, port: held.local_addr().unwrap().port() };
        let s = survey(room, &[port]);
        assert!(s.locals.contains(&lan));
        assert_eq!(s.rebound, [Rebound { port, address: lan }]);

        let everywhere = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = LanPort { proto: LanProto::Udp, port: everywhere.local_addr().unwrap().port() };
        assert!(survey(room, &[port]).rebound.is_empty(), "a game on every address needs no help");

        let tcp = std::net::TcpListener::bind((lan, 0)).unwrap();
        let port = LanPort { proto: LanProto::Tcp, port: tcp.local_addr().unwrap().port() };
        assert_eq!(survey(room, &[port]).rebound, [Rebound { port, address: lan }]);
    }
}
