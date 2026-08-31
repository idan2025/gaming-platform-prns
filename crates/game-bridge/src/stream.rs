//! `StreamRelay`: TCP games over a link, `DESIGN.md` §2.1.
//!
//! The datagram relay (`relay.rs`) pumps one game packet per link packet and
//! reassembles with `framing.rs`. A TCP game cannot use that path, and not
//! because of message sizes: **a stream needs the bytes to arrive once, in
//! order, and it needs the close to mean something.** A link data packet is
//! acknowledged individually and carries no sequence number
//! (`prns-core/src/routing/links/data.rs:149`), so a splice built on
//! `SendToLink` would have to invent sequencing, retransmission and a
//! half-close signal — a second transport protocol living inside a game bridge.
//!
//! The pinned engine already has one. RNS's `Channel` is reliable and
//! **in-order** — `ChannelSequence` is described as exactly that ordering key
//! (`prns-core/src/routing/links/channel/mod.rs:27`) — and the tokio runtime
//! layers a byte stream on top of it, handing out an `AsyncRead` and an
//! `AsyncWrite` with an EOF flag on the wire
//! (`prns-runtime/impls/tokio/src/runtime/node_facade/byte_stream/mod.rs:214`).
//!
//! So `StreamRelay` is a splice, not a protocol: copy the TCP socket into the
//! stream writer and the stream reader into the socket, and propagate each
//! end's close to the other. `framing.rs` is not used here at all — chunking
//! and reassembly are the channel's job, and adding a second layer of framing
//! would be a second place for a stream to lose its boundaries.
//!
//! # One link per TCP connection
//!
//! The datagram client multiplexes by the game client's source address; a TCP
//! client cannot, because a connection is a lifetime and not an address. Each
//! accepted connection gets its own link, and closing either closes the other.
//! That also makes the allowlist and the identify handshake work unchanged: a
//! link is still one player's session.
//!
//! # Why the reader is registered before the socket is connected
//!
//! `PrnsNodeHandle::byte_stream_reader` registers a sink with the run loop, and
//! stream data that arrives before it is registered is forwarded past it and
//! dropped. On the server side the allowlist can hold a link for seconds
//! waiting for the peer to identify, and a game client does not wait — TCP
//! games typically send their handshake immediately. So the server registers
//! the reader the moment the link is up and connects to the game only once the
//! peer is allowed: the bytes buffer in the sink, and an allowed peer loses
//! nothing, including its first packet. The datagram path has the same property
//! for the same reason, achieved with an mpsc buffer (`relay.rs`).

use std::io;

use personal_rns::prelude::PrnsNodeHandle;
use prns_core::routing::links::channel::byte_stream::StreamId;
use prns_core::routing::links::LinkId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

pub use personal_rns::runtime::{ByteStreamReader, ByteStreamWriter};

/// The stream id pair carrying one channel's two directions.
///
/// Derived from the channel rather than negotiated: one link is one connection,
/// so there is nothing to multiplex within a channel and nothing to agree on.
/// Channel 0 keeps ids 1 and 2, which is what the single-port stream relay
/// used before extra ports existed.
///
/// This is how a TCP extra port (`GAMES.md` §3, RCON beside a game port) rides
/// a link **without** touching framing's channel bits: those bits are a
/// datagram concern, and a stream never passes through `frame`.
pub const fn stream_ids(channel: u8) -> (StreamId, StreamId) {
    let to_server = match StreamId::new(2 * channel as u16 + 1) {
        Ok(id) => id,
        Err(_) => panic!("a channel is at most 7, so its stream ids are small"),
    };
    let to_client = match StreamId::new(2 * channel as u16 + 2) {
        Ok(id) => id,
        Err(_) => panic!("a channel is at most 7, so its stream ids are small"),
    };
    (to_server, to_client)
}

/// How much of the TCP socket to move per channel write.
///
/// The writer chunks to the channel's own ceiling internally; this only bounds
/// the buffer the relay holds per direction.
const COPY_BUF: usize = 16 * 1024;

/// Open the server side of a link's byte stream: read what the client wrote,
/// write what the game answers.
pub async fn server_stream(
    handle: &PrnsNodeHandle,
    link_id: LinkId,
    channel: u8,
) -> (ByteStreamReader, ByteStreamWriter) {
    let (to_server, to_client) = stream_ids(channel);
    handle.byte_stream(link_id, to_server, to_client).await
}

/// Open the client side of a link's byte stream. Mirrors `server_stream`.
pub async fn client_stream(
    handle: &PrnsNodeHandle,
    link_id: LinkId,
    channel: u8,
) -> (ByteStreamReader, ByteStreamWriter) {
    let (to_server, to_client) = stream_ids(channel);
    handle.byte_stream(link_id, to_client, to_server).await
}

/// Bytes moved by one splice, socket-to-link and link-to-socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpliceTotals {
    pub to_link: u64,
    pub to_socket: u64,
}

/// Pump a TCP connection against a link's byte stream until either side ends.
///
/// Returns when **both** directions have finished. A close in one direction
/// does not end the other: a game client that has said everything it intends to
/// say still has to hear the reply, and half-closing a connection is a thing
/// TCP clients actually do.
pub async fn splice(
    tcp: TcpStream,
    reader: ByteStreamReader,
    writer: ByteStreamWriter,
) -> io::Result<SpliceTotals> {
    splice_with_prefix(tcp, reader, writer, Vec::new()).await
}

/// `splice`, with bytes already read from the stream written to the socket
/// first.
///
/// An extra TCP port is connected **lazily**, on its first byte: a node should
/// not open an RCON connection to its own game server for every player who
/// joins, and most links never carry one. Those first bytes are already out of
/// the reader by then, so they are handed back in here rather than dropped.
pub async fn splice_with_prefix(
    tcp: TcpStream,
    mut reader: ByteStreamReader,
    mut writer: ByteStreamWriter,
    prefix: Vec<u8>,
) -> io::Result<SpliceTotals> {
    // Nagle would batch a game's small writes into the latency it is trying to
    // avoid, and every byte here already pays a mesh round trip.
    let _ = tcp.set_nodelay(true);
    let (mut socket_read, mut socket_write) = tcp.into_split();
    if !prefix.is_empty() {
        socket_write.write_all(&prefix).await?;
    }

    let to_link = tokio::spawn(async move {
        let mut buf = vec![0u8; COPY_BUF];
        let mut total = 0u64;
        loop {
            let n = match socket_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if writer.write_all(&buf[..n]).await.is_err() {
                break;
            }
            total += n as u64;
        }
        // The EOF flag on the far side's reader, so a game server sees its
        // client hang up rather than a connection that stays open forever.
        let _ = writer.shutdown().await;
        total
    });

    let to_socket = tokio::spawn(async move {
        let mut buf = vec![0u8; COPY_BUF];
        let mut total = 0u64;
        loop {
            let n = match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if socket_write.write_all(&buf[..n]).await.is_err() {
                break;
            }
            total += n as u64;
        }
        let _ = socket_write.shutdown().await;
        total
    });

    let (to_link, to_socket) = tokio::join!(to_link, to_socket);
    let totals = SpliceTotals {
        to_link: to_link.unwrap_or(0),
        to_socket: to_socket.unwrap_or(0),
    };
    debug!(
        to_link = totals.to_link,
        to_socket = totals.to_socket,
        "stream relay ended"
    );
    Ok(totals)
}

/// Wait for a stream's first bytes, then connect and splice.
///
/// The lazy half of an extra TCP port: the reader is registered when the link
/// comes up (see the module docs on why that has to be early), but nothing is
/// connected until a peer actually speaks on it.
pub async fn splice_on_first_byte(
    addr: std::net::SocketAddr,
    mut reader: ByteStreamReader,
    writer: ByteStreamWriter,
) -> io::Result<SpliceTotals> {
    let mut first = vec![0u8; COPY_BUF];
    let n = reader.read(&mut first).await?;
    if n == 0 {
        return Ok(SpliceTotals::default());
    }
    let tcp = TcpStream::connect(addr).await?;
    // The socket did not exist when these arrived, so they go out first.
    first.truncate(n);
    splice_with_prefix(tcp, reader, writer, first).await
}
