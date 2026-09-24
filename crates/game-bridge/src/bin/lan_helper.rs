//! `lan-helper`: Mode 3's privileged half (`PLAN.md` §14.1).
//!
//! The room itself runs in the unprivileged launcher; this is the one part
//! that needs privilege, and it does as little as each platform allows.
//!
//! **Linux** — create a room adapter owned by the calling user, or remove one.
//! The launcher then opens it itself (`lan_adapter::TunDevice::attach`).
//!
//! ```text
//! lan-helper up   <name> <address>/<prefix-len>
//! lan-helper down <name>
//! ```
//!
//! Grant it the one capability it needs rather than running it as root:
//! `sudo setcap cap_net_admin+ep lan-helper`.
//!
//! **Windows** — only an administrator can open a Wintun adapter, so the
//! helper holds it for the session: it creates it, connects *out* to the
//! launcher that started it, proves itself with the token that launcher made,
//! and relays packets until the launcher goes away (`lan_relay.rs`). The
//! launcher starts it through Windows' elevation prompt.
//!
//! ```text
//! lan-helper serve <name> <address>/<prefix-len> --connect 127.0.0.1:<port> --token <hex> [--wintun <dll>]
//! ```
//!
//! Every argument is the caller's, so `lan_adapter` refuses any name but
//! `gbl*` and any subnet outside the room range, and `lan_relay` any address
//! that is not this machine.

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
    game_bridge::lan_adapter::AdapterConfig::from_cidr(name, cidr).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;

    let args: Vec<String> = std::env::args().skip(1).collect();
    match serve(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lan-helper: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn serve(args: &[String]) -> Result<(), String> {
    use game_bridge::lan_adapter::{default_wintun_dll, AdapterConfig};
    use game_bridge::lan_relay::RelayToken;

    let usage = "usage: lan-helper serve <name> <address>/<prefix-len> --connect 127.0.0.1:<port> --token <hex> [--wintun <dll>]";
    let [cmd, name, cidr, rest @ ..] = args else { return Err(usage.to_string()) };
    if cmd != "serve" {
        return Err(usage.to_string());
    }
    let config = AdapterConfig::from_cidr(name, cidr).map_err(|e| e.to_string())?;
    let (mut connect, mut token, mut dll) = (None, None, None);
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--connect" => {
                connect = Some(value.parse().map_err(|_| format!("{value:?} is not an address"))?)
            }
            "--token" => token = Some(RelayToken::from_hex(value).map_err(|e| e.to_string())?),
            "--wintun" => dll = Some(std::path::PathBuf::from(value)),
            other => return Err(format!("unknown option {other:?}\n{usage}")),
        }
    }
    let connect = connect.ok_or_else(|| usage.to_string())?;
    let token = token.ok_or_else(|| usage.to_string())?;
    let dll = match dll {
        Some(d) => d,
        None => default_wintun_dll().map_err(|e| e.to_string())?,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime
        .block_on(game_bridge::lan_adapter::serve(&config, connect, &token, &dll))
        .map_err(|e| e.to_string())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn main() -> std::process::ExitCode {
    eprintln!("lan-helper: this platform's room adapter is not built yet (PLAN.md §14.3)");
    std::process::ExitCode::FAILURE
}
