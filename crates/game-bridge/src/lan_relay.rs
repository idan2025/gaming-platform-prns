//! Mode 3, virtual LAN: the link between an elevated helper and the launcher
//! (`PLAN.md` §14.1, step 4).
//!
//! On Windows only an administrator can open a Wintun adapter, so the helper
//! cannot create the adapter and step away as it does on Linux: it holds the
//! adapter for the whole session, and hands its packets to the launcher — which
//! runs the room, the filter and the pump unprivileged — over this link.
//!
//! # Rules a later change could quietly break
//!
//! - **The elevated side never listens.** The launcher binds a loopback port
//!   and the helper connects *out* to it, so an administrator process exposes no
//!   socket anything else on the machine could reach. The helper refuses any
//!   address that is not loopback.
//! - **The helper proves itself with a one-time token** the launcher made and
//!   passed on its command line. Without it, any process of the user's could
//!   connect first and read or inject the room's traffic. A connection with the
//!   wrong token is dropped and the launcher keeps waiting, so a squatter cannot
//!   deny the real helper either.
//! - **Only packets cross.** A frame is a length and an IP packet, in both
//!   directions; there is no message that asks the helper to *do* anything. Its
//!   configuration came from its own validated command line, once.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

use crate::lan_pump::PacketDevice;

/// Opens every helper connection, before the token.
pub const RELAY_MAGIC: &[u8; 4] = b"GBLR";
pub const RELAY_VERSION: u8 = 1;
pub const TOKEN_LEN: usize = 32;
/// How long a connection has to say who it is.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// The launcher's one-time secret for one helper.
#[derive(Clone, PartialEq, Eq)]
pub struct RelayToken([u8; TOKEN_LEN]);

impl core::fmt::Debug for RelayToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "RelayToken(..)")
    }
}

impl RelayToken {
    pub fn random() -> io::Result<Self> {
        let mut bytes = [0u8; TOKEN_LEN];
        getrandom::getrandom(&mut bytes)
            .map_err(|e| io::Error::other(format!("no entropy: {e}")))?;
        Ok(Self(bytes))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> io::Result<Self> {
        let bytes = hex::decode(s.trim()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "the relay token is not hex")
        })?;
        let arr: [u8; TOKEN_LEN] = bytes.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "the relay token is the wrong length")
        })?;
        Ok(Self(arr))
    }

    /// Constant-time, so a wrong guess learns nothing from how long it took.
    fn matches(&self, other: &[u8]) -> bool {
        other.len() == TOKEN_LEN
            && self.0.iter().zip(other).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }
}

/// Launcher side: wait up to `within` for the helper to connect and prove
/// itself. Connections that fail to are dropped and waiting continues.
pub async fn accept_helper(
    listener: &TcpListener,
    token: &RelayToken,
    within: Duration,
) -> io::Result<RemoteDevice> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let (mut stream, peer) =
            tokio::time::timeout_at(deadline, listener.accept()).await.map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "the LAN helper never connected")
            })??;
        if !peer.ip().is_loopback() {
            continue;
        }
        let mut hello = [0u8; 4 + 1 + TOKEN_LEN];
        let read = tokio::time::timeout(HELLO_TIMEOUT, stream.read_exact(&mut hello)).await;
        let proven = matches!(read, Ok(Ok(_)))
            && &hello[..4] == RELAY_MAGIC
            && hello[4] == RELAY_VERSION
            && token.matches(&hello[5..]);
        if !proven {
            tracing::warn!(%peer, "a connection to the LAN helper port did not prove itself; dropped");
            continue;
        }
        stream.set_nodelay(true)?;
        return Ok(RemoteDevice::new(stream));
    }
}

/// Helper side: connect out to the launcher and prove this is the helper it
/// started.
pub async fn connect_to_launcher(addr: SocketAddr, token: &RelayToken) -> io::Result<TcpStream> {
    if !addr.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{addr} is not loopback; the LAN helper only talks to this machine"),
        ));
    }
    let mut stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    let mut hello = Vec::with_capacity(4 + 1 + TOKEN_LEN);
    hello.extend_from_slice(RELAY_MAGIC);
    hello.push(RELAY_VERSION);
    hello.extend_from_slice(&token.0);
    stream.write_all(&hello).await?;
    Ok(stream)
}

async fn read_frame(reader: &mut OwnedReadHalf, buf: &mut [u8]) -> io::Result<Option<usize>> {
    let mut len = [0u8; 2];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u16::from_be_bytes(len) as usize;
    if len == 0 || len > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a relay frame of {len} bytes"),
        ));
    }
    reader.read_exact(&mut buf[..len]).await?;
    Ok(Some(len))
}

async fn write_frame(writer: &mut OwnedWriteHalf, packet: &[u8]) -> io::Result<()> {
    let len = u16::try_from(packet.len()).ok().filter(|&l| l > 0).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "a packet no frame can carry")
    })?;
    let mut frame = Vec::with_capacity(2 + packet.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(packet);
    writer.write_all(&frame).await
}

/// Helper side: move packets between the adapter and the launcher until the
/// launcher goes away — which is how the helper learns the room is over.
pub async fn relay<D: PacketDevice>(device: &D, stream: TcpStream) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let to_launcher = async {
        let mut buf = vec![0u8; 65536];
        loop {
            let n = device.recv(&mut buf).await?;
            write_frame(&mut writer, &buf[..n]).await?;
        }
    };
    let to_adapter = async {
        let mut buf = vec![0u8; 65536];
        while let Some(n) = read_frame(&mut reader, &mut buf).await? {
            device.send(&buf[..n]).await?;
        }
        Ok::<(), io::Error>(())
    };
    tokio::select! {
        r = to_launcher => r,
        r = to_adapter => r,
    }
}

/// Launcher side: an adapter held by the helper, seen through the link.
pub struct RemoteDevice {
    reader: tokio::sync::Mutex<OwnedReadHalf>,
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
}

impl RemoteDevice {
    fn new(stream: TcpStream) -> Self {
        let (reader, writer) = stream.into_split();
        Self { reader: tokio::sync::Mutex::new(reader), writer: tokio::sync::Mutex::new(writer) }
    }
}

impl PacketDevice for RemoteDevice {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match read_frame(&mut *self.reader.lock().await, buf).await? {
            Some(n) => Ok(n),
            None => Err(io::Error::new(io::ErrorKind::BrokenPipe, "the LAN helper went away")),
        }
    }

    async fn send(&self, packet: &[u8]) -> io::Result<()> {
        write_frame(&mut *self.writer.lock().await, packet).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex};

    /// An adapter stand-in: packets "from the machine" are pushed into
    /// `from_os`, and what the room writes lands in `to_os`.
    struct FakeAdapter {
        from_os: Mutex<mpsc::Receiver<Vec<u8>>>,
        to_os: mpsc::Sender<Vec<u8>>,
    }

    impl PacketDevice for FakeAdapter {
        async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
            let p = self.from_os.lock().await.recv().await.ok_or(io::ErrorKind::BrokenPipe)?;
            buf[..p.len()].copy_from_slice(&p);
            Ok(p.len())
        }
        async fn send(&self, packet: &[u8]) -> io::Result<()> {
            self.to_os.send(packet.to_vec()).await.map_err(|_| io::ErrorKind::BrokenPipe.into())
        }
    }

    async fn listener() -> (TcpListener, SocketAddr) {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let a = l.local_addr().unwrap();
        (l, a)
    }

    #[tokio::test]
    async fn packets_cross_both_ways_between_helper_and_launcher() {
        let (listener, addr) = listener().await;
        let token = RelayToken::random().unwrap();
        let (os_tx, os_rx) = mpsc::channel(8);
        let (to_os_tx, mut to_os_rx) = mpsc::channel(8);
        let adapter = Arc::new(FakeAdapter { from_os: Mutex::new(os_rx), to_os: to_os_tx });

        let helper_token = token.clone();
        let helper_adapter = adapter.clone();
        let helper = tokio::spawn(async move {
            let stream = connect_to_launcher(addr, &helper_token).await.unwrap();
            relay(&*helper_adapter, stream).await
        });
        let remote = accept_helper(&listener, &token, Duration::from_secs(5)).await.unwrap();

        os_tx.send(b"from the machine".to_vec()).await.unwrap();
        let mut buf = [0u8; 2048];
        let n = remote.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"from the machine");

        remote.send(b"from the room").await.unwrap();
        assert_eq!(to_os_rx.recv().await.unwrap(), b"from the room");

        // The launcher going away is how the helper learns the room is over.
        drop(remote);
        let ended = tokio::time::timeout(Duration::from_secs(5), helper).await;
        assert!(ended.is_ok(), "the helper did not notice the launcher leave");
    }

    /// A process that connects first without the token must neither get the
    /// room's traffic nor keep the real helper out.
    #[tokio::test]
    async fn a_connection_without_the_token_is_dropped_and_the_real_helper_still_gets_in() {
        let (listener, addr) = listener().await;
        let token = RelayToken::random().unwrap();
        let wrong = RelayToken::random().unwrap();

        let squatter = tokio::spawn(async move {
            let mut s = connect_to_launcher(addr, &wrong).await.unwrap();
            let mut buf = [0u8; 1];
            // Dropped without a byte: read returns 0 (or an error), never data.
            matches!(s.read(&mut buf).await, Ok(0) | Err(_))
        });
        let real_token = token.clone();
        let real = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            connect_to_launcher(addr, &real_token).await.unwrap()
        });

        let remote = accept_helper(&listener, &token, Duration::from_secs(5)).await;
        assert!(remote.is_ok(), "the real helper was kept out");
        assert!(squatter.await.unwrap(), "the squatter was sent something");
        drop(real.await.unwrap());
    }

    #[tokio::test]
    async fn the_helper_only_talks_to_this_machine() {
        let token = RelayToken::random().unwrap();
        let far = SocketAddr::from((Ipv4Addr::new(203, 0, 113, 7), 4000));
        let err = connect_to_launcher(far, &token).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_token_survives_the_command_line_and_nothing_else_passes_for_it() {
        let t = RelayToken::random().unwrap();
        assert_eq!(RelayToken::from_hex(&t.to_hex()).unwrap(), t);
        assert!(RelayToken::from_hex("abcd").is_err());
        assert!(RelayToken::from_hex(&"zz".repeat(TOKEN_LEN)).is_err());
        assert!(!t.matches(&[0u8; TOKEN_LEN]));
        assert!(!t.matches(&t.0[..TOKEN_LEN - 1]));
        assert_eq!(format!("{t:?}"), "RelayToken(..)", "a token is never logged");
    }
}
