//! Mode 3, virtual LAN: the room adapter (`PLAN.md` §14, steps 2 and 4).
//!
//! The one part of a LAN room that is written per operating system. Everything
//! else — the room, the filter, the pump — is shared, and this module's own
//! [`run_room_on_adapter`] is too: a platform supplies only how to bring an
//! adapter up and open it, and how to take it away.
//!
//! - **Linux** (`linux.rs`): a TUN device. `lan-helper up` creates it
//!   persistent and owned by the user, and the launcher opens it with no
//!   capability at all.
//! - **Windows** (`windows.rs`): a Wintun adapter. Wintun lets only an
//!   administrator open one, so the Linux trick does not carry over: the
//!   elevated `lan-helper serve` holds the adapter for the whole session and
//!   relays its packets to the unprivileged launcher (`lan_relay.rs`).
//!
//! # Rules a later change could quietly break
//!
//! - **Only `gbl*` names, only the room range.** The helper is privileged and
//!   its arguments are the caller's. [`AdapterConfig::validate`] refuses any
//!   other adapter name — so it cannot be pointed at `eth0` — and any subnet
//!   outside `198.18.0.0/15` (`lan::RoomSubnet::new`), so it cannot route a
//!   real LAN into a room.
//! - **No default route.** An adapter gets its subnet and the limited
//!   broadcast, and nothing else. It is not a VPN to the internet.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

use std::io;
use std::net::Ipv4Addr;

use crate::lan::RoomSubnet;

/// Every adapter this platform makes is named with this prefix.
pub const ADAPTER_PREFIX: &str = "gbl";

/// What the adapter is given once its member is seated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterConfig {
    pub name: String,
    pub address: Ipv4Addr,
    pub subnet: RoomSubnet,
}

impl AdapterConfig {
    /// Refuse anything the helper must not be talked into.
    pub fn validate(&self) -> io::Result<()> {
        validate_name(&self.name)?;
        if RoomSubnet::new(self.subnet.prefix, self.subnet.prefix_len) != Some(self.subnet) {
            return Err(invalid(format!(
                "{}/{} is not a room subnet (rooms live in 198.18.0.0/15)",
                self.subnet.prefix, self.subnet.prefix_len
            )));
        }
        if !self.subnet.contains(self.address) || self.address == self.subnet.broadcast() {
            return Err(invalid(format!("{} is not a member address in its subnet", self.address)));
        }
        Ok(())
    }

    /// Parse `<address>/<prefix-len>`, as the helper's command line carries it.
    pub fn from_cidr(name: &str, cidr: &str) -> io::Result<Self> {
        let (addr, len) = cidr
            .split_once('/')
            .ok_or_else(|| invalid(format!("{cidr:?} is not <address>/<prefix-len>")))?;
        let address: Ipv4Addr =
            addr.parse().map_err(|_| invalid(format!("{addr:?} is not an IPv4 address")))?;
        let prefix_len: u8 =
            len.parse().map_err(|_| invalid(format!("{len:?} is not a prefix length")))?;
        let subnet = RoomSubnet::new(address, prefix_len).ok_or_else(|| {
            invalid(format!("{cidr} is not a room subnet (rooms live in 198.18.0.0/15)"))
        })?;
        let config = Self { name: name.to_string(), address, subnet };
        config.validate()?;
        Ok(config)
    }

    /// `<address>/<prefix-len>`.
    pub fn cidr(&self) -> String {
        format!("{}/{}", self.address, self.subnet.prefix_len)
    }
}

/// An adapter name: `gbl` and then letters, digits or `-`, within the 15 bytes
/// Linux allows (Windows allows more; one rule for both).
pub fn validate_name(name: &str) -> io::Result<()> {
    let ok = name.starts_with(ADAPTER_PREFIX)
        && name.len() < 16
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "{name:?} is not a room adapter name ({ADAPTER_PREFIX}*, at most 15 bytes)"
        )))
    }
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// How the adapter gets its privileged setup.
#[derive(Debug, Clone)]
pub enum AdapterSetup {
    /// Run `lan-helper` at this path — the normal case: this process holds no
    /// privilege and never elevates itself.
    Helper(std::path::PathBuf),
    /// Make the adapter in this process. Only for a process that already holds
    /// the privilege — root or `CAP_NET_ADMIN` on Linux, an administrator on
    /// Windows — such as a test.
    InProcess,
}

/// Put `session`'s room on a local adapter and pump it until `stop` resolves
/// or the room ends; then take the adapter away again.
///
/// Waits to be seated first — the adapter's address *is* the seat. If the
/// room seats this member somewhere else later (it lost the link and came back
/// to find its address taken), the adapter is moved to the new address rather
/// than left answering for the old one.
#[cfg(any(target_os = "linux", windows))]
pub async fn run_room_on_adapter(
    session: std::sync::Arc<crate::lan_session::LanSession>,
    policy: crate::lan_filter::LanPolicy,
    name: String,
    setup: AdapterSetup,
    stop: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    validate_name(&name)?;
    tokio::pin!(stop);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut current: Option<(AdapterConfig, tokio::task::JoinHandle<io::Result<()>>)> = None;

    let result = loop {
        tokio::select! {
            _ = &mut stop => break Ok(()),
            _ = tick.tick() => {}
        }
        if let Some((_, pump)) = &current {
            if pump.is_finished() {
                break Err(anyhow::anyhow!("the room adapter's pump stopped"));
            }
        }
        let view = session.view();
        if let Some(refusal) = view.refused {
            break Err(anyhow::anyhow!("the room refused this member: {refusal}"));
        }
        let Some(address) = view.own_address else { continue };
        let wanted = AdapterConfig { name: name.clone(), address, subnet: view.subnet };
        if current.as_ref().is_some_and(|(config, _)| *config == wanted) {
            continue;
        }
        // Close before reconfiguring: an adapter takes one reader.
        if let Some((_, pump)) = current.take() {
            pump.abort();
            let _ = pump.await;
        }
        let device = std::sync::Arc::new(open(&setup, &wanted).await?);
        tracing::info!(
            adapter = %name,
            address = %address,
            prefix_len = view.subnet.prefix_len,
            "the room is on this machine's adapter"
        );
        let pump = tokio::spawn(crate::lan_pump::pump(device, session.clone(), policy.clone()));
        current = Some((wanted, pump));
    };

    if let Some((_, pump)) = current.take() {
        pump.abort();
        let _ = pump.await;
    }
    close(&setup, &name).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(name: &str, prefix: Ipv4Addr, len: u8, address: Ipv4Addr) -> AdapterConfig {
        AdapterConfig {
            name: name.to_string(),
            address,
            subnet: RoomSubnet { prefix, prefix_len: len },
        }
    }

    #[test]
    fn the_helper_cannot_be_pointed_at_a_real_interface() {
        for name in [
            "eth0",
            "lo",
            "wlan0",
            "Ethernet",
            "tailscale0",
            "gbl0; rm",
            "gbl0/../x",
            "gbl-this-is-too-long",
        ] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        assert!(validate_name("gbl0").is_ok());
        assert!(validate_name("gbl-room-1").is_ok());
    }

    #[test]
    fn the_helper_cannot_route_a_real_lan() {
        let lan = config("gbl0", Ipv4Addr::new(192, 168, 1, 0), 24, Ipv4Addr::new(192, 168, 1, 7));
        assert!(lan.validate().is_err());
        // A prefix that is not masked is not a subnet `RoomSubnet::new` made.
        let unmasked =
            config("gbl0", Ipv4Addr::new(198, 19, 3, 4), 16, Ipv4Addr::new(198, 19, 3, 4));
        assert!(unmasked.validate().is_err());
        let ok = config("gbl0", Ipv4Addr::new(198, 19, 0, 0), 16, Ipv4Addr::new(198, 19, 3, 4));
        assert!(ok.validate().is_ok());
        let outside =
            config("gbl0", Ipv4Addr::new(198, 19, 0, 0), 16, Ipv4Addr::new(198, 18, 3, 4));
        assert!(outside.validate().is_err(), "an address outside its own subnet");
        let bcast =
            config("gbl0", Ipv4Addr::new(198, 19, 0, 0), 16, Ipv4Addr::new(198, 19, 255, 255));
        assert!(bcast.validate().is_err(), "the broadcast address is nobody's");
    }

    #[test]
    fn a_helper_command_line_parses_only_into_a_room_adapter() {
        let c = AdapterConfig::from_cidr("gbl0", "198.19.3.4/16").unwrap();
        assert_eq!(c.cidr(), "198.19.3.4/16");
        for bad in ["192.168.1.7/24", "198.19.3.4", "198.19.3.4/8", "x/16", "198.19.255.255/16"] {
            assert!(AdapterConfig::from_cidr("gbl0", bad).is_err(), "{bad}");
        }
        assert!(AdapterConfig::from_cidr("eth0", "198.19.3.4/16").is_err());
    }
}
