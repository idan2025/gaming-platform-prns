//! Minecraft: Bedrock Edition's server-list ping, answered on the phone
//! (`MINECRAFT.md` §16).
//!
//! The Minecraft app asks each server it lists for its name and player count
//! with an unconnected RakNet ping on UDP 19132. A Bedrock server reached
//! through NetherNet answers none, so the app shows the entry blank. The
//! launcher on the same phone answers in its place, from the announce it
//! already has. This module is the wire format only; the socket is the
//! launcher's.
//!
//! Protocol: <https://wiki.bedrock.dev/servers/raknet-and-mcpe>. Version
//! numbers: <https://minecraft.wiki/w/Bedrock_Edition_26.52> (protocol 2193 for
//! server 1.26.52.3, the version the Bedrock pack pins). A new pack version
//! changes both constants here, and `a_ping_is_answered_with_the_announced_name_and_count` pins the values.

/// The RakNet offline-message magic every unconnected packet carries.
pub const MAGIC: [u8; 16] = [0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78];

/// Unconnected ping, and its variant that carries a client GUID.
const ID_PING: u8 = 0x01;
const ID_PING_OPEN: u8 = 0x02;
/// Unconnected pong.
const ID_PONG: u8 = 0x1c;

/// The network protocol of the server the pack pins (1.26.52.3).
pub const PROTOCOL: u32 = 2193;
/// The server version the pack pins. Shown in the app's list.
pub const VERSION: &str = "1.26.52.3";

/// The longest ping this answers: an id, a time, the magic and a GUID.
const PING_LEN: usize = 1 + 8 + 16 + 8;

/// What the app is told about a server. The count is the announce's; `None`
/// means it is not known now, and such a server is not answered at all, because
/// the status line has no way to say "unknown" and a zero would be a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Motd {
    pub name: String,
    pub online: Option<u8>,
    pub max: u8,
    pub port: u16,
}

/// The status string, in the order the protocol defines:
/// `MCPE;<name>;<protocol>;<version>;<online>;<max>;<server id>;<level>;<gamemode>;<gamemode number>;<port>;<port>;`.
/// A `;` in the name would end its field early, so it becomes `,`; the name is
/// otherwise data and goes through as it is.
pub fn status_text(motd: &Motd, server_id: u64) -> String {
    let name: String = motd.name.chars().map(|c| if c == ';' || c.is_control() { ',' } else { c }).collect();
    format!(
        "MCPE;{name};{PROTOCOL};{VERSION};{online};{max};{server_id};{name};Survival;1;{port};{port};",
        online = motd.online.unwrap_or(0),
        max = motd.max,
        port = motd.port,
    )
}

/// The reply to an unconnected ping, or `None` for anything else. A datagram
/// that is not a ping is never answered: the same UDP port carries nothing
/// else, but an answer to noise would be a reflector.
pub fn pong_for(ping: &[u8], server_id: u64, motd: &Motd) -> Option<Vec<u8>> {
    if ping.len() < PING_LEN || !matches!(ping[0], ID_PING | ID_PING_OPEN) {
        return None;
    }
    // No count known: say nothing, rather than a zero the app would show.
    motd.online?;
    if ping[9..25] != MAGIC {
        return None;
    }
    let text = status_text(motd, server_id);
    let mut out = Vec::with_capacity(1 + 8 + 8 + 16 + 2 + text.len());
    out.push(ID_PONG);
    out.extend_from_slice(&ping[1..9]); // the ping's own time, echoed back
    out.extend_from_slice(&server_id.to_be_bytes());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&(text.len() as u16).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn motd(name: &str) -> Motd {
        Motd { name: name.to_string(), online: Some(2), max: 10, port: 19132 }
    }

    fn ping(id: u8, time: i64, guid: u64) -> Vec<u8> {
        let mut p = vec![id];
        p.extend_from_slice(&time.to_be_bytes());
        p.extend_from_slice(&MAGIC);
        p.extend_from_slice(&guid.to_be_bytes());
        p
    }

    #[test]
    fn a_ping_is_answered_with_the_announced_name_and_count() {
        let pong = pong_for(&ping(ID_PING, 1234, 99), 7, &motd("Bedrock test")).expect("a ping is answered");
        assert_eq!(pong[0], ID_PONG);
        assert_eq!(&pong[1..9], &1234i64.to_be_bytes(), "the time is echoed");
        assert_eq!(&pong[9..17], &7u64.to_be_bytes());
        assert_eq!(&pong[17..33], &MAGIC);
        let len = u16::from_be_bytes([pong[33], pong[34]]) as usize;
        let text = std::str::from_utf8(&pong[35..35 + len]).unwrap();
        assert_eq!(
            text,
            "MCPE;Bedrock test;2193;1.26.52.3;2;10;7;Bedrock test;Survival;1;19132;19132;"
        );
    }

    #[test]
    fn the_open_connection_ping_is_answered_too() {
        assert!(pong_for(&ping(ID_PING_OPEN, 1, 1), 7, &motd("x")).is_some());
    }

    #[test]
    fn noise_is_never_answered() {
        let m = motd("x");
        assert!(pong_for(&[], 7, &m).is_none(), "empty");
        assert!(pong_for(&ping(ID_PING, 1, 1)[..20], 7, &m).is_none(), "short");
        assert!(pong_for(&ping(0x05, 1, 1), 7, &m).is_none(), "a connection request is not a ping");
        let mut wrong = ping(ID_PING, 1, 1);
        wrong[12] ^= 0xff;
        assert!(pong_for(&wrong, 7, &m).is_none(), "wrong magic");
    }

    #[test]
    fn a_semicolon_in_the_name_cannot_add_a_field() {
        let text = status_text(&motd("a;b;c"), 7);
        assert_eq!(text.split(';').count(), 13, "twelve fields and the empty tail: {text}");
        assert!(text.starts_with("MCPE;a,b,c;2193;"));
    }

    #[test]
    fn an_unknown_count_is_not_answered_rather_than_shown_as_zero() {
        let mut m = motd("x");
        m.online = None;
        assert!(pong_for(&ping(ID_PING, 1, 1), 7, &m).is_none());
    }
}
