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
//! So the room opens it, once, at the moment it already holds privilege, and
//! names what it cannot open:
//!
//! - [`ConnectWatch`] sits in the pump. A TCP connection the room delivered to
//!   this machine that the operating system never answered — no SYN-ACK, no
//!   reset — was dropped between the adapter and the program, which is a
//!   firewall. A port nobody listens on still answers with a reset, so this
//!   catches the firewall and not a game that is not running yet.
//! - [`open`] and [`close`] are `lan-helper`'s: the elevated helper opens the
//!   firewall to the room when it starts — on Windows every room's helper is
//!   elevated anyway, on Linux a per-room `pkexec` run and the one-time grant
//!   are — so a player sees no prompt they would not have seen already.
//! - [`windows_facts`] reads, without privilege, what `open` cannot fix: a
//!   Block rule for the game (someone once clicked "Cancel" on its firewall
//!   prompt, and a block beats any allow) and a third-party firewall, by name.
//! - [`detect`], [`fix_command`] and [`advice`] are the fallback for a player
//!   who wants to do it by hand, and for the CLI.
//!
//! # Rules a later change could quietly break
//!
//! - **Only an unanswered connection is evidence.** A reset is an answer: it
//!   means the packet reached the stack. Counting it would blame the firewall
//!   for a game that is simply not hosting yet.
//! - **What is opened is the room, never the machine.** ufw and firewalld are
//!   told about the `gbl*` name, which every room reuses; on Windows the rule
//!   matches the room range `198.18.0.0/15`, not the adapter, because each
//!   room's Wintun adapter is new. The pump still admits only declared ports.
//! - **An installed launcher keeps its rule; a portable one removes its own,
//!   and only its own.** Portable writes nothing outside its folder, so its
//!   Windows rule is [`PORTABLE_RULE_NAME`] and goes when the room does (a
//!   crash's leftover goes at the next portable room). A ufw rule that was
//!   already there — the player's, or an installed launcher's — is never
//!   deleted.
//! - **Only the helper opens anything, and only on its own.** The launcher's
//!   relay never carries a request to it (`lan_relay.rs`); the helper opens
//!   the firewall as part of `serve`, or because a person ran `allow-rooms`
//!   through the elevation prompt.
//! - **A Block rule is disabled, never deleted, and only one a person picked.**

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

    /// Forget what was dropped: the firewall was just opened, and an old
    /// warning must not outlive its cause.
    pub fn forget(&self) {
        let mut w = self.inner.lock().expect("connect watch lock");
        w.pending.clear();
        w.dropped.clear();
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
pub fn fix_command(firewall: LocalFirewall, adapter: &str) -> Option<String> {
    match firewall {
        LocalFirewall::Ufw => Some(format!("sudo ufw allow in on {adapter}")),
        LocalFirewall::Firewalld => Some(format!(
            "sudo firewall-cmd --permanent --zone=trusted --add-interface={adapter} && sudo firewall-cmd --reload"
        )),
        // Matched on the room's addresses, never on the adapter: Wintun gives
        // every room's adapter a fresh GUID, and a rule bound to one adapter
        // would quietly stop matching the next room's. Every room is inside
        // `ALLOWED_RANGE`, checked at decode, so one rule covers every room
        // and every game, once; the pump still admits only declared ports.
        LocalFirewall::Windows => Some(format!(
            "New-NetFirewallRule -DisplayName 'Lanthorn LAN rooms' -Direction Inbound \
             -RemoteAddress {}/{} -Action Allow",
            crate::lan::ALLOWED_RANGE,
            crate::lan::ALLOWED_RANGE_LEN
        )),
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
        LocalFirewall::Windows => "Windows Firewall is refusing them. Allow the rooms once — it covers every \
             room and game — in PowerShell run as administrator:"
            .to_string(),
        LocalFirewall::Unknown => format!(
            "Allow incoming connections on the room's adapter, {adapter}, in this machine's firewall."
        ),
    }
}

/// [`advice`] and [`fix_command`] as one line, for a log or a terminal.
pub fn advice_line(firewall: LocalFirewall, adapter: &str) -> String {
    match fix_command(firewall, adapter) {
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

// ---------------------------------------------------------------------------
// Opening the firewall: `lan-helper`'s half, run with privilege.

/// The rule an installed launcher's helper adds, and keeps.
pub const RULE_NAME: &str = "Lanthorn LAN rooms";
/// The rule a portable launcher's helper adds for one room, and removes.
pub const PORTABLE_RULE_NAME: &str = "Lanthorn LAN rooms (portable)";
/// The same two rules under the product's name before the rename to Lanthorn
/// (v0.2.31). An installed launcher keeps its rule on purpose, so without
/// this the old one would stay in every player's firewall forever: [`open`]
/// deletes the old rule of the same lifetime once the new one is in.
pub const LEGACY_RULE_NAMES: [&str; 2] =
    ["Mesh Game Servers LAN rooms", "Mesh Game Servers LAN rooms (portable)"];

/// How long what [`open`] adds should last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    /// An installed launcher: open once, for every room after.
    Kept,
    /// A portable launcher: open for this room, closed by [`close`].
    ThisRoom,
}

/// What [`open`] did, which is what [`close`] needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// This run opened it.
    Added(LocalFirewall),
    /// It was open already — the player's own rule, or an installed
    /// launcher's — and is not this run's to close.
    AlreadyOpen(LocalFirewall),
    /// No firewall this build knows how to open.
    Nothing,
}

/// Runs one program and says whether it succeeded, with what it printed. A
/// trait so a test can stand in for ufw, firewall-cmd, netsh and PowerShell.
pub trait Run {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<(bool, String)>;
}

/// The real programs.
pub struct System;

impl Run for System {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<(bool, String)> {
        let mut cmd = std::process::Command::new(program);
        // Every tool here prints translated text; this code reads English.
        cmd.args(args).env("LC_ALL", "C").stdin(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // A GUI launcher must not flash a console window per query.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let out = cmd.output()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok((out.status.success(), text))
    }
}

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

fn failed(what: &str, out: &str) -> std::io::Error {
    std::io::Error::other(format!("{what}: {}", out.trim()))
}

/// The room range as a firewall reads it.
fn room_range() -> String {
    format!("{}/{}", crate::lan::ALLOWED_RANGE, crate::lan::ALLOWED_RANGE_LEN)
}

/// Let the room's members through `firewall`: the adapter `adapter` on Linux,
/// the room range on Windows. Needs root, or an administrator.
pub fn open(
    r: &impl Run,
    firewall: LocalFirewall,
    adapter: &str,
    lifetime: Lifetime,
) -> std::io::Result<Opened> {
    crate::lan_adapter::validate_name(adapter)?;
    match firewall {
        LocalFirewall::Ufw => {
            let (ok, out) = r.run("ufw", &argv(&["allow", "in", "on", adapter]))?;
            if !ok {
                return Err(failed("ufw allow", &out));
            }
            // "Skipping adding existing rule": somebody's rule, never ours to
            // delete — even if only one of its v4/v6 halves was there.
            Ok(if out.contains("Skipping") {
                Opened::AlreadyOpen(firewall)
            } else {
                Opened::Added(firewall)
            })
        }
        LocalFirewall::Firewalld => {
            // `--change-interface`, not `--add-interface`: a desktop's network
            // manager may already have put a new adapter in another zone, and
            // adding it to a second one is an error.
            let change = format!("--change-interface={adapter}");
            let query = format!("--query-interface={adapter}");
            let mut added = false;
            if lifetime == Lifetime::Kept {
                let (there, _) =
                    r.run("firewall-cmd", &argv(&["--permanent", "--zone=trusted", &query]))?;
                if !there {
                    let (ok, out) =
                        r.run("firewall-cmd", &argv(&["--permanent", "--zone=trusted", &change]))?;
                    if !ok {
                        return Err(failed("firewall-cmd --permanent", &out));
                    }
                    added = true;
                }
            }
            let (there, _) = r.run("firewall-cmd", &argv(&["--zone=trusted", &query]))?;
            if !there {
                let (ok, out) = r.run("firewall-cmd", &argv(&["--zone=trusted", &change]))?;
                if !ok {
                    return Err(failed("firewall-cmd", &out));
                }
                added = true;
            }
            Ok(if added { Opened::Added(firewall) } else { Opened::AlreadyOpen(firewall) })
        }
        LocalFirewall::Windows => {
            let name = match lifetime {
                Lifetime::Kept => RULE_NAME,
                Lifetime::ThisRoom => PORTABLE_RULE_NAME,
            };
            // Replaced every time, so a rule somebody disabled or edited is
            // put back as it should be, and a crashed portable room's
            // leftover becomes this room's again.
            let name_arg = format!("name={name}");
            let _ =
                r.run("netsh", &argv(&["advfirewall", "firewall", "delete", "rule", &name_arg]))?;
            let remote = format!("remoteip={}", room_range());
            let (ok, out) = r.run(
                "netsh",
                &argv(&[
                    "advfirewall",
                    "firewall",
                    "add",
                    "rule",
                    &name_arg,
                    "dir=in",
                    "action=allow",
                    &remote,
                    "profile=any",
                    "enable=yes",
                ]),
            )?;
            if !ok {
                return Err(failed("netsh add rule", &out));
            }
            // The same rule under the old product name, now redundant.
            let legacy = match lifetime {
                Lifetime::Kept => LEGACY_RULE_NAMES[0],
                Lifetime::ThisRoom => LEGACY_RULE_NAMES[1],
            };
            let legacy_arg = format!("name={legacy}");
            let _ =
                r.run("netsh", &argv(&["advfirewall", "firewall", "delete", "rule", &legacy_arg]))?;
            Ok(Opened::Added(firewall))
        }
        LocalFirewall::Unknown => Ok(Opened::Nothing),
    }
}

/// Whether Windows holds a rule named `name` that lets the whole room range in,
/// on every network profile — not merely a rule of that name. Read through
/// PowerShell, whose values do not change with the display language the way
/// `netsh`'s labels do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleState {
    /// There, enabled, inbound, allowing the room range on every profile.
    Working,
    /// No rule of that name.
    Missing,
    /// There, but something about it keeps it from letting the room in.
    Broken(String),
}

fn rule_state_script(name: &str) -> String {
    format!(
        "$ErrorActionPreference = 'SilentlyContinue'; \
         $o = @(Get-NetFirewallRule -DisplayName '{}' | ForEach-Object {{ \
         [pscustomobject]@{{ enabled = ([string]$_.Enabled -eq 'True'); action = [string]$_.Action; \
         direction = [string]$_.Direction; profile = [string]$_.Profile; \
         remote = (($_ | Get-NetFirewallAddressFilter).RemoteAddress -join ',') }} }}); \
         [pscustomobject]@{{ rules = $o }} | ConvertTo-Json -Compress -Depth 4",
        name.replace('\'', "''")
    )
}

/// Read [`RuleState`] of the rule `name`.
pub fn rule_state(r: &impl Run, name: &str) -> std::io::Result<RuleState> {
    let (ok, out) = r.run("powershell", &powershell(&rule_state_script(name)))?;
    if !ok {
        return Err(failed("reading the firewall rule", &out));
    }
    parse_rule_state(&out).ok_or_else(|| failed("reading the firewall rule", &out))
}

/// [`rule_state`]'s parse, apart so it is tested without Windows.
pub fn parse_rule_state(json: &str) -> Option<RuleState> {
    use serde_json::Value;
    let v: Value = serde_json::from_str(json.trim()).ok()?;
    let rules: Vec<Value> = match &v["rules"] {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    };
    if rules.is_empty() {
        return Some(RuleState::Missing);
    }
    let text = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let mut why = String::new();
    for r in &rules {
        let problem = if !r.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
            "it is switched off"
        } else if !text(r, "action").eq_ignore_ascii_case("allow") {
            "it does not allow"
        } else if !text(r, "direction").eq_ignore_ascii_case("inbound") {
            "it is not an inbound rule"
        } else if !profile_is_every(&text(r, "profile")) {
            "it does not cover every network profile"
        } else if !remote_covers_room(&text(r, "remote")) {
            "it does not cover the room's addresses"
        } else {
            return Some(RuleState::Working);
        };
        why = problem.to_string();
    }
    Some(RuleState::Broken(why))
}

/// `Any`, or every named profile.
fn profile_is_every(profile: &str) -> bool {
    let p = profile.to_ascii_lowercase();
    p.contains("any") || ["domain", "private", "public"].iter().all(|n| p.contains(n))
}

/// Whether a rule's remote addresses (`Any`, or ranges as Windows prints
/// them: `198.18.0.0/255.254.0.0` or `198.18.0.0/15`, comma-separated) take in
/// the whole room range.
fn remote_covers_room(remote: &str) -> bool {
    let room_net = u32::from(crate::lan::ALLOWED_RANGE);
    let room_len = u32::from(crate::lan::ALLOWED_RANGE_LEN);
    remote.split(',').map(str::trim).any(|part| {
        if part.eq_ignore_ascii_case("any") {
            return true;
        }
        // A range: 198.18.0.0-198.19.255.255.
        if let Some((from, to)) = part.split_once('-') {
            let (Ok(from), Ok(to)) =
                (from.trim().parse::<std::net::Ipv4Addr>(), to.trim().parse::<std::net::Ipv4Addr>())
            else {
                return false;
            };
            let room_end = room_net | !(u32::MAX << (32 - room_len));
            return u32::from(from) <= room_net && u32::from(to) >= room_end;
        }
        let Some((net, mask)) = part.split_once('/') else { return false };
        let Ok(net) = net.parse::<std::net::Ipv4Addr>() else { return false };
        // The mask as bits, contiguous or not: a dotted mask Windows did not
        // validate (255.0.255.0) must not be read as its leading ones.
        let m = match mask.parse::<u32>() {
            Ok(n) if n <= 32 => if n == 0 { 0 } else { u32::MAX << (32 - n) },
            Ok(_) => return false,
            Err(_) => match mask.parse::<std::net::Ipv4Addr>() {
                Ok(m) => u32::from(m),
                Err(_) => return false,
            },
        };
        // It covers the room when it keeps none of the room's host bits and
        // agrees with the room on every network bit it keeps.
        let host_bits = !(u32::MAX << (32 - room_len));
        m & host_bits == 0 && (u32::from(net) ^ room_net) & m == 0
    })
}

/// Make sure the permanent room rule is there and working, adding or
/// replacing it only if it is not — so a check made while a room is up never
/// opens a gap by deleting a good rule. What the player never has to do.
///
/// `tried` remembers the state a repair was last made for: the same state
/// again means the repair changed nothing (or the check misreads a good rule),
/// and doing it every pass would only open a gap in a rule that is there.
pub fn ensure_kept(
    r: &impl Run,
    adapter: &str,
    tried: &mut Option<RuleState>,
) -> std::io::Result<RuleState> {
    let state = rule_state(r, RULE_NAME)?;
    if state == RuleState::Working {
        *tried = None;
        return Ok(state);
    }
    if tried.as_ref() == Some(&state) {
        return Ok(state);
    }
    *tried = Some(state.clone());
    open(r, LocalFirewall::Windows, adapter, Lifetime::Kept)?;
    rule_state(r, RULE_NAME)
}

/// Undo what [`open`] did for one room. Only a portable room closes anything,
/// and only what it added itself.
pub fn close(
    r: &impl Run,
    opened: Opened,
    adapter: &str,
    lifetime: Lifetime,
) -> std::io::Result<()> {
    let (Lifetime::ThisRoom, Opened::Added(firewall)) = (lifetime, opened) else { return Ok(()) };
    let (ok, out) = match firewall {
        LocalFirewall::Ufw => r.run("ufw", &argv(&["delete", "allow", "in", "on", adapter]))?,
        LocalFirewall::Firewalld => r.run(
            "firewall-cmd",
            &argv(&["--zone=trusted", &format!("--remove-interface={adapter}")]),
        )?,
        LocalFirewall::Windows => r.run(
            "netsh",
            &argv(&[
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={PORTABLE_RULE_NAME}"),
            ]),
        )?,
        LocalFirewall::Unknown => return Ok(()),
    };
    if ok {
        Ok(())
    } else {
        Err(failed("closing the firewall again", &out))
    }
}

/// A Windows Firewall rule blocking a program's incoming connections — what
/// Windows makes when someone clicks "Cancel" on a game's firewall prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRule {
    /// The rule's unique name, which [`unblock`] takes.
    pub name: String,
    pub display_name: String,
    pub program: String,
}

/// What Windows can say about its firewall without an administrator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsFacts {
    /// This build's room rule is there (either lifetime's).
    pub rule_present: bool,
    /// Enabled inbound Block rules for a program. A block beats any allow.
    pub blocks: Vec<BlockRule>,
    /// Third-party firewalls Windows Security reports as on, by name. They
    /// ignore Windows Firewall's rules, so only the player can open them.
    pub other_firewalls: Vec<String>,
    /// Each firewall profile's settings that decide whether an allow rule
    /// does anything: Domain, Private and Public.
    pub profiles: Vec<ProfileFacts>,
}

/// One Windows Firewall profile (Domain, Private or Public), as far as an
/// allow rule is concerned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileFacts {
    pub name: String,
    pub enabled: bool,
    /// "Block all incoming connections" is on, by the setting or by policy:
    /// every allow rule is ignored on this profile.
    pub blocks_all_incoming: bool,
    /// A policy makes this profile ignore rules made on this computer, so a
    /// rule Lanthorn adds does nothing until the policy allows it.
    pub ignores_local_rules: bool,
}

/// One PowerShell run; read-only, needs no administrator.
const FACTS_SCRIPT: &str = r#"$ErrorActionPreference = 'SilentlyContinue'
$rule = [bool](Get-NetFirewallRule -DisplayName 'Lanthorn LAN rooms*') -or [bool](Get-NetFirewallRule -DisplayName 'Mesh Game Servers LAN rooms*')
$blocks = @(Get-NetFirewallRule -Direction Inbound -Action Block -Enabled True | ForEach-Object {
  $p = ($_ | Get-NetFirewallApplicationFilter).Program
  if ($p -and $p -ne 'Any') { [pscustomobject]@{ name = $_.Name; display = $_.DisplayName; program = $p } }
})
$fw = @(Get-CimInstance -Namespace root/SecurityCenter2 -ClassName FirewallProduct | ForEach-Object {
  [pscustomobject]@{ name = $_.displayName; state = [int64]$_.productState }
})
$keys = @{ Domain = 'DomainProfile'; Private = 'StandardProfile'; Public = 'PublicProfile' }
$profiles = @(Get-NetFirewallProfile | ForEach-Object {
  $k = $keys[[string]$_.Name]
  $none = $false
  foreach ($path in @(('HKLM:\SOFTWARE\Policies\Microsoft\WindowsFirewall\' + $k), ('HKLM:\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\' + $k))) {
    if ((Get-ItemProperty -Path $path -Name DoNotAllowExceptions).DoNotAllowExceptions -eq 1) { $none = $true }
  }
  [pscustomobject]@{
    name = [string]$_.Name
    enabled = ([string]$_.Enabled -eq 'True')
    blocks_all = $none
    local_rules = [string]$_.AllowLocalFirewallRules
  }
})
[pscustomobject]@{ rule = $rule; blocks = $blocks; firewalls = $fw; profiles = $profiles } | ConvertTo-Json -Compress -Depth 4
"#;

/// What, in this machine's Windows Firewall, stops Lanthorn's rule from
/// letting a room through — whatever the game: the rule names no program and
/// no port, only the room's addresses, so a game added later needs nothing
/// more. Said before anything is dropped, in words a player can act on.
pub fn rule_problems(facts: &WindowsFacts) -> Vec<String> {
    let names = |pred: &dyn Fn(&ProfileFacts) -> bool| {
        facts
            .profiles
            .iter()
            .filter(|p| p.enabled && pred(p))
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(" and ")
    };
    let mut out = Vec::new();
    if !facts.rule_present {
        out.push(
            "Lanthorn's Windows Firewall rule is not there, so Windows will drop other players' \
             connections to games you host. Press Fix it to add it for good."
                .to_string(),
        );
    }
    let blocked = names(&|p| p.blocks_all_incoming);
    if !blocked.is_empty() {
        out.push(format!(
            "Windows is set to block all incoming connections on its {blocked} network profile, \
             which ignores every allow rule, Lanthorn's included. Turn off \"Block all incoming \
             connections\" for that network under Windows Security → Firewall & network protection."
        ));
    }
    let ignored = names(&|p| p.ignores_local_rules && !p.blocks_all_incoming);
    if !ignored.is_empty() {
        out.push(format!(
            "A policy on this computer makes the {ignored} network profile ignore firewall rules made \
             here, so Lanthorn's rule does nothing there until that policy allows it. Ask whoever \
             manages this computer."
        ));
    }
    out
}

fn powershell(script: &str) -> Vec<String> {
    argv(&["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
}

/// Read [`WindowsFacts`].
pub fn windows_facts(r: &impl Run) -> std::io::Result<WindowsFacts> {
    let (ok, out) = r.run("powershell", &powershell(FACTS_SCRIPT))?;
    if !ok {
        return Err(failed("reading Windows Firewall", &out));
    }
    parse_windows_facts(&out).ok_or_else(|| failed("reading Windows Firewall", &out))
}

/// [`windows_facts`]'s parse, apart so it is tested without Windows.
/// PowerShell 5.1 writes a one-element array as a bare object; both are read.
pub fn parse_windows_facts(json: &str) -> Option<WindowsFacts> {
    use serde_json::Value;
    let v: Value = serde_json::from_str(json.trim()).ok()?;
    let list = |v: &Value| -> Vec<Value> {
        match v {
            Value::Array(a) => a.clone(),
            Value::Null => Vec::new(),
            other => vec![other.clone()],
        }
    };
    let text =
        |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let blocks = list(&v["blocks"])
        .iter()
        .map(|b| BlockRule {
            name: text(b, "name"),
            display_name: text(b, "display"),
            program: text(b, "program"),
        })
        .filter(|b| !b.name.is_empty())
        .collect();
    let other_firewalls = list(&v["firewalls"])
        .iter()
        // Windows Security's productState: bits 12–15 are 1 when it is on.
        .filter(|f| f.get("state").and_then(Value::as_i64).is_some_and(|s| (s >> 12) & 0xf == 1))
        .map(|f| text(f, "name"))
        .filter(|n| !n.is_empty() && !n.to_ascii_lowercase().contains("windows"))
        .collect();
    let profiles = list(&v["profiles"])
        .iter()
        .map(|p| ProfileFacts {
            name: text(p, "name"),
            enabled: p.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            blocks_all_incoming: p.get("blocks_all").and_then(Value::as_bool).unwrap_or(false),
            // "False" is the only value that turns local rules off; "True" and
            // "NotConfigured" leave them on.
            ignores_local_rules: text(p, "local_rules").eq_ignore_ascii_case("false"),
        })
        .filter(|p| !p.name.is_empty())
        .collect();
    Some(WindowsFacts {
        profiles,
        rule_present: v["rule"].as_bool().unwrap_or(false),
        blocks,
        other_firewalls,
    })
}

/// Disable the Block rules named — each only if it really is an inbound Block
/// rule — and say how many were. Needs an administrator.
pub fn unblock(r: &impl Run, names: &[String]) -> std::io::Result<usize> {
    if names.is_empty() {
        return Ok(0);
    }
    // Single-quoted PowerShell strings interpolate nothing; a quote inside is
    // doubled, which is the whole of their escaping.
    let list: Vec<String> = names.iter().map(|n| format!("'{}'", n.replace('\'', "''"))).collect();
    let script = format!(
        "$c = 0; foreach ($n in @({})) {{ Get-NetFirewallRule -Name $n -ErrorAction SilentlyContinue | \
         Where-Object {{ $_.Direction -eq 'Inbound' -and $_.Action -eq 'Block' }} | \
         ForEach-Object {{ $_ | Disable-NetFirewallRule; $c++ }} }}; $c",
        list.join(",")
    );
    let (ok, out) = r.run("powershell", &powershell(&script))?;
    if !ok {
        return Err(failed("unblocking", &out));
    }
    out.trim()
        .lines()
        .last()
        .and_then(|l| l.trim().parse().ok())
        .ok_or_else(|| failed("unblocking", &out))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The commands open the room and nothing else.
    #[test]
    fn every_command_names_the_room_adapter() {
        for fw in [LocalFirewall::Ufw, LocalFirewall::Firewalld] {
            let cmd = fix_command(fw, "gbl0").unwrap();
            assert!(cmd.contains("gbl0"), "{fw:?}: {cmd}");
        }
        // Windows: one rule, for every room and game, matched on the room range
        // because each room's adapter is a new one.
        let win = fix_command(LocalFirewall::Windows, "gbl0").unwrap();
        assert!(win.contains("-RemoteAddress 198.18.0.0/15"), "{win}");
        assert!(!win.contains("InterfaceAlias"), "{win}");
        assert_eq!(win, fix_command(LocalFirewall::Windows, "gbl0").unwrap());
        assert!(fix_command(LocalFirewall::Unknown, "gbl0").is_none());
        let unknown = advice_line(LocalFirewall::Unknown, "gbl0");
        assert!(unknown.contains("gbl0"), "{unknown}");
        let ufw = advice_line(LocalFirewall::Ufw, "gbl0");
        assert!(ufw.ends_with("sudo ufw allow in on gbl0"), "{ufw}");
    }

    #[test]
    fn a_heads_up_is_only_for_a_firewall_whose_default_bites() {
        assert!(heads_up(LocalFirewall::Ufw).is_some());
        assert!(heads_up(LocalFirewall::Firewalld).is_some());
        assert!(heads_up(LocalFirewall::Windows).is_none());
        assert!(heads_up(LocalFirewall::Unknown).is_none());
    }

    /// Stands in for the firewall tools: answers by the start of the command
    /// line, and records every call.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        answers: Vec<(&'static str, bool, &'static str)>,
    }

    impl Run for Fake {
        fn run(&self, program: &str, args: &[String]) -> std::io::Result<(bool, String)> {
            let line = format!("{program} {}", args.join(" "));
            self.calls.lock().unwrap().push(line.clone());
            for (prefix, ok, out) in &self.answers {
                if line.starts_with(prefix) {
                    return Ok((*ok, out.to_string()));
                }
            }
            Ok((true, String::new()))
        }
    }

    impl Fake {
        fn with(answers: Vec<(&'static str, bool, &'static str)>) -> Self {
            Self { answers, ..Default::default() }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[test]
    fn ufw_is_opened_for_the_room_adapter_and_a_portable_room_closes_what_it_added() {
        let f = Fake::with(vec![("ufw allow", true, "Rule added\nRule added (v6)\n")]);
        let o = open(&f, LocalFirewall::Ufw, "gbl0", Lifetime::ThisRoom).unwrap();
        assert_eq!(o, Opened::Added(LocalFirewall::Ufw));
        close(&f, o, "gbl0", Lifetime::ThisRoom).unwrap();
        assert_eq!(f.calls(), ["ufw allow in on gbl0", "ufw delete allow in on gbl0"]);
    }

    /// The first player's own laptop: they had typed the rule by hand before
    /// the launcher could. A portable room must not take it away again.
    #[test]
    fn a_ufw_rule_that_was_already_there_is_never_deleted() {
        let f = Fake::with(vec![(
            "ufw allow",
            true,
            "Skipping adding existing rule\nSkipping adding existing rule (v6)\n",
        )]);
        let o = open(&f, LocalFirewall::Ufw, "gbl0", Lifetime::ThisRoom).unwrap();
        assert_eq!(o, Opened::AlreadyOpen(LocalFirewall::Ufw));
        close(&f, o, "gbl0", Lifetime::ThisRoom).unwrap();
        assert_eq!(f.calls().len(), 1, "{:?}", f.calls());
    }

    #[test]
    fn an_installed_room_never_closes_anything() {
        let f = Fake::with(vec![("ufw allow", true, "Rule added\n")]);
        let o = open(&f, LocalFirewall::Ufw, "gbl0", Lifetime::Kept).unwrap();
        close(&f, o, "gbl0", Lifetime::Kept).unwrap();
        assert_eq!(f.calls(), ["ufw allow in on gbl0"]);
    }

    #[test]
    fn firewalld_trusts_the_adapter_for_good_when_installed_and_for_now_when_portable() {
        let f = Fake::with(vec![
            ("firewall-cmd --permanent --zone=trusted --query", false, "no"),
            ("firewall-cmd --zone=trusted --query", false, "no"),
        ]);
        let o = open(&f, LocalFirewall::Firewalld, "gbl0", Lifetime::Kept).unwrap();
        assert_eq!(o, Opened::Added(LocalFirewall::Firewalld));
        let calls = f.calls();
        assert!(calls
            .iter()
            .any(|c| c == "firewall-cmd --permanent --zone=trusted --change-interface=gbl0"));
        assert!(calls.iter().any(|c| c == "firewall-cmd --zone=trusted --change-interface=gbl0"));

        let f = Fake::with(vec![("firewall-cmd --zone=trusted --query", false, "no")]);
        let o = open(&f, LocalFirewall::Firewalld, "gbl0", Lifetime::ThisRoom).unwrap();
        assert!(!f.calls().iter().any(|c| c.contains("--permanent")), "{:?}", f.calls());
        close(&f, o, "gbl0", Lifetime::ThisRoom).unwrap();
        assert_eq!(
            f.calls().last().unwrap(),
            "firewall-cmd --zone=trusted --remove-interface=gbl0"
        );

        let already = Fake::default(); // every query answers yes
        assert_eq!(
            open(&already, LocalFirewall::Firewalld, "gbl0", Lifetime::Kept).unwrap(),
            Opened::AlreadyOpen(LocalFirewall::Firewalld)
        );
    }

    /// One rule, on the room range, replaced every time; a portable room's
    /// own name, so it never removes an installed launcher's.
    #[test]
    fn windows_gets_one_rule_on_the_room_range_and_portable_keeps_to_its_own() {
        let f = Fake::default();
        let o = open(&f, LocalFirewall::Windows, "gbl0", Lifetime::Kept).unwrap();
        close(&f, o, "gbl0", Lifetime::Kept).unwrap();
        let calls = f.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[0], "netsh advfirewall firewall delete rule name=Lanthorn LAN rooms");
        assert_eq!(
            calls[1],
            "netsh advfirewall firewall add rule name=Lanthorn LAN rooms dir=in action=allow \
             remoteip=198.18.0.0/15 profile=any enable=yes"
        );
        // The pre-rename rule goes, or an installed launcher's would stay forever.
        assert_eq!(calls[2], "netsh advfirewall firewall delete rule name=Mesh Game Servers LAN rooms");

        let f = Fake::default();
        let o = open(&f, LocalFirewall::Windows, "gbl0", Lifetime::ThisRoom).unwrap();
        close(&f, o, "gbl0", Lifetime::ThisRoom).unwrap();
        // Its own rule, and the old name's portable rule, never an installed one.
        assert!(f.calls().iter().all(|c| c.contains("(portable)")), "{:?}", f.calls());
        assert!(f.calls().iter().any(|c| c.contains(PORTABLE_RULE_NAME)), "{:?}", f.calls());
        assert!(f.calls().last().unwrap().contains("delete rule"));
    }

    #[test]
    fn a_failed_open_is_an_error_and_an_unknown_firewall_is_left_alone() {
        let f = Fake::with(vec![("ufw", false, "ERROR: You need to be root to run this script")]);
        let e = open(&f, LocalFirewall::Ufw, "gbl0", Lifetime::Kept).unwrap_err().to_string();
        assert!(e.contains("root"), "{e}");
        let f = Fake::default();
        assert_eq!(
            open(&f, LocalFirewall::Unknown, "gbl0", Lifetime::Kept).unwrap(),
            Opened::Nothing
        );
        assert!(f.calls().is_empty());
        assert!(
            open(&f, LocalFirewall::Ufw, "eth0", Lifetime::Kept).is_err(),
            "only a room adapter"
        );
        assert!(f.calls().is_empty(), "refused before anything ran");
    }

    const GOOD: &str = r#"{"rules":{"enabled":true,"action":"Allow","direction":"Inbound","profile":"Any","remote":"198.18.0.0/255.254.0.0"}}"#;

    #[test]
    fn a_rule_is_working_only_if_it_lets_the_whole_room_in_on_every_profile() {
        assert_eq!(parse_rule_state(GOOD), Some(RuleState::Working));
        // Windows prints a range as a mask or a prefix; a wider one, or Any, covers it.
        for remote in ["198.18.0.0/15", "198.0.0.0/255.0.0.0", "Any", "10.0.0.0/8,198.18.0.0/15"] {
            let j = GOOD.replace("198.18.0.0/255.254.0.0", remote);
            assert_eq!(parse_rule_state(&j), Some(RuleState::Working), "{remote}");
        }
        let every = GOOD.replace(r#""Any""#, r#""Domain, Private, Public""#);
        assert_eq!(parse_rule_state(&every), Some(RuleState::Working));
        let broken = |from: &str, to: &str, why: &str| {
            assert_eq!(
                parse_rule_state(&GOOD.replace(from, to)),
                Some(RuleState::Broken(why.into())),
                "{to}"
            );
        };
        broken("true", "false", "it is switched off");
        broken("Allow", "Block", "it does not allow");
        broken("Inbound", "Outbound", "it is not an inbound rule");
        broken(r#""Any""#, r#""Public""#, "it does not cover every network profile");
        // Narrower than the room range: only half the rooms.
        broken("198.18.0.0/255.254.0.0", "198.19.0.0/255.255.0.0", "it does not cover the room's addresses");
        broken("198.18.0.0/255.254.0.0", "192.168.0.0/16", "it does not cover the room's addresses");
        assert_eq!(parse_rule_state(r#"{"rules":null}"#), Some(RuleState::Missing));
        assert_eq!(parse_rule_state("Get-NetFirewallRule : not recognized"), None);
    }

    #[test]
    fn a_good_rule_is_left_alone_and_a_missing_or_broken_one_is_put_back() {
        let broken = GOOD.replace("true", "false");
        let missing = r#"{"rules":null}"#;
        // Good: read, nothing changed — no gap while a room is up.
        let f = Fake::with(vec![("powershell", true, GOOD)]);
        assert_eq!(ensure_kept(&f, "gbl0", &mut None).unwrap(), RuleState::Working);
        assert!(f.calls().iter().all(|c| !c.starts_with("netsh")), "{:?}", f.calls());

        // Missing, then added: netsh ran, and the second read is good.
        struct Healing(Mutex<u32>);
        impl Run for Healing {
            fn run(&self, program: &str, _: &[String]) -> std::io::Result<(bool, String)> {
                if program == "netsh" {
                    *self.0.lock().unwrap() += 1;
                    return Ok((true, String::new()));
                }
                let added = *self.0.lock().unwrap() > 0;
                Ok((true, if added { GOOD } else { r#"{"rules":null}"# }.to_string()))
            }
        }
        let h = Healing(Mutex::new(0));
        assert_eq!(ensure_kept(&h, "gbl0", &mut None).unwrap(), RuleState::Working);
        assert!(*h.0.lock().unwrap() >= 2, "deleted the stale one and added ours");

        // A rule that stays broken is reported, not claimed fixed.
        let f = Fake::with(vec![("powershell", true, Box::leak(broken.into_boxed_str()))]);
        let mut tried = None;
        assert_eq!(
            ensure_kept(&f, "gbl0", &mut tried).unwrap(),
            RuleState::Broken("it is switched off".into())
        );
        let adds = |f: &Fake| f.calls().iter().filter(|c| c.contains("add")).count();
        assert_eq!(adds(&f), 1, "repaired once");
        // The same state again: that repair changed nothing, so the rule is
        // not deleted and re-added on every pass.
        ensure_kept(&f, "gbl0", &mut tried).unwrap();
        ensure_kept(&f, "gbl0", &mut tried).unwrap();
        assert_eq!(adds(&f), 1, "not repaired again: {:?}", f.calls());
        let _ = missing;
    }

    #[test]
    fn windows_may_print_the_rooms_range_as_a_range() {
        assert!(remote_covers_room("198.18.0.0-198.19.255.255"));
        assert!(remote_covers_room("10.0.0.1-10.0.0.9, 198.0.0.0-199.255.255.255"));
        assert!(!remote_covers_room("198.18.0.0-198.18.255.255"), "half the room range");
        assert!(!remote_covers_room("198.19.0.0-198.19.255.255"));
        assert!(!remote_covers_room("garbage-and-more"));
        // A non-contiguous mask is not a prefix: it must not "cover" the room.
        assert!(!remote_covers_room("198.18.0.0/255.0.255.0"));
        assert!(!remote_covers_room("198.0.0.0/255.0.255.0"));
        assert!(remote_covers_room("0.0.0.0/0"));
    }

    fn profile(name: &str, enabled: bool, blocks_all: bool, ignores: bool) -> ProfileFacts {
        ProfileFacts {
            name: name.into(),
            enabled,
            blocks_all_incoming: blocks_all,
            ignores_local_rules: ignores,
        }
    }

    #[test]
    fn profile_settings_are_read_and_only_a_false_local_rules_value_counts() {
        let json = r#"{"rule":true,"blocks":[],"firewalls":[],"profiles":[
            {"name":"Domain","enabled":true,"blocks_all":false,"local_rules":"NotConfigured"},
            {"name":"Private","enabled":true,"blocks_all":false,"local_rules":"False"},
            {"name":"Public","enabled":false,"blocks_all":true,"local_rules":"True"}]}"#;
        let f = parse_windows_facts(json).unwrap();
        assert_eq!(
            f.profiles,
            [
                profile("Domain", true, false, false),
                profile("Private", true, false, true),
                profile("Public", false, true, false),
            ]
        );
        // A PowerShell 5.1 single profile is a bare object; no profiles at all
        // (an old script's output) is none, not a failure.
        let one = r#"{"rule":true,"profiles":{"name":"Public","enabled":true,"blocks_all":false,"local_rules":"True"}}"#;
        assert_eq!(parse_windows_facts(one).unwrap().profiles.len(), 1);
        assert!(parse_windows_facts(r#"{"rule":true}"#).unwrap().profiles.is_empty());
    }

    /// Whatever the game: the rule names no program and no port, so what
    /// stops it from working is the same for every game, now and later.
    #[test]
    fn what_stops_the_rule_is_named_and_a_healthy_firewall_says_nothing() {
        let healthy = WindowsFacts {
            rule_present: true,
            profiles: vec![profile("Public", true, false, false)],
            ..Default::default()
        };
        assert!(rule_problems(&healthy).is_empty());

        let missing = WindowsFacts { rule_present: false, ..healthy.clone() };
        let said = rule_problems(&missing);
        assert_eq!(said.len(), 1);
        assert!(said[0].contains("rule is not there") && said[0].contains("Fix it"), "{said:?}");

        // "Block all incoming connections" beats every allow; a profile that
        // is switched off cannot block anything.
        let block_all = WindowsFacts {
            profiles: vec![profile("Public", true, true, false), profile("Private", false, true, false)],
            ..healthy.clone()
        };
        let said = rule_problems(&block_all);
        assert_eq!(said.len(), 1);
        assert!(said[0].contains("Public") && !said[0].contains("Private"), "{said:?}");
        assert!(said[0].contains("Block all incoming connections"), "{said:?}");

        // A policy that ignores local rules; said once, not also as the
        // block-all it would be redundant with.
        let policy = WindowsFacts {
            profiles: vec![profile("Domain", true, false, true), profile("Public", true, true, true)],
            ..healthy
        };
        let said = rule_problems(&policy);
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(said[1].contains("Domain") && !said[1].contains("Public"), "{said:?}");
    }

    #[test]
    fn windows_facts_read_one_or_many_and_name_only_a_firewall_that_is_on() {
        let one = r#"{"rule":false,"blocks":{"name":"TCP Query User{A}C:\\Games\\speed2.exe","display":"speed2.exe","program":"C:\\Games\\speed2.exe"},"firewalls":[{"name":"Norton Firewall","state":266240},{"name":"Old Firewall","state":262144},{"name":"Windows Firewall","state":266240}]}"#;
        let f = parse_windows_facts(one).unwrap();
        assert!(!f.rule_present);
        assert_eq!(f.blocks.len(), 1);
        assert_eq!(f.blocks[0].display_name, "speed2.exe");
        assert_eq!(f.other_firewalls, ["Norton Firewall"]);
        let none = r#"{"rule":true,"blocks":[],"firewalls":null}"#;
        assert_eq!(
            parse_windows_facts(none).unwrap(),
            WindowsFacts { rule_present: true, ..Default::default() }
        );
        assert!(parse_windows_facts("Get-NetFirewallRule : not recognized").is_none());
    }

    #[test]
    fn unblock_names_are_data_to_powershell() {
        let f = Fake::with(vec![("powershell", true, "1\n")]);
        let n = unblock(&f, &["it's'; Remove-Item C:\\ -Recurse; '".to_string()]).unwrap();
        assert_eq!(n, 1);
        let call = f.calls().pop().unwrap();
        assert!(call.contains(r"'it''s''; Remove-Item C:\ -Recurse; '''"), "{call}");
        assert_eq!(unblock(&Fake::default(), &[]).unwrap(), 0);
    }
}
