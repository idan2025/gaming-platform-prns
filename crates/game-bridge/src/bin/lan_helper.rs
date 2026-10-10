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
//!                  [--portable] [--wintun <dll>] [--remove-driver]
//! lan-helper check
//! lan-helper allow-rooms [<name>]
//! lan-helper grant                                         (Linux)
//! lan-helper unblock <hex rule name>...                    (Windows)
//! ```
//!
//! **The firewall** (`lan_firewall.rs`). A firewall that refuses incoming
//! connections drops what the room delivers, and a player sees a game they
//! cannot join. So `serve`, when it runs with full privilege — always on
//! Windows, as root through `pkexec` on Linux — opens the firewall to the room
//! before relaying: kept for good on an installed launcher, removed again
//! after a `--portable` one. `allow-rooms` does only that, for a launcher
//! whose helper has only a capability (it cannot run ufw); `grant` is the
//! one-time Linux grant and `allow-rooms` in one password prompt. `unblock`
//! disables Windows Block rules a person picked in the launcher. None of it
//! is ever asked for over the relay: the relay carries packets only.
//!
//! These are the only places the helper runs another program, and only with
//! full privilege: a child does not inherit a file capability, so under a
//! granted `cap_net_admin` alone the helper runs nothing.
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
    /// Leave nothing behind: the Wintun driver, and this room's firewall rule.
    portable: bool,
}

#[cfg(any(target_os = "linux", windows))]
const USAGE: &str =
    "usage: lan-helper serve <name> <address>/<prefix-len> --connect 127.0.0.1:<port> \
                     --token <hex> [--portable] [--wintun <dll>] [--remove-driver] | lan-helper check \
                     | lan-helper allow-rooms [<name>] | lan-helper grant | lan-helper unblock <hex>...";

#[cfg(any(target_os = "linux", windows))]
fn parse_serve(rest: &[String]) -> Result<ServeArgs, String> {
    use game_bridge::lan_relay::RelayToken;
    let (mut connect, mut token, mut wintun, mut remove_driver) = (None, None, None, false);
    let mut portable = false;
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        if flag == "--remove-driver" {
            remove_driver = true;
            continue;
        }
        if flag == "--portable" {
            portable = true;
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
        portable,
    })
}

#[cfg(any(target_os = "linux", windows))]
fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [cmd] if cmd == "check" => check(),
        [cmd, name, cidr, rest @ ..] if cmd == "serve" => serve(name, cidr, rest),
        [cmd] if cmd == "allow-rooms" => allow_rooms(DEFAULT_ADAPTER),
        [cmd, name] if cmd == "allow-rooms" => allow_rooms(name),
        #[cfg(target_os = "linux")]
        [cmd] if cmd == "grant" => grant(),
        #[cfg(windows)]
        [cmd, names @ ..] if cmd == "unblock" && !names.is_empty() => unblock(names),
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

/// The adapter the launcher makes; `allow-rooms` opens it when not told
/// another.
#[cfg(any(target_os = "linux", windows))]
const DEFAULT_ADAPTER: &str = "gbl0";

/// Whether this process may change the firewall: an administrator on Windows
/// (which `serve` always is), root on Linux — not a capability alone.
#[cfg(any(target_os = "linux", windows))]
fn fully_privileged() -> bool {
    #[cfg(windows)]
    {
        game_bridge::lan_adapter::is_elevated()
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
}

/// Open the firewall to the room; a failure is said and does not stop the
/// room — the launcher's watch still names what is dropped.
#[cfg(any(target_os = "linux", windows))]
fn open_firewall(
    name: &str,
    lifetime: game_bridge::lan_firewall::Lifetime,
) -> game_bridge::lan_firewall::Opened {
    use game_bridge::lan_firewall::{detect, open, Opened, System};
    if !fully_privileged() {
        return Opened::Nothing;
    }
    match open(&System, detect(), name, lifetime) {
        Ok(opened) => opened,
        Err(e) => {
            eprintln!("lan-helper: the firewall was not opened to the room: {e}");
            Opened::Nothing
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
fn allow_rooms(name: &str) -> Result<(), String> {
    use game_bridge::lan_firewall::{detect, open, Lifetime, Opened, System};
    if !fully_privileged() {
        return Err("allow-rooms needs an administrator (Windows) or root (Linux)".to_string());
    }
    let firewall = detect();
    match open(&System, firewall, name, Lifetime::Kept).map_err(|e| e.to_string())? {
        Opened::Added(_) => println!("opened {firewall:?} to the rooms"),
        Opened::AlreadyOpen(_) => println!("{firewall:?} was already open to the rooms"),
        Opened::Nothing => println!("no firewall here this helper knows how to open"),
    }
    Ok(())
}

/// The one-time Linux grant: the capability on this file, and the firewall
/// opened to the rooms, under one password prompt. Run through `pkexec` on the
/// installed helper itself — never a copy, or the capability would land on the
/// copy.
#[cfg(target_os = "linux")]
fn grant() -> Result<(), String> {
    if !fully_privileged() {
        return Err("grant needs root; the launcher runs it through pkexec".to_string());
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let status = std::process::Command::new("setcap")
        .arg("cap_net_admin+ep")
        .arg(&me)
        .status()
        .map_err(|e| format!("running setcap: {e}"))?;
    if !status.success() {
        return Err(format!("setcap failed ({status})"));
    }
    // The grant is what was asked for; the firewall is a kindness on top.
    if let Err(e) = allow_rooms(DEFAULT_ADAPTER) {
        eprintln!("lan-helper: granted, but the firewall was not opened: {e}");
    }
    Ok(())
}

/// Disable the Windows Block rules named, hex-encoded so a rule name — which
/// carries a program's path, spaces and all — survives the elevation prompt's
/// one command line intact.
#[cfg(windows)]
fn unblock(names: &[String]) -> Result<(), String> {
    if !fully_privileged() {
        return Err("unblock needs an administrator".to_string());
    }
    let names = names
        .iter()
        .map(|h| {
            hex::decode(h)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(|| format!("{h:?} is not a hex-encoded rule name"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let n = game_bridge::lan_firewall::unblock(&game_bridge::lan_firewall::System, &names)
        .map_err(|e| e.to_string())?;
    println!("unblocked {n}");
    Ok(())
}

#[cfg(any(target_os = "linux", windows))]
fn serve(name: &str, cidr: &str, rest: &[String]) -> Result<(), String> {
    let config = game_bridge::lan_adapter::AdapterConfig::from_cidr(name, cidr)
        .map_err(|e| e.to_string())?;
    let args = parse_serve(rest)?;
    let lifetime = if args.portable {
        game_bridge::lan_firewall::Lifetime::ThisRoom
    } else {
        game_bridge::lan_firewall::Lifetime::Kept
    };
    let opened = open_firewall(name, lifetime);
    // A game bound to the Wi-Fi's or the Ethernet's address answers through
    // the room only with this on (`lan_rebind.rs`); off again when the room
    // ends. Nothing on Linux.
    let weak_host = game_bridge::lan_rebind::WeakHostSend::on();
    let result = serve_room(&config, args);
    drop(weak_host);
    if let Err(e) = game_bridge::lan_firewall::close(
        &game_bridge::lan_firewall::System,
        opened,
        name,
        lifetime,
    ) {
        eprintln!("lan-helper: {e}");
    }
    result
}

#[cfg(any(target_os = "linux", windows))]
fn serve_room(
    config: &game_bridge::lan_adapter::AdapterConfig,
    args: ServeArgs,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    #[cfg(target_os = "linux")]
    let run = {
        let _ = (&args.wintun, args.remove_driver, args.portable);
        game_bridge::lan_adapter::serve(config, args.connect, &args.token)
    };
    #[cfg(windows)]
    let dll = match args.wintun {
        Some(d) => d,
        None => game_bridge::lan_adapter::default_wintun_dll().map_err(|e| e.to_string())?,
    };
    #[cfg(windows)]
    let run = game_bridge::lan_adapter::serve(
        config,
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
