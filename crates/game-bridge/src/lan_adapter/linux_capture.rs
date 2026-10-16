//! Linux: bringing to the room what routes alone cannot (`linux.rs`).
//!
//! The adapter's routes decide where a packet goes only when the kernel looks
//! them up, and for three kinds of game traffic it never does:
//!
//! - **A broadcast or multicast from a socket bound to the Wi-Fi's address.**
//!   The kernel sends it out of the interface that owns the address, before
//!   any route is consulted (`ip_route_output_key_hash_rcu`, the "special
//!   hack" for multicast and limited broadcast in `net/ipv4/route.c`). The
//!   room's `255.255.255.255` route helps only a socket on `0.0.0.0`.
//! - **A broadcast to the Wi-Fi's own network**, `192.168.1.255`, which a game
//!   computes from the Wi-Fi's address: the local table's broadcast route,
//!   consulted before any other, sends it out of the Wi-Fi.
//! - **Everything from a UDP socket Wine pinned to the Wi-Fi.** Wine turns a
//!   UDP bind to an interface's address into a bind to every address plus
//!   `SO_BINDTODEVICE` (`server/sock.c`, `bind_to_interface`): every packet it
//!   sends leaves by the Wi-Fi — its unicasts to room members too, to the
//!   default gateway — and it accepts only packets that *arrive on* the
//!   Wi-Fi.
//!
//! So while a room is up, the helper puts traffic-control rules (`clsact`,
//! `u32`, `mirred`: all in the kernel since long before any supported distro)
//! on this machine's other interfaces and on the room adapter:
//!
//! - **On each other interface, outgoing:** a packet for the room range is
//!   *redirected* into the room adapter (it could only have been lost there);
//!   a UDP broadcast or multicast from that interface's own address — limited,
//!   or to its own network — is *copied* into it, and still goes to the real
//!   network as before. The pump translates what arrives (`lan_rebind.rs`) and
//!   passes on only the game's ports (`lan_filter.rs`).
//! - **On the room adapter, incoming:** a UDP packet the pump translated for a
//!   socket pinned to an interface — and marked so, with
//!   [`crate::lan_rebind::PINNED_DSCP`] — is handed in *on that interface*,
//!   where the socket accepts it. Nothing else is touched: a packet without
//!   the mark, or for any address but that interface's own, goes in on the
//!   room adapter as always.
//!
//! # Rules a later change could quietly break
//!
//! - **Only ioctls and netlink, no programs.** Like the adapter: a helper with
//!   a file capability does not pass it to a child (`linux.rs`).
//! - **Gone with the room, and harmless if not.** The rules are removed when
//!   the helper's relay ends. A helper killed outright cannot remove them, so
//!   they are made harmless without it: a copy to a device that is gone is no
//!   copy, and a redirect to one drops only room-range packets, which had
//!   nowhere to go. The next room removes them first ([`RULE_PRIO`]).
//! - **Never another program's rules.** Every rule here sits at its own
//!   priority, removed by that priority alone; an existing `clsact` is used,
//!   never replaced, and removed only if this made it.
//! - **A copy, never a theft, on a real network.** What a real interface sends
//!   to its own network still goes there; only room-range packets, which
//!   belong to no real network, are taken.

use std::collections::HashMap;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::time::Duration;

use crate::lan_rebind::{local_networks, LocalNetwork, PINNED_DSCP};

/// The priority every rule here is added at, and removed by. Fixed, so a room
/// can remove what a killed one left behind.
pub const RULE_PRIO: u16 = 0xc1a5;
/// How often interfaces and their addresses are looked at again: the Wi-Fi
/// joining another network, a cable plugged in.
const REFRESH: Duration = Duration::from_secs(2);

const RTM_NEWQDISC: u16 = 36;
const RTM_DELQDISC: u16 = 37;
const RTM_NEWTFILTER: u16 = 44;
const RTM_DELTFILTER: u16 = 45;
const NLMSG_ERROR: u16 = 2;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLA_F_NESTED: u16 = 0x8000;

const TC_H_CLSACT: u32 = 0xffff_fff1;
const CLSACT_HANDLE: u32 = 0xffff_0000;
const PARENT_INGRESS: u32 = 0xffff_fff2;
const PARENT_EGRESS: u32 = 0xffff_fff3;
const ETH_P_IP: u16 = 0x0800;

const TCA_KIND: u16 = 1;
const TCA_OPTIONS: u16 = 2;
const TCA_U32_SEL: u16 = 5;
const TCA_U32_ACT: u16 = 7;
const TCA_ACT_KIND: u16 = 1;
const TCA_ACT_OPTIONS: u16 = 2;
const TCA_MIRRED_PARMS: u16 = 2;
const TC_U32_TERMINAL: u8 = 1;

const TCA_EGRESS_REDIR: i32 = 1;
const TCA_EGRESS_MIRROR: i32 = 2;
const TCA_INGRESS_REDIR: i32 = 3;
const TC_ACT_PIPE: i32 = 3;
const TC_ACT_STOLEN: i32 = 4;

/// One `u32` match: the 32-bit word at `off` bytes into the IPv4 header,
/// masked, equals `val`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    mask: u32,
    val: u32,
    off: i32,
}

fn prefix_key(off: i32, addr: Ipv4Addr, len: u8) -> Key {
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - u32::from(len)) };
    Key { mask, val: u32::from(addr) & mask, off }
}

fn dst(addr: Ipv4Addr, len: u8) -> Key {
    prefix_key(16, addr, len)
}

fn src(addr: Ipv4Addr) -> Key {
    prefix_key(12, addr, 32)
}

/// The protocol byte, the second of the word at offset 8.
const UDP: Key = Key { mask: 0x00ff_0000, val: 0x0011_0000, off: 8 };

/// The DSCP bits of the second byte of the header.
const PINNED: Key = Key { mask: 0x00fc_0000, val: (PINNED_DSCP as u32) << 18, off: 0 };

/// What a matching packet does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Out of `ifindex` instead.
    Redirect(u32),
    /// A copy out of `ifindex`; the original carries on.
    Mirror(u32),
    /// In on `ifindex` instead, as if it had arrived there.
    Deliver(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    keys: Vec<Key>,
    action: Action,
}

/// The rules one interface should have, at one hook.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wanted {
    ifindex: u32,
    parent: u32,
    rules: Vec<Rule>,
}

/// What this capture put on an interface.
#[derive(Debug)]
struct Installed {
    rules: Vec<Rule>,
    /// This capture added the interface's `clsact`, and so removes it.
    made_clsact: bool,
}

/// The rules for a room on the adapter `room` (index `room_index`), given
/// this machine's other networks. Pure, so the rule set is tested without a
/// kernel.
fn plan(room: &str, room_index: u32, networks: &[(LocalNetwork, u32)]) -> Vec<Wanted> {
    let range = (crate::lan::ALLOWED_RANGE, crate::lan::ALLOWED_RANGE_LEN);
    let mut by_index: Vec<Wanted> = Vec::new();
    let mut deliver: Vec<Rule> = Vec::new();
    for (n, ifindex) in networks {
        if n.name == "lo"
            || n.name.starts_with(super::ADAPTER_PREFIX)
            || n.name == room
            || *ifindex == room_index
            || n.address.is_loopback()
            || in_range(n.address, range)
        {
            continue;
        }
        let w = match by_index.iter_mut().find(|w| w.ifindex == *ifindex) {
            Some(w) => w,
            None => {
                by_index.push(Wanted {
                    ifindex: *ifindex,
                    parent: PARENT_EGRESS,
                    rules: vec![Rule {
                        keys: vec![dst(range.0, range.1)],
                        action: Action::Redirect(room_index),
                    }],
                });
                by_index.last_mut().expect("just pushed")
            }
        };
        let mut copies = vec![dst(Ipv4Addr::BROADCAST, 32), dst(Ipv4Addr::new(224, 0, 0, 0), 4)];
        if let Some(b) = n.broadcast() {
            copies.push(dst(b, 32));
        }
        for d in copies {
            let keys = vec![src(n.address), d, UDP];
            w.rules.push(Rule { keys, action: Action::Mirror(room_index) });
        }
        deliver.push(Rule {
            keys: vec![PINNED, dst(n.address, 32), UDP],
            action: Action::Deliver(*ifindex),
        });
    }
    if !deliver.is_empty() {
        by_index.push(Wanted { ifindex: room_index, parent: PARENT_INGRESS, rules: deliver });
    }
    by_index
}

fn in_range(a: Ipv4Addr, (prefix, len): (Ipv4Addr, u8)) -> bool {
    let mask = u32::MAX << (32 - u32::from(len));
    u32::from(a) & mask == u32::from(prefix) & mask
}

// ---------------------------------------------------------------------------
// Netlink

struct Netlink {
    fd: OwnedFd,
    seq: u32,
}

impl Netlink {
    fn open() -> io::Result<Self> {
        // SAFETY: plain socket creation.
        let raw = unsafe {
            libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE)
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `socket` returned a descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: sockaddr_nl is plain data; all-zero but the family is the
        // kernel picking our port id.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // SAFETY: a sockaddr_nl of the size given.
        let r = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_nl).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        // A request that is never answered must not hang the helper.
        let tv = libc::timeval { tv_sec: 2, tv_usec: 0 };
        // SAFETY: a timeval of the size given.
        unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&tv as *const libc::timeval).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        Ok(Self { fd, seq: 0 })
    }

    /// Send one request and wait for its acknowledgement.
    fn request(&mut self, kind: u16, flags: u16, body: &[u8]) -> io::Result<()> {
        self.seq = self.seq.wrapping_add(1);
        let len = 16 + body.len();
        let mut msg = Vec::with_capacity(len);
        msg.extend_from_slice(&(len as u32).to_ne_bytes());
        msg.extend_from_slice(&kind.to_ne_bytes());
        msg.extend_from_slice(&(flags | NLM_F_REQUEST | NLM_F_ACK).to_ne_bytes());
        msg.extend_from_slice(&self.seq.to_ne_bytes());
        msg.extend_from_slice(&0u32.to_ne_bytes());
        msg.extend_from_slice(body);
        // SAFETY: sending a buffer we own, of its own length.
        let sent = unsafe { libc::send(self.fd.as_raw_fd(), msg.as_ptr().cast(), msg.len(), 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; 16384];
        loop {
            // SAFETY: receiving into a buffer we own, of its own length.
            let n =
                unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut at = 0usize;
            let n = n as usize;
            while at + 16 <= n {
                let mlen = u32::from_ne_bytes(buf[at..at + 4].try_into().expect("4")) as usize;
                let mtype = u16::from_ne_bytes(buf[at + 4..at + 6].try_into().expect("2"));
                let mseq = u32::from_ne_bytes(buf[at + 8..at + 12].try_into().expect("4"));
                if mlen < 16 || at + mlen > n {
                    break;
                }
                if mtype == NLMSG_ERROR && mseq == self.seq && mlen >= 20 {
                    let code = i32::from_ne_bytes(buf[at + 16..at + 20].try_into().expect("4"));
                    return if code == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(-code)) };
                }
                at += (mlen + 3) & !3;
            }
        }
    }
}

/// A netlink message body: a `tcmsg` and its attributes.
struct Body(Vec<u8>);

impl Body {
    fn tc(ifindex: u32, handle: u32, parent: u32, info: u32) -> Self {
        let mut b = Vec::with_capacity(256);
        b.extend_from_slice(&[0u8, 0, 0, 0]); // family, padding
        b.extend_from_slice(&(ifindex as i32).to_ne_bytes());
        b.extend_from_slice(&handle.to_ne_bytes());
        b.extend_from_slice(&parent.to_ne_bytes());
        b.extend_from_slice(&info.to_ne_bytes());
        Self(b)
    }

    fn attr(&mut self, kind: u16, data: &[u8]) {
        self.0.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
        self.0.extend_from_slice(&kind.to_ne_bytes());
        self.0.extend_from_slice(data);
        self.pad();
    }

    fn nest(&mut self, kind: u16, fill: impl FnOnce(&mut Self)) {
        let at = self.0.len();
        self.0.extend_from_slice(&[0, 0]);
        self.0.extend_from_slice(&(kind | NLA_F_NESTED).to_ne_bytes());
        fill(self);
        let len = (self.0.len() - at) as u16;
        self.0[at..at + 2].copy_from_slice(&len.to_ne_bytes());
    }

    fn pad(&mut self) {
        while !self.0.len().is_multiple_of(4) {
            self.0.push(0);
        }
    }
}

/// `prio` and protocol, as `tcmsg.tcm_info` carries them.
fn filter_info() -> u32 {
    (u32::from(RULE_PRIO) << 16) | u32::from(ETH_P_IP.to_be())
}

/// `struct tc_u32_sel` and its keys.
fn selector(keys: &[Key]) -> Vec<u8> {
    let mut s = vec![TC_U32_TERMINAL, 0, keys.len() as u8, 0];
    s.extend_from_slice(&[0u8; 12]); // offmask, off, offoff, hoff, hmask
    for k in keys {
        s.extend_from_slice(&k.mask.to_be_bytes());
        s.extend_from_slice(&k.val.to_be_bytes());
        s.extend_from_slice(&k.off.to_ne_bytes());
        s.extend_from_slice(&0i32.to_ne_bytes()); // offmask
    }
    s
}

/// `struct tc_mirred`.
fn mirred(action: Action) -> Vec<u8> {
    let (verdict, eaction, ifindex) = match action {
        Action::Redirect(i) => (TC_ACT_STOLEN, TCA_EGRESS_REDIR, i),
        Action::Mirror(i) => (TC_ACT_PIPE, TCA_EGRESS_MIRROR, i),
        Action::Deliver(i) => (TC_ACT_STOLEN, TCA_INGRESS_REDIR, i),
    };
    let mut p = Vec::with_capacity(28);
    p.extend_from_slice(&0u32.to_ne_bytes()); // index
    p.extend_from_slice(&0u32.to_ne_bytes()); // capab
    p.extend_from_slice(&verdict.to_ne_bytes());
    p.extend_from_slice(&0i32.to_ne_bytes()); // refcnt
    p.extend_from_slice(&0i32.to_ne_bytes()); // bindcnt
    p.extend_from_slice(&eaction.to_ne_bytes());
    p.extend_from_slice(&ifindex.to_ne_bytes());
    p
}

fn add_clsact(nl: &mut Netlink, ifindex: u32) -> io::Result<bool> {
    let mut b = Body::tc(ifindex, CLSACT_HANDLE, TC_H_CLSACT, 0);
    b.attr(TCA_KIND, b"clsact\0");
    match nl.request(RTM_NEWQDISC, NLM_F_CREATE | NLM_F_EXCL, &b.0) {
        Ok(()) => Ok(true),
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(false),
        Err(e) => Err(e),
    }
}

fn del_clsact(nl: &mut Netlink, ifindex: u32) {
    let b = Body::tc(ifindex, CLSACT_HANDLE, TC_H_CLSACT, 0);
    let _ = nl.request(RTM_DELQDISC, 0, &b.0);
}

fn add_rule(nl: &mut Netlink, ifindex: u32, parent: u32, rule: &Rule) -> io::Result<()> {
    let mut b = Body::tc(ifindex, 0, parent, filter_info());
    b.attr(TCA_KIND, b"u32\0");
    b.nest(TCA_OPTIONS, |b| {
        b.attr(TCA_U32_SEL, &selector(&rule.keys));
        b.nest(TCA_U32_ACT, |b| {
            b.nest(1, |b| {
                b.attr(TCA_ACT_KIND, b"mirred\0");
                b.nest(TCA_ACT_OPTIONS, |b| b.attr(TCA_MIRRED_PARMS, &mirred(rule.action)));
            });
        });
    });
    nl.request(RTM_NEWTFILTER, NLM_F_CREATE | NLM_F_EXCL, &b.0)
}

/// Remove every rule at [`RULE_PRIO`] on one hook of one interface. Nothing
/// there, or no `clsact` at all, is not a failure.
fn del_rules(nl: &mut Netlink, ifindex: u32, parent: u32) {
    let b = Body::tc(ifindex, 0, parent, filter_info());
    let _ = nl.request(RTM_DELTFILTER, 0, &b.0);
}

fn ifindex(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: a nul-terminated name.
    let i = unsafe { libc::if_nametoindex(c.as_ptr()) };
    (i != 0).then_some(i)
}

/// Every interface's index, for removing a killed room's leftovers.
fn all_ifindexes() -> Vec<u32> {
    let mut out = Vec::new();
    // SAFETY: if_nameindex returns an array terminated by a zero index, freed
    // below; read only while alive.
    unsafe {
        let head = libc::if_nameindex();
        if head.is_null() {
            return out;
        }
        let mut p = head;
        while (*p).if_index != 0 {
            out.push((*p).if_index);
            p = p.add(1);
        }
        libc::if_freenameindex(head);
    }
    out
}

fn ifname(index: u32) -> Option<String> {
    let mut buf = [0 as libc::c_char; libc::IF_NAMESIZE];
    // SAFETY: a buffer of IF_NAMESIZE, as if_indextoname requires.
    let p = unsafe { libc::if_indextoname(index, buf.as_mut_ptr()) };
    if p.is_null() {
        return None;
    }
    // SAFETY: if_indextoname wrote a nul-terminated name into `buf`.
    Some(unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// rp_filter

/// A packet handed in on the Wi-Fi from a room address fails strict reverse
/// path filtering — the room's route is on the room adapter — so where it is
/// strict, the interface is set to loose for the room's lifetime (the kernel
/// uses the larger of `all` and the interface's own, and 2 is the largest).
/// Loose is systemd's default, so on most machines this changes nothing.
fn loosen_rp_filter(name: &str) -> Option<(String, String)> {
    let read = |p: &str| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string());
    let path = format!("/proc/sys/net/ipv4/conf/{name}/rp_filter");
    let own = read(&path)?;
    let all = read("/proc/sys/net/ipv4/conf/all/rp_filter").unwrap_or_default();
    if own.as_str().max(all.as_str()) != "1" {
        return None;
    }
    std::fs::write(&path, b"2").ok()?;
    Some((path, own))
}

fn restore_rp_filter(saved: Option<(String, String)>) {
    if let Some((path, value)) = saved {
        let _ = std::fs::write(path, value.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Keeping it in step

struct State {
    nl: Netlink,
    room: String,
    room_index: u32,
    installed: HashMap<(u32, u32), Installed>,
    /// Interfaces packets are delivered in on, and the `rp_filter` value this
    /// loosened there, if it did, to put back.
    loosened: HashMap<String, Option<(String, String)>>,
}

impl State {
    fn sync(&mut self) {
        let networks: Vec<(LocalNetwork, u32)> = local_networks()
            .into_iter()
            .filter_map(|n| ifindex(&n.name).map(|i| (n, i)))
            .collect();
        let wanted = plan(&self.room, self.room_index, &networks);
        // Gone, or no longer wanted.
        let keys: Vec<(u32, u32)> = self.installed.keys().copied().collect();
        for key in keys {
            if !wanted.iter().any(|w| (w.ifindex, w.parent) == key) {
                let was = self.installed.remove(&key).expect("listed");
                self.remove(key, was);
            }
        }
        for w in &wanted {
            let key = (w.ifindex, w.parent);
            if self.installed.get(&key).is_some_and(|i| i.rules == w.rules) {
                continue;
            }
            if let Err(e) = self.install(w) {
                eprintln!(
                    "lan-helper: could not bring {}'s traffic to the room: {e}",
                    ifname(w.ifindex).unwrap_or_else(|| w.ifindex.to_string())
                );
            }
        }
        let delivered_to: Vec<String> = wanted
            .iter()
            .flat_map(|w| &w.rules)
            .filter_map(|r| match r.action {
                Action::Deliver(i) => ifname(i),
                _ => None,
            })
            .collect();
        for name in &delivered_to {
            if !self.loosened.contains_key(name) {
                let saved = loosen_rp_filter(name);
                self.loosened.insert(name.clone(), saved);
            }
        }
        self.loosened.retain(|name, saved| {
            let keep = delivered_to.contains(name);
            if !keep {
                restore_rp_filter(saved.take());
            }
            keep
        });
    }

    fn install(&mut self, w: &Wanted) -> io::Result<()> {
        let key = (w.ifindex, w.parent);
        let made_clsact = match self.installed.remove(&key) {
            Some(p) => p.made_clsact,
            None => add_clsact(&mut self.nl, w.ifindex)?,
        };
        del_rules(&mut self.nl, w.ifindex, w.parent);
        // Recorded before adding, so a failure part way is still removed, and
        // tried again at the next refresh.
        self.installed.insert(key, Installed { rules: Vec::new(), made_clsact });
        for rule in &w.rules {
            add_rule(&mut self.nl, w.ifindex, w.parent, rule)?;
        }
        self.installed.get_mut(&key).expect("inserted above").rules = w.rules.clone();
        Ok(())
    }

    fn remove(&mut self, (ifindex, parent): (u32, u32), was: Installed) {
        del_rules(&mut self.nl, ifindex, parent);
        if was.made_clsact && !self.installed.keys().any(|(i, _)| *i == ifindex) {
            del_clsact(&mut self.nl, ifindex);
        }
    }

    fn clear(&mut self) {
        let keys: Vec<(u32, u32)> = self.installed.keys().copied().collect();
        for key in keys {
            let was = self.installed.remove(&key).expect("listed");
            self.remove(key, was);
        }
        for (_, saved) in self.loosened.drain() {
            restore_rp_filter(saved);
        }
    }
}

// Removing the rules goes with the state itself, so they go however it ends:
// the room over, or the refresh thread failing to start after the first rules
// were put in place.
impl Drop for State {
    fn drop(&mut self) {
        self.clear();
    }
}

/// The room's traffic-control rules, kept in step with this machine's
/// interfaces until dropped, then removed.
pub struct Capture {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Start for the room adapter `room`, which must exist. Needs
    /// `CAP_NET_ADMIN`. Removes any leftovers of a killed room first, and has
    /// the first set of rules in place when it returns.
    pub fn start(room: &str) -> io::Result<Self> {
        let room_index = ifindex(room)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no interface {room}")))?;
        let mut nl = Netlink::open()?;
        for i in all_ifindexes() {
            if i != room_index {
                del_rules(&mut nl, i, PARENT_EGRESS);
            }
        }
        let mut state = State {
            nl,
            room: room.to_string(),
            room_index,
            installed: HashMap::new(),
            loosened: HashMap::new(),
        };
        state.sync();
        let (tx, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("lan-capture".into()).spawn(move || {
            // A message, or the sender dropped, is the room ending; dropping
            // the state removes the rules.
            while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(REFRESH) {
                state.sync();
            }
        })?;
        Ok(Self { stop: Some(tx), thread: Some(thread) })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(name: &str, a: [u8; 4], len: u8) -> LocalNetwork {
        LocalNetwork { name: name.into(), address: Ipv4Addr::from(a), prefix_len: len }
    }

    #[test]
    fn a_real_interface_copies_its_broadcasts_and_gives_up_only_the_room_range() {
        let w = plan("gbl0", 9, &[(net("wlan0", [192, 168, 1, 7], 24), 3)]);
        let egress = w.iter().find(|w| w.ifindex == 3).unwrap();
        assert_eq!(egress.parent, PARENT_EGRESS);
        // The only redirect is the room range.
        let redirects: Vec<_> =
            egress.rules.iter().filter(|r| matches!(r.action, Action::Redirect(_))).collect();
        assert_eq!(redirects.len(), 1);
        assert_eq!(redirects[0].keys, [dst(Ipv4Addr::new(198, 18, 0, 0), 15)]);
        assert_eq!(redirects[0].action, Action::Redirect(9));
        // Everything else is a copy, from the interface's own address, UDP.
        let copies: Vec<_> =
            egress.rules.iter().filter(|r| r.action == Action::Mirror(9)).collect();
        assert_eq!(copies.len(), 3, "limited, multicast, its own network's broadcast");
        for c in &copies {
            assert!(c.keys.contains(&src(Ipv4Addr::new(192, 168, 1, 7))));
            assert!(c.keys.contains(&UDP));
        }
        assert!(copies.iter().any(|c| c.keys.contains(&dst(Ipv4Addr::new(192, 168, 1, 255), 32))));
        // In on the Wi-Fi: only marked UDP for the Wi-Fi's own address.
        let ingress = w.iter().find(|w| w.ifindex == 9).unwrap();
        assert_eq!(ingress.parent, PARENT_INGRESS);
        assert_eq!(
            ingress.rules,
            [Rule {
                keys: vec![PINNED, dst(Ipv4Addr::new(192, 168, 1, 7), 32), UDP],
                action: Action::Deliver(3)
            }]
        );
    }

    #[test]
    fn loopback_rooms_and_the_room_itself_get_nothing() {
        let w = plan(
            "gbl0",
            9,
            &[
                (net("lo", [127, 0, 0, 1], 8), 1),
                (net("gbl0", [198, 19, 4, 5], 16), 9),
                (net("gbl1", [198, 18, 4, 5], 16), 10),
            ],
        );
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn the_selector_is_the_kernels_layout() {
        let s = selector(&[UDP]);
        assert_eq!(s.len(), 16 + 16);
        assert_eq!(s[0], TC_U32_TERMINAL);
        assert_eq!(s[2], 1);
        assert_eq!(&s[16..20], &[0x00, 0xff, 0x00, 0x00], "mask, network order");
        assert_eq!(&s[20..24], &[0x00, 0x11, 0x00, 0x00], "value, network order");
        assert_eq!(mirred(Action::Mirror(7)).len(), 28);
        // DSCP 1 is a TOS byte of 0x04.
        assert_eq!(PINNED.val.to_be_bytes()[1], 0x04);
        assert_eq!(PINNED.mask.to_be_bytes()[1], 0xfc);
    }
}
