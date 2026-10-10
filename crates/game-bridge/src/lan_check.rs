//! Mode 3, virtual LAN: the room check — does this room actually work, *on this
//! machine*?
//!
//! CI proves the room on real adapters (`tests/lan_adapter.rs`,
//! `tests/lan_wintun.rs`, a real OpenTTD game in `tests/lan_openttd.rs`). None
//! of that runs on a player's computer, and the failures that matter there are
//! invisible: Windows sends `255.255.255.255` out of some other network, a
//! firewall eats the room's replies, or another member's adapter never came up.
//! The game just shows an empty LAN list, and nobody can tell which it was.
//!
//! So a room can check itself, the way a game would use it. The checker sends
//! a probe **from an ordinary socket bound to `0.0.0.0`** — what an old game
//! does — to the limited broadcast, to the room's subnet broadcast, and to each
//! member by unicast, all on one of the game's own declared UDP ports. Every
//! member's pump recognises the probe and answers it (see [`answer`]) instead
//! of handing it to that machine's game, and the answer comes back to the
//! checker's socket through this machine's adapter.
//!
//! Because the pump here also logs the probes it saw leave and the answers it
//! delivered ([`ProbeLog`]), a failure says *where* it failed:
//!
//! - a probe the pump never saw left by another network — the Hamachi metric
//!   problem, or a route the adapter did not get;
//! - an answer the pump delivered that the socket never got was stopped on this
//!   machine, between the adapter and the program — a firewall;
//! - a member that answered nothing has no working room on its side (or runs a
//!   launcher from before this check);
//! - a member the room reaches that did not answer a TCP connection to one of
//!   the game's declared TCP ports has a firewall dropping it. That is the
//!   one that lets a player *see* a game and hang joining it, and a broadcast
//!   cannot find it — the first real NFSU2 race in a room hit exactly this.
//!   The member's own launcher saw the connection go unanswered and names its
//!   firewall (`lan_firewall.rs`); [`CheckReport::dropped_here`] is that, here.
//!
//! # Rules a later change could quietly break
//!
//! - **The probe is sent from an OS socket, never injected into the pump.** An
//!   injected probe would pass on exactly the machines where the game fails,
//!   because the operating system's routing is the thing under test.
//! - **A member answers only to the probe's own source**, which the host has
//!   already checked is the sender's address (`lan::route`). So a probe cannot
//!   make a member send anything to a third party, and a broadcast probe costs
//!   the room what any broadcast does — the host's gate applies to it.
//! - **A probe is swallowed, not delivered.** A game listening on the port it
//!   arrived on would otherwise read 24 bytes it never asked for.
//! - **A TCP probe is a real connection, closed at once.** A refused one is a
//!   pass: the reset came from the member's stack, so nothing dropped it, and
//!   a game that is not hosting yet must not fail the check.
//! - **This proves the room, not the game.** A member's firewall rule for the
//!   game itself, or a game that advertises its real LAN address instead of
//!   the room's, are beyond it; the report says so rather than overclaim.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::net::{TcpSocket, UdpSocket};

use crate::lan_filter::{LanPolicy, LanProto};
use crate::lan_session::LanSession;

/// A probe's payload starts with this; an answer's with [`REPLY_TAG`].
pub const PROBE_TAG: [u8; 16] = *b"gbl-room-check?1";
pub const REPLY_TAG: [u8; 16] = *b"gbl-room-check!1";
/// Tag and an 8-byte nonce, exactly. Anything else on the port is the game's.
const PAYLOAD_LEN: usize = 24;
/// The port a room with `inbound = "any"` and no declared UDP port is checked
/// on: UDP discard, which nothing answers but the pump.
const FALLBACK_PORT: u16 = 9;

/// How many times each probe is sent, and how far apart. A LAN loses packets
/// and so does a mesh; one lost probe must not read as a broken room.
const ROUNDS: u32 = 3;
const ROUND_GAP: Duration = Duration::from_millis(300);
/// How long to wait for answers after the last round.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(3);
/// Probes and answers a [`ProbeLog`] remembers; a check needs a few dozen.
const LOG_CAP: usize = 512;
/// How long a TCP probe waits to connect. A stack answers a SYN at once; a
/// firewall that drops it is what makes one wait this long.
const TCP_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Probe,
    Reply,
}

/// The parts of a check packet that matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CheckPacket {
    kind: Kind,
    nonce: u64,
    src: Ipv4Addr,
    dst: Ipv4Addr,
    sport: u16,
    dport: u16,
}

fn parse(packet: &[u8]) -> Option<CheckPacket> {
    let (src, dst) = crate::lan::ipv4_endpoints(packet)?;
    let ihl = (packet[0] & 0x0f) as usize * 4;
    // A whole, unfragmented UDP datagram only.
    let flags_offset = u16::from_be_bytes([packet[6], packet[7]]);
    if packet[9] != 17 || flags_offset & 0x3fff != 0 {
        return None;
    }
    let udp = packet.get(ihl..)?;
    if udp.len() != 8 + PAYLOAD_LEN || u16::from_be_bytes([udp[4], udp[5]]) as usize != udp.len() {
        return None;
    }
    let payload = &udp[8..];
    let kind = if payload[..16] == PROBE_TAG {
        Kind::Probe
    } else if payload[..16] == REPLY_TAG {
        Kind::Reply
    } else {
        return None;
    };
    Some(CheckPacket {
        kind,
        nonce: u64::from_be_bytes(payload[16..24].try_into().ok()?),
        src,
        dst,
        sport: u16::from_be_bytes([udp[0], udp[1]]),
        dport: u16::from_be_bytes([udp[2], udp[3]]),
    })
}

/// If `packet`, delivered by the room, is a probe, the answer this member
/// sends back — from its own address, on the port the probe came in on, to
/// the probe's source. `None` means `packet` is the game's.
pub fn answer(packet: &[u8], own: Ipv4Addr) -> Option<Vec<u8>> {
    let p = parse(packet)?;
    if p.kind != Kind::Probe || p.src == own {
        return None;
    }
    let mut payload = REPLY_TAG.to_vec();
    payload.extend_from_slice(&p.nonce.to_be_bytes());
    Some(udp_packet(own, p.src, p.dport, p.sport, &payload))
}

/// Whether `packet` is a probe, whatever its direction.
pub fn is_probe(packet: &[u8]) -> bool {
    parse(packet).is_some_and(|p| p.kind == Kind::Probe)
}

/// An IPv4/UDP packet with both checksums filled in: a Windows stack drops a
/// datagram whose checksum is wrong, and Wintun offloads none.
fn udp_packet(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let udp_len = 8 + payload.len();
    let total = 20 + udp_len;
    let mut p = vec![0u8; total];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    p[8] = 64; // TTL
    p[9] = 17; // UDP
    p[12..16].copy_from_slice(&src.octets());
    p[16..20].copy_from_slice(&dst.octets());
    let ip_sum = checksum(&[&p[..20]]);
    p[10..12].copy_from_slice(&ip_sum.to_be_bytes());

    p[20..22].copy_from_slice(&sport.to_be_bytes());
    p[22..24].copy_from_slice(&dport.to_be_bytes());
    p[24..26].copy_from_slice(&(udp_len as u16).to_be_bytes());
    p[28..].copy_from_slice(payload);
    let mut pseudo = [0u8; 12];
    pseudo[..4].copy_from_slice(&src.octets());
    pseudo[4..8].copy_from_slice(&dst.octets());
    pseudo[9] = 17;
    pseudo[10..12].copy_from_slice(&(udp_len as u16).to_be_bytes());
    let udp_sum = match checksum(&[&pseudo, &p[20..]]) {
        0 => 0xffff, // 0 means "no checksum" in UDP
        s => s,
    };
    p[26..28].copy_from_slice(&udp_sum.to_be_bytes());
    p
}

/// The Internet checksum over several byte runs, each of even length but the
/// last.
fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum: u32 = 0;
    for part in parts {
        let mut chunks = part.chunks_exact(2);
        for c in &mut chunks {
            sum += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
        if let [last] = chunks.remainder() {
            sum += (*last as u32) << 8;
        }
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// What this machine's pump saw of a check: which probes left through the
/// adapter, and which answers it handed to the operating system. Kept on the
/// [`LanSession`], so the checker can ask the pump without a channel to it.
#[derive(Debug, Default)]
pub struct ProbeLog {
    inner: Mutex<LogInner>,
}

#[derive(Debug, Default)]
struct LogInner {
    left: HashMap<u64, Instant>,
    delivered: HashMap<u64, Vec<Ipv4Addr>>,
}

impl ProbeLog {
    /// The pump is sending `packet` into the room.
    pub fn saw_outbound(&self, packet: &[u8]) {
        let Some(p) = parse(packet) else { return };
        if p.kind == Kind::Probe {
            let mut log = self.inner.lock().expect("probe log lock");
            if log.left.len() >= LOG_CAP {
                log.left.clear();
            }
            log.left.insert(p.nonce, Instant::now());
        }
    }

    /// The pump handed `packet` to this machine's adapter.
    pub fn saw_delivered(&self, packet: &[u8]) {
        let Some(p) = parse(packet) else { return };
        if p.kind == Kind::Reply {
            let mut log = self.inner.lock().expect("probe log lock");
            if log.delivered.len() >= LOG_CAP {
                log.delivered.clear();
            }
            log.delivered.entry(p.nonce).or_default().push(p.src);
        }
    }

    fn left(&self, nonce: u64) -> bool {
        self.inner.lock().expect("probe log lock").left.contains_key(&nonce)
    }

    fn delivered(&self, nonce: u64) -> Vec<Ipv4Addr> {
        self.inner
            .lock()
            .expect("probe log lock")
            .delivered
            .get(&nonce)
            .cloned()
            .unwrap_or_default()
    }
}

/// The UDP port a room is checked on: the game's first declared one, so the
/// probe takes the path the game's own discovery takes. `None` for a game with
/// no UDP on its LAN — its discovery is not a broadcast, and this check is.
pub fn probe_port(policy: &LanPolicy) -> Option<u16> {
    policy
        .ports
        .iter()
        .find(|p| p.proto == LanProto::Udp)
        .map(|p| p.port)
        .or(policy.any.then_some(FALLBACK_PORT))
}

/// What a TCP connection to one of a member's declared ports did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpState {
    /// Something there accepted it.
    Open,
    /// Refused: the member's stack answered, nothing listens there yet.
    Closed,
    /// Nothing answered.
    NoAnswer,
}

/// One declared TCP port on one member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpCheck {
    pub port: u16,
    pub state: TcpState,
}

/// How one other member did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberCheck {
    pub address: Ipv4Addr,
    /// Answered a probe sent to `255.255.255.255`.
    pub heard_limited_broadcast: bool,
    /// Answered a probe sent to the room's subnet broadcast.
    pub heard_subnet_broadcast: bool,
    /// Answered a probe sent to its own address.
    pub answered_unicast: bool,
    /// Fastest answer to any probe, there and back.
    pub round_trip: Option<Duration>,
    /// Each of the game's declared TCP ports, connected to on this member.
    pub tcp: Vec<TcpCheck>,
}

impl MemberCheck {
    pub fn ok(&self) -> bool {
        self.heard_limited_broadcast
            && self.heard_subnet_broadcast
            && self.answered_unicast
            && self.tcp_unanswered().is_empty()
    }

    /// Declared TCP ports on this member nothing answered.
    pub fn tcp_unanswered(&self) -> Vec<u16> {
        self.tcp.iter().filter(|t| t.state == TcpState::NoAnswer).map(|t| t.port).collect()
    }
}

/// The result of [`check_room`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// This machine's address in the room.
    pub address: Ipv4Addr,
    /// The UDP port the probes went to.
    pub port: u16,
    /// Every other member seated when the check started.
    pub members: Vec<MemberCheck>,
    /// A probe to `255.255.255.255` went through this machine's room adapter.
    pub limited_broadcast_left: bool,
    /// A probe to the subnet broadcast went through it.
    pub subnet_broadcast_left: bool,
    /// Some answer reached this machine's adapter but not the socket that
    /// asked: something on this machine stopped it.
    pub answers_blocked_here: bool,
    /// Connections members made to this machine that nothing here answered,
    /// as `(port, member)` (`lan_firewall::ConnectWatch`).
    pub dropped_here: Vec<(u16, Ipv4Addr)>,
    /// What to run to open this machine's firewall to the room, once it has
    /// dropped something (`lan_firewall::advice_line`).
    pub firewall_advice: Option<String>,
    /// What this machine's room filter refused because the game's pack does
    /// not list it (`lan_filter::RefusedLog`).
    pub refused_here: Vec<crate::lan_filter::Refused>,
    /// The game's ports this machine's pump translates because the game
    /// listens on another of its addresses (`lan_rebind.rs`).
    pub rebound_here: Vec<crate::lan_rebind::Rebound>,
}

impl CheckReport {
    /// Everything answered, every way.
    pub fn ok(&self) -> bool {
        !self.members.is_empty()
            && self.limited_broadcast_left
            && self.subnet_broadcast_left
            && self.dropped_here.is_empty()
            && self.members.iter().all(MemberCheck::ok)
    }

    /// What went wrong, in words a player can act on — or that nothing did.
    /// The first line is the verdict.
    pub fn findings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.members.is_empty() {
            out.push(
                "Nobody else is in the room yet, so there is nobody to check against.".to_string(),
            );
            return out;
        }
        let tcp_ports = |ports: &[u16]| {
            ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
        };
        // Not a fault, but the one thing a refused connection can mean once a
        // game *is* hosted: it is not listening where the room delivers.
        let not_listening: Vec<String> = self
            .members
            .iter()
            .filter(|m| m.answered_unicast)
            .filter_map(|m| {
                let closed: Vec<u16> =
                    m.tcp.iter().filter(|t| t.state == TcpState::Closed).map(|t| t.port).collect();
                (!closed.is_empty()).then(|| {
                    format!(
                        "Nothing on {} is listening on TCP {} yet. That is normal before a game is \
                         hosted there; if one is hosted there now, the game is not listening on the \
                         room's address — a game run through Wine or with a fixed address set can do \
                         that.",
                        m.address,
                        tcp_ports(&closed)
                    )
                })
            })
            .collect();
        // Not a fault: said so a player comparing notes with the other
        // machine knows the room is doing it.
        let rebound: Vec<String> = self
            .rebound_here
            .iter()
            .map(|r| {
                let proto = match r.port.proto {
                    LanProto::Udp => "UDP",
                    LanProto::Tcp => "TCP",
                };
                format!(
                    "On this computer the game listens on {} for {proto} {}, not on the room's \
                     address (games run through Wine do this). The room passes that traffic \
                     through.",
                    r.address, r.port.port
                )
            })
            .collect();
        if self.ok() && !self.refused_here.is_empty() {
            // The room itself works; the game's pack is what is short — or the
            // game is not listening where the room delivers, which the same
            // hung join looks like, so both are said.
            out.push("The room works, but the game used ports its pack does not list:".to_string());
            out.push(missing_ports_finding(&self.refused_here));
            out.extend(not_listening);
            out.extend(rebound);
            return out;
        }
        if self.ok() {
            let slowest = self.members.iter().filter_map(|m| m.round_trip).max();
            let checked: Vec<u16> =
                self.members.first().map(|m| m.tcp.iter().map(|t| t.port).collect()).unwrap_or_default();
            out.push(match slowest {
                Some(rtt) => format!(
                    "The room works: every member heard this machine's LAN broadcasts and answered \
                     (slowest round trip {} ms).",
                    rtt.as_millis()
                ),
                None => {
                    "The room works: every member heard this machine's LAN broadcasts and answered."
                        .to_string()
                }
            });
            if !checked.is_empty() {
                out.push(format!(
                    "Every member let a connection to TCP {} through.",
                    tcp_ports(&checked)
                ));
            }
            out.extend(not_listening);
            out.extend(rebound.iter().cloned());
            out.push(
                "That proves the room, not the game: if the game still lists nothing, allow it \
                 through the firewall on every machine, and check it is set to LAN play."
                    .to_string(),
            );
            return out;
        }
        out.push("The room does not work fully from this machine:".to_string());
        if !self.limited_broadcast_left {
            out.push(
                "A broadcast to 255.255.255.255 did not go through the room's adapter — this machine \
                 sent it out another network. Games that search that way will find nothing. On \
                 Windows, another adapter (a VPN, Hamachi, a virtual machine's) is preferred over the \
                 room's: disable it while playing, or leave and rejoin the room."
                    .to_string(),
            );
        }
        if !self.subnet_broadcast_left {
            out.push(
                "A broadcast to the room's own subnet did not go through the room's adapter, so its \
                 route is missing: leave and rejoin the room."
                    .to_string(),
            );
        }
        for (port, from) in &self.dropped_here {
            out.push(format!(
                "{from} tried to connect to this machine on TCP {port} and nothing here answered: a firewall \
                 on this machine is dropping the room's connections, so the others can see a game hosted \
                 here but not join it. {}",
                self.firewall_advice.as_deref().unwrap_or(
                    "Allow incoming connections on the room's adapter in this machine's firewall."
                )
            ));
        }
        if !self.refused_here.is_empty() {
            out.push(missing_ports_finding(&self.refused_here));
        }
        out.extend(not_listening.iter().cloned());
        out.extend(rebound);
        if self.answers_blocked_here {
            out.push(
                "Answers reached this machine's room adapter but not the program that asked: a \
                 firewall on this machine is blocking the room's network."
                    .to_string(),
            );
        }
        let went_out = self.limited_broadcast_left || self.subnet_broadcast_left;
        for m in self.members.iter().filter(|m| !m.ok()) {
            let answered_something =
                m.heard_limited_broadcast || m.heard_subnet_broadcast || m.answered_unicast;
            if !answered_something && !self.answers_blocked_here {
                out.push(format!(
                    "{} answered nothing: its room adapter is not up, or its launcher is older than \
                     the room check.",
                    m.address
                ));
            } else if answered_something {
                let mut missed = Vec::new();
                if !m.heard_limited_broadcast && self.limited_broadcast_left {
                    missed.push("the 255.255.255.255 broadcast");
                }
                if !m.heard_subnet_broadcast && self.subnet_broadcast_left {
                    missed.push("the subnet broadcast");
                }
                if !m.answered_unicast {
                    missed.push("a direct message");
                }
                if !missed.is_empty() && went_out {
                    out.push(format!(
                        "{} did not answer {}; the room may be losing packets to it.",
                        m.address,
                        missed.join(" or ")
                    ));
                }
            }
            let unanswered = m.tcp_unanswered();
            if m.answered_unicast && !unanswered.is_empty() {
                out.push(format!(
                    "{} did not answer a connection to TCP {}, though the room reaches it: a firewall on \
                     that machine is dropping them, so a game hosted there can be seen but not joined. The \
                     launcher there offers to fix it.",
                    m.address,
                    tcp_ports(&unanswered)
                ));
            }
        }
        out
    }
}

/// The finding for what the room filter refused: what it was, and which ports
/// the pack most likely needs, in a form a player can paste into a report.
pub fn missing_ports_finding(refused: &[crate::lan_filter::Refused]) -> String {
    let mut wanted: Vec<String> = Vec::new();
    for r in refused {
        let proto = match r.proto {
            LanProto::Udp => "UDP",
            LanProto::Tcp => "TCP",
        };
        let w = format!("{proto} {}", r.suggested_port());
        if !wanted.contains(&w) {
            wanted.push(w);
        }
    }
    let seen: Vec<String> = refused.iter().map(|r| r.describe()).collect();
    format!(
        "The room blocked traffic the game's pack does not list, which is the likely reason a game \
         can be seen but not joined. The pack probably needs: {}. Please report this. What was \
         blocked: {}.",
        wanted.join(", "),
        seen.join("; ")
    )
}

/// Check `session`'s room from this machine, on `policy`'s UDP port and every
/// declared TCP port. The room must already be on this machine's adapter,
/// named `adapter`.
pub async fn check_room(
    session: &LanSession,
    policy: &LanPolicy,
    adapter: &str,
) -> Result<CheckReport> {
    // `0.0.0.0`, as a game binds: what is under test is where the operating
    // system sends a broadcast from a socket that did not pick an interface.
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    let firewall = crate::lan_firewall::detect();
    let mut report =
        check_room_with(session, policy, UdpSocket::from_std(socket)?, TcpSocket::new_v4, DEFAULT_WAIT)
            .await?;
    if !report.dropped_here.is_empty() {
        report.firewall_advice = Some(crate::lan_firewall::advice_line(firewall, adapter));
    }
    Ok(report)
}

/// [`check_room`] on sockets the caller makes — a test makes them inside a
/// network namespace. `socket` must allow broadcast; `new_tcp` makes one
/// socket per TCP probe.
pub async fn check_room_with(
    session: &LanSession,
    policy: &LanPolicy,
    socket: UdpSocket,
    new_tcp: impl Fn() -> std::io::Result<TcpSocket>,
    wait: Duration,
) -> Result<CheckReport> {
    let port = probe_port(policy).ok_or_else(|| {
        anyhow!("this game uses no UDP on its LAN, and the room check is a UDP broadcast")
    })?;
    let view = session.view();
    let address = view
        .own_address
        .ok_or_else(|| anyhow!("not seated in the room yet, so there is nothing to check"))?;
    let others: Vec<Ipv4Addr> =
        view.members.iter().map(|m| m.address).filter(|a| *a != address).collect();
    let subnet_broadcast = view.subnet.broadcast();

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Target {
        Limited,
        Subnet,
        Unicast(Ipv4Addr),
    }
    let mut sent: HashMap<u64, (Target, Instant)> = HashMap::new();
    let mut results: HashMap<Ipv4Addr, MemberCheck> = others
        .iter()
        .map(|&a| {
            (
                a,
                MemberCheck {
                    address: a,
                    heard_limited_broadcast: false,
                    heard_subnet_broadcast: false,
                    answered_unicast: false,
                    round_trip: None,
                    tcp: Vec::new(),
                },
            )
        })
        .collect();
    let mut received: std::collections::HashSet<u64> = std::collections::HashSet::new();

    let mut buf = [0u8; 2048];
    let mut record = |buf: &[u8],
                      from: SocketAddr,
                      results: &mut HashMap<Ipv4Addr, MemberCheck>,
                      sent: &HashMap<u64, (Target, Instant)>| {
        if buf.len() != PAYLOAD_LEN || buf[..16] != REPLY_TAG {
            return;
        }
        let nonce = u64::from_be_bytes(buf[16..24].try_into().expect("8 bytes"));
        let SocketAddr::V4(from) = from else { return };
        let Some(&(target, at)) = sent.get(&nonce) else { return };
        received.insert(nonce);
        let Some(m) = results.get_mut(from.ip()) else { return };
        match target {
            Target::Limited => m.heard_limited_broadcast = true,
            Target::Subnet => m.heard_subnet_broadcast = true,
            Target::Unicast(to) if to == *from.ip() => m.answered_unicast = true,
            Target::Unicast(_) => return,
        }
        let rtt = at.elapsed();
        m.round_trip = Some(m.round_trip.map_or(rtt, |r| r.min(rtt)));
    };

    // Every declared TCP port on every other member, all at once, alongside
    // the broadcasts.
    let tcp_ports: Vec<u16> = policy
        .ports
        .iter()
        .filter(|p| p.proto == LanProto::Tcp)
        .map(|p| p.port)
        .collect();
    let mut tcp_probes = tokio::task::JoinSet::new();
    for &member in &others {
        for &tcp_port in &tcp_ports {
            let sock = new_tcp()?;
            tcp_probes.spawn(async move {
                let state =
                    match tokio::time::timeout(TCP_WAIT, sock.connect((member, tcp_port).into())).await {
                        Ok(Ok(_stream)) => TcpState::Open,
                        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                            TcpState::Closed
                        }
                        Ok(Err(e)) => {
                            tracing::debug!(%member, tcp_port, error = %e, "a room check TCP probe failed");
                            TcpState::NoAnswer
                        }
                        Err(_) => TcpState::NoAnswer,
                    };
                (member, TcpCheck { port: tcp_port, state })
            });
        }
    }

    for round in 0..ROUNDS {
        let mut targets = vec![Target::Limited, Target::Subnet];
        targets.extend(
            others
                .iter()
                .filter(|a| !results.get(a).is_some_and(|m| m.answered_unicast))
                .map(|&a| Target::Unicast(a)),
        );
        for target in targets {
            let to = match target {
                Target::Limited => Ipv4Addr::BROADCAST,
                Target::Subnet => subnet_broadcast,
                Target::Unicast(a) => a,
            };
            let nonce = random_nonce()?;
            let mut payload = PROBE_TAG.to_vec();
            payload.extend_from_slice(&nonce.to_be_bytes());
            sent.insert(nonce, (target, Instant::now()));
            // A send that fails — no route at all — is a probe that did not
            // leave, which the log below reports; it is not an error of the
            // check.
            if let Err(e) = socket.send_to(&payload, (to, port)).await {
                tracing::debug!(%to, error = %e, "a room check probe could not be sent");
            }
        }
        let until =
            tokio::time::Instant::now() + if round + 1 == ROUNDS { wait } else { ROUND_GAP };
        loop {
            if results.values().all(udp_done) && !results.is_empty() && round > 0 {
                break;
            }
            match tokio::time::timeout_at(until, socket.recv_from(&mut buf)).await {
                Ok(Ok((n, from))) => record(&buf[..n], from, &mut results, &sent),
                Ok(Err(e)) => tracing::debug!(error = %e, "room check receive"),
                Err(_) => break,
            }
        }
        if results.values().all(udp_done) && !results.is_empty() {
            break;
        }
    }
    while let Some(done) = tcp_probes.join_next().await {
        let (member, check) = done.map_err(|e| anyhow!("a room check TCP probe: {e}"))?;
        if let Some(m) = results.get_mut(&member) {
            m.tcp.push(check);
        }
    }

    let log = session.probe_log();
    let left = |t: Target| sent.iter().any(|(n, (target, _))| *target == t && log.left(*n));
    let limited_broadcast_left = left(Target::Limited);
    let subnet_broadcast_left = left(Target::Subnet);
    let answers_blocked_here =
        sent.keys().any(|n| !received.contains(n) && !log.delivered(*n).is_empty());
    let mut members: Vec<MemberCheck> = results.into_values().collect();
    members.sort_by_key(|m| m.address);
    for m in &mut members {
        m.tcp.sort_by_key(|t| t.port);
    }
    let dropped_here = session
        .connect_watch()
        .dropped(Instant::now())
        .into_iter()
        .map(|d| (d.port, d.from))
        .collect();
    Ok(CheckReport {
        address,
        port,
        members,
        limited_broadcast_left,
        subnet_broadcast_left,
        answers_blocked_here,
        dropped_here,
        firewall_advice: None,
        refused_here: session.refused_log().refused(Instant::now()),
        rebound_here: session.rebound(),
    })
}

/// Every broadcast and unicast probe to `m` was answered; its TCP probes are
/// waited for separately.
fn udp_done(m: &MemberCheck) -> bool {
    m.heard_limited_broadcast && m.heard_subnet_broadcast && m.answered_unicast
}

fn random_nonce() -> Result<u64> {
    let mut b = [0u8; 8];
    getrandom::getrandom(&mut b).map_err(|e| anyhow!("no randomness for a room check: {e}"))?;
    Ok(u64::from_be_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lan_filter::LanPort;

    const A: Ipv4Addr = Ipv4Addr::new(198, 19, 1, 1);
    const B: Ipv4Addr = Ipv4Addr::new(198, 19, 2, 2);

    fn probe(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16, nonce: u64) -> Vec<u8> {
        let mut payload = PROBE_TAG.to_vec();
        payload.extend_from_slice(&nonce.to_be_bytes());
        udp_packet(src, dst, sport, dport, &payload)
    }

    #[test]
    fn a_member_answers_a_probe_to_its_source_from_the_port_it_came_in_on() {
        let p = probe(A, Ipv4Addr::BROADCAST, 50000, 9999, 7);
        let reply = answer(&p, B).expect("a probe is answered");
        let r = parse(&reply).unwrap();
        assert_eq!(
            (r.kind, r.nonce, r.src, r.dst, r.sport, r.dport),
            (Kind::Reply, 7, B, A, 9999, 50000)
        );
        assert!(answer(&reply, A).is_none(), "an answer is never answered");
        assert!(answer(&p, A).is_none(), "nor is this member's own probe");
    }

    #[test]
    fn a_game_datagram_is_never_mistaken_for_a_probe() {
        // NFSU2's LAN search is 384 bytes on 9999; one that happened to start
        // with the tag is still not a probe unless it is exactly the probe's size.
        let mut payload = PROBE_TAG.to_vec();
        payload.resize(384, 0);
        let game = udp_packet(A, Ipv4Addr::BROADCAST, 9999, 9999, &payload);
        assert!(answer(&game, B).is_none());
        let other = udp_packet(A, B, 9999, 9999, &[0u8; PAYLOAD_LEN]);
        assert!(answer(&other, B).is_none());
        let mut tcp = probe(A, B, 1, 2, 3);
        tcp[9] = 6;
        assert!(answer(&tcp, B).is_none());
        let mut fragment = probe(A, B, 1, 2, 3);
        fragment[6] = 0x20; // more fragments
        assert!(answer(&fragment, B).is_none());
    }

    /// The checksums a receiving stack verifies: a packet with a wrong one is
    /// dropped silently, and the check would blame the room.
    #[test]
    fn an_answer_carries_valid_checksums() {
        let reply = answer(&probe(A, B, 40000, 9999, 42), B).unwrap();
        assert_eq!(checksum(&[&reply[..20]]), 0, "IPv4 header checksum");
        let mut pseudo = [0u8; 12];
        pseudo[..4].copy_from_slice(&reply[12..16]);
        pseudo[4..8].copy_from_slice(&reply[16..20]);
        pseudo[9] = 17;
        pseudo[10..12].copy_from_slice(&((reply.len() - 20) as u16).to_be_bytes());
        assert_eq!(checksum(&[&pseudo, &reply[20..]]), 0, "UDP checksum");
    }

    #[test]
    fn the_check_uses_the_games_own_udp_port() {
        let policy = LanPolicy {
            ports: vec![
                LanPort { proto: LanProto::Tcp, port: 9900 },
                LanPort { proto: LanProto::Udp, port: 9999 },
            ],
            any: false,
        };
        assert_eq!(probe_port(&policy), Some(9999));
        let tcp_only =
            LanPolicy { ports: vec![LanPort { proto: LanProto::Tcp, port: 9900 }], any: false };
        assert_eq!(probe_port(&tcp_only), None);
        assert_eq!(probe_port(&LanPolicy { ports: vec![], any: true }), Some(FALLBACK_PORT));
    }

    fn member(address: Ipv4Addr, ok: bool) -> MemberCheck {
        MemberCheck {
            address,
            heard_limited_broadcast: ok,
            heard_subnet_broadcast: ok,
            answered_unicast: ok,
            round_trip: ok.then_some(Duration::from_millis(40)),
            tcp: Vec::new(),
        }
    }

    fn report(members: Vec<MemberCheck>) -> CheckReport {
        CheckReport {
            address: A,
            port: 9999,
            members,
            limited_broadcast_left: true,
            subnet_broadcast_left: true,
            answers_blocked_here: false,
            dropped_here: Vec::new(),
            firewall_advice: None,
            refused_here: Vec::new(),
            rebound_here: Vec::new(),
        }
    }

    /// A game under Wine on this machine: the check says the room is
    /// translating for it, so nobody goes looking for a fault.
    #[test]
    fn a_game_on_another_address_is_named_and_not_blamed() {
        let mut r = report(vec![member(B, true)]);
        r.rebound_here = vec![crate::lan_rebind::Rebound {
            port: crate::lan_filter::LanPort { proto: LanProto::Tcp, port: 9900 },
            address: Ipv4Addr::new(192, 168, 32, 203),
        }];
        assert!(r.ok());
        let f = r.findings().join("\n");
        assert!(f.contains("listens on 192.168.32.203 for TCP 9900"), "{f}");
        assert!(f.contains("passes that traffic through"), "{f}");
    }

    #[test]
    fn ports_the_pack_lacks_are_named_even_when_the_room_works() {
        let log = crate::lan_filter::RefusedLog::default();
        let mut p = vec![0u8; 28];
        p[0] = 0x45;
        p[9] = 17;
        p[12..16].copy_from_slice(&B.octets());
        p[16..20].copy_from_slice(&A.octets());
        p[20..22].copy_from_slice(&3660u16.to_be_bytes());
        p[22..24].copy_from_slice(&3660u16.to_be_bytes());
        log.saw(crate::lan_filter::DropDirection::Inbound, &p, Instant::now());
        let mut m = member(B, true);
        m.tcp = vec![TcpCheck { port: 9900, state: TcpState::Closed }];
        let mut r = report(vec![m]);
        r.refused_here = log.refused(Instant::now());
        assert!(r.ok(), "the room itself works");
        let f = r.findings().join("\n");
        assert!(f.contains("ports its pack does not list"), "{f}");
        assert!(f.contains("probably needs: UDP 3660"), "{f}");
        assert!(f.contains("from 198.19.2.2:3660"), "{f}");
        // The same hung join can be a game under Wine listening elsewhere, so
        // a missing port never hides that.
        assert!(f.contains("listening on TCP 9900"), "{f}");
    }

    /// What a member's firewall dropping the game's TCP looks like from the
    /// other side: the broadcasts all work, and a join would hang.
    #[test]
    fn a_member_whose_firewall_drops_the_games_tcp_is_named() {
        let mut m = member(B, true);
        m.tcp = vec![
            TcpCheck { port: 3282, state: TcpState::NoAnswer },
            TcpCheck { port: 9900, state: TcpState::Closed },
        ];
        let r = report(vec![m]);
        assert!(!r.ok());
        let f = r.findings().join("\n");
        assert!(f.contains("198.19.2.2 did not answer a connection to TCP 3282,"), "{f}");
        assert!(f.contains("firewall on that machine"), "{f}");
        assert!(
            f.contains("connection to TCP 3282, though"),
            "only the unanswered port is blamed; a refused one reached the member's stack: {f}"
        );
        assert!(f.contains("listening on TCP 9900"), "{f}");
    }

    /// The rule that keeps a game that is not hosting yet from failing the
    /// check: a refused connection was answered.
    #[test]
    fn a_refused_tcp_port_passes_and_says_nothing_listens_there() {
        let mut m = member(B, true);
        m.tcp = vec![TcpCheck { port: 9900, state: TcpState::Closed }];
        let r = report(vec![m]);
        assert!(r.ok());
        let f = r.findings().join("\n");
        assert!(f.contains("TCP 9900 through"), "{f}");
        assert!(f.contains("Nothing on 198.19.2.2 is listening on TCP 9900 yet"), "{f}");
    }

    #[test]
    fn a_connection_this_machine_dropped_is_named_with_the_fix() {
        let mut r = report(vec![member(B, true)]);
        r.dropped_here = vec![(9900, B)];
        r.firewall_advice = Some("Run: sudo ufw allow in on gbl0".into());
        assert!(!r.ok());
        let f = r.findings().join("\n");
        assert!(f.contains("198.19.2.2 tried to connect to this machine on TCP 9900"), "{f}");
        assert!(f.contains("sudo ufw allow in on gbl0"), "{f}");
    }

    #[test]
    fn a_broadcast_that_left_by_another_network_is_named_as_that() {
        let mut r = report(vec![MemberCheck { heard_limited_broadcast: false, ..member(B, true) }]);
        r.limited_broadcast_left = false;
        assert!(!r.ok());
        let f = r.findings().join("\n");
        assert!(f.contains("255.255.255.255") && f.contains("another network"), "{f}");
        assert!(!f.contains("answered nothing"), "the member is not the one at fault: {f}");
    }

    #[test]
    fn a_firewall_here_is_not_blamed_on_the_member() {
        let mut r = report(vec![member(B, false)]);
        r.answers_blocked_here = true;
        let f = r.findings().join("\n");
        assert!(f.contains("firewall on this machine"), "{f}");
        assert!(!f.contains("answered nothing"), "{f}");
    }

    #[test]
    fn a_silent_member_is_named() {
        let f = report(vec![member(B, false)]).findings().join("\n");
        assert!(f.contains("198.19.2.2 answered nothing"), "{f}");
    }

    #[test]
    fn a_working_room_says_it_proves_the_room_and_not_the_game() {
        let r = report(vec![member(B, true)]);
        assert!(r.ok());
        let f = r.findings();
        assert!(f[0].starts_with("The room works") && f[0].contains("40 ms"), "{f:?}");
        assert!(f[1].contains("not the game"), "{f:?}");
        assert!(!report(vec![]).ok(), "an empty room proves nothing");
    }
}
