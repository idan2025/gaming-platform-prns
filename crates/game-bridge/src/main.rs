//! `game-bridge` — run one of the four roles (`PLAN.md` §1) from a terminal.
//!
//! The crate is a library first: the launcher drives `BridgeSession` directly
//! and needs no process boundary. This binary exists because two of the four
//! roles have no other way to run today — a person with a game server and no
//! desktop cannot host from the launcher, and someone donating transit should
//! not need a GUI to do it.
//!
//! Which game a role speaks comes from a **pack read off disk**, never from
//! flags. A pack is the schema for that (`pack.rs`), and a `--game-port` that
//! could contradict it would be a second place the same fact is written down.
//! The one exception is `--game-port`, which says where the *already running*
//! game server is, not what game it is.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use game_bridge::config::{
    AnnounceFormat, BridgeConfig, BrowserArgs, ClientArgs, RelayArgs, ServerArgs,
};
use game_bridge::signing::{PackSignature, TrustPolicy, SIGNATURE_SUFFIX};
use game_bridge::{run_bridge, GamePack};
use personal_rns::identity::PrivateIdentityMaterial;
use personal_rns::load_or_create_identity_secret;
use personal_rns::prelude::*;

const USAGE: &str = "\
game-bridge — bridge a game server, or a game client, over Reticulum

usage: game-bridge server <game-id> [options]
       game-bridge client <game-id> [options]
       game-bridge relay  [options]
       game-bridge browse [options]
       game-bridge lan-host <game-id> [options]
       game-bridge lan-join <game-id> [options]
       game-bridge sign   <pack.toml> [options]
       game-bridge verify <pack.toml> [options]
       game-bridge repo build <dir> [--name NAME] [--identity PATH [--days N]]
       game-bridge repo mirror [<source>] <dir> [--key HEX]
       game-bridge repo seed <dir> [--from <source>|--official] [--key HEX]
                             [--every HOURS] [--identity PATH] [--tcp HOST:PORT] [--auto]
       game-bridge review <pack.toml>... [--against DIR] [--markdown]
       game-bridge --help | --version

roles (PLAN.md §1)
  server   announce a destination and bridge links to a game server already
           running on this machine
  client   bind a local port a game client connects to, and relay it to an
           announced server
  relay    donate transit and nothing else: no game, no announced destination
  browse   listen and list; binds no port, holds no identity, forwards nothing
  lan-host open a Mode 3 LAN room for a game whose pack has a [lan] block,
           and put it on this machine's adapter (PLAN.md §14; Linux and
           Windows)
  lan-join join a LAN room and put it on this machine's adapter
  sign     write a detached signature beside a pack (PLAN.md §11.3)
  verify   check the signature beside a pack, and say which tier it earns
  repo     write the index.json that makes a directory of packs a repository
           a launcher can add (PLAN.md §13.3 step 6); serve the directory
           from any web host. `repo mirror` copies a signed repository
           (the official one when no source is given) into a folder, exactly
           and one way: run it again to take what was added since. Serve the
           folder from any web host, or add it in a launcher or node as is.
           `repo seed` serves a folder on the mesh (rns://<destination>/);
           with --from or --official it mirrors first and again every
           --every hours (default 6), so packs reach launchers and nodes with
           no internet
  review   name what a person must read in a pack before sharing or merging
           it, and refuse what no honest pack needs; exits 1 on a refusal.
           --against DIR compares with the packs it would sit beside. A pack
           that passes is not thereby safe: the format's limits are what keep
           a pack from running anything, and this is a reviewer's aid

common options
  --packs DIR        where to read game packs from (default: ./packs)
  --tcp HOST:PORT    dial a Reticulum TCP peer, or 0.0.0.0:PORT to bind one
  --auto             attach auto-discovered local interfaces
  --identity PATH    where to keep this node's identity (generated on first run)

server options
  --game-host HOST   where the game server is (default: 127.0.0.1)
  --game-port PORT   which port it listens on (default: the pack's)
  --name NAME        display name to announce
  --players N --max-players N --map NAME
  --legacy-announce  announce a bare name, byte-identical to svencoop-prns
                     v0.1.10, instead of the §3.3 record. Costs the game id, so
                     a platform browser can only list the server unattributed.
  --allow HASH       identity hash permitted to link; repeatable. Absent means
                     open to anyone.
  --no-transit       do not also carry other people's traffic
  --chat             offer chat to players who joined through a launcher
                     (ROADMAP.md §6.1); off unless given

client options
  --server HASH      destination to join. Absent means take the first server
                     announcing this game.
  --listen PORT      local port the game client connects to (default: the
                     pack's own port)

lan options
  --name NAME        the room's name in the browser (lan-host)
  --max-members N    most members, host included (lan-host, default 8)
  --allow HASH       identity hash that may be seated; repeatable (lan-host)
  --room HASH        room to join; absent means the first one announcing this
                     game (lan-join)
  --adapter NAME     adapter to create, gbl* (default: gbl0)
  --helper PATH      lan-helper to create it with (default: beside this binary).
                     Linux: grant it once, sudo setcap cap_net_admin+ep lan-helper.
                     Windows: it asks for administrator rights when the room
                     starts, and needs wintun.dll beside it.
  --no-transit / --transit
                     whether to carry other people's traffic (host default on,
                     member default off)
  --check            check the room from this machine whenever its members
                     change, and print what works and what does not: a real
                     broadcast on the game's own port, answered by every member

sign options
  --identity PATH    the signing key (generated on first run, like any role's)
  --days N           how long the signature stays valid (default: 90)
  --start UNIX       when it becomes valid (default: now)
  --force            overwrite an existing .sig

  A pack signature goes stale on purpose: nothing guarantees a node on a mesh
  ever fetches a revocation list, so an unrefreshed node fails closed instead of
  trusting a compromised pack forever. Re-sign before the window closes.

verify options
  --identity PATH    a key to treat as trusted, so the tier reads 'signed
                     community' rather than 'signed by an unknown key'
  --at UNIX          check as of this time instead of now

A game id is a pack's `id`, e.g. sven-coop. `game-bridge browse` prints what it
hears and is the cheapest way to check an interface works at all.

RUST_LOG sets log filtering (default: game_bridge=info).";

fn main() -> Result<()> {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "game_bridge=info".to_string());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(role) = args.first().map(String::as_str) else {
        println!("{USAGE}");
        return Ok(());
    };
    if matches!(role, "-h" | "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    if matches!(role, "-V" | "--version") {
        println!("game-bridge {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let cfg = match role {
        "server" | "client" => build_game_role(role, &args[1..])?,
        "relay" => BridgeConfig::Relay(build_relay(&args[1..])?),
        "browse" => BridgeConfig::Browse(build_browse(&args[1..])?),
        // Neither of these starts a bridge, so both return before the runtime
        // is built.
        "lan-host" | "lan-join" => return run_lan(role, &args[1..]),
        "sign" => return sign_pack(&args[1..]),
        "verify" => return verify_pack_cli(&args[1..]),
        "repo" => return repo_cli(&args[1..]),
        "review" => return review_cli(&args[1..]),
        other => bail!("unknown role {other:?}\n\n{USAGE}"),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the tokio runtime")?;
    runtime.block_on(run_bridge(cfg))
}

/// One flag's value, or a sentence naming the flag that is missing one.
fn value(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String> {
    it.next()
        .cloned()
        .ok_or_else(|| anyhow!("{flag} needs a value"))
}

fn parse_num<T: std::str::FromStr>(raw: &str, flag: &str) -> Result<T> {
    raw.parse::<T>()
        .map_err(|_| anyhow!("{flag} needs a number, not {raw:?}"))
}

/// Load the pack directory and pick one game out of it.
///
/// A missing game names what *is* installed rather than only what is not: the
/// usual cause is a pack directory that was never copied next to the binary,
/// and an empty list says that far more clearly than "unknown game".
fn profile_for(pack_dir: &Path, game_id: &str) -> Result<game_bridge::profile::GameProfile> {
    let pack = pack_for(pack_dir, game_id)?;
    pack.to_profile().map_err(|e| anyhow!("pack {game_id:?} is not usable: {e}"))
}

fn pack_for(pack_dir: &Path, game_id: &str) -> Result<GamePack> {
    let loaded = GamePack::load_dir(pack_dir)
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("loading packs from {}", pack_dir.display()))?;
    for (path, e) in &loaded.errors {
        tracing::warn!(path = %path, error = %e, "skipping an unreadable game pack");
    }
    let pack = loaded
        .packs
        .iter()
        .find(|p| p.id == game_id)
        .ok_or_else(|| {
            let known: Vec<&str> = loaded.packs.iter().map(|p| p.id.as_str()).collect();
            anyhow!(
                "no game pack with id {game_id:?} in {}. Installed: {}",
                pack_dir.display(),
                if known.is_empty() { "none".to_string() } else { known.join(", ") }
            )
        })?;
    Ok(pack.clone())
}

fn build_game_role(role: &str, rest: &[String]) -> Result<BridgeConfig> {
    let mut it = rest.iter();
    let game_id = it
        .next()
        .cloned()
        .ok_or_else(|| anyhow!("game-bridge {role} needs a game id, e.g. sven-coop\n\n{USAGE}"))?;
    if game_id.starts_with('-') {
        bail!("game-bridge {role} needs a game id before its options\n\n{USAGE}");
    }

    // Two passes: the pack directory has to be known before the pack is read,
    // and everything else is applied on top of the profile's own defaults.
    let mut pack_dir = PathBuf::from("packs");
    let flags: Vec<String> = it.cloned().collect();
    let mut scan = flags.iter();
    while let Some(flag) = scan.next() {
        if flag == "--packs" {
            pack_dir = PathBuf::from(value(&mut scan, "--packs")?);
        }
    }
    let profile = profile_for(&pack_dir, &game_id)?;

    let mut it = flags.iter();
    if role == "server" {
        let mut args = ServerArgs::new(profile);
        while let Some(flag) = it.next() {
            match flag.as_str() {
                "--packs" => {
                    let _ = value(&mut it, "--packs")?;
                }
                "--tcp" => args.tcp = Some(value(&mut it, "--tcp")?),
                "--auto" => args.auto = true,
                "--identity" => args.identity = PathBuf::from(value(&mut it, "--identity")?),
                "--game-host" => args.game_host = value(&mut it, "--game-host")?,
                "--game-port" => {
                    args.game_port = parse_num(&value(&mut it, "--game-port")?, "--game-port")?
                }
                "--name" => args.name = Some(value(&mut it, "--name")?),
                "--map" => args.map = Some(value(&mut it, "--map")?),
                "--players" => args.players = parse_num(&value(&mut it, "--players")?, "--players")?,
                "--max-players" => {
                    args.max_players =
                        parse_num(&value(&mut it, "--max-players")?, "--max-players")?
                }
                "--announce-interval" => {
                    args.announce_interval =
                        parse_num(&value(&mut it, "--announce-interval")?, "--announce-interval")?
                }
                "--legacy-announce" => args.announce_format = AnnounceFormat::Legacy,
                "--allow" => args.allowlist.push(value(&mut it, "--allow")?),
                "--no-transit" => args.relay_transit = false,
                "--passworded" => args.passworded = true,
                "--chat" => args.chat = true,
                other => bail!("unknown option {other:?} for game-bridge server\n\n{USAGE}"),
            }
        }
        return Ok(BridgeConfig::Server(args));
    }

    let listen_port = profile.default_port;
    let mut args = ClientArgs::new(profile);
    args.listen_port = listen_port;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--packs" => {
                let _ = value(&mut it, "--packs")?;
            }
            "--tcp" => args.tcp = Some(value(&mut it, "--tcp")?),
            "--auto" => args.auto = true,
            "--identity" => args.identity = PathBuf::from(value(&mut it, "--identity")?),
            "--server" => args.server_hash = Some(value(&mut it, "--server")?),
            "--listen" => args.listen_port = parse_num(&value(&mut it, "--listen")?, "--listen")?,
            "--transit" => args.relay_transit = true,
            other => bail!("unknown option {other:?} for game-bridge client\n\n{USAGE}"),
        }
    }
    Ok(BridgeConfig::Client(args))
}

fn build_relay(rest: &[String]) -> Result<RelayArgs> {
    let mut args = RelayArgs::new();
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--tcp" => args.tcp = Some(value(&mut it, "--tcp")?),
            "--auto" => args.auto = true,
            "--identity" => args.identity = PathBuf::from(value(&mut it, "--identity")?),
            other => bail!("unknown option {other:?} for game-bridge relay\n\n{USAGE}"),
        }
    }
    Ok(args)
}

fn build_browse(rest: &[String]) -> Result<BrowserArgs> {
    let mut args = BrowserArgs::new();
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--tcp" => args.tcp = Some(value(&mut it, "--tcp")?),
            "--auto" => args.auto = true,
            other => bail!("unknown option {other:?} for game-bridge browse\n\n{USAGE}"),
        }
    }
    Ok(args)
}

// ---------------------------------------------------------------------------
// Mode 3 LAN rooms (PLAN.md §14)
// ---------------------------------------------------------------------------

fn run_lan(role: &str, rest: &[String]) -> Result<()> {
    use game_bridge::lan_session::{LanHostArgs, LanMemberArgs};

    let mut it = rest.iter();
    let game_id = it
        .next()
        .cloned()
        .filter(|g| !g.starts_with('-'))
        .ok_or_else(|| anyhow!("game-bridge {role} needs a game id first, e.g. openttd\n\n{USAGE}"))?;
    let flags: Vec<String> = it.cloned().collect();

    let mut pack_dir = PathBuf::from("packs");
    let mut tcp = None;
    let mut auto = false;
    let mut identity = PathBuf::from("./game-bridge-lan.identity");
    let mut name = None;
    let mut max_members: usize = 8;
    let mut allow = Vec::new();
    let mut room = None;
    let mut adapter = "gbl0".to_string();
    let mut helper: Option<PathBuf> = None;
    let mut transit = role == "lan-host";
    let mut check = false;
    let mut it = flags.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--packs" => pack_dir = PathBuf::from(value(&mut it, "--packs")?),
            "--tcp" => tcp = Some(value(&mut it, "--tcp")?),
            "--auto" => auto = true,
            "--identity" => identity = PathBuf::from(value(&mut it, "--identity")?),
            "--name" if role == "lan-host" => name = Some(value(&mut it, "--name")?),
            "--max-members" if role == "lan-host" => {
                max_members = parse_num(&value(&mut it, "--max-members")?, "--max-members")?
            }
            "--allow" if role == "lan-host" => allow.push(value(&mut it, "--allow")?),
            "--room" if role == "lan-join" => room = Some(value(&mut it, "--room")?),
            "--adapter" => adapter = value(&mut it, "--adapter")?,
            "--helper" => helper = Some(PathBuf::from(value(&mut it, "--helper")?)),
            "--no-transit" => transit = false,
            "--transit" => transit = true,
            "--check" => check = true,
            other => bail!("unknown option {other:?} for game-bridge {role}\n\n{USAGE}"),
        }
    }

    let pack = pack_for(&pack_dir, &game_id)?;
    // Only a game that says which ports it uses on a LAN gets a room: those
    // ports are all the adapter will let in (`lan_filter.rs`).
    let lan = pack.lan.clone().ok_or_else(|| {
        anyhow!("pack {game_id:?} has no [lan] block, so it is not offered as a LAN room")
    })?;
    if !lan.tested {
        tracing::warn!(game = %game_id, "nobody has played this game in a LAN room yet; it is untested");
    }
    if lan.inbound == game_bridge::pack::PackLanInbound::Any {
        tracing::warn!(
            game = %game_id,
            "this game's pack admits every port: other members can reach any service on this machine while the room is up"
        );
    }
    let profile = pack.to_profile().map_err(|e| anyhow!("pack {game_id:?} is not usable: {e}"))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the tokio runtime")?;
    runtime.block_on(async move {
        let session = if role == "lan-host" {
            let mut args = LanHostArgs::new(profile);
            args.identity = identity;
            args.tcp = tcp;
            args.auto = auto;
            args.name = name;
            args.max_members = max_members;
            args.allowlist = allow;
            args.relay_transit = transit;
            game_bridge::lan_session::LanSession::host(args).await?
        } else {
            let mut args = LanMemberArgs::new(profile);
            args.identity = identity;
            args.tcp = tcp;
            args.auto = auto;
            args.room_hash = room;
            args.relay_transit = transit;
            game_bridge::lan_session::LanSession::join(args).await?
        };
        if let Some(hash) = session.room_hash() {
            println!("room {}", hex::encode(hash.as_bytes()));
        }
        run_lan_adapter(std::sync::Arc::new(session), lan.policy(), adapter, helper, check).await
    })
}

#[cfg(any(target_os = "linux", windows))]
async fn run_lan_adapter(
    session: std::sync::Arc<game_bridge::lan_session::LanSession>,
    policy: game_bridge::lan_filter::LanPolicy,
    adapter: String,
    helper: Option<PathBuf>,
    check: bool,
) -> Result<()> {
    use game_bridge::lan_adapter::{run_room_on_adapter_reporting, AdapterPhase, AdapterSetup};

    let helper = helper.or_else(|| {
        let beside = std::env::current_exe()
            .ok()?
            .with_file_name(format!("lan-helper{}", std::env::consts::EXE_SUFFIX));
        beside.exists().then_some(beside)
    });
    let setup = match helper {
        Some(path) => AdapterSetup::Helper { path, portable: false },
        None => AdapterSetup::InProcess,
    };
    let stop = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let (phase, phase_rx) = tokio::sync::watch::channel(AdapterPhase::WaitingForSeat);
    if check {
        tokio::spawn(check_room_on_change(session.clone(), policy.clone(), adapter.clone(), phase_rx));
    }
    run_room_on_adapter_reporting(session, policy, adapter, setup, stop, phase).await
}

/// `--check`: run the room check (`lan_check.rs`) each time the room's members
/// change while the adapter is up, and print what it found.
#[cfg(any(target_os = "linux", windows))]
async fn check_room_on_change(
    session: std::sync::Arc<game_bridge::lan_session::LanSession>,
    policy: game_bridge::lan_filter::LanPolicy,
    adapter: String,
    phase: tokio::sync::watch::Receiver<game_bridge::lan_adapter::AdapterPhase>,
) {
    use game_bridge::lan_adapter::AdapterPhase;
    let mut checked: Option<Vec<std::net::Ipv4Addr>> = None;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if !matches!(*phase.borrow(), AdapterPhase::Up { .. }) {
            continue;
        }
        let mut members: Vec<_> = session.view().members.iter().map(|m| m.address).collect();
        members.sort();
        if members.len() < 2 || checked.as_ref() == Some(&members) {
            continue;
        }
        match game_bridge::lan_check::check_room(&session, &policy, &adapter).await {
            Ok(report) => {
                println!("room check from {} on UDP {}:", report.address, report.port);
                for line in report.findings() {
                    println!("  {line}");
                }
            }
            Err(e) => println!("room check: {e:#}"),
        }
        checked = Some(members);
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
async fn run_lan_adapter(
    _session: std::sync::Arc<game_bridge::lan_session::LanSession>,
    _policy: game_bridge::lan_filter::LanPolicy,
    _adapter: String,
    _helper: Option<PathBuf>,
    _check: bool,
) -> Result<()> {
    bail!("this platform's LAN room adapter is not built yet (PLAN.md §14.3)")
}

// ---------------------------------------------------------------------------
// Pack signing (PLAN.md §11.3)
//
// The library could already verify a signature and classify a signer before
// anything could produce one — the tiers were enforceable and unreachable at
// the same time. These two subcommands are what closes that: `sign` is the only
// way a `.sig` gets written, and `verify` is how a signer checks what they just
// made without standing up a node.
// ---------------------------------------------------------------------------

fn sign_pack(rest: &[String]) -> Result<()> {
    let mut path: Option<PathBuf> = None;
    let mut identity = PathBuf::from("./game-bridge-signing.identity");
    let mut days: u64 = 90;
    let mut start: Option<u64> = None;
    let mut force = false;

    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--identity" => identity = PathBuf::from(value(&mut it, "--identity")?),
            "--days" => days = parse_num(&value(&mut it, "--days")?, "--days")?,
            "--start" => start = Some(parse_num(&value(&mut it, "--start")?, "--start")?),
            "--force" => force = true,
            other if other.starts_with('-') => {
                bail!("unknown option {other:?} for game-bridge sign\n\n{USAGE}")
            }
            other if path.is_none() => path = Some(PathBuf::from(other)),
            other => bail!("game-bridge sign takes one pack, not also {other:?}"),
        }
    }
    let path = path.ok_or_else(|| anyhow!("game-bridge sign needs a pack file\n\n{USAGE}"))?;
    if days == 0 {
        bail!("--days 0 would sign for no time at all; a window has to contain an instant");
    }

    // Parse before signing. Signing a file that is not a valid pack produces a
    // signature that verifies over bytes nothing can load — a valid answer to
    // the wrong question, and the sort of artifact that gets published once and
    // debugged for an afternoon.
    let src = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let pack = GamePack::parse(&src)
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("{} is not a usable pack, so it is not signed", path.display()))?;

    let sig_path = signature_path(&path);
    if sig_path.exists() && !force {
        bail!(
            "{} already exists; pass --force to replace it",
            sig_path.display()
        );
    }

    let secret_bytes = load_signing_identity(&identity)?;
    let secret = PrivateIdentityMaterial::from_slice(&secret_bytes[..])
        .map_err(|e| anyhow!("identity at {}: {e:?}", identity.display()))?;

    let not_before = match start {
        Some(unix) => std::time::UNIX_EPOCH + Duration::from_secs(unix),
        None => SystemTime::now(),
    };
    let signature = PackSignature::sign(
        src.as_bytes(),
        &secret,
        not_before,
        Duration::from_secs(days * 86_400),
    )
    .map_err(|e| anyhow!("{e}"))?;

    std::fs::write(&sig_path, signature.to_toml())
        .with_context(|| format!("writing {}", sig_path.display()))?;

    println!("signed {} as {}", path.display(), pack.id);
    println!("  signature  {}", sig_path.display());
    println!("  signer     {}", hex::encode(secret.public().identity_hash().as_bytes()));
    println!("  valid      unix {} .. {}", signature.not_before, signature.not_after);
    println!(
        "\nThe window is inside the signed material, so editing it invalidates the signature.\n\
         Re-sign before it closes: a node that has not refreshed refuses the pack rather than\n\
         trusting it forever (PLAN.md §11.3)."
    );
    Ok(())
}

/// `game-bridge review <pack.toml>... [--against DIR] [--markdown]`: what a
/// reviewer must read in each pack (`review.rs`). Exits non-zero when any pack
/// is refused or does not parse, so CI can gate a contribution on it.
fn review_cli(rest: &[String]) -> Result<()> {
    use game_bridge::review::{review, Severity};
    let mut files: Vec<PathBuf> = Vec::new();
    let mut against: Option<PathBuf> = None;
    let mut markdown = false;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--against" => against = Some(PathBuf::from(value(&mut it, "--against")?)),
            "--markdown" => markdown = true,
            other if other.starts_with('-') => {
                bail!("unknown option {other:?} for game-bridge review\n\n{USAGE}")
            }
            other => files.push(PathBuf::from(other)),
        }
    }
    if files.is_empty() {
        bail!("game-bridge review needs at least one pack\n\n{USAGE}");
    }
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let mut others: Vec<(PathBuf, GamePack)> = Vec::new();
    if let Some(dir) = &against {
        let entries =
            std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
        for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            if let Ok(pack) = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|s| GamePack::parse(&s).map_err(|e| e.to_string()))
            {
                others.push((canonical(&path), pack));
            }
        }
    }
    let mut refused = 0;
    for file in &files {
        let name = file.display();
        let parsed = std::fs::read_to_string(file)
            .map_err(|e| e.to_string())
            .and_then(|s| GamePack::parse(&s).map_err(|e| e.to_string()));
        let findings = match parsed {
            // Compared with every pack but this file itself.
            Ok(pack) => {
                let me = canonical(file);
                let beside: Vec<GamePack> =
                    others.iter().filter(|(p, _)| *p != me).map(|(_, o)| o.clone()).collect();
                review(&pack, &beside)
            }
            Err(e) => {
                refused += 1;
                if markdown {
                    println!("### `{name}`\n\n- **refuse**: does not parse: {e}\n");
                } else {
                    println!("{name}: refuse: does not parse: {e}");
                }
                continue;
            }
        };
        if findings.iter().any(|f| f.severity == Severity::Refuse) {
            refused += 1;
        }
        if markdown {
            println!("### `{name}`\n");
            if findings.is_empty() {
                println!("Nothing to flag.\n");
            }
            for f in &findings {
                println!("- **{}** `{}`: {}", f.severity.label(), f.field, f.message);
            }
            println!();
        } else if findings.is_empty() {
            println!("{name}: nothing to flag");
        } else {
            for f in &findings {
                println!("{name}: {f}");
            }
        }
    }
    if refused > 0 {
        bail!("{refused} pack(s) refused");
    }
    Ok(())
}

/// `game-bridge repo build <dir> [--name NAME]`: list every pack in `dir` in
/// its `index.json`, written beside and renamed so a web host never serves a
/// half-written one.
fn repo_cli(rest: &[String]) -> Result<()> {
    let mut it = rest.iter();
    match it.next().map(String::as_str) {
        Some("build") => {}
        Some("mirror") => return repo_mirror_cli(&rest[1..]),
        Some("seed") => return repo_seed_cli(&rest[1..]),
        _ => bail!("game-bridge repo takes `build <dir>` or `mirror [<source>] <dir>`\n\n{USAGE}"),
    }
    let mut dir: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut identity: Option<PathBuf> = None;
    let mut days: u64 = 90;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--name" => name = Some(value(&mut it, "--name")?),
            "--identity" => identity = Some(PathBuf::from(value(&mut it, "--identity")?)),
            "--days" => days = parse_num(&value(&mut it, "--days")?, "--days")?,
            other if other.starts_with('-') => {
                bail!("unknown option {other:?} for game-bridge repo build\n\n{USAGE}")
            }
            other if dir.is_none() => dir = Some(PathBuf::from(other)),
            other => bail!("game-bridge repo build takes one directory, not also {other:?}"),
        }
    }
    let dir = dir.ok_or_else(|| anyhow!("game-bridge repo build needs a directory\n\n{USAGE}"))?;
    let name = name.unwrap_or_else(|| {
        dir.file_name().and_then(|n| n.to_str()).unwrap_or("Game packs").to_string()
    });
    let index = game_bridge::repo::RepoIndex::build(&dir, &name)?;
    let json = serde_json::to_vec_pretty(&index)?;
    let path = dir.join(game_bridge::repo::INDEX_FILE);
    let staging = dir.join(".index.json.partial");
    std::fs::write(&staging, &json).with_context(|| format!("writing {}", staging.display()))?;
    std::fs::rename(&staging, &path).with_context(|| format!("writing {}", path.display()))?;
    // A signed index lets a launcher notice the list changing hands: it pins
    // the key it first saw. Written after the index, and removed when this
    // build is unsigned, so a signature never sits beside another list.
    let sig_path = dir.join(format!("{}{SIGNATURE_SUFFIX}", game_bridge::repo::INDEX_FILE));
    match &identity {
        Some(key_path) => {
            let bytes = load_signing_identity(key_path)?;
            let secret = PrivateIdentityMaterial::from_slice(&bytes[..])
                .map_err(|e| anyhow!("identity at {}: {e:?}", key_path.display()))?;
            let sig = PackSignature::sign_index(
                &json,
                &secret,
                SystemTime::now(),
                Duration::from_secs(days * 86_400),
            )
            .map_err(|e| anyhow!("{e}"))?;
            std::fs::write(&sig_path, sig.to_toml())
                .with_context(|| format!("writing {}", sig_path.display()))?;
            println!(
                "signed by {}, valid {days} days: rebuild before then",
                hex::encode(secret.public().identity_hash().as_bytes())
            );
        }
        None => {
            let _ = std::fs::remove_file(&sig_path);
        }
    }
    let signed = index.packs.iter().filter(|p| p.signed).count();
    println!(
        "{}: {} pack(s), {signed} signed, in {}",
        index.name,
        index.packs.len(),
        path.display()
    );
    Ok(())
}

/// `game-bridge repo mirror [<source>] <dir> [--key HEX]`: an exact, one-way
/// copy of a signed repository (`repo::mirror`). With one path, the source is
/// the official repository and the key the project's.
fn repo_mirror_cli(rest: &[String]) -> Result<()> {
    let mut paths: Vec<String> = Vec::new();
    let mut key: Option<String> = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--key" => key = Some(value(&mut it, "--key")?),
            other if other.starts_with('-') => {
                bail!("unknown option {other:?} for game-bridge repo mirror\n\n{USAGE}")
            }
            other => paths.push(other.to_string()),
        }
    }
    let (source, dir) = match paths.as_slice() {
        [dir] => (game_bridge::repo::OFFICIAL_REPOSITORY.to_string(), PathBuf::from(dir)),
        [source, dir] => (source.clone(), PathBuf::from(dir)),
        _ => bail!("game-bridge repo mirror takes [<source>] <dir>\n\n{USAGE}"),
    };
    let key = match key {
        None => game_bridge::signing::project_key(),
        Some(hex_key) => {
            let mut b = [0u8; 16];
            hex::decode_to_slice(hex_key.trim(), &mut b)
                .map_err(|_| anyhow!("--key is an identity hash, 32 hex digits"))?;
            prns_core::identity::IdentityHash::new(b)
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let report = runtime.block_on(game_bridge::repo::mirror(&source, &dir, key))?;
    println!(
        "{}: {} pack(s) in {}, {} new or changed{}",
        report.name,
        report.packs,
        dir.display(),
        report.changed.len(),
        if report.changed.is_empty() { String::new() } else { format!(" ({})", report.changed.join(", ")) }
    );
    Ok(())
}

/// `game-bridge repo seed <dir> ...`: a repository on the mesh
/// (`repo_mesh.rs`), kept an exact mirror of `--from` when one is given.
fn repo_seed_cli(rest: &[String]) -> Result<()> {
    let mut dir: Option<PathBuf> = None;
    let mut from: Option<String> = None;
    let mut key: Option<String> = None;
    let mut every_hours: u64 = 6;
    let mut identity = PathBuf::from("repo-seed.identity");
    let mut tcp: Option<String> = None;
    let mut auto = false;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--from" => from = Some(value(&mut it, "--from")?),
            "--official" => from = Some(game_bridge::repo::OFFICIAL_REPOSITORY.to_string()),
            "--key" => key = Some(value(&mut it, "--key")?),
            "--every" => every_hours = parse_num(&value(&mut it, "--every")?, "--every")?,
            "--identity" => identity = PathBuf::from(value(&mut it, "--identity")?),
            "--tcp" => tcp = Some(value(&mut it, "--tcp")?),
            "--auto" => auto = true,
            other if other.starts_with('-') => {
                bail!("unknown option {other:?} for game-bridge repo seed\n\n{USAGE}")
            }
            other if dir.is_none() => dir = Some(PathBuf::from(other)),
            other => bail!("game-bridge repo seed takes one directory, not also {other:?}"),
        }
    }
    let dir = dir.ok_or_else(|| anyhow!("game-bridge repo seed needs a directory\n\n{USAGE}"))?;
    if tcp.is_none() && !auto {
        bail!("a seed nobody can reach serves nothing: give --tcp HOST:PORT or --auto");
    }
    let key = match key {
        None => game_bridge::signing::project_key(),
        Some(hex_key) => {
            let mut b = [0u8; 16];
            hex::decode_to_slice(hex_key.trim(), &mut b)
                .map_err(|_| anyhow!("--key is an identity hash, 32 hex digits"))?;
            prns_core::identity::IdentityHash::new(b)
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let remirror = |source: String| {
            let dir = dir.clone();
            async move {
                match game_bridge::repo::mirror(&source, &dir, key).await {
                    Ok(r) => println!("mirrored {}: {} pack(s), {} new or changed", r.name, r.packs, r.changed.len()),
                    // Offline is normal for a seed: it keeps serving the last copy.
                    Err(e) => eprintln!("could not mirror {source} ({e:#}); serving the copy already here"),
                }
            }
        };
        if let Some(source) = &from {
            remirror(source.clone()).await;
        }
        if !dir.join(game_bridge::repo::INDEX_FILE).is_file() {
            bail!("{} holds no repository to serve", dir.display());
        }
        let session = game_bridge::repo_mesh::start_seed(game_bridge::repo_mesh::SeedArgs {
            dir: dir.clone(),
            identity,
            tcp,
            auto,
            announce_every: game_bridge::repo_mesh::ANNOUNCE_EVERY,
        })
        .await?;
        let own = session.own_hash().map(|h| hex::encode(h.as_bytes())).unwrap_or_default();
        println!("serving {} on the mesh as rns://{own}/", dir.display());
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = tokio::time::sleep(Duration::from_secs(every_hours.max(1) * 3600)) => {
                    if let Some(source) = &from {
                        remirror(source.clone()).await;
                        session.request_announce();
                    }
                }
            }
        }
        Ok(())
    })
}

fn verify_pack_cli(rest: &[String]) -> Result<()> {
    let mut path: Option<PathBuf> = None;
    let mut trusted: Vec<PathBuf> = Vec::new();
    let mut at: Option<u64> = None;

    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--identity" => trusted.push(PathBuf::from(value(&mut it, "--identity")?)),
            "--at" => at = Some(parse_num(&value(&mut it, "--at")?, "--at")?),
            other if other.starts_with('-') => {
                bail!("unknown option {other:?} for game-bridge verify\n\n{USAGE}")
            }
            other if path.is_none() => path = Some(PathBuf::from(other)),
            other => bail!("game-bridge verify takes one pack, not also {other:?}"),
        }
    }
    let path = path.ok_or_else(|| anyhow!("game-bridge verify needs a pack file\n\n{USAGE}"))?;

    let mut policy = TrustPolicy::allowing_unsigned();
    for key_path in &trusted {
        let bytes = load_signing_identity(key_path)?;
        let secret = PrivateIdentityMaterial::from_slice(&bytes[..])
            .map_err(|e| anyhow!("identity at {}: {e:?}", key_path.display()))?;
        policy = policy.trusting(secret.public().identity_hash());
    }

    let now = match at {
        Some(unix) => std::time::UNIX_EPOCH + Duration::from_secs(unix),
        None => SystemTime::now(),
    };
    let verified = GamePack::load_verified(&path, &policy, now).map_err(|e| anyhow!("{e}"))?;

    println!("{} — {}", verified.pack.id, verified.trust.label());
    println!("  {}", verified.trust.explanation());
    if let Some(signer) = verified.trust.signer() {
        println!("  signer     {}", hex::encode(signer.as_bytes()));
    }
    if let Some(expires) = verified.expires_at {
        println!("  expires    unix {expires}");
    }
    // The tier a node would give it is not the same question as whether the
    // signature is good, and the difference is the whole point of §11.4.
    if !policy.may_deploy(&verified.trust) {
        println!(
            "\nA node running the default strict policy would refuse to deploy this. Either the\n\
             operator adds this key to pack_trust.trusted_keys, or they set\n\
             pack_trust.allow_unsigned = true."
        );
    }
    Ok(())
}

/// A pack's signature file: the pack's own name plus the suffix, so the two
/// never drift apart.
fn signature_path(pack: &Path) -> PathBuf {
    let mut name = pack.as_os_str().to_os_string();
    name.push(SIGNATURE_SUFFIX);
    PathBuf::from(name)
}

/// The signing key, created on first use like every other role's identity.
///
/// Deliberately the same file format and the same helper the bridge roles use:
/// a signing key here is a Reticulum identity, not a second kind of key an
/// operator has to learn to keep.
fn load_signing_identity(path: &Path) -> Result<Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>> {
    load_or_create_identity_secret(path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading signing identity at {}", path.display()))
}
