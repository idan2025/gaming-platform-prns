//! `lan-helper`: Mode 3's privileged half (`PLAN.md` §14.1).
//!
//! Creates a room adapter owned by the calling user, or removes one. Nothing
//! else — the room itself runs in the unprivileged launcher, which opens the
//! adapter this made (`lan_adapter::TunDevice::attach`).
//!
//! ```text
//! lan-helper up   <name> <address>/<prefix-len>
//! lan-helper down <name>
//! ```
//!
//! Grant it the one capability it needs rather than running it as root:
//!
//! ```text
//! sudo setcap cap_net_admin+ep lan-helper
//! ```
//!
//! Every argument is the caller's, so `lan_adapter` refuses any name but
//! `gbl*` and any subnet outside the room range.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["up", name, cidr] => parse_config(name, cidr).and_then(|config| {
            game_bridge::lan_adapter::create(&config, owner()).map_err(|e| e.to_string())
        }),
        ["down", name] => game_bridge::lan_adapter::destroy(name).map_err(|e| e.to_string()),
        _ => Err("usage: lan-helper up <name> <address>/<prefix-len> | lan-helper down <name>"
            .to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lan-helper: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Whose adapter this is.
///
/// Run under `setcap`, the real uid is the user, and that is the answer — an
/// environment variable must not be able to hand the adapter to somebody else.
/// Only when actually running as root, via `sudo` or `pkexec`, is the invoking
/// user read from where those two record it.
#[cfg(target_os = "linux")]
fn owner() -> u32 {
    // SAFETY: getuid and geteuid cannot fail.
    let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
    if uid == 0 && euid == 0 {
        for var in ["PKEXEC_UID", "SUDO_UID"] {
            if let Some(id) = std::env::var(var).ok().and_then(|v| v.parse().ok()) {
                return id;
            }
        }
    }
    uid
}

#[cfg(target_os = "linux")]
fn parse_config(name: &str, cidr: &str) -> Result<game_bridge::lan_adapter::AdapterConfig, String> {
    use game_bridge::lan::RoomSubnet;
    use std::net::Ipv4Addr;

    let (addr, len) =
        cidr.split_once('/').ok_or_else(|| format!("{cidr:?} is not <address>/<prefix-len>"))?;
    let address: Ipv4Addr = addr.parse().map_err(|_| format!("{addr:?} is not an IPv4 address"))?;
    let prefix_len: u8 = len.parse().map_err(|_| format!("{len:?} is not a prefix length"))?;
    let subnet = RoomSubnet::new(address, prefix_len)
        .ok_or_else(|| format!("{cidr} is not a room subnet (rooms live in 198.18.0.0/15)"))?;
    let config =
        game_bridge::lan_adapter::AdapterConfig { name: name.to_string(), address, subnet };
    config.validate().map_err(|e| e.to_string())?;
    Ok(config)
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("lan-helper: this platform's room adapter is not built yet (PLAN.md §14.3)");
    std::process::ExitCode::FAILURE
}
