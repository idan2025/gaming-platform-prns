//! Mode 3, virtual LAN: the pump between an adapter and a room
//! (`PLAN.md` §14, step 2).
//!
//! Packets the machine sends into the adapter go to the room; packets the room
//! delivers go to the machine — each way through the member's [`LanFilter`].
//! Generic over [`PacketDevice`], so the Windows adapter (step 4) plugs into
//! the same pump and the same filter as the Linux one.

use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tracing::debug;

use crate::lan_filter::{LanFilter, LanPolicy};
use crate::lan_session::{LanSendError, LanSession};

/// Anything that reads and writes whole IPv4 packets.
pub trait PacketDevice {
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;
    fn send(&self, packet: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
}

/// Pump until the adapter fails or the room stops. Returns the adapter's error,
/// if that is what ended it.
pub async fn pump<D>(device: Arc<D>, room: Arc<LanSession>, policy: LanPolicy) -> io::Result<()>
where
    D: PacketDevice + Send + Sync + 'static,
{
    let filter = Arc::new(Mutex::new(LanFilter::new(policy)));

    let up = {
        let (device, room, filter) = (device.clone(), room.clone(), filter.clone());
        tokio::spawn(async move {
            // A TUN read returns one packet; the adapter's MTU bounds it, but
            // a buffer smaller than a packet would silently truncate one.
            let mut buf = vec![0u8; 65536];
            loop {
                let n = device.recv(&mut buf).await?;
                let packet = &buf[..n];
                let subnet = room.subnet();
                let allowed =
                    filter.lock().expect("filter lock").outbound(packet, &subnet, Instant::now());
                if !allowed {
                    continue;
                }
                match room.send(packet.to_vec()) {
                    Ok(()) => {}
                    Err(LanSendError::Stopped) => return Ok(()),
                    Err(e) => debug!(error = %e, "not sending a packet into the room"),
                }
            }
        })
    };

    let down = async {
        while let Some(packet) = room.recv().await {
            let allowed = filter.lock().expect("filter lock").inbound(&packet, Instant::now());
            if allowed {
                device.send(&packet).await?;
            } else {
                debug!("the room delivered a packet this member does not admit; dropped");
            }
        }
        Ok::<(), io::Error>(())
    };

    let mut up = up;
    let result = tokio::select! {
        r = &mut up => r.unwrap_or(Ok(())),
        r = down => r,
    };
    // Dropping a JoinHandle detaches the task rather than stopping it.
    up.abort();
    result
}
