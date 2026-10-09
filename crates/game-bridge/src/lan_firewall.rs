//! Mode 3, virtual LAN: telling a player that *their own* firewall is what
//! keeps the others out, and how to open it.
//!
//! The pump admits only the game's declared ports (`lan_filter.rs`), so a room
//! needs nothing from the operating system's firewall — except that the
//! firewall must not drop what the pump lets through. Two common setups do:
//! CachyOS ships ufw on with "deny incoming", and Windows files an unknown
//! adapter as a *Public* network. The first real Need for Speed: Underground 2
//! race hit the first: the member saw the host's race in the game's LAN list
//! (that only needs the member's side open) and hung on joining it, because
//! the host's ufw dropped the connection to TCP 9900.
//!
//! The launcher never changes the firewall itself: a rule outlives the room,
//! and the launcher never elevates (`CLAUDE.md`, Mode 3). What it can do is
//! notice and say what to run.
//!
//! - [`ConnectWatch`] sits in the pump. A TCP connection the room delivered to
//!   this machine that the operating system never answered — no SYN-ACK, no
//!   reset — was dropped between the adapter and the program, which is a
//!   firewall. A port nobody listens on still answers with a reset, so this
//!   catches the firewall and not a game that is not running yet.
//! - [`detect`] and [`advice`] name the firewall and the command for it.
//!
//! # Rules a later change could quietly break
//!
//! - **Only an unanswered connection is evidence.** A reset is an answer: it
//!   means the packet reached the stack. Counting it would blame the firewall
//!   for a game that is simply not hosting yet.
//! - **The command opens the room adapter, never the machine.** ufw and
//!   firewalld are told about `gbl*` only; on Windows the rule is bound to the
//!   adapter's alias and the game's declared ports. The pump still filters
//!   what comes in on it.
//! - **Advice, never action.** Nothing here runs a command.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::lan_filter::{LanPolicy, LanProto};

/// How long the operating system has to answer a connection. A kernel answers
/// a SYN at once, listener or not; this is margin, not a guess at latency.
pub const ANSWER_GRACE: Duration = Duration::from_secs(3);
/// How long a dropped connection is remembered and shown.
pub const REMEMBER: Duration = Duration::from_secs(600);
/// Connections waiting for an answer at once; past this, new ones are not
/// watched.
const PENDING_CAP: usize = 512;

const PROTO_TCP: u8 = 6;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const ACK: u8 = 0x10;

/// The parts of a TCP segment the watch reads.
#[derive(Debug, Clone, Copy)]
struct Segment {
    src: Ipv4Addr,
    dst: Ipv4Addr,
    sport: u16,
    dport: u16,
    flags: u8,
}

fn segment(packet: &[u8]) -> Option<Segment> {
    let (src, dst) = crate::lan::ipv4_endpoints(packet)?;
    let ihl = (packet[0] & 0x0f) as usize * 4;
    let flags_offset = u16::from_be_bytes([packet[6], packet[7]]);
    if packet[9] != PROTO_TCP || flags_offset & 0x1fff != 0 {
        return None;
    }
    let t = packet.get(ihl..ihl + 14)?;
    Some(Segment {
        src,
        dst,
        sport: u16::from_be_bytes([t[0], t[1]]),
        dport: u16::from_be_bytes([t[2], t[3]]),
        flags: t[13],
    })
}

/// A connection the room delivered to this machine that nothing here answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dropped {
    /// This machine's port it was for.
    pub port: u16,
    /// The member that tried.
    pub from: Ipv4Addr,
    /// When it was found unanswered.
    pub at: Instant,
}

/// Remembers new inbound connections until this machine answers them.
#[derive(Debug, Default)]
pub struct ConnectWatch {
    inner: Mutex<WatchInner>,
}

#[derive(Debug, Default)]
struct WatchInner {
    /// (member, its port, this machine's port) → when the first SYN arrived.
    pending: HashMap<(Ipv4Addr, u16, u16), Instant>,
    /// This machine's port → the latest unanswered connection to it.
    dropped: HashMap<u16, Dropped>,
}

impl WatchInner {
    /// Move every connection past its grace into `dropped`; return those.
    fn sweep(&mut self, now: Instant) -> Vec<Dropped> {
        let mut found = Vec::new();
        self.pending.retain(|&(from, _, port), &mut seen| {
            if now.duration_since(seen) < ANSWER_GRACE {
                return true;
            }
            found.push(Dropped { port, from, at: now });
            false
        });
        for d in &found {
            self.dropped.insert(d.port, *d);
        }
        self.dropped.retain(|_, d| now.duration_since(d.at) < REMEMBER);
        found
    }
}

impl ConnectWatch {
    /// The pump handed `packet` to this machine. Returns connections newly
    /// found unanswered, so the caller can say so once.
    pub fn saw_delivered(&self, packet: &[u8], now: Instant) -> Vec<Dropped> {
        let mut w = self.inner.lock().expect("connect watch lock");
        if let Some(s) = segment(packet) {
            if s.flags & (SYN | ACK) == SYN && w.pending.len() < PENDING_CAP {
                // A retransmitted SYN keeps the first one's time.
                w.pending.entry((s.src, s.sport, s.dport)).or_insert(now);
            }
        }
        w.sweep(now)
    }

    /// This machine sent `packet` into the room. A SYN-ACK or a reset answers
    /// a pending connection.
    pub fn saw_outbound(&self, packet: &[u8], now: Instant) -> Vec<Dropped> {
        let mut w = self.inner.lock().expect("connect watch lock");
        if let Some(s) = segment(packet) {
            if s.flags & RST != 0 || s.flags & (SYN | ACK) == SYN | ACK {
                let key = (s.dst, s.dport, s.sport);
                if w.pending.remove(&key).is_some() {
                    // Answered now, so whatever dropped it earlier is gone.
                    w.dropped.remove(&s.sport);
                }
            }
        }
        w.sweep(now)
    }

    /// Unanswered connections of the last [`REMEMBER`], one per port, oldest
    /// port first.
    pub fn dropped(&self, now: Instant) -> Vec<Dropped> {
        let mut w = self.inner.lock().expect("connect watch lock");
        w.sweep(now);
        let mut out: Vec<Dropped> = w.dropped.values().copied().collect();
        out.sort_by_key(|d| d.port);
        out
    }
}

/// The firewall this machine runs, as far as can be told without privilege.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalFirewall {
    /// ufw, enabled, refusing incoming connections by default.
    Ufw,
    /// firewalld, running.
    Firewalld,
    /// Windows Defender Firewall, which is on unless someone turned it off.
    Windows,
    /// Nothing recognised; something else may still be there.
    Unknown,
}

/// Which firewall is here. Reads world-readable configuration only.
pub fn detect() -> LocalFirewall {
    #[cfg(windows)]
    {
        LocalFirewall::Windows
    }
    #[cfg(not(windows))]
    {
        let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
        if ufw_refuses_incoming(&read("/etc/ufw/ufw.conf"), &read("/etc/default/ufw")) {
            return LocalFirewall::Ufw;
        }
        let firewalld = std::process::Command::new("systemctl")
            .args(["is-active", "--quiet", "firewalld"])
            .status()
            .is_ok_and(|s| s.success());
        if firewalld {
            return LocalFirewall::Firewalld;
        }
        LocalFirewall::Unknown
    }
}

/// ufw is on (`ENABLED=yes`) and its default input policy is not ACCEPT.
/// An absent policy line is ufw's own default, DROP.
#[cfg_attr(windows, allow(dead_code))]
fn ufw_refuses_incoming(ufw_conf: &str, defaults: &str) -> bool {
    let value = |text: &str, key: &str| {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_ascii_uppercase())
    };
    value(ufw_conf, "ENABLED").as_deref() == Some("YES")
        && value(defaults, "DEFAULT_INPUT_POLICY").as_deref() != Some("ACCEPT")
}

/// The command that lets the room's members reach this machine through
/// `firewall`, for the adapter named `adapter`. `None` when there is no one
/// command to give.
pub fn fix_command(firewall: LocalFirewall, adapter: &str, policy: &LanPolicy) -> Option<String> {
    match firewall {
        LocalFirewall::Ufw => Some(format!("sudo ufw allow in on {adapter}")),
        LocalFirewall::Firewalld => Some(format!(
            "sudo firewall-cmd --permanent --zone=trusted --add-interface={adapter} && sudo firewall-cmd --reload"
        )),
        LocalFirewall::Windows => {
            let ports = |proto: LanProto| {
                let list: Vec<String> = policy
                    .ports
                    .iter()
                    .filter(|p| p.proto == proto)
                    .map(|p| p.port.to_string())
                    .collect();
                list.join(",")
            };
            let rule = |proto: &str, ports: String| {
                let local = if policy.any { String::new() } else { format!(" -LocalPort {ports}") };
                format!(
                    "New-NetFirewallRule -DisplayName 'Mesh Game Servers room {proto}' -Direction Inbound \
                     -InterfaceAlias {adapter} -Protocol {proto}{local} -Action Allow"
                )
            };
            let mut lines = Vec::new();
            for (proto, name) in [(LanProto::Tcp, "TCP"), (LanProto::Udp, "UDP")] {
                let list = ports(proto);
                if policy.any || !list.is_empty() {
                    lines.push(rule(name, list));
                }
            }
            (!lines.is_empty()).then(|| lines.join("\n"))
        }
        LocalFirewall::Unknown => None,
    }
}

/// What to tell a player whose firewall dropped a room connection, without the
/// command itself: [`fix_command`] is shown beside it, where it can be copied.
pub fn advice(firewall: LocalFirewall, adapter: &str) -> String {
    match firewall {
        LocalFirewall::Ufw => {
            "ufw is refusing incoming connections here. Allow the room's adapter once, in a terminal:".to_string()
        }
        LocalFirewall::Firewalld => {
            "firewalld is refusing incoming connections here. Trust the room's adapter once, in a terminal:"
                .to_string()
        }
        LocalFirewall::Windows => "Windows Firewall is refusing them. Allow the game's ports on the room's \
             adapter once, in PowerShell run as administrator:"
            .to_string(),
        LocalFirewall::Unknown => format!(
            "Allow incoming connections on the room's adapter, {adapter}, in this machine's firewall."
        ),
    }
}

/// [`advice`] and [`fix_command`] as one line, for a log or a terminal.
pub fn advice_line(firewall: LocalFirewall, adapter: &str, policy: &LanPolicy) -> String {
    match fix_command(firewall, adapter, policy) {
        Some(cmd) => format!("{} {}", advice(firewall, adapter), cmd.replace('\n', " ; ")),
        None => advice(firewall, adapter),
    }
}

/// A warning to show before anyone has tried to connect, when this machine's
/// firewall is known to refuse incoming connections by default; the command
/// is [`fix_command`]. Only for firewalls whose default is known to bite — on
/// Windows the watch decides.
pub fn heads_up(firewall: LocalFirewall) -> Option<String> {
    match firewall {
        LocalFirewall::Ufw => Some(
            "ufw is on here and refuses incoming connections by default, so other members cannot join a game \
             you host until the room's adapter is allowed. If you have not done it before, run once:"
                .to_string(),
        ),
        LocalFirewall::Firewalld => Some(
            "firewalld is on here and may refuse incoming connections on the room's adapter. If other members \
             cannot join a game you host, run once:"
                .to_string(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lan_filter::LanPort;

    const ME: Ipv4Addr = Ipv4Addr::new(198, 19, 1, 1);
    const PEER: Ipv4Addr = Ipv4Addr::new(198, 19, 2, 2);

    fn tcp(src: Ipv4Addr, sport: u16, dst: Ipv4Addr, dport: u16, flags: u8) -> Vec<u8> {
        let mut p = vec![0u8; 40];
        p[0] = 0x45;
        p[9] = PROTO_TCP;
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        p[20..22].copy_from_slice(&sport.to_be_bytes());
        p[22..24].copy_from_slice(&dport.to_be_bytes());
        p[33] = flags;
        p
    }

    /// The NFSU2 race: the member's join to the host's 9900, which the host's
    /// ufw dropped. Nothing came back, and the watch must say so.
    #[test]
    fn a_connection_nothing_here_answered_is_reported() {
        let w = ConnectWatch::default();
        let t0 = Instant::now();
        assert!(w.saw_delivered(&tcp(PEER, 50000, ME, 9900, SYN), t0).is_empty());
        assert!(w.dropped(t0 + Duration::from_secs(1)).is_empty(), "still within its grace");
        // The member retries; the first SYN's time still counts.
        w.saw_delivered(&tcp(PEER, 50000, ME, 9900, SYN), t0 + Duration::from_secs(2));
        let found = w.saw_delivered(&[], t0 + ANSWER_GRACE);
        assert_eq!(found.len(), 1, "found once, by whichever call sweeps first");
        assert_eq!((found[0].port, found[0].from), (9900, PEER));
        assert_eq!(w.dropped(t0 + ANSWER_GRACE).len(), 1);
        assert!(w.dropped(t0 + ANSWER_GRACE + REMEMBER).is_empty(), "and forgotten later");
    }

    #[test]
    fn a_syn_ack_is_an_answer() {
        let w = ConnectWatch::default();
        let t0 = Instant::now();
        w.saw_delivered(&tcp(PEER, 50000, ME, 9900, SYN), t0);
        w.saw_outbound(&tcp(ME, 9900, PEER, 50000, SYN | ACK), t0);
        assert!(w.dropped(t0 + ANSWER_GRACE * 2).is_empty());
    }

    /// The rule that keeps a game that is not hosting yet from being blamed on
    /// the firewall: nobody listening still answers, with a reset.
    #[test]
    fn a_reset_is_an_answer_too() {
        let w = ConnectWatch::default();
        let t0 = Instant::now();
        w.saw_delivered(&tcp(PEER, 50000, ME, 9900, SYN), t0);
        w.saw_outbound(&tcp(ME, 9900, PEER, 50000, RST | ACK), t0);
        assert!(w.dropped(t0 + ANSWER_GRACE * 2).is_empty());
    }

    #[test]
    fn an_answer_to_another_connection_answers_nothing() {
        let w = ConnectWatch::default();
        let t0 = Instant::now();
        w.saw_delivered(&tcp(PEER, 50000, ME, 9900, SYN), t0);
        w.saw_outbound(&tcp(ME, 9900, PEER, 50001, SYN | ACK), t0);
        w.saw_outbound(&tcp(ME, 9901, PEER, 50000, SYN | ACK), t0);
        assert_eq!(w.dropped(t0 + ANSWER_GRACE).len(), 1);
    }

    #[test]
    fn only_a_new_connection_is_watched() {
        let w = ConnectWatch::default();
        let t0 = Instant::now();
        // A SYN-ACK arriving is a reply to this side, and an ACK is mid-flow.
        w.saw_delivered(&tcp(PEER, 9900, ME, 50000, SYN | ACK), t0);
        w.saw_delivered(&tcp(PEER, 9900, ME, 50000, ACK), t0);
        assert!(w.dropped(t0 + ANSWER_GRACE).is_empty());
    }

    #[test]
    fn a_later_answer_clears_the_warning() {
        let w = ConnectWatch::default();
        let t0 = Instant::now();
        w.saw_delivered(&tcp(PEER, 50000, ME, 9900, SYN), t0);
        assert_eq!(w.dropped(t0 + ANSWER_GRACE).len(), 1);
        // The player ran the command; the next join is answered.
        let t1 = t0 + Duration::from_secs(30);
        w.saw_delivered(&tcp(PEER, 50002, ME, 9900, SYN), t1);
        w.saw_outbound(&tcp(ME, 9900, PEER, 50002, SYN | ACK), t1);
        assert!(w.dropped(t1).is_empty());
    }

    #[test]
    fn ufw_is_named_only_when_it_refuses_incoming() {
        assert!(ufw_refuses_incoming("ENABLED=yes\n", "DEFAULT_INPUT_POLICY=\"DROP\"\n"));
        assert!(ufw_refuses_incoming("ENABLED=yes\n", "DEFAULT_INPUT_POLICY=\"REJECT\"\n"));
        assert!(ufw_refuses_incoming("ENABLED=yes\n", ""), "absent means ufw's own default");
        assert!(!ufw_refuses_incoming("ENABLED=no\n", "DEFAULT_INPUT_POLICY=\"DROP\"\n"));
        assert!(!ufw_refuses_incoming("#ENABLED=yes\n", ""));
        assert!(!ufw_refuses_incoming("ENABLED=yes\n", "DEFAULT_INPUT_POLICY=\"ACCEPT\"\n"));
        assert!(!ufw_refuses_incoming("", ""), "no ufw at all");
    }

    fn nfsu2() -> LanPolicy {
        let tcp = |port| LanPort { proto: LanProto::Tcp, port };
        let udp = |port| LanPort { proto: LanProto::Udp, port };
        LanPolicy { ports: vec![udp(9999), udp(3658), tcp(9900), tcp(3282)], any: false }
    }

    /// The commands open the room adapter and nothing else.
    #[test]
    fn every_command_names_the_room_adapter() {
        for fw in [LocalFirewall::Ufw, LocalFirewall::Firewalld, LocalFirewall::Windows] {
            let cmd = fix_command(fw, "gbl0", &nfsu2()).unwrap();
            assert!(cmd.contains("gbl0"), "{fw:?}: {cmd}");
        }
        let win = fix_command(LocalFirewall::Windows, "gbl0", &nfsu2()).unwrap();
        assert!(win.contains("-Protocol TCP -LocalPort 9900,3282"), "{win}");
        assert!(win.contains("-Protocol UDP -LocalPort 9999,3658"), "{win}");
        assert_eq!(win.lines().count(), 2);
        assert!(fix_command(LocalFirewall::Unknown, "gbl0", &nfsu2()).is_none());
        let unknown = advice_line(LocalFirewall::Unknown, "gbl0", &nfsu2());
        assert!(unknown.contains("gbl0"), "{unknown}");
        let ufw = advice_line(LocalFirewall::Ufw, "gbl0", &nfsu2());
        assert!(ufw.ends_with("sudo ufw allow in on gbl0"), "{ufw}");
    }

    #[test]
    fn a_heads_up_is_only_for_a_firewall_whose_default_bites() {
        assert!(heads_up(LocalFirewall::Ufw).is_some());
        assert!(heads_up(LocalFirewall::Firewalld).is_some());
        assert!(heads_up(LocalFirewall::Windows).is_none());
        assert!(heads_up(LocalFirewall::Unknown).is_none());
    }
}
