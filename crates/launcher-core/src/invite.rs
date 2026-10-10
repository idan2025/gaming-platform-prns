//! Invite links (`ROADMAP.md` §2):
//! `lanthorn://join/<destination>?game=<id>&name=<name>&room=1`.
//!
//! A destination hash is all a join needs, so a link carries everything a
//! friend needs to find a server or room — including before its announce has
//! reached them, which is exactly when "find it in the list" fails.
//!
//! Rules a later change could quietly break:
//! - **A link is untrusted input from a chat message.** Every field is
//!   validated as data here, and an invalid link is refused, never repaired.
//! - **A link never joins anything.** It selects which server the launcher
//!   shows; a person presses Join. Nothing in an invite may set a port, a
//!   path, an interface or anything else the launcher does.
//! - The name is only a label shown until the server's own announce arrives;
//!   it is never trusted for identity, which is the destination.

use serde::Serialize;

pub const SCHEME: &str = "lanthorn";

/// The longest name an invite carries; an announce's own is shorter still.
const MAX_NAME: usize = 64;
/// Pack ids are short ASCII (`pack.rs`: at most 24 bytes).
const MAX_GAME_ID: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Invite {
    /// 32 lowercase hex characters.
    pub destination_hash: String,
    pub game_id: Option<String>,
    pub name: Option<String>,
    /// A LAN room rather than a server: joined as a room, never bridged as a
    /// game server (`CLAUDE.md`, Mode 3). Only selects which button is shown;
    /// the room's own announce decides once it is heard.
    pub room: bool,
}

/// Read an invite from a link — or from a bare destination hash, which is what
/// a player pastes when someone sent them only the address.
pub fn parse(input: &str) -> Result<Invite, String> {
    let input = input.trim();
    if is_hash(input) {
        return Ok(Invite { destination_hash: input.to_ascii_lowercase(), game_id: None, name: None, room: false });
    }
    let rest = input
        .strip_prefix(&format!("{SCHEME}://join/"))
        .ok_or_else(|| format!("not an invite: it should start with {SCHEME}://join/"))?;
    let (hash, query) = match rest.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (rest, None),
    };
    let hash = hash.trim_end_matches('/');
    if !is_hash(hash) {
        return Err("the invite's server address is not 32 hexadecimal characters".to_string());
    }
    let mut invite = Invite { destination_hash: hash.to_ascii_lowercase(), game_id: None, name: None, room: false };
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = percent_decode(v).ok_or("the invite is not valid text")?;
        match k {
            "game" => {
                if v.is_empty()
                    || v.len() > MAX_GAME_ID
                    || !v.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                {
                    return Err("the invite names a game id that is not one".to_string());
                }
                invite.game_id = Some(v);
            }
            "name" => {
                let clean: String = v.chars().filter(|c| !c.is_control()).collect();
                let clean = clean.trim();
                if !clean.is_empty() {
                    invite.name = Some(clean.chars().take(MAX_NAME).collect());
                }
            }
            "room" => invite.room = v == "1",
            // A newer launcher may add fields; an older one ignores them
            // rather than refusing an invite it can still act on.
            _ => {}
        }
    }
    Ok(invite)
}

/// The link for a server or room.
pub fn link(destination_hash: &str, game_id: Option<&str>, name: Option<&str>, room: bool) -> String {
    let mut out = format!("{SCHEME}://join/{}", destination_hash.to_ascii_lowercase());
    let mut sep = '?';
    if let Some(g) = game_id.filter(|g| !g.is_empty()) {
        out.push(sep);
        out.push_str("game=");
        out.push_str(&percent_encode(g));
        sep = '&';
    }
    if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
        out.push(sep);
        out.push_str("name=");
        out.push_str(&percent_encode(&n.chars().take(MAX_NAME).collect::<String>()));
        sep = '&';
    }
    if room {
        out.push(sep);
        out.push_str("room=1");
    }
    out
}

fn is_hash(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_link_round_trips() {
        let l = link(H, Some("nfs-underground-2"), Some("Friday drag races & more"), true);
        assert!(l.starts_with("lanthorn://join/"), "{l}");
        let i = parse(&l).unwrap();
        assert_eq!(i.destination_hash, H);
        assert_eq!(i.game_id.as_deref(), Some("nfs-underground-2"));
        assert_eq!(i.name.as_deref(), Some("Friday drag races & more"));
        assert!(i.room);
        assert!(!parse(&link(H, None, None, false)).unwrap().room);
    }

    #[test]
    fn a_bare_address_is_an_invite_too() {
        let i = parse(&format!("  {}  ", H.to_uppercase())).unwrap();
        assert_eq!(i.destination_hash, H);
        assert!(i.game_id.is_none() && i.name.is_none());
    }

    #[test]
    fn anything_else_is_refused_not_repaired() {
        for bad in [
            "https://example.com/join/0123456789abcdef0123456789abcdef",
            "lanthorn://join/0123",
            "lanthorn://join/zz23456789abcdef0123456789abcdef",
            "lanthorn://join/0123456789abcdef0123456789abcdef?game=../../etc",
            "lanthorn://join/0123456789abcdef0123456789abcdef?game=Sven%20Coop",
            "lanthorn://join/0123456789abcdef0123456789abcdef?name=%ZZ",
        ] {
            assert!(parse(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn a_name_is_only_a_label() {
        let i = parse(&format!("lanthorn://join/{H}?name=%0Aevil%07%20name&port=1&exec=x")).unwrap();
        assert_eq!(i.name.as_deref(), Some("evil name"), "control characters are dropped");
        let long = "x".repeat(500);
        let i = parse(&format!("lanthorn://join/{H}?name={long}")).unwrap();
        assert_eq!(i.name.unwrap().len(), MAX_NAME);
    }

    #[test]
    fn invite_keys_are_the_frontend_contract() {
        let v = serde_json::to_value(parse(H).unwrap()).unwrap();
        for key in ["destination_hash", "game_id", "name", "room"] {
            assert!(v.get(key).is_some(), "the UI reads `{key}`");
        }
    }
}
