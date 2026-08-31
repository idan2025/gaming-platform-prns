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
       game-bridge sign   <pack.toml> [options]
       game-bridge verify <pack.toml> [options]
       game-bridge --help | --version

roles (PLAN.md §1)
  server   announce a destination and bridge links to a game server already
           running on this machine
  client   bind a local port a game client connects to, and relay it to an
           announced server
  relay    donate transit and nothing else: no game, no announced destination
  browse   listen and list; binds no port, holds no identity, forwards nothing
  sign     write a detached signature beside a pack (PLAN.md §11.3)
  verify   check the signature beside a pack, and say which tier it earns

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

client options
  --server HASH      destination to join. Absent means take the first server
                     announcing this game.
  --listen PORT      local port the game client connects to (default: the
                     pack's own port)

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
        "sign" => return sign_pack(&args[1..]),
        "verify" => return verify_pack_cli(&args[1..]),
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
    pack.to_profile().map_err(|e| anyhow!("pack {game_id:?} is not usable: {e}"))
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
