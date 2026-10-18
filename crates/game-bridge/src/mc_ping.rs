//! Minecraft's Server List Ping: how many players are on a Java Edition server,
//! and a sample of their names (`MINECRAFT.md` §5).
//!
//! The same role `a2s.rs` plays for GoldSrc and Source, and aimed the same
//! way: at a server this host runs, never at strangers. The answer is returned
//! in A2S's shape so every caller that already reads a player count — the
//! relay's live stats, reaping, "when empty" restarts and moves — reads this
//! one too, with nothing new to learn. A server that does not answer is an
//! error, which callers already treat as "could not ask", never as zero.
//!
//! Protocol: <https://minecraft.wiki/w/Java_Edition_protocol/Server_List_Ping>.
//! One TCP connection: a handshake asking for the status state, a status
//! request, and one JSON reply.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::a2s::{A2sInfo, A2sPlayer, A2sStats};

const PING_TIMEOUT: Duration = Duration::from_secs(3);
/// Largest status reply read. A real one is a few kilobytes even with a
/// server icon; past this it is not a status reply.
const MAX_REPLY: usize = 64 * 1024;
/// Any protocol number works for a status request; this is 1.8's.
const PROTOCOL: i32 = 47;

/// What a Java Edition server says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingStatus {
    pub online: u32,
    pub max: u32,
    /// Up to the dozen names a server samples. Not the whole list.
    pub sample: Vec<String>,
    pub motd: String,
    pub version: String,
}

/// Ask a Java Edition server for its status.
pub async fn ping(addr: SocketAddr) -> Result<PingStatus> {
    timeout(PING_TIMEOUT, ping_inner(addr)).await.map_err(|_| anyhow!("no status reply from {addr}"))?
}

/// [`ping`], in the shape A2S callers read. Minecraft has no map and no bots,
/// so those are empty and zero.
pub async fn query_as_a2s(addr: SocketAddr) -> Result<A2sStats> {
    Ok(to_a2s(ping(addr).await?))
}

pub fn to_a2s(s: PingStatus) -> A2sStats {
    A2sStats {
        info: A2sInfo {
            server_name: s.motd,
            map: String::new(),
            game_dir: "minecraft".to_string(),
            game_description: "Minecraft".to_string(),
            players: s.online.min(u32::from(u8::MAX)) as u8,
            max_players: s.max.min(u32::from(u8::MAX)) as u8,
            bots: 0,
            server_type: "dedicated".to_string(),
            environment: "linux".to_string(),
            password_protected: false,
            vac_secured: false,
            version: s.version,
        },
        players_list: s
            .sample
            .into_iter()
            .enumerate()
            .map(|(i, name)| A2sPlayer { index: i as u8, name, score: 0, duration_secs: 0.0 })
            .collect(),
    }
}

async fn ping_inner(addr: SocketAddr) -> Result<PingStatus> {
    let mut stream = TcpStream::connect(addr).await.with_context(|| format!("connecting to {addr}"))?;
    let mut handshake = Vec::new();
    write_varint(&mut handshake, 0x00);
    write_varint(&mut handshake, PROTOCOL);
    let host = addr.ip().to_string();
    write_varint(&mut handshake, host.len() as i32);
    handshake.extend_from_slice(host.as_bytes());
    handshake.extend_from_slice(&addr.port().to_be_bytes());
    write_varint(&mut handshake, 1); // next state: status
    let mut out = Vec::new();
    write_varint(&mut out, handshake.len() as i32);
    out.extend_from_slice(&handshake);
    out.extend_from_slice(&[0x01, 0x00]); // status request: length 1, id 0
    stream.write_all(&out).await?;

    let len = read_varint(&mut stream).await? as usize;
    if len == 0 || len > MAX_REPLY {
        bail!("a status reply of {len} bytes is not one");
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    parse_reply(&body)
}

/// A status reply's body: packet id 0, then the JSON as a length-prefixed
/// string.
pub fn parse_reply(body: &[u8]) -> Result<PingStatus> {
    let mut at = 0;
    if varint_from(body, &mut at)? != 0 {
        bail!("not a status reply");
    }
    let n = varint_from(body, &mut at)? as usize;
    let text = body.get(at..at + n).ok_or_else(|| anyhow!("the status reply is cut short"))?;
    parse_status_json(std::str::from_utf8(text)?)
}

pub fn parse_status_json(text: &str) -> Result<PingStatus> {
    let v: serde_json::Value = serde_json::from_str(text)?;
    let players = &v["players"];
    let online = players["online"].as_u64().ok_or_else(|| anyhow!("the status has no player count"))?;
    let max = players["max"].as_u64().unwrap_or(0);
    let sample = players["sample"]
        .as_array()
        .map(|s| s.iter().filter_map(|p| p["name"].as_str().map(str::to_string)).take(32).collect())
        .unwrap_or_default();
    Ok(PingStatus {
        online: online.min(u64::from(u32::MAX)) as u32,
        max: max.min(u64::from(u32::MAX)) as u32,
        sample,
        motd: description_text(&v["description"]),
        version: v["version"]["name"].as_str().unwrap_or("").to_string(),
    })
}

/// A description is a string or a chat component (`text` plus `extra`).
fn description_text(d: &serde_json::Value) -> String {
    match d {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(o) => {
            let mut s = o.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
            if let Some(extra) = o.get("extra").and_then(|e| e.as_array()) {
                for e in extra {
                    s.push_str(&description_text(e));
                }
            }
            s
        }
        _ => String::new(),
    }
}

pub fn write_varint(out: &mut Vec<u8>, value: i32) {
    let mut v = value as u32;
    loop {
        if v & !0x7f == 0 {
            out.push(v as u8);
            return;
        }
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
}

fn varint_from(b: &[u8], at: &mut usize) -> Result<i32> {
    let mut value: u32 = 0;
    for i in 0..5 {
        let byte = *b.get(*at).ok_or_else(|| anyhow!("a number is cut short"))?;
        *at += 1;
        value |= u32::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(value as i32);
        }
    }
    bail!("a number longer than five bytes")
}

async fn read_varint(stream: &mut TcpStream) -> Result<i32> {
    let mut value: u32 = 0;
    for i in 0..5 {
        let byte = stream.read_u8().await?;
        value |= u32::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(value as i32);
        }
    }
    bail!("a number longer than five bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn varints_round_trip() {
        for v in [0, 1, 127, 128, 255, 25565, 2_097_151, i32::MAX, -1] {
            let mut out = Vec::new();
            write_varint(&mut out, v);
            let mut at = 0;
            assert_eq!(varint_from(&out, &mut at).unwrap(), v);
            assert_eq!(at, out.len());
        }
    }

    #[test]
    fn a_status_with_a_chat_component_motd_and_a_sample_parses() {
        let s = parse_status_json(
            r#"{"version":{"name":"Paper 26.3","protocol":775},"players":{"max":20,"online":3,
                "sample":[{"name":"alice","id":"x"},{"name":"bob","id":"y"}]},
                "description":{"text":"Friday ","extra":[{"text":"night"}]}}"#,
        )
        .unwrap();
        assert_eq!((s.online, s.max, s.sample.len()), (3, 20, 2));
        assert_eq!(s.motd, "Friday night");
        assert!(parse_status_json(r#"{"description":"x"}"#).is_err(), "no count is not zero");
    }

    /// A stand-in server that reads the handshake and status request and
    /// answers like a Java Edition server, so the whole exchange is exercised.
    #[tokio::test]
    async fn a_ping_reads_the_count_from_a_server_and_silence_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf).await.unwrap();
            let json = br#"{"version":{"name":"1.21.11"},"players":{"max":10,"online":2},"description":"hi"}"#;
            let mut body = Vec::new();
            write_varint(&mut body, 0);
            write_varint(&mut body, json.len() as i32);
            body.extend_from_slice(json);
            let mut out = Vec::new();
            write_varint(&mut out, body.len() as i32);
            out.extend_from_slice(&body);
            s.write_all(&out).await.unwrap();
        });
        let stats = query_as_a2s(addr).await.unwrap();
        assert_eq!((stats.info.players, stats.info.max_players), (2, 10));

        // Nothing listening: an error, which every caller reads as unknown.
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        assert!(ping(closed).await.is_err());
    }
}
