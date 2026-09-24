//! `lan-helper`: Mode 3's privileged half (`PLAN.md` §14.1).
//!
//! The room itself runs in the unprivileged launcher; this is the one part
//! that needs privilege, and it does one thing: hold the room's adapter for as
//! long as the launcher that started it is there. It creates the adapter,
//! connects *out* to that launcher, proves itself with the token the launcher
//! made, and relays packets until the launcher goes away (`lan_relay.rs`).
//! Then it exits, and the adapter goes with it — on Linux a non-persistent
//! TUN device, on Windows a Wintun adapter.
//!
//! ```text
//! lan-helper serve <name> <address>/<prefix-len> --connect 127.0.0.1:<port> --token <hex>
//!                  [--wintun <dll>] [--remove-driver]      (Windows)
//! lan-helper check
//! ```
//!
//! How it gets its privilege is the platform's: on Linux, `cap_net_admin`
//! granted once (`sudo setcap cap_net_admin+ep lan-helper`) or `pkexec` per
//! room; on Windows, the elevation prompt. `check` says whether `serve` could
//! work right now, without doing anything.
//!
//! Every argument is the caller's, so `lan_adapter` refuses any name but
//! `gbl*` and any subnet outside the room range, and `lan_relay` any address
//! that is not this machine.

/// `serve`'s arguments after the adapter name and CIDR.
#[cfg(any(target_os = "linux", windows))]
struct ServeArgs {
    connect: std::net::SocketAddr,
    token: game_bridge::lan_relay::RelayToken,
    wintun: Option<std::path::PathBuf>,
    remove_driver: bool,
}

#[cfg(any(target_os = "linux", windows))]
const USAGE: &str =
    "usage: lan-helper serve <name> <address>/<prefix-len> --connect 127.0.0.1:<port> \
                     --token <hex> [--wintun <dll>] [--remove-driver] | lan-helper check";

#[cfg(any(target_os = "linux", windows))]
fn parse_serve(rest: &[String]) -> Result<ServeArgs, String> {
    use game_bridge::lan_relay::RelayToken;
    let (mut connect, mut token, mut wintun, mut remove_driver) = (None, None, None, false);
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        if flag == "--remove-driver" {
            remove_driver = true;
            continue;
        }
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--connect" => {
                connect = Some(value.parse().map_err(|_| format!("{value:?} is not an address"))?)
            }
            "--token" => token = Some(RelayToken::from_hex(value).map_err(|e| e.to_string())?),
            "--wintun" => wintun = Some(value.into()),
            other => return Err(format!("unknown option {other:?}\n{USAGE}")),
        }
    }
    Ok(ServeArgs {
        connect: connect.ok_or(USAGE)?,
        token: token.ok_or(USAGE)?,
        wintun,
        remove_driver,
    })
}

#[cfg(any(target_os = "linux", windows))]
fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [cmd] if cmd == "check" => check(),
        [cmd, name, cidr, rest @ ..] if cmd == "serve" => serve(name, cidr, rest),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lan-helper: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
fn serve(name: &str, cidr: &str, rest: &[String]) -> Result<(), String> {
    let config = game_bridge::lan_adapter::AdapterConfig::from_cidr(name, cidr)
        .map_err(|e| e.to_string())?;
    let args = parse_serve(rest)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    #[cfg(target_os = "linux")]
    let run = {
        let _ = (&args.wintun, args.remove_driver);
        game_bridge::lan_adapter::serve(&config, args.connect, &args.token)
    };
    #[cfg(windows)]
    let dll = match args.wintun {
        Some(d) => d,
        None => game_bridge::lan_adapter::default_wintun_dll().map_err(|e| e.to_string())?,
    };
    #[cfg(windows)]
    let run = game_bridge::lan_adapter::serve(
        &config,
        args.connect,
        &args.token,
        &dll,
        args.remove_driver,
    );
    runtime.block_on(run).map_err(|e| e.to_string())
}

/// Whether `serve` could make an adapter now.
///
/// Linux: this process holds `CAP_NET_ADMIN`, granted by file capability or by
/// running as root. A capability on a binary in a `nosuid` mount — an
/// AppImage's — is silently not applied, and this is where that shows.
#[cfg(target_os = "linux")]
fn check() -> Result<(), String> {
    const CAP_NET_ADMIN: u32 = 12;
    let status = std::fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?;
    let eff = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        .ok_or("could not read this process's capabilities")?;
    if eff & (1 << CAP_NET_ADMIN) != 0 {
        println!("ok");
        Ok(())
    } else {
        Err("no CAP_NET_ADMIN: grant it with `sudo setcap cap_net_admin+ep` on this file"
            .to_string())
    }
}

/// Windows: `wintun.dll` is where `serve` will load it from. Needs no
/// elevation — the prompt is `serve`'s, when a room starts.
#[cfg(windows)]
fn check() -> Result<(), String> {
    let dll = game_bridge::lan_adapter::default_wintun_dll().map_err(|e| e.to_string())?;
    if dll.is_file() {
        println!("ok");
        Ok(())
    } else {
        Err(format!("no wintun.dll at {}", dll.display()))
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn main() -> std::process::ExitCode {
    eprintln!("lan-helper: this platform's room adapter is not built yet (PLAN.md §14.3)");
    std::process::ExitCode::FAILURE
}
