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
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::lan_filter::{DropDirection, LanFilter, LanPolicy};
use crate::lan_rebind::Rebinder;
use crate::lan_session::{LanSendError, LanSession};

/// Anything that reads and writes whole IPv4 packets.
pub trait PacketDevice {
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;
    fn send(&self, packet: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
}

/// How often the pump looks for the game on this machine's other addresses
/// (`lan_rebind::survey`): soon enough that a join seconds after hosting finds
/// it, rarely enough to cost nothing.
const SURVEY_EVERY: Duration = Duration::from_secs(2);

/// Pump until the adapter fails or the room stops. Returns the adapter's error,
/// if that is what ended it.
pub async fn pump<D>(device: Arc<D>, room: Arc<LanSession>, policy: LanPolicy) -> io::Result<()>
where
    D: PacketDevice + Send + Sync + 'static,
{
    pump_with(device, room, policy, Rebinder::default(), crate::lan_rebind::survey).await
}

/// [`pump`] for the room adapter named `adapter`, which lets the survey tell a
/// game socket pinned to another interface (Wine's UDP) from one that is not.
pub async fn pump_on<D>(
    device: Arc<D>,
    room: Arc<LanSession>,
    policy: LanPolicy,
    adapter: &str,
) -> io::Result<()>
where
    D: PacketDevice + Send + Sync + 'static,
{
    let name = adapter.to_string();
    let survey = move |own: std::net::Ipv4Addr, ports: &[crate::lan_filter::LanPort]| {
        crate::lan_rebind::survey_on(own, ports, Some(&name))
    };
    pump_with(device, room, policy, Rebinder::on_adapter(adapter), survey).await
}

/// [`pump`], looking for the game with `survey`: the machine's own, except
/// where the adapter lives in another network namespace than the pump, as in
/// tests.
pub async fn pump_surveying<D, S>(
    device: Arc<D>,
    room: Arc<LanSession>,
    policy: LanPolicy,
    survey: S,
) -> io::Result<()>
where
    D: PacketDevice + Send + Sync + 'static,
    S: Fn(std::net::Ipv4Addr, &[crate::lan_filter::LanPort]) -> crate::lan_rebind::Survey,
{
    pump_with(device, room, policy, Rebinder::default(), survey).await
}

async fn pump_with<D, S>(
    device: Arc<D>,
    room: Arc<LanSession>,
    policy: LanPolicy,
    rebinder: Rebinder,
    survey: S,
) -> io::Result<()>
where
    D: PacketDevice + Send + Sync + 'static,
    S: Fn(std::net::Ipv4Addr, &[crate::lan_filter::LanPort]) -> crate::lan_rebind::Survey,
{
    let filter = Mutex::new(LanFilter::new(policy.clone()));
    let rebinder = Mutex::new(rebinder);

    // Both halves are futures of this one task, not a spawned one: dropping
    // the pump must close the device at once. A spawned half outlives an
    // aborted pump, keeps the descriptor open, and the adapter cannot be
    // removed ("Device or resource busy").
    let up = async {
        // A TUN read returns one packet; the adapter's MTU bounds it, but a
        // buffer smaller than a packet would silently truncate one.
        let mut buf = vec![0u8; 65536];
        loop {
            let n = device.recv(&mut buf).await?;
            // A game that listens on another of this machine's addresses
            // also answers from it; the room only ever sees the room address
            // (`lan_rebind.rs`), so the filter and the watches do too.
            //
            // What the adapter brings in from the other networks — a socket
            // pinned to the Wi-Fi, a broadcast to the Wi-Fi's own network —
            // is translated the same way, and anything still not from this
            // member's address after that is not this member's to send: a
            // forwarding host's traffic, DHCP's `0.0.0.0`.
            let subnet = room.subnet();
            let mut translated = crate::lan_rebind::Outbound::default();
            if let Some(own) = room.own_address() {
                translated = rebinder.lock().expect("rebinder lock").outbound(
                    &mut buf[..n],
                    own,
                    &subnet,
                    Instant::now(),
                );
                if crate::lan::ipv4_endpoints(&buf[..n]).is_some_and(|(src, _)| src != own) {
                    debug!("a packet from another address than this member's; not sent into the room");
                    continue;
                }
                // A broadcast the adapter copied from another network is any
                // program's, and with `inbound = "any"` the filter would pass
                // them all: mDNS and SSDP name this machine and what is on it.
                // The operating system's own chatter never goes, whatever the
                // pack says.
                if translated.source && is_chatter_broadcast(&buf[..n], &subnet) {
                    continue;
                }
            }
            let packet = &buf[..n];
            let allowed = {
                let mut f = filter.lock().expect("filter lock");
                f.sync_extra(room.extra_ports());
                f.outbound(packet, &subnet, Instant::now())
            };
            if !allowed {
                // Only a broadcast is ever refused on the way out. One the
                // adapter brought from another network is as likely another
                // program's as the game's (Steam's, a music player's), so it
                // is not named as a port the pack lacks.
                if translated.source {
                    debug!("a broadcast from another network to a port the game did not declare; dropped");
                    continue;
                }
                report_refused(room.refused_log().saw(
                    DropDirection::OutboundBroadcast,
                    packet,
                    Instant::now(),
                ));
                continue;
            }
            room.probe_log().saw_outbound(packet);
            report_dropped(room.connect_watch().saw_outbound(packet, Instant::now()));
            match room.send(packet.to_vec()) {
                Ok(()) => {}
                Err(LanSendError::Stopped) => return Ok::<(), io::Error>(()),
                Err(e) => debug!(error = %e, "not sending a packet into the room"),
            }
        }
    };

    let down = async {
        while let Some(packet) = room.recv().await {
            let allowed = {
                let mut f = filter.lock().expect("filter lock");
                f.sync_extra(room.extra_ports());
                f.inbound(&packet, Instant::now())
            };
            // Only what is addressed to this member — its room address or a
            // broadcast to the room. The adapter accepts more than its own
            // address (Linux always; Windows once weak-host, for a game on
            // another address), so a packet aimed at the Wi-Fi's address must
            // not get through by naming it.
            let own = room.own_address();
            let for_us = crate::lan::ipv4_endpoints(&packet).is_some_and(|(_, dst)| {
                Some(dst) == own || room.subnet().is_broadcast(dst) || dst.is_multicast()
            });
            if !for_us {
                debug!("the room delivered a packet addressed to someone else; dropped");
                continue;
            }
            if !allowed {
                debug!("the room delivered a packet this member does not admit; dropped");
                report_refused(room.refused_log().saw(DropDirection::Inbound, &packet, Instant::now()));
                continue;
            }
            // A room check's probe is answered here, never handed to the
            // game listening on its port (`lan_check.rs`).
            if crate::lan_check::is_probe(&packet) {
                let reply = room.own_address().and_then(|own| crate::lan_check::answer(&packet, own));
                if let Some(reply) = reply {
                    let _ = room.send(reply);
                }
                continue;
            }
            let translated = room
                .own_address()
                .and_then(|own| rebinder.lock().expect("rebinder lock").inbound(&packet, own, Instant::now()));
            device.send(translated.as_deref().unwrap_or(&packet)).await?;
            room.probe_log().saw_delivered(&packet);
            report_dropped(room.connect_watch().saw_delivered(&packet, Instant::now()));
        }
        Ok::<(), io::Error>(())
    };

    // Where the game is: on the room address, on every address, or — under
    // Wine — on the Wi-Fi's or the Ethernet's alone.
    let watch = async {
        let mut tick = tokio::time::interval(SURVEY_EVERY);
        loop {
            tick.tick().await;
            let Some(own) = room.own_address() else { continue };
            // With `any`, every port is the game's and there is no list to
            // look for; this machine's other addresses and networks are still
            // needed, for what the adapter brings in from them.
            let mut ports = Vec::new();
            if !policy.any {
                ports = policy.ports.clone();
                for p in room.extra_ports().ports() {
                    if !ports.contains(&p) {
                        ports.push(p);
                    }
                }
            }
            let survey = survey(own, &ports);
            let mut r = rebinder.lock().expect("rebinder lock");
            if r.update(survey) {
                for b in r.rebound() {
                    info!(
                        port = b.port.port,
                        address = %b.address,
                        "the game listens on {} and not on the room's address {own}; the room \
                         passes its traffic through",
                        b.address
                    );
                }
                room.set_rebound(r.rebound().to_vec());
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };

    tokio::select! {
        r = up => r,
        r = down => r,
        r = watch => r,
    }
}

/// A UDP broadcast or multicast to a port operating systems chatter on.
fn is_chatter_broadcast(packet: &[u8], subnet: &crate::lan::RoomSubnet) -> bool {
    let Some((_, dst)) = crate::lan::ipv4_endpoints(packet) else { return false };
    let ihl = (packet[0] & 0x0f) as usize * 4;
    let first = u16::from_be_bytes([packet[6], packet[7]]) & 0x1fff == 0;
    subnet.is_broadcast(dst)
        && packet[9] == 17
        && first
        && packet
            .get(ihl + 2..ihl + 4)
            .is_some_and(|p| crate::lan_filter::is_os_chatter(u16::from_be_bytes([p[0], p[1]])))
}

/// Say once, in the log, that this machine left a member's connection
/// unanswered (`lan_firewall.rs`). The launcher shows it from the watch.
fn report_dropped(found: Vec<crate::lan_firewall::Dropped>) {
    for d in found {
        warn!(
            port = d.port,
            from = %d.from,
            "a room member's TCP connection reached this machine and nothing answered it: \
             a firewall here is dropping the room's connections"
        );
    }
}

/// Say once, in the log, that the filter refused a kind of packet the game's
/// pack does not list (`lan_filter::RefusedLog`).
fn report_refused(new: Option<crate::lan_filter::Refused>) {
    if let Some(r) = new {
        warn!(
            port = r.suggested_port(),
            "the room refused {}: the game's pack does not list that port",
            r.describe()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn udp_to(dst: Ipv4Addr, dport: u16) -> Vec<u8> {
        let mut p = vec![0u8; 28];
        p[0] = 0x45;
        p[9] = 17;
        p[12..16].copy_from_slice(&[198, 19, 0, 5]);
        p[16..20].copy_from_slice(&dst.octets());
        p[22..24].copy_from_slice(&dport.to_be_bytes());
        p
    }

    /// Another program's mDNS or SSDP, copied in from the Wi-Fi, never
    /// reaches the room, even for a pack that admits every port.
    #[test]
    fn operating_system_chatter_is_never_a_broadcast_the_room_gets() {
        let subnet = crate::lan::RoomSubnet::new(Ipv4Addr::new(198, 19, 0, 0), 16).unwrap();
        assert!(is_chatter_broadcast(&udp_to(Ipv4Addr::new(224, 0, 0, 251), 5353), &subnet));
        assert!(is_chatter_broadcast(&udp_to(Ipv4Addr::new(239, 255, 255, 250), 1900), &subnet));
        assert!(is_chatter_broadcast(&udp_to(subnet.broadcast(), 27036), &subnet));
        assert!(!is_chatter_broadcast(&udp_to(Ipv4Addr::BROADCAST, 9999), &subnet), "a game's");
        assert!(
            !is_chatter_broadcast(&udp_to(Ipv4Addr::new(198, 19, 0, 7), 5353), &subnet),
            "a unicast is not a broadcast"
        );
    }
}
