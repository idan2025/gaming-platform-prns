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

/// Stream id carrying game-client bytes towards the server.
///
/// Fixed rather than negotiated: one link is one connection here, so there is
/// nothing to multiplex and nothing to agree on. The pair below is the whole
/// wire contract of the stream relay, and both ends read the direction they do
/// not write.
pub const CLIENT_TO_SERVER: StreamId = match StreamId::new(1) {
    Ok(id) => id,
    Err(_) => panic!("stream id 1 is in range"),
};

/// Stream id carrying game-server bytes towards the client.
pub const SERVER_TO_CLIENT: StreamId = match StreamId::new(2) {
    Ok(id) => id,
    Err(_) => panic!("stream id 2 is in range"),
};

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
) -> (ByteStreamReader, ByteStreamWriter) {
    handle
        .byte_stream(link_id, CLIENT_TO_SERVER, SERVER_TO_CLIENT)
        .await
}

/// Open the client side of a link's byte stream. Mirrors `server_stream`.
pub async fn client_stream(
    handle: &PrnsNodeHandle,
    link_id: LinkId,
) -> (ByteStreamReader, ByteStreamWriter) {
    handle
        .byte_stream(link_id, SERVER_TO_CLIENT, CLIENT_TO_SERVER)
        .await
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
    mut reader: ByteStreamReader,
    mut writer: ByteStreamWriter,
) -> io::Result<SpliceTotals> {
    // Nagle would batch a game's small writes into the latency it is trying to
    // avoid, and every byte here already pays a mesh round trip.
    let _ = tcp.set_nodelay(true);
    let (mut socket_read, mut socket_write) = tcp.into_split();

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
