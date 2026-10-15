//! Chat on a server, for its bridged players (`ROADMAP.md` §6.1).
//!
//! A LAN room's links carry only room messages, so its chat could be new
//! message types (`lan.rs`). A server's links are the game's: every link-data
//! packet is a framing chunk, and a deployed peer reassembles whatever it
//! receives there as game data (`framing.rs`). So server chat never touches a
//! game link. It rides **its own link** to the server's destination, on a
//! byte stream with ids no game stream uses ([`STREAM_TO_SERVER`],
//! [`STREAM_TO_CLIENT`]; a game's are `stream::stream_ids(0..=7)` = 1..16).
//!
//! Rules a later change could quietly break:
//! - **Only a server whose announce set the chat flag is ever sent chat**
//!   (`AnnounceFlags::chat`); the gate reads the peer's announce, as
//!   `relay::may_use_channel` does for framing channels.
//! - **The client says `Hello` before it identifies, and identifies only
//!   after `Welcome`.** An open server starts a UDP link at once and connects
//!   a TCP game when a link identifies, so a chat link that identified first
//!   would open a phantom game connection. `Hello` is what tells the server
//!   the link is for chat; it then never starts the game for it.
//! - **The server writes chat only to a link that said `Hello`.** A game link
//!   and an older client's link never receive a byte of it.
//! - **Who said it is the server's word**: a line is stamped with the
//!   identity the link proved, and a link that has not identified may only
//!   say `Hello`. Names and lines are refused at decode unless they are one
//!   line of plain text within their cap (`lan::valid_name`,
//!   `lan::valid_chat`), never stripped, and lines are gated per identity
//!   (`lan::ChatGate`).
//! - **Kept in memory only**: the last [`HISTORY`] lines, for `Welcome`.
//!
//! # Wire format
//!
//! Each message is a big-endian `u16` length, then that many bytes: a type
//! byte and its body. Strings are a `u8` length and UTF-8; an identity is
//! its 16-byte hash.
//!
//! ```text
//! 0x01 Hello   name                       client → server, first message
//! 0x02 Say     text                       client → server, after identify
//! 0x10 Welcome                            server → client, answers Hello
//! 0x11 Names   u8 count, (identity, name)*
//! 0x12 Line    identity, u64 at_unix, text
//! ```
//!
//! An unknown type is skipped, so a later build can add one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prns_core::identity::IdentityHash;
use prns_core::routing::links::channel::byte_stream::StreamId;
use prns_core::routing::links::LinkId;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::lan::{valid_chat, valid_name, ChatGate};
pub use crate::lan_session::ChatLine;

/// The client's half of the chat stream.
pub const STREAM_TO_SERVER: u16 = 0x100;
/// The server's half.
pub const STREAM_TO_CLIENT: u16 = 0x101;

/// Lines a server keeps for a newcomer's `Welcome`.
pub const HISTORY: usize = 50;

/// Most members a server's roster names; more still chat, unnamed.
pub const MAX_NAMES: usize = 64;

/// Longest message either side accepts: a full roster, with room.
pub const MAX_MESSAGE: usize = 8 * 1024;

/// How long a client waits for `Welcome` before giving up on the server.
pub const WELCOME_TIMEOUT: Duration = Duration::from_secs(15);

const HELLO: u8 = 0x01;
const SAY: u8 = 0x02;
const WELCOME: u8 = 0x10;
const NAMES: u8 = 0x11;
const LINE: u8 = 0x12;

pub fn stream_ids() -> (StreamId, StreamId) {
    (
        StreamId::new(STREAM_TO_SERVER).expect("under STREAM_ID_MAX"),
        StreamId::new(STREAM_TO_CLIENT).expect("under STREAM_ID_MAX"),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatMessage {
    Hello { name: String },
    Say { text: String },
    Welcome,
    Names(Vec<(IdentityHash, String)>),
    Line { from: IdentityHash, at_unix: u64, text: String },
    Unknown(u8),
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.push(s.len() as u8);
    out.extend_from_slice(s.as_bytes());
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn string(&mut self) -> Option<String> {
        let n = self.byte()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    fn identity(&mut self) -> Option<IdentityHash> {
        let b: [u8; 16] = self.take(16)?.try_into().ok()?;
        Some(IdentityHash::new(b))
    }
}

impl ChatMessage {
    /// The message's bytes, without its length prefix. Text that would not
    /// decode on the far side is refused here too.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        match self {
            Self::Hello { name } => {
                if !valid_name(name) {
                    return None;
                }
                out.push(HELLO);
                put_str(&mut out, name);
            }
            Self::Say { text } => {
                if !valid_chat(text) {
                    return None;
                }
                out.push(SAY);
                put_str(&mut out, text);
            }
            Self::Welcome => out.push(WELCOME),
            Self::Names(names) => {
                out.push(NAMES);
                let names: Vec<_> =
                    names.iter().filter(|(_, n)| valid_name(n)).take(MAX_NAMES).collect();
                out.push(names.len() as u8);
                for (id, name) in names {
                    out.extend_from_slice(id.as_bytes());
                    put_str(&mut out, name);
                }
            }
            Self::Line { from, at_unix, text } => {
                if !valid_chat(text) {
                    return None;
                }
                out.push(LINE);
                out.extend_from_slice(from.as_bytes());
                out.extend_from_slice(&at_unix.to_be_bytes());
                put_str(&mut out, text);
            }
            Self::Unknown(_) => return None,
        }
        Some(out)
    }

    /// Read one message's bytes. A name or line that is not one line of plain
    /// text within its cap is refused, never repaired.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut c = Cursor(bytes);
        let msg = match c.byte()? {
            HELLO => {
                let name = c.string()?;
                valid_name(&name).then_some(Self::Hello { name })?
            }
            SAY => {
                let text = c.string()?;
                valid_chat(&text).then_some(Self::Say { text })?
            }
            WELCOME => Self::Welcome,
            NAMES => {
                let n = c.byte()? as usize;
                let mut names = Vec::with_capacity(n);
                for _ in 0..n {
                    let id = c.identity()?;
                    let name = c.string()?;
                    if !valid_name(&name) {
                        return None;
                    }
                    names.push((id, name));
                }
                Self::Names(names)
            }
            LINE => {
                let from = c.identity()?;
                let at_unix = u64::from_be_bytes(c.take(8)?.try_into().ok()?);
                let text = c.string()?;
                valid_chat(&text).then_some(Self::Line { from, at_unix, text })?
            }
            other => return Some(Self::Unknown(other)),
        };
        Some(msg)
    }

    /// The message with its length prefix, ready for the stream.
    pub fn frame(&self) -> Option<Vec<u8>> {
        let body = self.encode()?;
        let mut out = Vec::with_capacity(body.len() + 2);
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        out.extend_from_slice(&body);
        Some(out)
    }
}

/// Read one length-prefixed message's bytes. `Ok(None)` is a clean end.
pub async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 2];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u16::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "chat message size"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(body))
}

/// Drain `rx` into `w`, one framed message at a time, until either ends.
pub async fn pump_writes<W: AsyncWrite + Unpin>(
    mut w: W,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    while let Some(frame) = rx.recv().await {
        if w.write_all(&frame).await.is_err() || w.flush().await.is_err() {
            return;
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Default)]
struct Log {
    lines: VecDeque<ChatLine>,
    next: u64,
}

impl Log {
    fn push(&mut self, from: IdentityHash, text: String, at_unix: u64) -> ChatLine {
        self.next += 1;
        let line = ChatLine { seq: self.next, from, text, at_unix };
        self.lines.push_back(line.clone());
        while self.lines.len() > HISTORY {
            self.lines.pop_front();
        }
        line
    }
    fn since(&self, seq: u64) -> Vec<ChatLine> {
        self.lines.iter().filter(|l| l.seq > seq).cloned().collect()
    }
}

struct Member {
    identity: Option<IdentityHash>,
    name: String,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

struct HubInner {
    members: HashMap<LinkId, Member>,
    log: Log,
    gate: ChatGate,
    muted: HashSet<[u8; 16]>,
    host_name: String,
}

/// A server's chat: who said `Hello`, what was said, who is muted.
///
/// Every method is synchronous and sends through each member's own queue,
/// so nothing here waits on the network while holding the lock.
pub struct ChatHub {
    enabled: AtomicBool,
    host: IdentityHash,
    inner: Mutex<HubInner>,
}

/// Why a host's line was not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SayError {
    NotPlainText,
    ChatOff,
}

impl core::fmt::Display for SayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotPlainText => {
                write!(
                    f,
                    "a chat line is one line of plain text, at most {} bytes",
                    crate::lan::MAX_CHAT_BYTES
                )
            }
            Self::ChatOff => write!(f, "chat is off on this server"),
        }
    }
}

impl std::error::Error for SayError {}

impl ChatHub {
    /// `host` is the server's own identity: the host's lines come from it.
    pub fn new(enabled: bool, host: IdentityHash) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            host,
            inner: Mutex::new(HubInner {
                members: HashMap::new(),
                log: Log::default(),
                gate: ChatGate::default(),
                muted: HashSet::new(),
                host_name: "Host".to_string(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Turn chat on or off. Off ends every chat link's membership; the links
    /// themselves are the router's to close.
    pub fn set_enabled(&self, on: bool) -> Vec<LinkId> {
        self.enabled.store(on, Ordering::Relaxed);
        if on {
            return Vec::new();
        }
        let mut inner = self.lock();
        inner.members.drain().map(|(link, _)| link).collect()
    }

    pub fn host_identity(&self) -> IdentityHash {
        self.host
    }

    /// The name the host's own lines go under. Refused unless it is a valid
    /// name.
    pub fn set_host_name(&self, name: &str) -> bool {
        if !valid_name(name) {
            return false;
        }
        self.lock().host_name = name.to_string();
        self.broadcast_names();
        true
    }

    fn names_of(inner: &HubInner, host: IdentityHash) -> Vec<(IdentityHash, String)> {
        let mut names = vec![(host, inner.host_name.clone())];
        let mut seen: HashSet<[u8; 16]> = HashSet::new();
        seen.insert(*host.as_bytes());
        for m in inner.members.values() {
            if let Some(id) = m.identity {
                if seen.insert(*id.as_bytes()) {
                    names.push((id, m.name.clone()));
                }
            }
        }
        names
    }

    fn send_all(inner: &HubInner, msg: &ChatMessage) {
        if let Some(frame) = msg.frame() {
            for m in inner.members.values() {
                let _ = m.tx.send(frame.clone());
            }
        }
    }

    fn broadcast_names(&self) {
        let inner = self.lock();
        Self::send_all(&inner, &ChatMessage::Names(Self::names_of(&inner, self.host)));
    }

    /// A link said `Hello`. It hears `Welcome`, who is here, and what was said.
    pub fn hello(&self, link: LinkId, name: String, tx: mpsc::UnboundedSender<Vec<u8>>) {
        let mut inner = self.lock();
        let welcome = [ChatMessage::Welcome, ChatMessage::Names(Self::names_of(&inner, self.host))];
        for msg in welcome.iter().chain(
            inner
                .log
                .lines
                .iter()
                .map(|l| ChatMessage::Line {
                    from: l.from,
                    at_unix: l.at_unix,
                    text: l.text.clone(),
                })
                .collect::<Vec<_>>()
                .iter(),
        ) {
            if let Some(frame) = msg.frame() {
                let _ = tx.send(frame);
            }
        }
        inner.members.insert(link, Member { identity: None, name, tx });
    }

    /// The link proved who it is: from now on it may speak, under that
    /// identity.
    pub fn identified(&self, link: LinkId, identity: IdentityHash) {
        {
            let mut inner = self.lock();
            let Some(m) = inner.members.get_mut(&link) else { return };
            m.identity = Some(identity);
        }
        self.broadcast_names();
    }

    pub fn is_identified(&self, link: LinkId) -> bool {
        self.lock().members.get(&link).is_some_and(|m| m.identity.is_some())
    }

    pub fn left(&self, link: LinkId) {
        let gone = self.lock().members.remove(&link);
        if gone.is_some_and(|m| m.identity.is_some()) {
            self.broadcast_names();
        }
    }

    /// A member's line. Dropped, not answered, when the link has not
    /// identified, is muted, or is over its rate; the sender sees it never
    /// came back, as in a room.
    pub fn say(&self, link: LinkId, text: String, now: Instant) -> bool {
        if !self.enabled() || !valid_chat(&text) {
            return false;
        }
        let mut inner = self.lock();
        let Some(identity) = inner.members.get(&link).and_then(|m| m.identity) else {
            return false;
        };
        if inner.muted.contains(identity.as_bytes()) || !inner.gate.allow(identity, now) {
            return false;
        }
        let line = inner.log.push(identity, text, now_unix());
        Self::send_all(
            &inner,
            &ChatMessage::Line { from: line.from, at_unix: line.at_unix, text: line.text },
        );
        true
    }

    /// The host's own line, under the server's identity.
    pub fn host_say(&self, text: &str) -> Result<ChatLine, SayError> {
        if !self.enabled() {
            return Err(SayError::ChatOff);
        }
        if !valid_chat(text) {
            return Err(SayError::NotPlainText);
        }
        let mut inner = self.lock();
        let line = inner.log.push(self.host, text.to_string(), now_unix());
        Self::send_all(
            &inner,
            &ChatMessage::Line { from: line.from, at_unix: line.at_unix, text: line.text.clone() },
        );
        Ok(line)
    }

    /// Mute or unmute an identity for as long as this server runs.
    pub fn set_muted(&self, identity: IdentityHash, muted: bool) {
        let mut inner = self.lock();
        if muted {
            inner.muted.insert(*identity.as_bytes());
        } else {
            inner.muted.remove(identity.as_bytes());
        }
    }

    pub fn muted(&self) -> Vec<IdentityHash> {
        self.lock().muted.iter().map(|b| IdentityHash::new(*b)).collect()
    }

    pub fn lines_since(&self, seq: u64) -> Vec<ChatLine> {
        self.lock().log.since(seq)
    }

    /// Who is here by name, the host first.
    pub fn names(&self) -> Vec<(IdentityHash, String)> {
        Self::names_of(&self.lock(), self.host)
    }

    /// Links that said `Hello`, for counting.
    pub fn members(&self) -> usize {
        self.lock().members.len()
    }
}

/// The client's view of a server's chat.
#[derive(Debug, Default)]
pub(crate) struct ClientChatState {
    pub(crate) log: VecDeque<ChatLine>,
    pub(crate) next: u64,
    pub(crate) names: Vec<(IdentityHash, String)>,
    pub(crate) open: bool,
}

/// One chat membership on a server, from the client's side.
pub struct ChatClient {
    pub(crate) link_id: LinkId,
    pub(crate) handle: personal_rns::prelude::PrnsNodeHandle,
    pub(crate) tx: mpsc::UnboundedSender<Vec<u8>>,
    pub(crate) state: Arc<Mutex<ClientChatState>>,
}

impl ChatClient {
    fn state(&self) -> std::sync::MutexGuard<'_, ClientChatState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Send a line. The server stamps who said it and sends it back to
    /// everyone, this client included; a line that does not come back was
    /// dropped by the server's gate.
    pub fn say(&self, text: &str) -> Result<(), SayError> {
        let frame =
            ChatMessage::Say { text: text.to_string() }.frame().ok_or(SayError::NotPlainText)?;
        if !self.is_open() {
            return Err(SayError::ChatOff);
        }
        self.tx.send(frame).map_err(|_| SayError::ChatOff)
    }

    pub fn lines_since(&self, seq: u64) -> Vec<ChatLine> {
        self.state().log.iter().filter(|l| l.seq > seq).cloned().collect()
    }

    pub fn names(&self) -> Vec<(IdentityHash, String)> {
        self.state().names.clone()
    }

    /// Whether the link is still up.
    pub fn is_open(&self) -> bool {
        self.state().open
    }

    pub fn close(&self) {
        self.state().open = false;
        let _ = self.handle.close_link(self.link_id);
    }
}

impl Drop for ChatClient {
    fn drop(&mut self) {
        let _ = self.handle.close_link(self.link_id);
    }
}

/// Apply a message from the server to the client's view.
pub(crate) fn client_apply(state: &Mutex<ClientChatState>, msg: ChatMessage) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    match msg {
        ChatMessage::Names(names) => s.names = names,
        ChatMessage::Line { from, at_unix, text } => {
            s.next += 1;
            let seq = s.next;
            s.log.push_back(ChatLine { seq, from, text, at_unix });
            while s.log.len() > HISTORY * 4 {
                s.log.pop_front();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> IdentityHash {
        IdentityHash::new([n; 16])
    }

    #[test]
    fn every_message_round_trips_and_bad_text_is_refused_not_repaired() {
        for m in [
            ChatMessage::Hello { name: "Ada".into() },
            ChatMessage::Say { text: "gg".into() },
            ChatMessage::Welcome,
            ChatMessage::Names(vec![(id(1), "Ada".into()), (id(2), "Bo".into())]),
            ChatMessage::Line { from: id(3), at_unix: 1_700_000_000, text: "hi all".into() },
        ] {
            assert_eq!(ChatMessage::decode(&m.encode().unwrap()), Some(m.clone()), "{m:?}");
        }
        assert!(ChatMessage::Say { text: "two\nlines".into() }.encode().is_none());
        assert!(ChatMessage::Hello { name: "x".repeat(200) }.encode().is_none());
        // Bytes a hostile peer wrote by hand are refused at decode as well.
        let mut raw = vec![SAY, 3];
        raw.extend_from_slice(b"a\nb");
        assert_eq!(ChatMessage::decode(&raw), None);
        assert_eq!(ChatMessage::decode(&[0x7f, 1, 2]), Some(ChatMessage::Unknown(0x7f)));
        assert_eq!(ChatMessage::decode(&[LINE, 1, 2]), None, "truncated, never a panic");
    }

    fn link(n: u8) -> LinkId {
        LinkId::new([n; 16])
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<ChatMessage> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.push(ChatMessage::decode(&frame[2..]).unwrap());
        }
        out
    }

    /// A link speaks only once it has proved who it is, under that identity;
    /// a muted identity and one over its rate are dropped; and a link that
    /// never said `Hello` hears nothing.
    #[test]
    fn a_line_is_from_the_identity_its_link_proved_and_only_members_hear_it() {
        let hub = ChatHub::new(true, id(9));
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        hub.hello(link(1), "Ada".into(), a_tx);
        assert_eq!(drain(&mut a_rx)[0], ChatMessage::Welcome);
        let t = Instant::now();
        assert!(!hub.say(link(1), "before identify".into(), t), "not identified yet");
        hub.identified(link(1), id(1));
        hub.hello(link(2), "Bo".into(), b_tx);
        hub.identified(link(2), id(2));
        drain(&mut a_rx);
        drain(&mut b_rx);

        assert!(hub.say(link(1), "hello".into(), t));
        let got = drain(&mut b_rx);
        assert!(
            got.contains(&ChatMessage::Line {
                from: id(1),
                at_unix: got
                    .iter()
                    .find_map(|m| match m {
                        ChatMessage::Line { at_unix, .. } => Some(*at_unix),
                        _ => None,
                    })
                    .unwrap(),
                text: "hello".into()
            }),
            "{got:?}"
        );
        assert!(!hub.say(link(3), "a link that never said hello".into(), t));

        hub.set_muted(id(2), true);
        assert!(!hub.say(link(2), "muted".into(), t));
        hub.set_muted(id(2), false);
        for _ in 0..crate::lan::CHAT_BURST - 1 {
            assert!(hub.say(link(1), "spam".into(), t));
        }
        assert!(!hub.say(link(1), "over the burst".into(), t));

        // A newcomer is welcomed with what was said.
        let (c_tx, mut c_rx) = mpsc::unbounded_channel();
        hub.hello(link(3), "Cy".into(), c_tx);
        let got = drain(&mut c_rx);
        assert!(got.iter().any(|m| matches!(m, ChatMessage::Line { text, .. } if text == "hello")));
        assert_eq!(hub.host_say("server restarting soon").unwrap().from, id(9));
        hub.set_enabled(false);
        assert_eq!(hub.host_say("x"), Err(SayError::ChatOff));
        assert_eq!(hub.members(), 0, "turning chat off ends every membership");
    }
}
