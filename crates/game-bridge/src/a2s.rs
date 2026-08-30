//! Minimal client for Valve's server query protocol (A2S): live player count,
//! map and player list, straight from a running GoldSrc or Source dedicated
//! server over UDP. No RCON and no game-side integration — any external tool
//! may query these; it is the same protocol Valve's own server browsers use.
//! Reference: <https://developer.valvesoftware.com/wiki/Server_queries>.
//!
//! Copied from `idan2025/Svencoop-Prns` `controller/src/a2s.rs`, one-directional
//! per `PLAN.md` §5.
//!
//! **This is game-family-specific, and that is the point of keeping it here
//! rather than in the relay.** A2S answers for GoldSrc and Source and for
//! nothing else; Minecraft, Terraria and Valheim each speak something
//! different. `details.rs` therefore treats a live query as *optional*: a
//! server that cannot be queried still answers a detail probe, with the
//! numbers it announced and an honest flag saying they are announced rather
//! than live. When a second game family arrives, this becomes one
//! implementation behind a `GameQuery` trait selected by the pack — it is not
//! that yet, because one implementation does not tell you where the seam goes.
//!
//! The query is aimed at a server the *bridge host itself* runs, over
//! localhost. It is not a way to browse arbitrary remote servers: on this
//! platform a remote server is reached by a Link and a detail probe, never by
//! spraying UDP at strangers.

use std::io::{Cursor, Read};
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::time::timeout;

const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize)]
pub struct A2sInfo {
    pub server_name: String,
    pub map: String,
    pub game_dir: String,
    pub game_description: String,
    pub players: u8,
    pub max_players: u8,
    pub bots: u8,
    /// `"dedicated"`, `"listen"`, `"proxy"`, or the raw byte if unrecognized.
    pub server_type: String,
    /// `"linux"`, `"windows"`, `"mac"`, or the raw byte if unrecognized.
    pub environment: String,
    pub password_protected: bool,
    pub vac_secured: bool,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct A2sPlayer {
    pub index: u8,
    pub name: String,
    pub score: i32,
    pub duration_secs: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct A2sStats {
    pub info: A2sInfo,
    pub players_list: Vec<A2sPlayer>,
}

/// Query both A2S_INFO and A2S_PLAYER from a locally-running DS. `addr` is
/// almost always `127.0.0.1:<port>` — this is meant for polling our own
/// child process, not for browsing arbitrary remote servers.
pub async fn query(addr: SocketAddr) -> Result<A2sStats> {
    let sock = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("binding UDP socket for A2S query")?;
    sock.connect(addr).await.with_context(|| format!("connecting to {addr}"))?;

    let info = query_info(&sock).await?;
    // Player list is best-effort: some configs disable it, or the challenge
    // handshake can hiccup on a query sent right as the map changes. Don't
    // let that take down the whole stats panel.
    let players_list = query_players(&sock).await.unwrap_or_default();

    Ok(A2sStats { info, players_list })
}

async fn send_recv(sock: &UdpSocket, payload: &[u8]) -> Result<Vec<u8>> {
    sock.send(payload).await.context("sending A2S query")?;
    let mut buf = vec![0u8; 4096];
    let n = timeout(QUERY_TIMEOUT, sock.recv(&mut buf))
        .await
        .context("A2S query timed out — is the DS running and reachable?")??;
    buf.truncate(n);
    Ok(buf)
}

async fn query_info(sock: &UdpSocket) -> Result<A2sInfo> {
    let mut req = vec![0xFF, 0xFF, 0xFF, 0xFF, b'T'];
    req.extend_from_slice(b"Source Engine Query\0");
    let mut resp = send_recv(sock, &req).await?;

    // Some (newer-protocol) servers challenge A2S_INFO too: reply is
    // FF FF FF FF 41 <4-byte challenge>, requiring the request resent with
    // the challenge appended.
    if resp.len() >= 5 && resp[0..4] == [0xFF, 0xFF, 0xFF, 0xFF] && resp[4] == b'A' {
        let challenge = resp.get(5..9).ok_or_else(|| anyhow!("truncated A2S challenge"))?;
        req.extend_from_slice(challenge);
        resp = send_recv(sock, &req).await?;
    }

    parse_info(&resp)
}

fn parse_info(resp: &[u8]) -> Result<A2sInfo> {
    if resp.len() < 6 || resp[0..4] != [0xFF, 0xFF, 0xFF, 0xFF] {
        return Err(anyhow!("not an A2S response (bad header)"));
    }
    let mut c = Cursor::new(&resp[4..]);
    let header = read_u8(&mut c)?;
    match header {
        // Modern (post-2008) format, byte 'I'.
        b'I' => {
            let _protocol = read_u8(&mut c)?;
            let server_name = read_cstring(&mut c)?;
            let map = read_cstring(&mut c)?;
            let game_dir = read_cstring(&mut c)?;
            let game_description = read_cstring(&mut c)?;
            let _app_id = read_i16le(&mut c)?;
            let players = read_u8(&mut c)?;
            let max_players = read_u8(&mut c)?;
            let bots = read_u8(&mut c)?;
            let server_type = decode_server_type(read_u8(&mut c)?);
            let environment = decode_environment(read_u8(&mut c)?);
            let password_protected = read_u8(&mut c)? != 0;
            let vac_secured = read_u8(&mut c)? != 0;
            let version = read_cstring(&mut c)?;
            Ok(A2sInfo {
                server_name,
                map,
                game_dir,
                game_description,
                players,
                max_players,
                bots,
                server_type,
                environment,
                password_protected,
                vac_secured,
                version,
            })
        }
        // Legacy GoldSrc format, byte 'm' — obsolete but some older/custom
        // GoldSrc builds still answer this way.
        b'm' => {
            let _address = read_cstring(&mut c)?;
            let server_name = read_cstring(&mut c)?;
            let map = read_cstring(&mut c)?;
            let game_dir = read_cstring(&mut c)?;
            let game_description = read_cstring(&mut c)?;
            let _version = read_u8(&mut c)?;
            let players = read_u8(&mut c)?;
            let max_players = read_u8(&mut c)?;
            let _protocol = read_u8(&mut c)?;
            let server_type = decode_server_type(read_u8(&mut c)?);
            let environment = decode_environment(read_u8(&mut c)?);
            let password_protected = read_u8(&mut c)? != 0;
            let is_mod = read_u8(&mut c)? != 0;
            if is_mod {
                let _url_info = read_cstring(&mut c)?;
                let _url_dl = read_cstring(&mut c)?;
                let _ = read_u8(&mut c)?; // null byte separator
                let _mod_version = read_i32le(&mut c)?;
                let _mod_size = read_i32le(&mut c)?;
                let _svonly = read_u8(&mut c)?;
                let _cldll = read_u8(&mut c)?;
            }
            let vac_secured = read_u8(&mut c)? != 0;
            let bots = read_u8(&mut c)?;
            Ok(A2sInfo {
                server_name,
                map,
                game_dir,
                game_description,
                players,
                max_players,
                bots,
                server_type,
                environment,
                password_protected,
                vac_secured,
                version: String::new(),
            })
        }
        other => Err(anyhow!("unrecognized A2S_INFO response type {other:#x}")),
    }
}

async fn query_players(sock: &UdpSocket) -> Result<Vec<A2sPlayer>> {
    // Request a challenge: FF FF FF FF 55 FF FF FF FF.
    let challenge_req = [0xFF, 0xFF, 0xFF, 0xFF, b'U', 0xFF, 0xFF, 0xFF, 0xFF];
    let resp = send_recv(sock, &challenge_req).await?;
    if resp.len() < 9 || resp[0..4] != [0xFF, 0xFF, 0xFF, 0xFF] {
        return Err(anyhow!("not an A2S response (bad header)"));
    }
    match resp[4] {
        // Challenge reply: resend A2S_PLAYER with it.
        b'A' => {
            let challenge = &resp[5..9];
            let mut req = vec![0xFF, 0xFF, 0xFF, 0xFF, b'U'];
            req.extend_from_slice(challenge);
            let resp = send_recv(sock, &req).await?;
            parse_players(&resp)
        }
        // Some servers answer A2S_PLAYER directly without a challenge.
        b'D' => parse_players(&resp),
        other => Err(anyhow!("unexpected A2S_PLAYER reply type {other:#x}")),
    }
}

fn parse_players(resp: &[u8]) -> Result<Vec<A2sPlayer>> {
    if resp.len() < 6 || resp[0..4] != [0xFF, 0xFF, 0xFF, 0xFF] || resp[4] != b'D' {
        return Err(anyhow!("not an A2S_PLAYER response"));
    }
    let mut c = Cursor::new(&resp[5..]);
    let count = read_u8(&mut c)?;
    let mut players = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let index = read_u8(&mut c)?;
        let name = read_cstring(&mut c)?;
        let score = read_i32le(&mut c)?;
        let duration_secs = read_f32le(&mut c)?;
        players.push(A2sPlayer { index, name, score, duration_secs });
    }
    Ok(players)
}

fn decode_server_type(b: u8) -> String {
    match b {
        b'd' => "dedicated".to_string(),
        b'l' => "listen".to_string(),
        b'p' => "proxy".to_string(),
        other => format!("{other:#x}"),
    }
}

fn decode_environment(b: u8) -> String {
    match b {
        b'l' => "linux".to_string(),
        b'w' => "windows".to_string(),
        b'm' | b'o' => "mac".to_string(),
        other => format!("{other:#x}"),
    }
}

fn read_u8(c: &mut Cursor<&[u8]>) -> Result<u8> {
    let mut b = [0u8; 1];
    c.read_exact(&mut b).context("unexpected end of A2S response")?;
    Ok(b[0])
}

fn read_i16le(c: &mut Cursor<&[u8]>) -> Result<i16> {
    let mut b = [0u8; 2];
    c.read_exact(&mut b).context("unexpected end of A2S response")?;
    Ok(i16::from_le_bytes(b))
}

fn read_i32le(c: &mut Cursor<&[u8]>) -> Result<i32> {
    let mut b = [0u8; 4];
    c.read_exact(&mut b).context("unexpected end of A2S response")?;
    Ok(i32::from_le_bytes(b))
}

fn read_f32le(c: &mut Cursor<&[u8]>) -> Result<f32> {
    let mut b = [0u8; 4];
    c.read_exact(&mut b).context("unexpected end of A2S response")?;
    Ok(f32::from_le_bytes(b))
}

/// Null-terminated string, as A2S encodes all its text fields.
fn read_cstring(c: &mut Cursor<&[u8]>) -> Result<String> {
    let mut bytes = Vec::new();
    loop {
        let b = read_u8(c)?;
        if b == 0 {
            break;
        }
        bytes.push(b);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
