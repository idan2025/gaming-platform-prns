//! `platform-agent join`: make this machine a worker of an index, with the one
//! code an admin made (`HOSTING.md`, "Adding a worker").
//!
//! It makes the agent's identity, presents the code and that identity's public
//! control address to the index, and writes the config the index's answer
//! describes: the index it trusts and the hubs it dials. The index adds the
//! worker to its hosting file; nothing here edits another machine.
//!
//! What this does **not** do is decide what the worker runs. A game's image is
//! chosen by whoever operates this machine (`GameRuntime.image`), so the index's
//! answer carries no image and no `[games.*]` section; the joined worker takes
//! no game until its operator adds one.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

pub const USAGE: &str = "\
usage: platform-agent join --url <index http address> --token <join code>
           [--dir DIR] [--name NAME] [--packs DIR] [--run]

  --url    the index's HTTP address, as this machine reaches it
  --token  a join code an admin made on the node page's Admin tab
  --dir    where this worker keeps its config, identity and data
           (default: ~/lanthorn-worker)
  --name   a name for this worker if the code did not fix one (default: this
           machine's host name)
  --run    start the worker once it has joined. Run again after a restart: a
           machine that has joined skips joining and just starts.";

#[derive(Debug, Clone)]
pub struct Options {
    pub url: String,
    pub token: String,
    pub dir: PathBuf,
    pub name: Option<String>,
    pub packs: String,
    pub run: bool,
}

impl Options {
    pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let (mut url, mut token, mut dir, mut name, mut packs, mut run) =
            (None, None, None, None, None, false);
        fn value(args: &mut impl Iterator<Item = String>, what: &str) -> Result<String> {
            args.next().ok_or_else(|| anyhow!("{what} needs a value\n\n{USAGE}"))
        }
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--url" => url = Some(value(&mut args, "--url")?),
                "--token" => token = Some(value(&mut args, "--token")?),
                "--dir" => dir = Some(value(&mut args, "--dir")?),
                "--name" => name = Some(value(&mut args, "--name")?),
                "--packs" => packs = Some(value(&mut args, "--packs")?),
                "--run" => run = true,
                "-h" | "--help" => bail!("{USAGE}"),
                other => bail!("unknown option {other:?}\n\n{USAGE}"),
            }
        }
        let url = url.ok_or_else(|| anyhow!("--url is required\n\n{USAGE}"))?;
        let url = url.trim_end_matches('/').to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!("--url must start with http:// or https://");
        }
        let token = token.ok_or_else(|| anyhow!("--token is required\n\n{USAGE}"))?;
        if token.is_empty()
            || token.len() > 128
            || !token.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            bail!("--token is not a join code");
        }
        let dir = match dir {
            Some(d) => PathBuf::from(d),
            None => PathBuf::from(std::env::var("HOME").context("no --dir and no HOME")?)
                .join("lanthorn-worker"),
        };
        let dir = if dir.is_absolute() { dir } else { std::env::current_dir()?.join(dir) };
        if let Some(n) = &name {
            if n.is_empty()
                || n.len() > 64
                || !n
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
            {
                bail!("--name is 1-64 characters: letters, digits, . _ -");
            }
        }
        // The image carries the shipped packs; a checkout has its own; with
        // neither the worker gets a folder of its own, because an agent that
        // cannot read its pack directory does not start.
        let packs = packs.unwrap_or_else(|| {
            let shipped = "/opt/gaming-platform-prns/packs";
            if Path::new(shipped).is_dir() {
                shipped.to_string()
            } else if Path::new("packs").is_dir() {
                "packs".to_string()
            } else {
                dir.join("packs").to_string_lossy().into_owned()
            }
        });

        Ok(Self { url, token, dir, name, packs, run })
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("agent.toml")
    }
}

/// What the index answers to an accepted code.
#[derive(Debug, Deserialize)]
struct Accepted {
    index: String,
    name: String,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    hubs: Vec<String>,
}

fn is_hash(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_hub(s: &str) -> bool {
    s.rsplit_once(':').is_some_and(|(h, p)| {
        !h.is_empty()
            && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            && p.parse::<u16>().is_ok_and(|p| p != 0)
    })
}

/// Join, unless this directory already has. Returns whether it joined now.
pub async fn run(opts: &Options) -> Result<bool> {
    if opts.run && !Path::new(&opts.packs).exists() {
        std::fs::create_dir_all(&opts.packs)
            .with_context(|| format!("making the pack folder {}", opts.packs))?;
    }
    let config_path = opts.config_path();
    if config_path.exists() {
        tracing::info!(config = %config_path.display(), "already joined; not joining again");
        return Ok(false);
    }
    let data = opts.dir.join("data");
    std::fs::create_dir_all(&data).with_context(|| format!("making {}", data.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&opts.dir, std::fs::Permissions::from_mode(0o700))?;
    }

    // The agent's identity is made here, as it would be at its first start, and
    // only its public control address leaves this machine.
    let identity_path = opts.dir.join("agent.identity");
    let (_, control) = crate::uplink::control_address_of(&crate::config::UplinkConfig {
        identity_secret_path: identity_path.clone(),
        tcp: None,
        auto: false,
        trusted_indexes: Vec::new(),
    })?;
    let control = hex::encode(control.as_bytes());

    let name = opts.name.clone().or_else(host_name);
    let body = serde_json::json!({ "token": opts.token, "agent": control, "name": name });
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build()?;
    let response = client
        .post(format!("{}/join", opts.url))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("reaching the index at {}", opts.url))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let why = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| format!("the index answered {status}"));
        // Nothing was written but an identity; a new code can use the same directory.
        bail!("the index refused to add this machine: {why}");
    }
    let accepted: Accepted =
        serde_json::from_str(&text).context("the index's answer was not what a join answers")?;
    if !is_hash(&accepted.index) {
        bail!("the index's answer carried no identity hash");
    }
    if let Some(bad) = accepted.hubs.iter().find(|h| !is_hub(h)) {
        bail!("the index named a hub that is not host:port: {bad:?}");
    }

    write_config(opts, &accepted, &data, &identity_path)?;
    tracing::info!(name = %accepted.name, region = ?accepted.region, hubs = accepted.hubs.len(), "joined the index");
    println!(
        "joined as {}{}",
        accepted.name,
        accepted.region.as_deref().map(|r| format!(" in {r}")).unwrap_or_default()
    );
    Ok(true)
}

fn host_name() -> Option<String> {
    let raw = std::fs::read_to_string("/etc/hostname").ok()?;
    let name: String = raw.trim().chars().take(64).collect();
    (!name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-'))
    .then_some(name)
}

/// The config the answer describes, and the hubs in the sidecar the page edits.
/// Interfaces first and the config last: a half-joined directory has no
/// `agent.toml`, so the next run joins again instead of starting unreachable.
fn write_config(opts: &Options, accepted: &Accepted, data: &Path, identity: &Path) -> Result<()> {
    if !accepted.hubs.is_empty() {
        let interfaces = crate::interfaces::InterfaceSettings {
            interfaces: accepted
                .hubs
                .iter()
                .map(|h| crate::interfaces::InterfaceDescriptor {
                    id: None,
                    kind: "tcp".into(),
                    addr: Some(h.clone()),
                    ifac_name: None,
                    ifac_passphrase: None,
                })
                .collect(),
        };
        interfaces.save(&data.join("agent-interfaces.json")).context("writing the hubs")?;
    }
    let mesh = match accepted.hubs.first() {
        Some(hub) => format!("\n[mesh]\ntcp = \"{hub}\"\nannounce_interval = 15\n"),
        None => String::new(),
    };
    let text = format!(
        "# Written by `platform-agent join`. Add [games.<id>] sections for what this\n\
         # worker runs; a game with no entry cannot be started here.\n\
         data_root = \"{data}\"\n\
         max_instances = 8\n\
         api_bind = \"127.0.0.1:4750\"\n\
         \n\
         [port_range]\n\
         start = 27100\n\
         end = 27199\n\
         \n\
         [uplink]\n\
         identity_secret_path = \"{identity}\"\n\
         trusted_indexes = [\"{index}\"]\n\
         {mesh}",
        data = data.display(),
        identity = identity.display(),
        index = accepted.index,
    );
    // Parsed the way the agent will parse it, before it is the config.
    toml::from_str::<crate::config::AgentConfig>(&text)
        .context("the config written for this worker does not parse")?;
    let partial = opts.dir.join("agent.toml.partial");
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&partial)?;
        f.write_all(text.as_bytes())?;
    }
    std::fs::rename(&partial, opts.config_path()).context("writing agent.toml")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options> {
        Options::parse(args.iter().map(|s| s.to_string()))
    }

    /// Everything on the command line is checked as data before a file is made.
    #[test]
    fn the_command_line_is_checked_before_anything_is_written() {
        assert!(parse(&["--url", "http://10.0.0.1:4761", "--token", "abc123"]).is_ok());
        assert!(parse(&["--url", "ftp://x", "--token", "abc"]).is_err());
        assert!(parse(&["--url", "http://x"]).is_err(), "a code is required");
        assert!(parse(&["--url", "http://x", "--token", "a b"]).is_err());
        assert!(parse(&["--url", "http://x", "--token", "abc", "--name", "../x"]).is_err());
        assert!(parse(&["--url", "http://x", "--token", "abc", "--bogus"]).is_err());
        let o = parse(&["--url", "http://x/", "--token", "abc", "--dir", "rel"]).unwrap();
        assert_eq!(o.url, "http://x", "a trailing slash is dropped");
        assert!(o.dir.is_absolute());
    }

    /// The config a join writes loads as an agent config, trusts only the index
    /// that answered, names no game, and the hubs are in the sidecar.
    #[test]
    fn a_joined_config_trusts_the_answering_index_and_names_no_game() {
        let dir = tempfile::tempdir().unwrap();
        let opts = Options {
            url: "http://x".into(),
            token: "t".into(),
            dir: dir.path().to_path_buf(),
            name: None,
            packs: "packs".into(),
            run: false,
        };
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let accepted = Accepted {
            index: "f337905bb69b37dcd91723710b6a3c14".into(),
            name: "w9".into(),
            region: Some("eu".into()),
            hubs: vec!["hub.example:4790".into(), "10.0.0.2:4790".into()],
        };
        write_config(&opts, &accepted, &data, &dir.path().join("agent.identity")).unwrap();
        let config = crate::config::AgentConfig::load(&opts.config_path()).unwrap();
        let uplink = config.uplink.unwrap();
        assert_eq!(uplink.trusted_indexes, ["f337905bb69b37dcd91723710b6a3c14"]);
        assert!(config.games.is_empty(), "the index never says what a worker runs");
        assert_eq!(config.mesh.unwrap().tcp.as_deref(), Some("hub.example:4790"));
        let saved = crate::interfaces::InterfaceSettings::load(&data.join("agent-interfaces.json"))
            .unwrap();
        assert_eq!(saved.interfaces.len(), 2);
    }
}
