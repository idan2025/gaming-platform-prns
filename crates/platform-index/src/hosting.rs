//! Hosted deploy: running a server on somebody else's behalf.
//!
//! This is the one part of the platform that is genuinely centralized, and
//! `DESIGN.md` §0 allows it on one condition — it is a convenience, never a
//! dependency. Everything else keeps working with hosting switched off: servers
//! announce, launchers browse, players join. Turning this on adds "and you can
//! ask someone else to run one for you".
//!
//! # The operator decides what they will host, and the platform ships no list
//!
//! `GAMES.md` §5 and `PLAN.md` §10 leave open which games are legally hostable,
//! because the answer is jurisdictional and depends on agreements the platform
//! cannot see. So there is **no default**: `HostingConfig::games` is empty until
//! an operator writes one, and an empty list means hosting is off. Exactly the
//! same shape as `platform-agent` making the container image an operator's
//! choice — the person who owns the hardware decides what runs on it, and the
//! project does not decide for them.
//!
//! # Ownership lives on the container, not in a database here
//!
//! A deploy stamps the requesting identity onto the instance as a container
//! label, and this module reconstructs who owns what by asking the node. There
//! is deliberately no index-side instance table: it would be a second source of
//! truth that drifts the first time an operator stops a container by hand, and
//! it would have to survive restarts that this service is otherwise free to not
//! survive.
//!
//! # Reach, honestly
//!
//! An agent's HTTP API is loopback-only and refuses to bind anything else,
//! because it creates containers and has no authentication. A node on another
//! machine is driven over its Reticulum uplink instead (`NodeConfig::agent`,
//! `agent_client.rs`), which authenticates this index and is checked against
//! the node's `trusted_indexes` on every op. Telling operators to expose the
//! agent's HTTP API to the network instead would trade the whole
//! authentication story for a shortcut.


use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use personal_rns::prelude::{DestinationHash, Zeroizing};
use platform_agent::instance::InstancePort;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use platform_agent::uplink_wire::CapacityResp;

use crate::agent_client::AgentClient;
use platform_agent::backups::Backup;
use platform_agent::uplink_wire::{BackupsResp, LogsResp};
use crate::quota::{AccountId, AccountLimits, InstanceRecord, QuotaPolicy, Quotas, ReapReason};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostingConfig {
    /// Game ids this operator is willing to host for other people.
    ///
    /// **Empty means hosting is off**, and empty is the default. The platform
    /// ships no list; see the module docs.
    #[serde(default)]
    pub games: Vec<String>,
    /// Agents this index can drive. Loopback only, in practice — see the module
    /// docs on reach.
    #[serde(default)]
    pub nodes: Vec<NodeConfig>,
    #[serde(default)]
    pub quota: QuotaSettings,
    /// Accounts (identity hashes) that may decide region move requests. None
    /// by default: with nobody to decide, a server stays in its region.
    #[serde(default)]
    pub admins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub name: String,
    /// Base URL of the agent's local API, e.g. `http://127.0.0.1:4750`. Used
    /// when `agent` is absent; ignored when `agent` is set.
    pub api: String,
    /// Hex destination hash of the agent's `platform-agent.control`
    /// destination, for a node on another host reached over Reticulum instead
    /// of loopback HTTP. When present, the index drives this node through
    /// `agent_client` (`PLAN.md` §8 phase 4); `api` is the loopback fallback.
    #[serde(default)]
    pub agent: Option<String>,
    /// Which nodes to fill first. Among nodes with room, a higher priority
    /// always wins; equal ones go to whichever has the most free memory, then
    /// the most free slots. Default 0, so an overflow node (a home machine
    /// behind a rented one) is the one with a lower number.
    #[serde(default)]
    pub priority: i32,
    /// An operator-chosen label. A node with one takes only creates that name
    /// it; a node without one is the default pool. The index only matches the
    /// word: what a class is for is the operator's business.
    #[serde(default)]
    pub class: Option<String>,
    /// Where the node is, as the operator names it (`eu`, `il-north`). A
    /// create may ask for a region; nodes in it are placed on first, and the
    /// others still take the server when none there has room. Same shape as a
    /// class.
    #[serde(default)]
    pub region: Option<String>,
}

/// A class label is data that is matched and shown, never run: short, lower
/// case, digits, `-` and `_`. Refused, never repaired.
pub fn valid_class(class: &str) -> bool {
    !class.is_empty()
        && class.len() <= 32
        && class.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The quota knobs, as an operator writes them. Any left out take the
/// default, so a file may carry only `[quota.accounts.<hash>]` sections.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct QuotaSettings {
    pub max_instances_per_account: u32,
    pub max_total_instances: u32,
    pub idle_timeout_secs: Option<u64>,
    pub min_lifetime_secs: u64,
    pub create_cooldown_secs: Option<u64>,
    /// Accounts with rules of their own, keyed by identity hash in hex
    /// (`[quota.accounts.<hash>]`). Anyone not listed gets the rules above.
    #[serde(default)]
    pub accounts: std::collections::BTreeMap<String, AccountSettings>,
    /// Remove a server stopped for this many days. Absent keeps stopped servers
    /// for ever. Removing takes the container, never the server's files.
    pub stopped_delete_after_days: Option<u64>,
    /// Backups one account may hold across its servers. Absent is no cap; 0 is
    /// refused (it would forbid every backup, and nothing an operator wrote
    /// asks for that).
    #[serde(default)]
    pub max_backups_per_account: Option<u32>,
}

/// One account's rules, as an operator writes them. A field left out means
/// "as everyone else".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountSettings {
    pub max_instances: Option<u32>,
    /// Seconds between creates for this account; 0 means no cooldown at all.
    pub create_cooldown_secs: Option<u64>,
    /// Never stop this account's servers for being idle.
    #[serde(default)]
    pub never_reap: bool,
    /// Node classes this account may ask for. None listed means none allowed.
    #[serde(default)]
    pub classes: Vec<String>,
    /// Backups this account may hold, in place of the shared cap. At least 1.
    #[serde(default)]
    pub max_backups: Option<u32>,
}

impl QuotaSettings {
    /// An account key is an identity hash, and a key that is not one can
    /// never match anybody. That is refused, not kept: a typo'd key would
    /// otherwise leave the account on the shared rules without a word.
    pub fn validate(&self) -> Result<()> {
        // Zero would remove every server the moment it stopped: a stop would
        // be a delete, and nothing an operator wrote asks for that.
        if self.stopped_delete_after_days == Some(0) {
            return Err(anyhow!("stopped_delete_after_days must be at least 1"));
        }
        if self.max_backups_per_account == Some(0) {
            return Err(anyhow!("max_backups_per_account must be at least 1"));
        }
        for (key, account) in &self.accounts {
            if account.max_backups == Some(0) {
                return Err(anyhow!("[quota.accounts.{key}] max_backups must be at least 1"));
            }
            if let Some(bad) = account.classes.iter().find(|c| !valid_class(c)) {
                return Err(anyhow!("[quota.accounts.{key}] names a class {bad:?} that is not a valid label"));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for key in self.accounts.keys() {
            let ok = key.len() == 32 && key.bytes().all(|b| b.is_ascii_hexdigit());
            if !ok {
                return Err(anyhow!(
                    "[quota.accounts.{key}] is not an identity hash (32 hex characters)"
                ));
            }
            if !seen.insert(key.to_ascii_lowercase()) {
                return Err(anyhow!("[quota.accounts.{key}] is listed twice"));
            }
        }
        Ok(())
    }
}

impl Default for QuotaSettings {
    fn default() -> Self {
        let d = QuotaPolicy::default();
        Self {
            max_instances_per_account: d.max_instances_per_account,
            max_total_instances: d.max_total_instances,
            idle_timeout_secs: d.idle_timeout.map(|t| t.as_secs()),
            min_lifetime_secs: d.min_lifetime.as_secs(),
            create_cooldown_secs: d.create_cooldown.map(|t| t.as_secs()),
            accounts: Default::default(),
            stopped_delete_after_days: None,
            max_backups_per_account: None,
        }
    }
}

impl From<&QuotaSettings> for QuotaPolicy {
    fn from(s: &QuotaSettings) -> Self {
        use std::time::Duration;
        Self {
            max_instances_per_account: s.max_instances_per_account,
            max_total_instances: s.max_total_instances,
            idle_timeout: s.idle_timeout_secs.map(Duration::from_secs),
            min_lifetime: Duration::from_secs(s.min_lifetime_secs),
            create_cooldown: s.create_cooldown_secs.map(Duration::from_secs),
            stopped_expiry: s
                .stopped_delete_after_days
                .map(|days| Duration::from_secs(days.saturating_mul(86_400))),
            max_backups_per_account: s.max_backups_per_account,
            overrides: s
                .accounts
                .iter()
                .map(|(key, a)| {
                    let limits = AccountLimits {
                        max_instances: a.max_instances,
                        create_cooldown: a
                            .create_cooldown_secs
                            .map(|n| (n > 0).then(|| Duration::from_secs(n))),
                        never_reap: a.never_reap,
                        classes: a.classes.clone(),
                        max_backups: a.max_backups,
                    };
                    (AccountId(key.to_ascii_lowercase()), limits)
                })
                .collect(),
        }
    }
}

impl HostingConfig {
    /// The regions the nodes are in, sorted and once each: what a create form
    /// may offer. Only the words, never which node is where.
    pub fn regions(&self) -> Vec<String> {
        let mut regions: Vec<String> = self.nodes.iter().filter_map(|n| n.region.clone()).collect();
        regions.sort();
        regions.dedup();
        regions
    }

    pub fn enabled(&self) -> bool {
        !self.games.is_empty() && !self.nodes.is_empty()
    }

    pub fn hosts_game(&self, game_id: &str) -> bool {
        self.games.iter().any(|g| g == game_id)
    }
}

/// Read a node's port set out of its JSON reply.
///
/// A row this build cannot parse is skipped rather than failing the listing:
/// the index is a cache, and a node speaking a shape it does not recognise is
/// still a node whose instances a user has to be able to see and stop.
fn ports_from_json(value: &serde_json::Value) -> Vec<InstancePort> {
    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| serde_json::from_value(row.clone()).ok())
        .collect()
}

/// One hosted instance, as the index reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedInstance {
    pub instance_id: String,
    pub node: String,
    pub game_id: String,
    pub name: String,
    pub state: String,
    /// The game's own port, which is what a player needs.
    pub port: Option<u16>,
    /// Every port the node published for this instance, channel 0 included
    /// (`GAMES.md` §3). Empty for a single-port game and for a node running a
    /// build that predates port sets, which is why `port` stays on its own.
    ///
    /// Passed through, never invented: the node allocated these and the index
    /// is a cache of what the node says.
    #[serde(default)]
    pub ports: Vec<InstancePort>,
    /// Seconds since a stopped server last ended, as the node reports it.
    /// `None` while it runs, or when the node did not say.
    #[serde(default)]
    pub stopped_secs: Option<u64>,
    /// For a stopped server on an index with a removal rule: seconds until the
    /// index removes it. Set by the index, never by a node. Absent means it
    /// will not be removed.
    #[serde(default)]
    pub delete_in_secs: Option<u64>,
    pub owner: Option<String>,
    /// Seconds this instance has existed, as the node reports it. `None` means
    /// the node did not say — which is "age unknown", never "brand new".
    #[serde(default)]
    pub uptime_secs: Option<u64>,
    /// Players on it right now, as the node reports it.
    ///
    /// `None` is **not zero**. A game that speaks no query protocol, or one
    /// that did not answer, is unknown, and reaping treats unknown as "leave it
    /// alone" — stopping a busy server because a UDP query was dropped is worse
    /// than letting an idle one run.
    #[serde(default)]
    pub players_now: Option<u32>,
    /// The server's mesh address as its node reports it: what an invite names
    /// and a launcher joins. Passed through, never invented; absent while the
    /// node has none (stopped, or no `[mesh]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_destination: Option<String>,
    /// The region of the worker it runs on, which is the server's home: a
    /// move leaves it only by an approved request (`regions.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// What a caller asks for.
///
/// Note the absence of an instance id: the index generates it. A client-chosen
/// id is a way to collide with — or guess at — another account's instance, and
/// there is no reason a caller needs to pick one.
#[derive(Debug, Clone, Deserialize)]
pub struct DeployRequest {
    pub game_id: String,
    pub name: String,
    #[serde(default = "default_max_players")]
    pub max_players: u8,
    /// Which map to start on. Absent leaves it to the node's image default.
    /// Passed through untouched: the **node** validates it, because the node is
    /// where it becomes a container's environment, and an index that judged it
    /// here would be a second place the rule could drift.
    #[serde(default)]
    pub map: Option<String>,
    /// Ask for a node of this class. Absent means the default pool, the nodes
    /// with no class. Only an account the operator allowed it to may ask.
    #[serde(default)]
    pub class: Option<String>,
    /// Place it near here if a node in this region has room (`NodeConfig::region`).
    /// A wish, not a rule: with no room there it goes wherever placement
    /// would have put it.
    #[serde(default)]
    pub region: Option<String>,
    /// Bots to start with, when the caller asked for some. Passed through for
    /// the node to judge (only a game whose pack has bots takes them); the
    /// index never invents a number.
    #[serde(default)]
    pub bots: Option<u8>,
    /// Restart the server if it crashes. Off unless the caller asked.
    #[serde(default)]
    pub restart_on_crash: bool,
    /// The game's own choices: a Minecraft build, its heap, the EULA tick,
    /// Bedrock's settings. Flattened, so a deploy body reads the same as a
    /// node's own create. Passed through for the **node** to validate, as the
    /// map is; the one thing set here is the EULA time, which is the index's
    /// own clock and never a caller's.
    #[serde(flatten, default)]
    pub options: platform_agent::instance::InstanceOptions,
}

fn default_max_players() -> u8 {
    8
}

pub struct Hosting {
    /// Swapped whole by [`reload`](Self::reload); every operation takes one
    /// snapshot and works from it, so a reload never changes the rules
    /// halfway through a deploy.
    config: std::sync::RwLock<Arc<HostingConfig>>,
    http: reqwest::Client,
    /// The Reticulum uplink to remote agents. `None` when this index runs no
    /// Reticulum node, in which case nodes with `agent` set are unreachable
    /// (and reported as such) and loopback-HTTP nodes keep working. Set after
    /// construction once `node.rs` has produced a handle (`set_agent_client`).
    agent_client: Mutex<Option<Arc<AgentClient>>>,
    /// Servers being moved right now (`relocate`), so a second move of the
    /// same one is refused rather than run beside the first.
    moving: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Region move requests waiting for, or decided by, an admin.
    requests: crate::regions::RegionRequests,
    /// Admins granted here (beyond the hosting file's), and requests to be one.
    admin_store: crate::admins::AdminStore,
}

/// One admin of the index and where the role comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminView {
    pub identity: String,
    /// `file` (the hosting file's `admins`, changed only there) or `granted`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_by: Option<String>,
}

/// A worker as an admin sees it: where it is and what it reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerView {
    pub name: String,
    pub region: Option<String>,
    pub class: Option<String>,
    pub priority: i32,
    /// Whether it answered just now. A silent worker reports nothing else.
    pub answered: bool,
    /// Servers the index sees on it.
    pub servers: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<platform_agent::uplink_wire::CapacityResp>,
}

/// What an erase removed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Erased {
    pub servers_removed: Vec<String>,
    pub backups_deleted: u32,
    pub requests_forgotten: usize,
}

/// Where a move may go. A server stays in its region unless an admin approved
/// a request for another (`regions.rs`), which is the only way `Region` is used.
#[derive(Debug, Clone, Copy)]
pub enum MoveTarget<'a> {
    /// The worker with the most room in the server's own region.
    Auto,
    /// This worker, which must be in the server's region.
    Worker(&'a str),
    /// The worker with the most room in this region: an approved request only.
    Region(&'a str),
}

/// Where one server could be moved, for its owner to choose from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveTargets {
    /// The worker it is on now.
    pub worker: String,
    /// Its region (its home); `None` is a worker with no region.
    pub region: Option<String>,
    /// The other workers in its region that take its class of server.
    pub workers: Vec<MoveWorker>,
    /// Regions it could be moved to by a request, other than its own.
    pub regions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveWorker {
    pub name: String,
    /// `room`, `full`, or `silent` (it did not answer, so it is not offered).
    pub room: String,
}

impl Hosting {
    pub fn new(config: HostingConfig) -> Self {
        Self {
            config: std::sync::RwLock::new(Arc::new(config)),
            http: reqwest::Client::builder()
                // An agent that has stopped answering must not hold a request
                // open until the caller gives up on the index itself.
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
            agent_client: Mutex::new(None),
            moving: Default::default(),
            requests: Default::default(),
            admin_store: Default::default(),
        }
    }

    /// The configuration in force right now.
    pub fn config(&self) -> Arc<HostingConfig> {
        self.config.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Put a new configuration in force: nodes, games and quotas, with no
    /// restart. A node removed from the list is no longer placed on or
    /// listed; the servers on it keep running, since only its operator
    /// stops them. Ownership needs nothing carried over: it lives on the
    /// nodes' containers.
    pub fn reload(&self, config: HostingConfig) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(config);
    }

    /// Read and parse a hosting config file. A file that does not parse is an
    /// error, never an empty config: an operator's typo must not switch
    /// hosting off.
    pub fn load(path: &std::path::Path) -> Result<HostingConfig> {
        let src = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: HostingConfig =
            toml::from_str(&src).with_context(|| format!("parsing {}", path.display()))?;
        config.quota.validate().with_context(|| format!("in {}", path.display()))?;
        if let Some(bad) = config.nodes.iter().filter_map(|n| n.class.as_deref()).find(|c| !valid_class(c)) {
            return Err(anyhow!("a node's class {bad:?} is not a valid label")).with_context(|| format!("in {}", path.display()));
        }
        if let Some(bad) = config.nodes.iter().filter_map(|n| n.region.as_deref()).find(|r| !valid_class(r)) {
            return Err(anyhow!("a node's region {bad:?} is not a valid label")).with_context(|| format!("in {}", path.display()));
        }
        // An admin decides other people's requests, so a key that can never
        // match anybody is refused rather than kept: a typo would leave
        // requests with nobody to decide them, unnoticed.
        if let Some(bad) = config.admins.iter().find(|a| !(a.len() == 32 && a.bytes().all(|b| b.is_ascii_hexdigit()))) {
            return Err(anyhow!("admin {bad:?} is not an identity hash (32 hex characters)")).with_context(|| format!("in {}", path.display()));
        }
        Ok(config)
    }

    /// Re-read `path` and put it in force if it parses. On an error the
    /// configuration in force stays as it was, and the error says why.
    pub fn reload_from(&self, path: &std::path::Path) -> Result<()> {
        let config = Self::load(path)?;
        tracing::info!(
            games = ?config.games,
            nodes = ?config.nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            "hosting config reloaded"
        );
        self.reload(config);
        Ok(())
    }

    fn quotas(config: &HostingConfig) -> Quotas {
        Quotas::new(QuotaPolicy::from(&config.quota))
    }

    /// Attach the Reticulum uplink, once the index's node has produced a handle.
    /// Before this is called, nodes reached via `agent` answer "no uplink";
    /// loopback-HTTP nodes are unaffected.
    pub async fn set_agent_client(&self, client: Arc<AgentClient>) {
        *self.agent_client.lock().await = Some(client);
    }

    /// The configured Reticulum uplink, cloned out of the lock so a slow Link
    /// does not serialize every other op.
    async fn agent_client(&self) -> Option<Arc<AgentClient>> {
        self.agent_client.lock().await.clone()
    }

    /// Parse a node's `agent` hex into a destination hash, or `None` when the
    /// node is loopback-HTTP only.
    fn agent_dest(node: &NodeConfig) -> Option<Result<DestinationHash>> {
        let hex = node.agent.as_deref()?;
        let bytes = match hex::decode(hex) {
            Ok(b) => b,
            Err(e) => return Some(Err(anyhow!("node {} has a bad agent hash: {e}", node.name))),
        };
        Some(
            DestinationHash::from_slice(&bytes)
                .ok_or_else(|| anyhow!("node {} has a bad agent hash: not 16 bytes", node.name)),
        )
    }

    /// Everything running across every node this index drives.
    pub async fn all_instances(&self) -> Result<Vec<HostedInstance>> {
        let config = self.config();
        let mut out = Vec::new();
        for node in &config.nodes {
            match self.node_instances(node).await {
                Ok(mut rows) => out.append(&mut rows),
                // One unreachable node must not blank the whole listing: the
                // other nodes' instances are still running and still the
                // caller's.
                Err(e) => tracing::warn!(node = %node.name, error = %e, "node did not answer"),
            }
        }
        // A stopped server shows how long it has left, so its owner can see it
        // before it goes. Only this index knows its rule, so only it can say.
        let quotas = Self::quotas(&config);
        for instance in out.iter_mut() {
            instance.delete_in_secs = delete_in_secs(&quotas, instance);
        }
        out.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        Ok(out)
    }

    async fn node_instances(&self, node: &NodeConfig) -> Result<Vec<HostedInstance>> {
        // A node with an `agent` hash is reached over Reticulum; otherwise the
        // loopback HTTP path. The RNS path needs the uplink; without it the node
        // is unreachable rather than silently empty.
        if let Some(dest) = Self::agent_dest(node) {
            let dest = dest?;
            let client = self
                .agent_client()
                .await
                .ok_or_else(|| anyhow!("node {} is remote but this index has no uplink", node.name))?;
            let rows = client.list(dest).await.with_context(|| {
                format!("asking agent {} over Reticulum", node.name)
            })?;
            return Ok(rows
                .into_iter()
                .map(|r| HostedInstance {
                    instance_id: r.instance_id,
                    node: node.name.clone(),
                    game_id: r.game_id,
                    name: r.name,
                    state: format!("{:?}", r.state).to_ascii_lowercase(),
                    port: r.port,
                    ports: r.ports,
                    owner: r.owner,
                    uptime_secs: r.uptime_secs,
                    stopped_secs: r.stopped_secs,
                    delete_in_secs: None,
                    players_now: r.players_now,
                    mesh_destination: r.mesh_destination,
                    region: node.region.clone(),
                })
                .collect());
        }

        let url = format!("{}/instances", node.api.trim_end_matches('/'));
        let rows: Vec<serde_json::Value> = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("asking node {}", node.name))?
            .json()
            .await
            .with_context(|| format!("reading node {}'s reply", node.name))?;
        Ok(rows
            .into_iter()
            .map(|r| HostedInstance {
                instance_id: r["instance_id"].as_str().unwrap_or_default().to_string(),
                node: node.name.clone(),
                game_id: r["game_id"].as_str().unwrap_or_default().to_string(),
                name: r["name"].as_str().unwrap_or_default().to_string(),
                state: r["state"].as_str().unwrap_or("unknown").to_string(),
                port: r["port"].as_u64().map(|p| p as u16),
                ports: ports_from_json(&r["ports"]),
                owner: r["owner"].as_str().map(str::to_string),
                uptime_secs: r["uptime_secs"].as_u64(),
                stopped_secs: r["stopped_secs"].as_u64(),
                delete_in_secs: None,
                players_now: r["players_now"].as_u64().map(|p| p as u32),
                mesh_destination: r["mesh_destination"].as_str().map(str::to_string),
                region: node.region.clone(),
            })
            .collect())
    }

    /// Instances belonging to one account.
    pub async fn instances_for(&self, account: &AccountId) -> Result<Vec<HostedInstance>> {
        Ok(self
            .all_instances()
            .await?
            .into_iter()
            .filter(|i| i.owner.as_deref() == Some(account.0.as_str()))
            .collect())
    }

    /// Deploy on an account's behalf.
    pub async fn deploy(
        &self,
        account: &AccountId,
        request: &DeployRequest,
        now: std::time::SystemTime,
    ) -> Result<HostedInstance> {
        let config = self.config();
        if !config.enabled() {
            return Err(anyhow!("this index does not offer hosting"));
        }
        if !config.hosts_game(&request.game_id) {
            // Named rather than a bare 404: an operator's list is a deliberate
            // choice, and a caller should be able to see what it is.
            return Err(anyhow!(
                "this index does not host {:?}. It hosts: {}",
                request.game_id,
                config.games.join(", ")
            ));
        }
        if request.name.trim().is_empty() {
            return Err(anyhow!("a server needs a name"));
        }
        // The node refuses it too; refusing here means a caller hears why
        // before any quota is spent or node is picked.
        game_bridge::console::validate_server_name(&request.name)
            .map_err(|e| anyhow!("{e}"))?;

        // Before any quota is spent. The same words whether or not such a class
        // exists, so asking is no way to learn the operator's class names.
        let quotas = Self::quotas(&config);
        if let Some(class) = request.class.as_deref() {
            if !valid_class(class) || !quotas.may_use_class(account, class) {
                return Err(anyhow!("this account cannot place servers in that class"));
            }
        }

        let existing = self.all_instances().await?;
        let records: Vec<InstanceRecord> = existing.iter().map(|i| record_for(i, now)).collect();
        quotas.admit(account, &records, now).map_err(|e| anyhow!("{e}"))?;

        if let Some(region) = request.region.as_deref() {
            if !valid_class(region) {
                return Err(anyhow!("that is not a region name"));
            }
        }
        let node = self.pick_node(&config, &existing, request.class.as_deref(), request.region.as_deref()).await.ok_or_else(|| {
            anyhow!("no node with room for another instance")
        })?;

        // The index picks the id. See DeployRequest.
        let instance_id = new_instance_id(&request.game_id)?;

        let mut options = request.options.clone();
        if options.eula_accepted_at.is_some() {
            options.eula_accepted_at = Some(
                now.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            );
        }

        // Remote agent over Reticulum, when the chosen node names one.
        if let Some(dest) = Self::agent_dest(node) {
            let dest = dest?;
            let client = self
                .agent_client()
                .await
                .ok_or_else(|| anyhow!("node {} is remote but this index has no uplink", node.name))?;
            let spec = platform_agent::instance::InstanceSpec {
                instance_id: instance_id.clone(),
                game_id: request.game_id.clone(),
                name: request.name.clone(),
                max_players: request.max_players,
                // The node picks every port, game and extras alike. An index
                // that named one would be choosing from the wrong side of the
                // machine: only the node knows what is free there.
                port: None,
                extra_ports: Default::default(),
                map: request.map.clone(),
                // Only what the caller asked for. Inventing a default here
                // would put players in a game nobody asked to share.
                bots: request.bots,
                restart_on_crash: request.restart_on_crash,
                owner: None,
                options,
            };
            let created = client
                .create(dest, spec, Some(account.0.clone()))
                .await
                .with_context(|| format!("asking agent {} to deploy", node.name))?;
            return Ok(HostedInstance {
                instance_id,
                node: node.name.clone(),
                game_id: request.game_id.clone(),
                name: request.name.clone(),
                state: format!("{:?}", created.state).to_ascii_lowercase(),
                port: created.port,
                ports: created.ports,
                owner: Some(account.0.clone()),
                // Just created, and nobody has had time to join.
                uptime_secs: Some(0),
                stopped_secs: None,
                delete_in_secs: None,
                players_now: Some(0),
                mesh_destination: None,
                region: None,
            });
        }

        let url = format!("{}/instances", node.api.trim_end_matches('/'));
        let mut body = serde_json::json!({
            "instance_id": instance_id,
            "game_id": request.game_id,
            "name": request.name,
            "max_players": request.max_players,
            "map": request.map,
            "bots": request.bots,
            "restart_on_crash": request.restart_on_crash,
            "owner": account.0,
        });
        if let (Some(body), Ok(serde_json::Value::Object(extra))) =
            (body.as_object_mut(), serde_json::to_value(&options))
        {
            body.extend(extra);
        }
        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("asking node {} to deploy", node.name))?;
        if !response.status().is_success() {
            let detail: serde_json::Value = response.json().await.unwrap_or_default();
            let msg = detail["error"].as_str().unwrap_or("the node refused");
            // Pass the node's own sentence through. It is the one that knows
            // the game content is missing, and swallowing it would leave the
            // caller with "deploy failed".
            return Err(anyhow!("{msg}"));
        }
        let created: serde_json::Value = response.json().await.context("reading the node's reply")?;
        Ok(HostedInstance {
            instance_id,
            node: node.name.clone(),
            game_id: request.game_id.clone(),
            name: request.name.clone(),
            state: created["state"].as_str().unwrap_or("unknown").to_string(),
            port: created["port"].as_u64().map(|p| p as u16),
            ports: ports_from_json(&created["ports"]),
            owner: Some(account.0.clone()),
            uptime_secs: Some(0),
            stopped_secs: None,
            delete_in_secs: None,
            players_now: Some(0),
            mesh_destination: None,
            region: None,
        })
    }

    /// One of the caller's own instances, and the node holding it.
    ///
    /// Ownership is checked against the node's label, not against anything this
    /// index remembers, so a restarted index still enforces it correctly.
    async fn owned(&self, account: &AccountId, instance_id: &str) -> Result<(HostedInstance, NodeConfig)> {
        let all = self.all_instances().await?;
        let found = all
            .into_iter()
            .find(|i| i.instance_id == instance_id)
            // Not found and not yours are the same answer on purpose: a
            // different one would let anyone enumerate other people's
            // instances by watching which ids come back "forbidden".
            .filter(|i| i.owner.as_deref() == Some(account.0.as_str()))
            .ok_or_else(|| anyhow!("no such instance"))?;
        let node = self
            .config()
            .nodes
            .iter()
            .find(|n| n.name == found.node)
            .cloned()
            .ok_or_else(|| anyhow!("the node holding it is no longer configured"))?;
        Ok((found, node))
    }

    /// Stop one of the caller's own instances. Its files, ports and address
    /// stay; [`start`](Self::start) brings it back.
    pub async fn stop(&self, account: &AccountId, instance_id: &str) -> Result<()> {
        let (_, node) = self.owned(account, instance_id).await?;
        self.stop_on(&node, instance_id).await
    }

    /// Start one of the caller's own stopped instances again, on the node that
    /// holds it, with the same files, ports and mesh address.
    ///
    /// No quota admission: a stopped instance already counts against its
    /// account (`Quotas::held_by` counts every instance the nodes report), so
    /// starting it takes nothing new. A running one is refused, here and again
    /// by the node, so "start" can never restart a server out from under its
    /// players.
    pub async fn start(&self, account: &AccountId, instance_id: &str) -> Result<()> {
        let (found, node) = self.owned(account, instance_id).await?;
        if found.state == "running" {
            return Err(anyhow!("it is already running"));
        }
        self.start_on(&node, instance_id).await
    }

    /// Stop and remove one of the caller's own instances.
    pub async fn destroy(&self, account: &AccountId, instance_id: &str) -> Result<()> {
        let (_, node) = self.owned(account, instance_id).await?;
        self.remove_on(&node, instance_id).await
    }

    /// Remove one instance from the node that holds it, on whichever transport
    /// that node uses. The caller has already checked whose it is.
    async fn remove_on(&self, node: &NodeConfig, instance_id: &str) -> Result<()> {
        // Remote agent over Reticulum: ask it to remove the instance. Ownership
        // was just checked against the label the agent itself reported, so a
        // restarted index still enforces it correctly on either path.
        if let Some(dest) = Self::agent_dest(node) {
            let dest = dest?;
            let client = self
                .agent_client()
                .await
                .ok_or_else(|| anyhow!("node {} is remote but this index has no uplink", node.name))?;
            return client
                .remove(dest, instance_id)
                .await
                .with_context(|| format!("asking agent {} to remove it", node.name));
        }

        let url = format!(
            "{}/instances/{}",
            node.api.trim_end_matches('/'),
            instance_id
        );
        let response = self.http.delete(&url).send().await.context("asking the node")?;
        if !response.status().is_success() {
            return Err(anyhow!("the node refused to remove it"));
        }
        Ok(())
    }

    /// Ask one node to stop one instance, on whichever transport it uses.
    ///
    /// Stop rather than remove: the instance's writable state is whatever
    /// players built there, and reclaiming compute is not a reason to destroy
    /// it. The node's own `orphan_dirs` refuses to delete such state
    /// automatically for the same reason.
    async fn stop_on(&self, node: &NodeConfig, instance_id: &str) -> Result<()> {
        if let Some(dest) = Self::agent_dest(node) {
            let dest = dest?;
            let client = self
                .agent_client()
                .await
                .ok_or_else(|| anyhow!("node {} is remote but this index has no uplink", node.name))?;
            return client
                .stop(dest, instance_id)
                .await
                .with_context(|| format!("asking agent {} to stop it", node.name));
        }
        let url = format!(
            "{}/instances/{}/stop",
            node.api.trim_end_matches('/'),
            instance_id
        );
        let response = self.http.post(&url).send().await.context("asking the node")?;
        if !response.status().is_success() {
            return Err(anyhow!("the node refused to stop it"));
        }
        Ok(())
    }

    /// The newest lines one of the caller's own servers wrote. A node whose agent
    /// predates the op says so, rather than "unknown op".
    pub async fn logs(&self, account: &AccountId, instance_id: &str, lines: u32) -> Result<LogsResp> {
        let (_, node) = self.owned(account, instance_id).await?;
        let too_old = || anyhow!("node {} runs an agent too old to show logs; update it", node.name);
        if let Some(dest) = Self::agent_dest(&node) {
            let dest = dest?;
            let client = self.agent_client().await.ok_or_else(|| {
                anyhow!("node {} is remote but this index has no uplink", node.name)
            })?;
            return match client.logs(dest, instance_id, lines).await {
                Err(e) if format!("{e:#}").contains("unknown op") => Err(too_old()),
                other => other.with_context(|| format!("asking agent {} for its logs", node.name)),
            };
        }
        let url = format!("{}/instances/{}/logs?lines={}", node.api.trim_end_matches('/'), instance_id, lines);
        let response = self.http.get(&url).send().await.context("asking the node")?;
        if !response.status().is_success() {
            return Err(anyhow!("the node refused to show its logs"));
        }
        let text = response.text().await.context("reading the node's logs")?;
        Ok(LogsResp { text, truncated: false })
    }

    /// The caller's own server's backups, and the calls that make, restore and
    /// remove one. Restore and remove take a name the agent made; a name that is
    /// not one of its shapes is refused here before it reaches a URL or a node.
    pub async fn backups(&self, account: &AccountId, instance_id: &str) -> Result<BackupsResp> {
        let (_, node) = self.owned(account, instance_id).await?;
        if let Some(dest) = Self::agent_dest(&node) {
            let dest = dest?;
            let client = self.agent_client().await.ok_or_else(|| {
                anyhow!("node {} is remote but this index has no uplink", node.name)
            })?;
            return client
                .backups(dest, instance_id)
                .await
                .map_err(|e| too_old_or(e, &node, "list backups"))
                .with_context(|| format!("asking agent {} for its backups", node.name));
        }
        let url = format!("{}/instances/{}/backups", node.api.trim_end_matches('/'), instance_id);
        let backups: Vec<Backup> = self.node_json(self.http.get(&url), &node).await?;
        Ok(BackupsResp { backups, truncated: false })
    }

    pub async fn backup_create(&self, account: &AccountId, instance_id: &str) -> Result<Backup> {
        let (_, node) = self.owned(account, instance_id).await?;
        // Held to the account's cap, if there is one, before any node is asked
        // to spend disk on a backup.
        let quotas = Self::quotas(&self.config());
        if quotas.max_backups_for(account).is_some() {
            let held = self.backups_held(account).await?;
            quotas.admit_backup(account, held).map_err(|e| anyhow!("{e}"))?;
        }
        if let Some(dest) = Self::agent_dest(&node) {
            let dest = dest?;
            let client = self.agent_client().await.ok_or_else(|| {
                anyhow!("node {} is remote but this index has no uplink", node.name)
            })?;
            return client
                .backup_create(dest, instance_id)
                .await
                .map_err(|e| too_old_or(e, &node, "make backups"))
                .with_context(|| format!("asking agent {} to back up", node.name));
        }
        let url = format!("{}/instances/{}/backups", node.api.trim_end_matches('/'), instance_id);
        self.node_json(self.http.post(&url), &node).await
    }

    /// How many backups `account` holds across all its servers. Every worker is
    /// asked, and one that cannot be makes the count unknown, which is refused,
    /// never read as zero: silence is not room. (`all_instances` skips a silent
    /// worker so a listing survives an outage; a limit must not.)
    async fn backups_held(&self, account: &AccountId) -> Result<u32> {
        let config = self.config();
        let mut held = 0u32;
        for node in &config.nodes {
            let rows = self.node_instances(node).await.map_err(|e| {
                anyhow!("could not ask {} for your backups to check your limit: {e:#}", node.name)
            })?;
            for instance in rows.iter().filter(|i| i.owner.as_deref() == Some(account.0.as_str())) {
                let listed = self.backups(account, &instance.instance_id).await.map_err(|e| {
                    anyhow!("could not count your backups on {} to check your limit: {e:#}", node.name)
                })?;
                held = held.saturating_add(listed.backups.len() as u32);
            }
        }
        Ok(held)
    }

    /// The account's servers on every worker, each worker asked itself. One
    /// that cannot answer fails the whole call: an erase that skipped a worker
    /// would read as complete and not be.
    async fn owned_everywhere(&self, account: &AccountId) -> Result<Vec<HostedInstance>> {
        let config = self.config();
        let mut owned = Vec::new();
        for node in &config.nodes {
            let rows = self
                .node_instances(node)
                .await
                .map_err(|e| anyhow!("could not ask {} for your servers to remove them; try again later: {e:#}", node.name))?;
            owned.extend(rows.into_iter().filter(|i| i.owner.as_deref() == Some(account.0.as_str())));
        }
        Ok(owned)
    }

    /// Remove everything this index and its workers hold under one account:
    /// every backup of every server it owns, then the servers, then its region
    /// requests. Nothing is touched unless every worker answered, and a failure
    /// part way stops there; asking again carries on from what is left. A
    /// server's files stay on its worker until that worker's leftover clean-up,
    /// as for any removed server.
    pub async fn erase_account(&self, account: &AccountId) -> Result<Erased> {
        let owned = self.owned_everywhere(account).await?;
        let mut erased = Erased::default();
        for instance in &owned {
            for backup in self.backups(account, &instance.instance_id).await?.backups {
                self.backup_delete(account, &instance.instance_id, &backup.name).await?;
                erased.backups_deleted += 1;
            }
            self.destroy(account, &instance.instance_id).await?;
            erased.servers_removed.push(instance.instance_id.clone());
        }
        erased.requests_forgotten = self.requests.forget(&account.0)?;
        Ok(erased)
    }

    pub async fn backup_restore(&self, account: &AccountId, instance_id: &str, name: &str) -> Result<()> {
        self.backup_named(account, instance_id, name, Restore::Restore).await
    }

    pub async fn backup_delete(&self, account: &AccountId, instance_id: &str, name: &str) -> Result<()> {
        self.backup_named(account, instance_id, name, Restore::Delete).await
    }

    async fn backup_named(&self, account: &AccountId, instance_id: &str, name: &str, what: Restore) -> Result<()> {
        if !platform_agent::backups::valid_name(name) {
            return Err(anyhow!("that is not a backup name the platform made"));
        }
        let (_, node) = self.owned(account, instance_id).await?;
        if let Some(dest) = Self::agent_dest(&node) {
            let dest = dest?;
            let client = self.agent_client().await.ok_or_else(|| {
                anyhow!("node {} is remote but this index has no uplink", node.name)
            })?;
            return match what {
                Restore::Restore => client.backup_restore(dest, instance_id, name).await,
                Restore::Delete => client.backup_delete(dest, instance_id, name).await,
            }
            .map_err(|e| too_old_or(e, &node, "restore or delete backups"))
            .with_context(|| format!("asking agent {} about that backup", node.name));
        }
        let base = format!("{}/instances/{}/backups/{}", node.api.trim_end_matches('/'), instance_id, name);
        let request = match what {
            Restore::Restore => self.http.post(format!("{base}/restore")),
            Restore::Delete => self.http.delete(&base),
        };
        let response = request.send().await.context("asking the node")?;
        if !response.status().is_success() {
            return Err(anyhow!("the node refused that backup action"));
        }
        Ok(())
    }

    /// Move one of the caller's own servers to another node (ROADMAP §13 item
    /// 7), with its files and its address, and remove it from the first only
    /// once it stands on the second.
    ///
    /// **The server keeps running while its files are copied.** The new
    /// server is made first and held stopped; a full copy of the files goes
    /// across while the old one still serves, then up to `MAX_LIVE_DELTAS`
    /// copies of only what changed since the previous one (`backups::create_since`),
    /// until one is small. Only then is the old server stopped, one last
    /// delta (what the players did since) laid over, the old server's identity
    /// put on the new one so it comes up at the **same address**, and the new
    /// one started. Players are down for that last delta and the game's own
    /// boot, not for the copy.
    ///
    /// **The old server is removed last**, and until it is stopped it is not
    /// touched at all: a failure before that removes only what was made on
    /// the new node. After the stop, a failure removes the new copy and starts
    /// the old one again if it was running. A server that was not running
    /// moves in one full copy, nothing else.
    ///
    /// The new node is picked by the same placement as a create, among the
    /// nodes of the old node's class, never the old node itself. No quota is
    /// taken: the account holds one server before and after.
    ///
    /// **The identity passes through the index** (as a request on the
    /// uplink's encrypted link, held in memory, never written here): the
    /// address is the identity, and two nodes can reach each other only
    /// through the index. It is a power an index on a node's
    /// `trusted_indexes` already has (it can create, stop and remove there),
    /// and the old node's copy goes with the old server's folder. The server
    /// gets a new instance id; players' invite links keep working because
    /// they name the address.
    pub async fn relocate(&self, account: &AccountId, instance_id: &str) -> Result<HostedInstance> {
        self.relocate_to(account, instance_id, MoveTarget::Auto).await
    }

    /// [`relocate`](Self::relocate) to a chosen worker, or to another region
    /// (an approved request only; see `MoveTarget`).
    pub async fn relocate_to(&self, account: &AccountId, instance_id: &str, target: MoveTarget<'_>) -> Result<HostedInstance> {
        let config = self.config();
        let (found, from) = self.owned(account, instance_id).await?;
        let _held = MoveGuard::take(&self.moving, instance_id)
            .ok_or_else(|| anyhow!("this server is already being moved"))?;
        let existing = self.all_instances().await?;
        let to = self.move_target(&config, &existing, &from, target).await?;
        let too_old = |node: &NodeConfig| anyhow!("node {} runs an agent too old to move servers; update it", node.name);
        let old_agent = |e: anyhow::Error, node: &NodeConfig| {
            if format!("{e:#}").contains("unknown op") { too_old(node) } else { e }
        };

        let mut spec = self.spec_on(&from, instance_id).await.map_err(|e| old_agent(e, &from))?;
        let was_running = found.state == "running";
        spec.instance_id = new_instance_id(&spec.game_id)?;
        // The new node picks every port, as on a create.
        spec.port = None;
        spec.extra_ports = Default::default();
        let new_id = spec.instance_id.clone();

        // Made and held stopped: a create starts the server, and its files are
        // swapped only while it is stopped.
        let (port, ports) = match self.create_on(&to, spec.clone(), &account.0).await {
            Ok(picked) => picked,
            Err(e) => return Err(e.context(format!("creating it on {}", to.name))),
        };
        let remove_new = |e: anyhow::Error| async {
            if let Err(undo) = self.remove_on(&to, &new_id).await {
                tracing::warn!(instance = %new_id, node = %to.name, error = %undo, "a half-moved server was left on the new node");
            }
            e
        };
        if let Err(e) = self.stop_on(&to, &new_id).await {
            return Err(remove_new(e).await);
        }

        // Copy while it serves. Nothing here touches the old server.
        let (mut since, _) = match self.copy_pass(&from, &to, instance_id, &new_id, None).await {
            Ok(done) => done,
            Err(e) => return Err(remove_new(old_agent(e, &from)).await),
        };
        if was_running {
            for _ in 0..MAX_LIVE_DELTAS {
                match self.copy_pass(&from, &to, instance_id, &new_id, Some(since)).await {
                    Ok((next, bytes)) => {
                        since = next;
                        if bytes <= SMALL_DELTA_BYTES {
                            break;
                        }
                    }
                    Err(e) => return Err(remove_new(old_agent(e, &from)).await),
                }
            }
        }

        // From here the old server is stopped, and put back if this fails.
        let went_down = std::time::Instant::now();
        if was_running {
            if let Err(e) = self.stop_on(&from, instance_id).await {
                return Err(remove_new(e).await);
            }
        }
        let old_node = &from;
        let restart_old = |e: anyhow::Error| async move {
            if was_running {
                if let Err(again) = self.start_on(old_node, instance_id).await {
                    tracing::warn!(instance = %instance_id, error = %again, "could not start a server again after its move failed");
                    return e.context(format!("and it could not be started again on {}", old_node.name));
                }
            }
            e
        };
        let landed = async {
            if was_running {
                self.copy_pass(&from, &to, instance_id, &new_id, Some(since)).await.map_err(|e| old_agent(e, &from))?;
            }
            // The address: the old identity on the new server, if the old
            // node has one to give.
            if let Some(identity) = self.identity_on(&from, instance_id).await.map_err(|e| old_agent(e, &from))? {
                self.set_identity_on(&to, &new_id, &identity).await.map_err(|e| old_agent(e, &to))?;
            }
            if was_running {
                self.start_on(&to, &new_id).await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(e) = landed {
            let e = e.context(format!("moving it onto {}", to.name));
            return Err(restart_old(remove_new(e).await).await);
        }
        let downtime_ms = went_down.elapsed().as_millis() as u64;

        // It stands on the new node. A failure to remove the old one leaves two
        // copies, which is said rather than undone: the new one is the move.
        if let Err(e) = self.remove_on(&from, instance_id).await {
            tracing::warn!(instance = %instance_id, node = %from.name, error = %e, "moved, but the old server was not removed");
        }
        tracing::info!(from = %instance_id, to = %new_id, from_node = %from.name, to_node = %to.name, downtime_ms, "moved a server");
        // A request waiting for this server follows it to its new id.
        if let Err(e) = self.requests.retarget(instance_id, &new_id) {
            tracing::warn!(error = %format!("{e:#}"), "a region request did not follow its server's move");
        }
        Ok(HostedInstance {
            instance_id: new_id,
            node: to.name.clone(),
            game_id: spec.game_id,
            name: spec.name,
            state: if was_running { "running" } else { "stopped" }.to_string(),
            // What the new node picked: a caller pointing a game at the
            // server needs the port, not just the address.
            port,
            ports,
            owner: Some(account.0.clone()),
            uptime_secs: Some(0),
            stopped_secs: None,
            delete_in_secs: None,
            players_now: None,
            mesh_destination: None,
            region: None,
        })
    }

    /// The worker a move goes to. Only same-class workers, never the one it is
    /// on, and only workers that answered: a move onto one that did not would
    /// fail after the copy began. `Auto` and `Worker` stay in the server's
    /// region; only an approved request moves it out (`Region`).
    async fn move_target(
        &self,
        config: &HostingConfig,
        existing: &[HostedInstance],
        from: &NodeConfig,
        target: MoveTarget<'_>,
    ) -> Result<NodeConfig> {
        let eligible = |n: &&NodeConfig| n.class == from.class && n.name != from.name;
        let candidates: Vec<&NodeConfig> = match target {
            MoveTarget::Auto => config.nodes.iter().filter(eligible).filter(|n| n.region == from.region).collect(),
            MoveTarget::Worker(name) => {
                let node = config.nodes.iter().find(|n| n.name == name).ok_or_else(|| anyhow!("no such worker"))?;
                if node.name == from.name {
                    return Err(anyhow!("it is already on that worker"));
                }
                if node.class != from.class {
                    return Err(anyhow!("that worker takes another class of server"));
                }
                if node.region != from.region {
                    return Err(anyhow!("that worker is in another region; ask an admin to move it there"));
                }
                vec![node]
            }
            MoveTarget::Region(region) => {
                config.nodes.iter().filter(eligible).filter(|n| n.region.as_deref() == Some(region)).collect()
            }
        };
        let mut best: Option<(&NodeConfig, Rank)> = None;
        for node in candidates {
            let here = existing.iter().filter(|i| i.node == node.name).count();
            let rank = match rank_node(self.node_capacity(node).await, here, node.priority) {
                Some(rank) if rank.0 == 1 => rank,
                Some(_) if matches!(target, MoveTarget::Worker(_)) => return Err(anyhow!("that worker did not answer")),
                None if matches!(target, MoveTarget::Worker(_)) => return Err(anyhow!("that worker has no room")),
                _ => continue,
            };
            if best.is_none_or(|(_, current)| rank > current) {
                best = Some((node, rank));
            }
        }
        best.map(|(node, _)| node.clone()).ok_or_else(|| match target {
            MoveTarget::Region(region) => anyhow!("no worker in region {region} has room for it"),
            _ => anyhow!("no other worker in this server's region has room; ask an admin to move it to another region"),
        })
    }

    /// The workers one of the caller's servers could move to, for them to pick.
    pub async fn move_targets(&self, account: &AccountId, instance_id: &str) -> Result<MoveTargets> {
        let config = self.config();
        let (_, from) = self.owned(account, instance_id).await?;
        let existing = self.all_instances().await?;
        let mut workers = Vec::new();
        for node in config.nodes.iter().filter(|n| n.class == from.class && n.name != from.name && n.region == from.region) {
            let here = existing.iter().filter(|i| i.node == node.name).count();
            let room = match rank_node(self.node_capacity(node).await, here, node.priority) {
                Some(rank) if rank.0 == 1 => "room",
                Some(_) => "silent",
                None => "full",
            };
            workers.push(MoveWorker { name: node.name.clone(), room: room.to_string() });
        }
        let mut regions: Vec<String> = config
            .nodes
            .iter()
            .filter(|n| n.class == from.class)
            .filter_map(|n| n.region.clone())
            .filter(|r| Some(r) != from.region.as_ref())
            .collect();
        regions.sort();
        regions.dedup();
        Ok(MoveTargets { worker: from.name.clone(), region: from.region.clone(), workers, regions })
    }

    /// An admin from the hosting file, or one granted here.
    pub fn is_admin(&self, account: &AccountId) -> bool {
        self.config().admins.iter().any(|a| a.eq_ignore_ascii_case(&account.0)) || self.admin_store.is_granted(&account.0)
    }

    /// Ask to become an admin of this index. An admin already is one.
    pub fn request_admin(&self, account: &AccountId, note: Option<String>, now: SystemTime) -> Result<crate::admins::AdminRequest> {
        if self.is_admin(account) {
            return Err(anyhow!("you are already an admin of this index"));
        }
        self.admin_store.ask(&account.0, note, unix(now))
    }

    /// The caller's admin status and their latest request to become one.
    pub fn admin_status(&self, account: &AccountId) -> (bool, Option<crate::admins::AdminRequest>) {
        (self.is_admin(account), self.admin_store.latest_for(&account.0))
    }

    pub fn admin_requests(&self, admin: &AccountId) -> Result<Vec<crate::admins::AdminRequest>> {
        self.admin_only(admin)?;
        Ok(self.admin_store.requests())
    }

    pub fn approve_admin_request(&self, admin: &AccountId, id: &str, now: SystemTime) -> Result<crate::admins::AdminRequest> {
        self.admin_only(admin)?;
        let approved = self.admin_store.approve(id, &admin.0, unix(now))?;
        tracing::info!(identity = %approved.identity, by = %admin.0, "an account was made an admin of this index");
        Ok(approved)
    }

    pub fn deny_admin_request(
        &self,
        admin: &AccountId,
        id: &str,
        reason: Option<String>,
        now: SystemTime,
    ) -> Result<crate::admins::AdminRequest> {
        self.admin_only(admin)?;
        self.admin_store.deny(id, &admin.0, unix(now), reason)
    }

    /// Every admin: the hosting file's, then the ones granted here.
    pub fn admins(&self, admin: &AccountId) -> Result<Vec<AdminView>> {
        self.admin_only(admin)?;
        let mut out: Vec<AdminView> = self
            .config()
            .admins
            .iter()
            .map(|a| AdminView { identity: a.to_ascii_lowercase(), source: "file".into(), granted_by: None })
            .collect();
        for g in self.admin_store.granted() {
            if !out.iter().any(|a| a.identity.eq_ignore_ascii_case(&g.identity)) {
                out.push(AdminView { identity: g.identity, source: "granted".into(), granted_by: Some(g.granted_by) });
            }
        }
        Ok(out)
    }

    /// Take away an admin granted here. One from the hosting file is changed
    /// there; and the last admin stays, or nobody could decide anything.
    pub fn revoke_admin(&self, admin: &AccountId, identity: &str) -> Result<()> {
        self.admin_only(admin)?;
        if self.config().admins.iter().any(|a| a.eq_ignore_ascii_case(identity)) {
            return Err(anyhow!("that admin is written in the hosting file; remove it there"));
        }
        if self.admins(admin)?.len() <= 1 {
            return Err(anyhow!("that is the last admin of this index"));
        }
        self.admin_store.revoke(identity)?;
        tracing::info!(identity = %identity, by = %admin.0, "an admin of this index was revoked");
        Ok(())
    }

    /// Every worker as an admin sees it, asked now.
    pub async fn workers(&self, admin: &AccountId) -> Result<Vec<WorkerView>> {
        self.admin_only(admin)?;
        let config = self.config();
        let existing = self.all_instances().await?;
        let mut out = Vec::new();
        for node in &config.nodes {
            let capacity = self.node_capacity(node).await;
            out.push(WorkerView {
                name: node.name.clone(),
                region: node.region.clone(),
                class: node.class.clone(),
                priority: node.priority,
                answered: capacity.is_some(),
                servers: existing.iter().filter(|i| i.node == node.name).count(),
                capacity,
            });
        }
        Ok(out)
    }

    fn admin_only(&self, account: &AccountId) -> Result<()> {
        if self.is_admin(account) {
            Ok(())
        } else {
            Err(anyhow!("only an admin of this index can decide region requests"))
        }
    }

    /// The owner asks for one of their servers to move to another region.
    pub async fn request_region(
        &self,
        account: &AccountId,
        instance_id: &str,
        region: &str,
        now: SystemTime,
    ) -> Result<crate::regions::RegionRequest> {
        let config = self.config();
        let (found, node) = self.owned(account, instance_id).await?;
        if !valid_class(region) {
            return Err(anyhow!("that is not a region name"));
        }
        if node.region.as_deref() == Some(region) {
            return Err(anyhow!("this server is already in region {region}"));
        }
        if !config.nodes.iter().any(|n| n.class == node.class && n.region.as_deref() == Some(region)) {
            return Err(anyhow!("no worker in region {region} takes this server"));
        }
        self.requests.create(crate::regions::RegionRequest {
            id: String::new(),
            instance_id: instance_id.to_string(),
            owner: account.0.clone(),
            game_id: found.game_id,
            name: found.name,
            from_region: node.region.clone(),
            to_region: region.to_string(),
            created_unix: unix(now),
            state: crate::regions::RequestState::Pending,
            decided_unix: None,
            decided_by: None,
            note: None,
            moved_to: None,
        })
    }

    pub fn region_requests_for(&self, account: &AccountId) -> Vec<crate::regions::RegionRequest> {
        self.requests.for_owner(&account.0)
    }

    pub fn cancel_region_request(&self, account: &AccountId, id: &str) -> Result<crate::regions::RegionRequest> {
        self.requests.cancel(&account.0, id)
    }

    pub fn all_region_requests(&self, admin: &AccountId) -> Result<Vec<crate::regions::RegionRequest>> {
        self.admin_only(admin)?;
        Ok(self.requests.all())
    }

    /// An admin approves: the server moves to a worker with room in the asked
    /// region, which becomes its home. A move that cannot happen (no room
    /// there now) leaves the request pending, saying why, for another try.
    pub async fn approve_region_request(
        &self,
        admin: &AccountId,
        id: &str,
        now: SystemTime,
    ) -> Result<crate::regions::RegionRequest> {
        self.admin_only(admin)?;
        let request = self.requests.get(id).ok_or_else(|| anyhow!("no such request"))?;
        if request.state != crate::regions::RequestState::Pending {
            return Err(anyhow!("that request was already decided"));
        }
        let owner = AccountId(request.owner.clone());
        match self.relocate_to(&owner, &request.instance_id, MoveTarget::Region(&request.to_region)).await {
            Ok(moved) => self.requests.approve(id, &admin.0, unix(now), &moved.instance_id),
            Err(e) => {
                let why = format!("{e:#}");
                let _ = self.requests.note_failure(id, &why);
                Err(e)
            }
        }
    }

    pub fn deny_region_request(
        &self,
        admin: &AccountId,
        id: &str,
        note: Option<String>,
        now: SystemTime,
    ) -> Result<crate::regions::RegionRequest> {
        self.admin_only(admin)?;
        self.requests.deny(id, &admin.0, unix(now), note)
    }

    /// Keep region requests and granted admins in files beside the hosting
    /// file from now on (made on first write), reading any already there.
    pub fn keep_state_beside(mut self, hosting_file: &std::path::Path) -> Result<Self> {
        self.requests = crate::regions::RegionRequests::load(&hosting_file.with_file_name("region-requests.json"))?;
        self.admin_store = crate::admins::AdminStore::load(&hosting_file.with_file_name("index-admins.json"))?;
        Ok(self)
    }

    /// One copy of a server's files from `from` onto the stopped new server
    /// on `to`: all of them (`since` none) or what changed since then. Returns
    /// when the node began making it (the `since` for the next pass) and how
    /// big it was. The archive is deleted again on the old node, and the new
    /// node deletes what it merged or restored.
    async fn copy_pass(
        &self,
        from: &NodeConfig,
        to: &NodeConfig,
        old_id: &str,
        new_id: &str,
        since: Option<i64>,
    ) -> Result<(i64, u64)> {
        let made = self.make_on(from, old_id, since).await.context("backing it up")?;
        // Refused before a byte moves if the new node could never take it.
        if made.bytes > platform_agent::backups::MAX_IMPORT_BYTES {
            return Err(anyhow!("its files are over 4 GB, more than {} takes in", to.name));
        }
        let started = made.started_unix.ok_or_else(|| anyhow!("unknown op: node {} did not say when it began the copy", from.name))?;
        let carried = self.fetch_from(from, old_id, &made).await;
        let _ = self.backup_delete_on(from, old_id, &made.name).await;
        let carried = carried?;
        let landed = self.push_backup(to, new_id, carried.path()).await?;
        match since {
            None => {
                self.restore_on(to, new_id, &landed.name).await?;
                // A full copy stays on the new node as a backup; it is not wanted there.
                let _ = self.backup_delete_on(to, new_id, &landed.name).await;
            }
            Some(_) => self.merge_on(to, new_id, &landed.name).await?,
        }
        Ok((started, made.bytes))
    }

    /// The Reticulum client and destination for a node driven over its uplink,
    /// or `None` for a loopback-HTTP node.
    async fn uplink_of(&self, node: &NodeConfig) -> Result<Option<(Arc<AgentClient>, DestinationHash)>> {
        let Some(dest) = Self::agent_dest(node) else { return Ok(None) };
        let dest = dest?;
        let client = self
            .agent_client()
            .await
            .ok_or_else(|| anyhow!("node {} is remote but this index has no uplink", node.name))?;
        Ok(Some((client, dest)))
    }

    async fn spec_on(&self, node: &NodeConfig, instance_id: &str) -> Result<platform_agent::instance::InstanceSpec> {
        if let Some((client, dest)) = self.uplink_of(node).await? {
            return client.spec(dest, instance_id).await.with_context(|| format!("asking agent {} for the server's settings", node.name));
        }
        let url = format!("{}/instances/{}/spec", node.api.trim_end_matches('/'), instance_id);
        let response = self.http.get(&url).send().await.with_context(|| format!("asking node {}", node.name))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            // An agent without the route says 404 too; either way the move
            // cannot read what to make.
            return Err(anyhow!("unknown op: node {} could not say how the server was made", node.name));
        }
        if !response.status().is_success() {
            return Err(anyhow!("node {} refused the request", node.name));
        }
        response.json().await.with_context(|| format!("reading node {}'s reply", node.name))
    }

    /// Create on one node and return the ports it picked.
    async fn create_on(
        &self,
        node: &NodeConfig,
        spec: platform_agent::instance::InstanceSpec,
        owner: &str,
    ) -> Result<(Option<u16>, Vec<InstancePort>)> {
        if let Some((client, dest)) = self.uplink_of(node).await? {
            let created = client.create(dest, spec, Some(owner.to_string())).await?;
            return Ok((created.port, created.ports));
        }
        let mut spec = spec;
        spec.owner = Some(owner.to_string());
        let url = format!("{}/instances", node.api.trim_end_matches('/'));
        let response = self.http.post(&url).json(&spec).send().await.with_context(|| format!("asking node {}", node.name))?;
        if !response.status().is_success() {
            let detail: serde_json::Value = response.json().await.unwrap_or_default();
            return Err(anyhow!("{}", detail["error"].as_str().unwrap_or("the node refused")));
        }
        let created: serde_json::Value = response.json().await.unwrap_or_default();
        Ok((created["port"].as_u64().map(|p| p as u16), ports_from_json(&created["ports"])))
    }

    async fn restore_on(&self, node: &NodeConfig, instance_id: &str, name: &str) -> Result<()> {
        if let Some((client, dest)) = self.uplink_of(node).await? {
            return client.backup_restore(dest, instance_id, name).await;
        }
        let url = format!("{}/instances/{}/backups/{}/restore", node.api.trim_end_matches('/'), instance_id, name);
        let response = self.http.post(&url).send().await.context("asking the node")?;
        if !response.status().is_success() {
            let detail: serde_json::Value = response.json().await.unwrap_or_default();
            return Err(anyhow!("{}", detail["error"].as_str().unwrap_or("the node refused to restore it")));
        }
        Ok(())
    }

    /// Make a backup of a server on `node`, or a delta of what changed since
    /// `since`, on whichever transport the node uses.
    async fn make_on(&self, node: &NodeConfig, instance_id: &str, since: Option<i64>) -> Result<Backup> {
        if let Some((client, dest)) = self.uplink_of(node).await? {
            return match since {
                None => client.backup_create(dest, instance_id).await,
                Some(since) => client.delta_create(dest, instance_id, since).await,
            };
        }
        let base = format!("{}/instances/{}", node.api.trim_end_matches('/'), instance_id);
        let request = match since {
            None => self.http.post(format!("{base}/backups")),
            Some(since) => self.http.post(format!("{base}/delta")).json(&serde_json::json!({ "since": since })),
        };
        let response = request.timeout(TRANSFER_HTTP_TIMEOUT).send().await.with_context(|| format!("asking node {}", node.name))?;
        if since.is_some() && response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(anyhow!("unknown op: node {} cannot make a delta", node.name));
        }
        if !response.status().is_success() {
            let detail: serde_json::Value = response.json().await.unwrap_or_default();
            return Err(anyhow!("{}", detail["error"].as_str().unwrap_or("the node refused to back it up")));
        }
        response.json().await.with_context(|| format!("reading node {}'s reply", node.name))
    }

    /// Bring a backup made on `from` here, into a temporary file that is
    /// deleted when the move is done with it. The file passes through the
    /// index because two nodes have no way to reach each other but through
    /// it; it never holds more than one piece in memory.
    async fn fetch_from(&self, from: &NodeConfig, instance_id: &str, backup: &Backup) -> Result<TempFile> {
        use tokio::io::AsyncWriteExt;
        let file = TempFile::new()?;
        let mut out = tokio::fs::File::create(file.path()).await?;
        if let Some((client, dest)) = self.uplink_of(from).await? {
            let mut offset = 0;
            loop {
                let (total, data) = client.backup_read(dest, instance_id, &backup.name, offset).await?;
                if data.is_empty() && offset < total {
                    return Err(anyhow!("node {} sent an empty piece of the backup", from.name));
                }
                out.write_all(&data).await?;
                offset += data.len() as u64;
                if offset >= total {
                    break;
                }
            }
        } else {
            let url = format!("{}/instances/{}/backups/{}/file", from.api.trim_end_matches('/'), instance_id, backup.name);
            let mut response = self.http.get(url).timeout(TRANSFER_HTTP_TIMEOUT).send().await.context("asking the node for the backup")?;
            if !response.status().is_success() {
                return Err(anyhow!("node {} refused to hand over the backup", from.name));
            }
            while let Some(piece) = response.chunk().await.context("reading the backup")? {
                out.write_all(&piece).await?;
            }
        }
        out.flush().await?;
        Ok(file)
    }

    /// Delete one backup of one server on a node, whichever transport.
    async fn backup_delete_on(&self, node: &NodeConfig, instance_id: &str, name: &str) -> Result<()> {
        if let Some((client, dest)) = self.uplink_of(node).await? {
            return client.backup_delete(dest, instance_id, name).await;
        }
        let url = format!("{}/instances/{}/backups/{}", node.api.trim_end_matches('/'), instance_id, name);
        let response = self.http.delete(&url).send().await.context("asking the node")?;
        if !response.status().is_success() {
            return Err(anyhow!("the node refused to delete the backup"));
        }
        Ok(())
    }

    /// Lay a delta already on `node` over a stopped server's files.
    async fn merge_on(&self, node: &NodeConfig, instance_id: &str, name: &str) -> Result<()> {
        if let Some((client, dest)) = self.uplink_of(node).await? {
            return client.delta_merge(dest, instance_id, name).await;
        }
        let url = format!("{}/instances/{}/backups/{}/merge", node.api.trim_end_matches('/'), instance_id, name);
        let response = self.http.post(&url).send().await.context("asking the node")?;
        if !response.status().is_success() {
            let detail: serde_json::Value = response.json().await.unwrap_or_default();
            return Err(anyhow!("{}", detail["error"].as_str().unwrap_or("the node refused to merge it")));
        }
        Ok(())
    }

    /// A server's mesh identity (64 bytes) from its node, or none where the
    /// node has no mesh. Held in memory only, and never logged.
    async fn identity_on(&self, node: &NodeConfig, instance_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        if let Some((client, dest)) = self.uplink_of(node).await? {
            return Ok(client.identity_read(dest, instance_id).await?.map(Zeroizing::new));
        }
        let url = format!("{}/instances/{}/identity", node.api.trim_end_matches('/'), instance_id);
        let response = self.http.get(&url).send().await.with_context(|| format!("asking node {}", node.name))?;
        if !response.status().is_success() {
            return Err(anyhow!("node {} refused the request", node.name));
        }
        let reply: platform_agent::uplink_wire::IdentityResp =
            response.json().await.with_context(|| format!("reading node {}'s reply", node.name))?;
        reply
            .identity
            .map(|h| hex::decode(h).map(Zeroizing::new).map_err(|_| anyhow!("the node sent an identity we cannot read")))
            .transpose()
    }

    /// Give a stopped server on `node` another server's identity.
    async fn set_identity_on(&self, node: &NodeConfig, instance_id: &str, identity: &[u8]) -> Result<()> {
        if let Some((client, dest)) = self.uplink_of(node).await? {
            return client.identity_set(dest, instance_id, identity).await;
        }
        let url = format!("{}/instances/{}/identity", node.api.trim_end_matches('/'), instance_id);
        let response = self
            .http
            .put(&url)
            .json(&serde_json::json!({ "identity": hex::encode(identity) }))
            .send()
            .await
            .with_context(|| format!("asking node {}", node.name))?;
        if !response.status().is_success() {
            let detail: serde_json::Value = response.json().await.unwrap_or_default();
            return Err(anyhow!("{}", detail["error"].as_str().unwrap_or("the node refused the identity")));
        }
        Ok(())
    }

    /// Send a backup file to a node as an import for `instance_id`; the node
    /// checks it like any uploaded backup and names it.
    async fn push_backup(&self, node: &NodeConfig, instance_id: &str, file: &std::path::Path) -> Result<Backup> {
        use tokio::io::AsyncReadExt;
        let total = tokio::fs::metadata(file).await?.len();
        if let Some((client, dest)) = self.uplink_of(node).await? {
            let mut input = tokio::fs::File::open(file).await?;
            let mut upload: Option<String> = None;
            let mut offset = 0u64;
            let mut buf = vec![0u8; platform_agent::uplink_wire::TRANSFER_CHUNK];
            loop {
                let mut filled = 0;
                while filled < buf.len() {
                    let n = input.read(&mut buf[filled..]).await?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                let progress = client
                    .backup_write(dest, instance_id, upload.as_deref(), offset, total, &buf[..filled])
                    .await
                    .context("sending the backup")?;
                offset = progress.received;
                if let Some(backup) = progress.backup {
                    return Ok(backup);
                }
                if filled == 0 {
                    return Err(anyhow!("node {} did not take the whole backup", node.name));
                }
                upload = Some(progress.upload);
            }
        }
        // Streamed, as the uplink path is in pieces: a backup can be gigabytes.
        let input = tokio::fs::File::open(file).await?;
        let stream = futures_util::stream::unfold(input, |mut input| async move {
            let mut buf = vec![0u8; 64 * 1024];
            match input.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok::<_, std::io::Error>(buf), input))
                }
                Err(e) => Some((Err(e), input)),
            }
        });
        let body = reqwest::Body::wrap_stream(stream);
        let url = format!("{}/instances/{}/backups/import", node.api.trim_end_matches('/'), instance_id);
        let response = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_LENGTH, total)
            .body(body)
            .timeout(TRANSFER_HTTP_TIMEOUT)
            .send()
            .await
            .context("sending the backup")?;
        if !response.status().is_success() {
            let detail: serde_json::Value = response.json().await.unwrap_or_default();
            return Err(anyhow!("{}", detail["error"].as_str().unwrap_or("the node refused the backup")));
        }
        response.json().await.context("reading the node's reply")
    }

    /// Send one loopback request to a node and read its JSON reply.
    async fn node_json<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        node: &NodeConfig,
    ) -> Result<T> {
        let response = request.send().await.with_context(|| format!("asking node {}", node.name))?;
        if !response.status().is_success() {
            return Err(anyhow!("node {} refused the request", node.name));
        }
        response.json().await.with_context(|| format!("reading node {}'s reply", node.name))
    }

    /// Ask one node to start one stopped instance, on whichever transport it
    /// uses. A node whose agent predates the op says so in words an operator
    /// can act on, rather than "unknown op".
    async fn start_on(&self, node: &NodeConfig, instance_id: &str) -> Result<()> {
        let too_old = || anyhow!("node {} runs an agent too old to start a stopped server; update it", node.name);
        if let Some(dest) = Self::agent_dest(node) {
            let dest = dest?;
            let client = self
                .agent_client()
                .await
                .ok_or_else(|| anyhow!("node {} is remote but this index has no uplink", node.name))?;
            return match client.start(dest, instance_id).await {
                Err(e) if format!("{e:#}").contains("unknown op") => Err(too_old()),
                other => other.with_context(|| format!("asking agent {} to start it", node.name)),
            };
        }
        let url = format!(
            "{}/instances/{}/start",
            node.api.trim_end_matches('/'),
            instance_id
        );
        let response = self.http.post(&url).send().await.context("asking the node")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND
            || response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED
        {
            return Err(too_old());
        }
        if !response.status().is_success() {
            let detail: serde_json::Value = response.json().await.unwrap_or_default();
            let msg = detail["error"].as_str().unwrap_or("the node refused to start it");
            return Err(anyhow!("{msg}"));
        }
        Ok(())
    }

    /// Stop the instances the quota policy says have outlived their welcome.
    ///
    /// `PLAN.md` §8 phase 4 promises idle reaping "from day one", because
    /// public deploy plus anonymous identities is free compute. The policy has
    /// always been here; until now nothing called it, so nothing was ever
    /// reaped and an abandoned server ran until an operator noticed.
    ///
    /// Returns what it stopped, so a caller can log it. A node that refuses is
    /// logged and skipped rather than failing the sweep: the other instances
    /// are still costing the same node time.
    pub async fn reap_idle(&self, now: SystemTime) -> Result<Vec<(String, ReapReason)>> {
        let config = self.config();
        let quotas = Self::quotas(&config);
        if quotas.policy().idle_timeout.is_none() {
            return Ok(Vec::new());
        }
        let existing = self.all_instances().await?;
        let records: Vec<InstanceRecord> = existing.iter().map(|i| record_for(i, now)).collect();

        let mut reaped = Vec::new();
        for (instance_id, reason) in quotas.to_reap(&records, now) {
            let Some(instance) = existing.iter().find(|i| i.instance_id == instance_id) else {
                continue;
            };
            let Some(node) = config.nodes.iter().find(|n| n.name == instance.node) else {
                continue;
            };
            // Stop, not remove. The instance directory holds whatever players
            // built there, and reaping is about reclaiming compute, not about
            // destroying state — `orphan_dirs` on the node already refuses to
            // delete that automatically for the same reason.
            match self.stop_on(node, &instance_id).await {
                Ok(()) => {
                    tracing::info!(instance = %instance_id, node = %node.name, ?reason, "reaped an idle instance");
                    reaped.push((instance_id, reason));
                }
                Err(e) => {
                    tracing::warn!(instance = %instance_id, node = %node.name, error = %e, "could not reap")
                }
            }
        }
        Ok(reaped)
    }

    /// Remove stopped servers past the operator's expiry. A node that refuses
    /// is logged and skipped, as in `reap_idle`.
    ///
    /// Only the container goes: the node keeps the server's files, so a removal
    /// the operator did not mean is recoverable by hand.
    pub async fn expire_stopped(&self) -> Result<Vec<String>> {
        let config = self.config();
        let quotas = Self::quotas(&config);
        if quotas.policy().stopped_expiry.is_none() {
            return Ok(Vec::new());
        }
        let existing = self.all_instances().await?;
        let mut removed = Vec::new();
        for instance in existing.iter().filter(|i| i.state == "stopped") {
            let Some(stopped_for) = instance.stopped_secs else { continue };
            let account = AccountId(instance.owner.clone().unwrap_or_default());
            if !quotas.stopped_expired(&account, Duration::from_secs(stopped_for)) {
                continue;
            }
            let Some(node) = config.nodes.iter().find(|n| n.name == instance.node) else {
                continue;
            };
            match self.remove_on(node, &instance.instance_id).await {
                Ok(()) => {
                    tracing::info!(instance = %instance.instance_id, node = %node.name, stopped_for, "removed a server stopped too long");
                    removed.push(instance.instance_id.clone());
                }
                Err(e) => {
                    tracing::warn!(instance = %instance.instance_id, node = %node.name, error = %e, "could not remove a stopped server")
                }
            }
        }
        Ok(removed)
    }

    /// Most free room wins, asking each node what it actually has.
    ///
    /// The index's own instance count is not the node's capacity. A node has a
    /// `max_instances` its operator set and a port range it can exhaust, and an
    /// index that placed on a full node would tell a user it had chosen one and
    /// then fail the create — after the quota admission has already run. So the
    /// node is asked, and a node reporting itself full is skipped rather than
    /// picked and disappointed.
    ///
    /// **A node that cannot answer is not treated as empty.** It falls back to
    /// the index's own count and ranks behind every node that did answer: an
    /// unreachable node is the least attractive place to put something, and
    /// "silence means room" is how an outage becomes a pile-up.
    async fn pick_node<'c>(
        &self,
        config: &'c HostingConfig,
        existing: &[HostedInstance],
        class: Option<&str>,
        region: Option<&str>,
    ) -> Option<&'c NodeConfig> {
        self.pick_node_except(config, existing, class, region, "").await
    }

    /// [`pick_node`](Self::pick_node) leaving out the node named `except`, the
    /// one a move is leaving.
    async fn pick_node_except<'c>(
        &self,
        config: &'c HostingConfig,
        existing: &[HostedInstance],
        class: Option<&str>,
        region: Option<&str>,
        except: &str,
    ) -> Option<&'c NodeConfig> {
        // The asked-for region comes right after "answered": a node in it
        // beats every other node that answered, and a silent node in it still
        // loses to all of them, as silence always does.
        let mut best: Option<(&NodeConfig, (u8, bool, Rank))> = None;
        // A classed node takes only creates that name its class, and a create
        // that names none takes only the nodes that have none.
        for node in config.nodes.iter().filter(|n| n.class.as_deref() == class && n.name != except) {
            let here = existing.iter().filter(|i| i.node == node.name).count();
            let Some(rank) = rank_node(self.node_capacity(node).await, here, node.priority) else {
                continue;
            };
            let ranked = (rank.0, region.is_some() && node.region.as_deref() == region, rank);
            if best.is_none_or(|(_, current)| ranked > current) {
                best = Some((node, ranked));
            }
        }
        best.map(|(node, _)| node)
    }

    /// Ask one node what it has room for, or `None` if it cannot be asked.
    ///
    /// Both transports answer from the agent's own `capacity()`, so a loopback
    /// node and a mesh node cannot give different stories.
    async fn node_capacity(&self, node: &NodeConfig) -> Option<CapacityResp> {
        if let Some(dest) = Self::agent_dest(node) {
            let dest = dest.ok()?;
            let client = self.agent_client().await?;
            return client.capacity(dest).await.ok();
        }
        let url = format!("{}/capacity", node.api.trim_end_matches('/'));
        let response = self.http.get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.json::<CapacityResp>().await.ok()
    }
}

/// A node's view of an instance, as the quota engine wants to see it.
///
/// Two conversions carry the weight:
///
/// * **`created_at` comes from the node's `uptime_secs`**, not from `now`. The
///   index used to stamp every instance as created this instant, which made the
///   create cooldown ineffective and would make reaping impossible — nothing is
///   ever old enough to judge. A node that reports no uptime still reads as
///   "created now", which is the safe direction: unknown age means too young to
///   reap.
/// * **`players_now: None` becomes `exempt_from_reaping`.** The quota engine's
///   `players_now` is a count, so unknown has nowhere to live in it. Rather than
///   flatten unknown to zero — which would stop a busy server whose query was
///   dropped — an instance nobody can ask is pinned. An idle instance that is
///   never reaped is a wasted slot; a populated one that is reaped is players
///   thrown out of a game.
fn record_for(instance: &HostedInstance, now: SystemTime) -> InstanceRecord {
    let created_at = instance
        .uptime_secs
        .and_then(|secs| now.checked_sub(Duration::from_secs(secs)))
        .unwrap_or(now);
    InstanceRecord {
        instance_id: instance.instance_id.clone(),
        account: AccountId(instance.owner.clone().unwrap_or_default()),
        created_at,
        // The node reports a count now, not a history. An instance with players
        // right now is never reaped regardless of this, and one without is
        // judged on age, which is `NeverHadPlayers` — conservative, and correct
        // until a node keeps a last-seen timestamp of its own.
        last_player_seen: None,
        players_now: instance.players_now.unwrap_or(0),
        exempt_from_reaping: instance.players_now.is_none(),
    }
}

/// Seconds until the index removes a stopped server, or `None` when it will
/// not: it is running, the node did not say how long it has been stopped, or
/// no removal rule applies to its owner.
fn delete_in_secs(quotas: &Quotas, instance: &HostedInstance) -> Option<u64> {
    if instance.state != "stopped" {
        return None;
    }
    let stopped = instance.stopped_secs?;
    let after = quotas.stopped_expiry_for(&AccountId(instance.owner.clone().unwrap_or_default()))?;
    Some(after.as_secs().saturating_sub(stopped))
}

/// Which backup call a named request makes. Two calls, one check on the name.
#[derive(Clone, Copy)]
enum Restore {
    Restore,
    Delete,
}

/// A node reporting less free memory than this is full, whatever its slot
/// count says: a game server needs about a gigabyte, and a node that starts
/// one with less swaps or kills something.
pub const MIN_FREE_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;

/// `(answered, priority, free memory in MiB, free slots)`, compared in that
/// order. Higher is better.
type Rank = (u8, i32, u64, u64, usize);

/// How attractive a node is to place on. Higher is better; `None` means skip.
///
/// **A reported figure always beats a guess**: `answered` comes first, so a
/// node that told us it has one slot left outranks a node that said nothing,
/// however empty the index believes that one to be, whatever its priority.
/// Silence is not room, and treating it as room is how one unreachable node
/// collects every create request during an outage. Among nodes that answered,
/// the operator's priority decides, then free memory (an older agent that
/// does not report it counts as none), then free slots.
fn rank_node(capacity: Option<CapacityResp>, own_count: usize, priority: i32) -> Option<Rank> {
    match capacity {
        // Full is full. Picking it would pass quota admission and then fail the
        // create, after the user has been told a node was chosen.
        Some(cap) if cap.running >= cap.max_instances => None,
        Some(cap) if cap.memory_available_bytes.is_some_and(|m| m < MIN_FREE_MEMORY_BYTES) => None,
        // Under the node's own reserve is full: its disk is what a download or a
        // server's files would fill, and the node refuses at the same line.
        Some(cap) if matches!((cap.disk_free_bytes, cap.disk_reserve_bytes), (Some(free), Some(reserve)) if free < reserve) => None,
        Some(cap) => Some((
            1,
            priority,
            cap.memory_available_bytes.unwrap_or(0) / (1024 * 1024),
            // Room above the reserve. A node that did not report disk counts as
            // none, like memory, so a node that said more outranks one that said
            // nothing.
            match (cap.disk_free_bytes, cap.disk_reserve_bytes) {
                (Some(free), Some(reserve)) => (free - reserve) / (1024 * 1024),
                _ => 0,
            },
            cap.max_instances - cap.running,
        )),
        None => Some((0, priority, 0, 0, usize::MAX - own_count)),
    }
}

/// How long a loopback node may take to hand over or take in a whole backup.
/// The client's usual 20 s is for one small reply.
const TRANSFER_HTTP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How many copies of only what changed a move makes while the server still
/// runs, before the last one with it stopped, and the size at which a copy
/// counts as small enough to stop doing that.
const MAX_LIVE_DELTAS: usize = 3;
const SMALL_DELTA_BYTES: u64 = 1024 * 1024;

/// A server id held by a move in progress, given back when the move ends
/// however it ends.
struct MoveGuard<'a> {
    set: &'a std::sync::Mutex<std::collections::HashSet<String>>,
    id: String,
}

impl<'a> MoveGuard<'a> {
    fn take(set: &'a std::sync::Mutex<std::collections::HashSet<String>>, id: &str) -> Option<Self> {
        set.lock().unwrap_or_else(|e| e.into_inner()).insert(id.to_string()).then(|| Self { set, id: id.to_string() })
    }
}

impl Drop for MoveGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
    }
}

/// A file in the system's temporary directory, deleted on drop.
struct TempFile(std::path::PathBuf);

impl TempFile {
    fn new() -> Result<Self> {
        let mut seed = [0u8; 8];
        getrandom::getrandom(&mut seed).map_err(|e| anyhow!("no entropy: {e}"))?;
        Ok(Self(std::env::temp_dir().join(format!("lanthorn-move-{}.tar.gz", hex::encode(seed)))))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A random, unguessable instance id, prefixed by the game so `docker ps` on the
/// node reads sensibly.
/// Seconds since the epoch; a clock before it reads as 0.
fn unix(t: SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn new_instance_id(game_id: &str) -> Result<String> {
    let mut bytes = [0u8; 6];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("no entropy: {e}"))?;
    let prefix: String = game_id
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .take(16)
        .collect();
    let prefix = if prefix.is_empty() { "game".to_string() } else { prefix };
    Ok(format!("{prefix}-{}", hex::encode(bytes)))
}

/// An agent that predates an op answers "unknown op". Say what that means and
/// what to do about it; any other error passes through unchanged.
fn too_old_or(e: anyhow::Error, node: &NodeConfig, what: &str) -> anyhow::Error {
    if format!("{e:#}").contains("unknown op") {
        anyhow!("node {} runs an agent too old to {what}; update it", node.name)
    } else {
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> HostingConfig {
        HostingConfig {
            games: vec!["sven-coop".to_string()],
            nodes: vec![NodeConfig {
                agent: None,
                priority: 0,
                class: None,
                region: None,
                name: "local".to_string(),
                api: "http://127.0.0.1:4750".to_string(),
            }],
            quota: QuotaSettings::default(),
            admins: Vec::new(),
        }
    }

    /// An old agent's "unknown op" is turned into what the operator can do
    /// about it; every other error is left as it was.
    #[test]
    fn an_agent_too_old_for_an_op_is_named_and_other_errors_pass_through() {
        let node = &config().nodes[0];
        let old = too_old_or(anyhow!("agent said: unknown op"), node, "make backups").to_string();
        assert_eq!(old, "node local runs an agent too old to make backups; update it");
        let other = too_old_or(anyhow!("the disk is full"), node, "make backups").to_string();
        assert_eq!(other, "the disk is full");
    }

    /// The platform ships no list of hostable games. An operator who has not
    /// written one is not hosting, rather than hosting everything.
    #[test]
    fn hosting_is_off_until_an_operator_opts_in() {
        assert!(!HostingConfig::default().enabled());
        assert!(!HostingConfig { games: vec!["sven-coop".into()], ..Default::default() }.enabled());
        assert!(config().enabled());
    }

    #[test]
    fn only_configured_games_are_hosted() {
        let c = config();
        assert!(c.hosts_game("sven-coop"));
        assert!(!c.hosts_game("minecraft"));
    }

    #[tokio::test]
    async fn deploying_a_game_the_operator_does_not_host_says_what_is_hosted() {
        let h = Hosting::new(config());
        let err = h
            .deploy(
                &AccountId("aa".into()),
                &DeployRequest {
                    game_id: "minecraft".into(),
                    name: "nope".into(),
                    max_players: 8,
                    map: None,
                    class: None,
                    region: None,
                    bots: None,
                    restart_on_crash: false,
                    options: Default::default(),
                },
                std::time::SystemTime::now(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not host"), "{err}");
        assert!(err.contains("sven-coop"), "the caller should see the list: {err}");
    }

    #[tokio::test]
    async fn hosting_disabled_refuses_before_touching_a_node() {
        let h = Hosting::new(HostingConfig::default());
        let err = h
            .deploy(
                &AccountId("aa".into()),
                &DeployRequest {
                    game_id: "sven-coop".into(),
                    name: "x".into(),
                    max_players: 8,
                    map: None,
                    class: None,
                    region: None,
                    bots: None,
                    restart_on_crash: false,
                    options: Default::default(),
                },
                std::time::SystemTime::now(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not offer hosting"), "{err}");
    }

    /// Refused before a node is picked: the config's node is unreachable, so
    /// reaching it would fail with a different error than this one.
    #[tokio::test]
    async fn a_name_that_would_type_at_the_console_is_refused_at_the_index() {
        let h = Hosting::new(config());
        let err = h
            .deploy(
                &AccountId("aa".into()),
                &DeployRequest {
                    game_id: "sven-coop".into(),
                    name: "fun +rcon_password mine".into(),
                    max_players: 8,
                    map: None,
                    class: None,
                    region: None,
                    bots: None,
                    restart_on_crash: false,
                    options: Default::default(),
                },
                std::time::SystemTime::now(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("server name"), "{err}");
    }

    /// Ids are generated, not supplied, and are not guessable.
    #[test]
    fn generated_ids_are_prefixed_unguessable_and_agent_safe() {
        let a = new_instance_id("sven-coop").unwrap();
        let b = new_instance_id("sven-coop").unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("sven-coop-"));
        // Must satisfy the agent's own id rules, or the deploy fails at the node.
        assert!(a.len() <= 64);
        assert!(a
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.' || c == b'_'));

        // A hostile game id cannot inject path or shell characters into it.
        let nasty = new_instance_id("../../etc/passwd").unwrap();
        assert!(!nasty.contains('/'), "{nasty}");
        assert!(!nasty.contains('.') || nasty.starts_with("etcpasswd-"), "{nasty}");
    }

    /// A node's port set has to survive the HTTP hop intact, or a user hosting
    /// a Source server is told the game port and left guessing at RCON. A row
    /// this build cannot read is skipped, never fatal — the index is a cache.
    #[test]
    fn a_nodes_port_set_survives_the_json_hop() {
        // `Udp`/`Tcp`, not `udp`/`tcp`: `GameTransport` serializes by variant
        // name everywhere else a launcher or index reads it, and this is the
        // same enum, not a second spelling of it.
        let ports = ports_from_json(&serde_json::json!([
            {"channel": 0, "host_port": 27151, "transport": "Udp"},
            {"channel": 1, "host_port": 27152, "transport": "Tcp"}
        ]));
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[1].host_port, 27152);
        assert_eq!(ports[1].transport, game_bridge::profile::GameTransport::Tcp);

        assert!(ports_from_json(&serde_json::Value::Null).is_empty());

        // One unreadable row costs that row, not the whole set — and it is
        // never guessed into a port number.
        let mixed = ports_from_json(&serde_json::json!([
            {"channel": 0},
            {"channel": 1, "host_port": 27152, "transport": "Tcp"}
        ]));
        assert_eq!(mixed.len(), 1);
        assert_eq!(mixed[0].channel, 1);
    }

    #[tokio::test]
    async fn a_node_is_picked_by_load() {
        let mut c = config();
        c.nodes.push(NodeConfig { name: "second".into(), api: "http://127.0.0.1:4751".into(), agent: None, priority: 0, class: None, region: None });
        let h = Hosting::new(c);
        let existing = vec![HostedInstance {
            instance_id: "a".into(),
            node: "local".into(),
            game_id: "sven-coop".into(),
            name: "a".into(),
            state: "running".into(),
            port: None,
            ports: Vec::new(),
            stopped_secs: None,
            delete_in_secs: None,
            owner: None,
            uptime_secs: None,
            players_now: None,
            mesh_destination: None,
            region: None,
        }];
        // Neither node answers `/capacity` here — nothing is listening — so
        // both fall back to the index's own count and the emptier one wins.
        assert_eq!(h.pick_node(&h.config(), &existing, None, None).await.unwrap().name, "second");
    }

    fn hosted(id: &str, uptime: Option<u64>, players: Option<u32>) -> HostedInstance {
        HostedInstance {
            instance_id: id.into(),
            node: "local".into(),
            game_id: "sven-coop".into(),
            name: id.into(),
            state: "running".into(),
            port: None,
            ports: Vec::new(),
            stopped_secs: None,
            delete_in_secs: None,
            owner: Some("acct".into()),
            uptime_secs: uptime,
            players_now: players,
            mesh_destination: None,
            region: None,
        }
    }

    fn t() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    /// The index used to stamp every instance as created *now*, which made the
    /// create cooldown ineffective and would make reaping impossible: nothing
    /// is ever old enough to judge.
    #[test]
    fn age_comes_from_the_node_not_from_the_clock() {
        let r = record_for(&hosted("a", Some(3600), Some(0)), t());
        assert_eq!(r.created_at, t() - Duration::from_secs(3600));
    }

    /// A node that did not report uptime reads as brand new — too young to
    /// reap. Unknown age must fall on the safe side.
    #[test]
    fn an_unknown_age_reads_as_too_young_rather_than_ancient() {
        let r = record_for(&hosted("a", None, Some(0)), t());
        assert_eq!(r.created_at, t());
    }

    /// The load-bearing one. `players_now: None` means "could not ask", and
    /// flattening it to zero would stop a busy server whose UDP query was
    /// dropped. It is pinned instead.
    #[test]
    fn an_instance_nobody_can_ask_is_never_reaped() {
        let unknown = record_for(&hosted("a", Some(86_400), None), t());
        assert!(unknown.exempt_from_reaping, "unknown players must not be read as empty");

        let known_empty = record_for(&hosted("b", Some(86_400), Some(0)), t());
        assert!(!known_empty.exempt_from_reaping, "a node that says zero means zero");
    }

    /// End to end through the quota engine: an old, empty, answerable instance
    /// is reaped, and neither an unanswerable nor a populated one is.
    #[test]
    fn the_reaper_picks_only_the_instance_it_should() {
        let policy = QuotaPolicy {
            idle_timeout: Some(Duration::from_secs(600)),
            min_lifetime: Duration::from_secs(300),
            ..QuotaPolicy::default()
        };
        let q = Quotas::new(policy);
        let old = 86_400;
        let records = vec![
            record_for(&hosted("empty", Some(old), Some(0)), t()),
            record_for(&hosted("busy", Some(old), Some(7)), t()),
            record_for(&hosted("unasked", Some(old), None), t()),
            record_for(&hosted("young", Some(60), Some(0)), t()),
        ];
        let reaped: Vec<String> = q.to_reap(&records, t()).into_iter().map(|(id, _)| id).collect();
        assert_eq!(reaped, vec!["empty".to_string()]);
    }

    fn cap(running: usize, max: usize) -> CapacityResp {
        CapacityResp {
            max_instances: max,
            running,
            port_range_start: 27100,
            port_range_end: 27199,
            memory_total_bytes: None,
            memory_available_bytes: None,
            cpus: None,
            disk_free_bytes: None,
            disk_reserve_bytes: None,
        }
    }

    fn with_memory(mut cap: CapacityResp, gib_free: u64) -> CapacityResp {
        cap.memory_total_bytes = Some(16 << 30);
        cap.memory_available_bytes = Some(gib_free << 30);
        cap
    }

    /// A node that says it is nearly out of memory is full, whatever its slot
    /// count; the most free memory wins among the rest; and the operator's
    /// priority wins over both, so a home machine set lower only takes what
    /// the rented one cannot. None of it lets a silent node jump the queue.
    #[test]
    fn placement_follows_priority_then_memory_and_silence_still_loses() {
        assert_eq!(rank_node(Some(with_memory(cap(0, 8), 0)), 0, 0), None, "under 1 GiB free is full");
        assert!(
            rank_node(Some(with_memory(cap(6, 8), 10)), 0, 0) > rank_node(Some(with_memory(cap(0, 8), 2)), 0, 0),
            "more free memory wins over more free slots"
        );
        assert!(
            rank_node(Some(with_memory(cap(0, 8), 2)), 0, 0) > rank_node(Some(cap(0, 8)), 0, 0),
            "a node that reports memory beats one that does not"
        );
        let rented = rank_node(Some(with_memory(cap(5, 8), 2)), 0, 10);
        let home = rank_node(Some(with_memory(cap(0, 8), 12)), 0, 0);
        assert!(rented > home, "priority comes first among nodes that answered");
        assert!(
            rank_node(Some(with_memory(cap(7, 8), 1)), 0, -5) > rank_node(None, 0, 100),
            "silence is not room, whatever its priority"
        );
    }

    /// The whole reason placement asks: a full node must not be chosen and then
    /// fail the create.
    #[test]
    fn a_full_node_is_skipped_rather_than_picked() {
        assert_eq!(rank_node(Some(cap(4, 4)), 0, 0), None);
        assert_eq!(rank_node(Some(cap(5, 4)), 0, 0), None, "over its own limit is still full");
        assert!(rank_node(Some(cap(3, 4)), 0, 0).is_some());
    }

    /// A node that answered outranks one that did not, even when the index
    /// believes the silent one is emptier. Silence is not room.
    #[test]
    fn a_reported_figure_beats_a_guess() {
        let answered = rank_node(Some(cap(3, 4)), 99, 0).expect("has one slot");
        let silent = rank_node(None, 0, 0).expect("unknown, not full");
        assert!(answered > silent, "{answered:?} should outrank {silent:?}");
    }

    #[test]
    fn among_nodes_that_answered_the_emptiest_wins() {
        assert!(rank_node(Some(cap(1, 8)), 0, 0) > rank_node(Some(cap(6, 8)), 0, 0));
    }

    /// An operator adds a node by editing the file, with no restart; a file
    /// that no longer parses changes nothing and says why.
    #[test]
    fn a_node_added_to_the_file_is_in_force_without_a_restart_and_a_typo_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("gpp-hosting-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hosting.toml");
        let one = "games = [\"sven-coop\"]\n[[nodes]]\nname = \"local\"\napi = \"http://127.0.0.1:4750\"\n";
        std::fs::write(&path, one).unwrap();
        let h = Hosting::new(Hosting::load(&path).unwrap());
        assert_eq!(h.config().nodes.len(), 1);

        let two = format!("{one}[[nodes]]\nname = \"eu-1\"\napi = \"http://127.0.0.1:4751\"\npriority = 10\n");
        std::fs::write(&path, two).unwrap();
        h.reload_from(&path).unwrap();
        assert_eq!(h.config().nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["local", "eu-1"]);

        std::fs::write(&path, "games = [\"sven-coop\"\n[[nodes]]").unwrap();
        assert!(h.reload_from(&path).is_err());
        assert_eq!(h.config().nodes.len(), 2, "a file that does not parse changed what is in force");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quota_settings_round_trip_into_a_policy() {
        let s = QuotaSettings {
            max_instances_per_account: 2,
            max_total_instances: 5,
            idle_timeout_secs: Some(60),
            min_lifetime_secs: 30,
            create_cooldown_secs: None,
            accounts: Default::default(),
            stopped_delete_after_days: None,
            max_backups_per_account: Some(4),
        };
        let p = QuotaPolicy::from(&s);
        assert_eq!(p.max_backups_per_account, Some(4));
        assert_eq!(p.max_instances_per_account, 2);
        assert_eq!(p.idle_timeout, Some(std::time::Duration::from_secs(60)));
        assert_eq!(p.create_cooldown, None);
    }

    fn hosting_file(dir: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hosting.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn an_account_section_in_the_file_becomes_an_override_for_that_identity() {
        let hash = "B7F20B85976CA7B7B6C56A883992FB9A".to_ascii_lowercase();
        let path = hosting_file(
            "gpp-index-quota-accounts-ok",
            &format!(
                "games = [\"x\"]\n[quota.accounts.{hash}]\nmax_instances = 9\ncreate_cooldown_secs = 0\nnever_reap = true\n"
            ),
        );
        let config = Hosting::load(&path).unwrap();
        let q = Quotas::new(QuotaPolicy::from(&config.quota));
        let who = AccountId(hash);
        assert_eq!(q.max_instances_for(&who), 9);
        assert_eq!(q.max_instances_for(&AccountId("0".repeat(32))), 3, "others keep the shared limit");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_account_key_that_is_not_an_identity_hash_is_refused_not_ignored() {
        let path = hosting_file(
            "gpp-index-quota-accounts-bad",
            "games = [\"x\"]\n[quota.accounts.not-a-hash]\nmax_instances = 9\n",
        );
        let err = Hosting::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("not an identity hash"), "{err:#}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_same_identity_in_two_cases_is_refused() {
        let lower = "b7f20b85976ca7b7b6c56a883992fb9a";
        let path = hosting_file(
            "gpp-index-quota-accounts-twice",
            &format!(
                "[quota.accounts.{lower}]\nmax_instances = 1\n[quota.accounts.{}]\nmax_instances = 2\n",
                lower.to_ascii_uppercase()
            ),
        );
        let err = Hosting::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("listed twice"), "{err:#}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    fn stopped_on_index(owner: &str, stopped_secs: Option<u64>) -> HostedInstance {
        HostedInstance {
            instance_id: "x".into(),
            node: "n".into(),
            game_id: "g".into(),
            name: "x".into(),
            state: "stopped".into(),
            port: None,
            ports: Vec::new(),
            stopped_secs,
            delete_in_secs: None,
            owner: Some(owner.into()),
            uptime_secs: None,
            players_now: None,
            mesh_destination: None,
            region: None,
        }
    }

    #[test]
    fn a_stopped_server_shows_how_long_until_it_is_removed() {
        let settings = QuotaSettings { stopped_delete_after_days: Some(30), ..Default::default() };
        let q = Quotas::new(QuotaPolicy::from(&settings));
        let day = 86_400;
        assert_eq!(delete_in_secs(&q, &stopped_on_index("aa", Some(10 * day))), Some(20 * day));
        assert_eq!(delete_in_secs(&q, &stopped_on_index("aa", Some(40 * day))), Some(0));
        // A node that did not say how long is never counted down from zero.
        assert_eq!(delete_in_secs(&q, &stopped_on_index("aa", None)), None);
    }

    #[test]
    fn nothing_shows_a_countdown_when_no_rule_or_not_stopped() {
        let q = Quotas::new(QuotaPolicy::from(&QuotaSettings::default()));
        assert_eq!(delete_in_secs(&q, &stopped_on_index("aa", Some(5))), None, "no rule set");
        let settings = QuotaSettings { stopped_delete_after_days: Some(30), ..Default::default() };
        let q = Quotas::new(QuotaPolicy::from(&settings));
        let mut running = stopped_on_index("aa", Some(5));
        running.state = "running".into();
        assert_eq!(delete_in_secs(&q, &running), None);
    }

    #[test]
    fn a_zero_day_expiry_is_refused_so_a_stop_is_never_a_delete() {
        let s = QuotaSettings { stopped_delete_after_days: Some(0), ..Default::default() };
        assert!(s.validate().is_err());
    }

    #[test]
    fn a_stopped_expiry_in_the_file_becomes_the_policy() {
        let path = hosting_file(
            "gpp-index-stopped-expiry",
            "games = [\"x\"]\n[quota]\nstopped_delete_after_days = 14\n",
        );
        let config = Hosting::load(&path).unwrap();
        let p = QuotaPolicy::from(&config.quota);
        assert_eq!(p.stopped_expiry, Some(std::time::Duration::from_secs(14 * 86_400)));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A loopback node that lists four servers and records every removal.
    async fn fake_node() -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::extract::{Path, State};
        use axum::http::StatusCode;
        use axum::routing::{delete, get};
        use axum::{Json, Router};

        async fn list() -> Json<serde_json::Value> {
            let row = |id: &str, state: &str, stopped: Option<u64>, owner: &str| {
                serde_json::json!({
                    "instance_id": id, "game_id": "sven-coop", "name": id, "state": state,
                    "port": null, "ports": [], "owner": owner, "uptime_secs": 100,
                    "players_now": 0, "stopped_secs": stopped,
                })
            };
            let day = 86_400;
            Json(serde_json::json!([
                row("old", "stopped", Some(40 * day), &"aa".repeat(16)),
                row("fresh", "stopped", Some(day), &"aa".repeat(16)),
                row("kept", "stopped", Some(40 * day), &"bb".repeat(16)),
                row("up", "running", None, &"aa".repeat(16)),
            ]))
        }
        async fn remove(State(removed): State<Arc<std::sync::Mutex<Vec<String>>>>, Path(id): Path<String>) -> StatusCode {
            removed.lock().unwrap().push(id);
            StatusCode::NO_CONTENT
        }

        let removed: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let app = Router::new()
            .route("/instances", get(list))
            .route("/instances/:id", delete(remove))
            .with_state(removed.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, removed)
    }

    #[tokio::test]
    async fn an_expired_stopped_server_is_removed_and_nothing_else_is() {
        let (addr, removed) = fake_node().await;
        let mut c = config();
        c.nodes[0].api = format!("http://{addr}");
        c.quota.stopped_delete_after_days = Some(30);
        c.quota.accounts.insert(
            "bb".repeat(16),
            AccountSettings { never_reap: true, ..Default::default() },
        );
        let h = Hosting::new(c);

        // `old` is past 30 days, `fresh` is not, `kept` belongs to an account
        // that asked to keep its servers, and `up` is running.
        assert_eq!(h.expire_stopped().await.unwrap(), vec!["old".to_string()]);
        assert_eq!(*removed.lock().unwrap(), vec!["old".to_string()]);

        let listed = h.all_instances().await.unwrap();
        let fresh = listed.iter().find(|i| i.instance_id == "fresh").unwrap();
        assert_eq!(fresh.delete_in_secs, Some(29 * 86_400), "a countdown is shown to the owner");
        let kept = listed.iter().find(|i| i.instance_id == "kept").unwrap();
        assert_eq!(kept.delete_in_secs, None, "never_reap shows no countdown either");
    }

    #[tokio::test]
    async fn with_no_expiry_set_the_sweep_removes_nothing_and_asks_nothing() {
        let (addr, removed) = fake_node().await;
        let mut c = config();
        c.nodes[0].api = format!("http://{addr}");
        let h = Hosting::new(c);
        assert!(h.expire_stopped().await.unwrap().is_empty());
        assert!(removed.lock().unwrap().is_empty());
    }

    fn with_disk(mut cap: CapacityResp, free_gib: u64, reserve_gib: u64) -> CapacityResp {
        cap.disk_free_bytes = Some(free_gib << 30);
        cap.disk_reserve_bytes = Some(reserve_gib << 30);
        cap
    }

    /// A node under its own disk reserve is full, as one short of memory is;
    /// the node refuses at the same line, so picking it only fails later.
    #[test]
    fn a_node_under_its_disk_reserve_is_full_and_one_that_did_not_say_is_not() {
        assert_eq!(rank_node(Some(with_disk(cap(0, 8), 4, 5)), 0, 0), None);
        assert!(rank_node(Some(with_disk(cap(0, 8), 5, 5)), 0, 0).is_some(), "at the reserve is not under it");
        assert!(rank_node(Some(cap(0, 8)), 0, 0).is_some(), "an older agent reports no disk and is not full");
    }

    /// Priority and memory still decide first; disk breaks the tie, and a node
    /// that reported room outranks one that reported nothing.
    #[test]
    fn among_equals_more_disk_above_the_reserve_wins_and_priority_still_beats_it() {
        let more = rank_node(Some(with_disk(cap(0, 8), 60, 10)), 0, 0);
        let less = rank_node(Some(with_disk(cap(0, 8), 20, 10)), 0, 0);
        let silent_disk = rank_node(Some(cap(0, 8)), 0, 0);
        assert!(more > less);
        assert!(less > silent_disk);
        let higher_priority = rank_node(Some(with_disk(cap(0, 8), 11, 10)), 0, 1);
        assert!(higher_priority > more, "priority outranks disk");
    }

    fn classed(name: &str, class: Option<&str>) -> NodeConfig {
        NodeConfig {
            name: name.into(),
            api: "http://127.0.0.1:1".into(),
            agent: None,
            priority: 0,
            class: class.map(String::from),
            region: None,
        }
    }

    /// Neither node answers, so both rank as unanswered and the class filter is
    /// the only thing deciding who is eligible.
    #[tokio::test]
    async fn a_classed_node_takes_only_creates_naming_its_class() {
        let mut c = config();
        c.nodes = vec![classed("plain", None), classed("big", Some("gold"))];
        let h = Hosting::new(c);
        let config = h.config();
        let pick = |class: Option<&'static str>| {
            let h = &h;
            let config = &config;
            async move { h.pick_node(config, &[], class, None).await.map(|n| n.name.clone()) }
        };
        assert_eq!(pick(None).await.as_deref(), Some("plain"), "a plain create stays off the classed node");
        assert_eq!(pick(Some("gold")).await.as_deref(), Some("big"));
        assert_eq!(pick(Some("silver")).await, None, "no node of that class");
    }

    /// A loopback node that answers capacity with room to spare.
    async fn roomy_node() -> String {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/capacity",
            get(|| async {
                axum::Json(serde_json::json!({
                    "max_instances": 8, "running": 0, "port_range_start": 27100, "port_range_end": 27199,
                    "memory_available_bytes": 8u64 << 30,
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// A region is a wish: a node there that answered wins, a node there that
    /// did not answer still loses to every node that did (silence is never
    /// room), and a region nobody has places as if none was asked.
    #[tokio::test]
    async fn a_create_lands_in_its_region_when_a_node_there_answered() {
        let at = |name: &str, api: &str, region: &str| NodeConfig {
            region: Some(region.into()),
            api: api.into(),
            ..classed(name, None)
        };
        let (far, near) = (roomy_node().await, roomy_node().await);
        let mut c = config();
        c.nodes = vec![at("far", &far, "us"), at("near", &near, "eu")];
        // `far` first, so a tie (no region asked) goes to it.
        let h = Hosting::new(c);
        let config = h.config();
        let pick = |region: Option<&'static str>| {
            let (h, config) = (&h, &config);
            async move { h.pick_node(config, &[], None, region).await.map(|n| n.name.clone()) }
        };
        assert_eq!(pick(Some("eu")).await.as_deref(), Some("near"));
        assert_eq!(pick(Some("us")).await.as_deref(), Some("far"));
        assert_eq!(pick(Some("mars")).await.as_deref(), Some("far"), "a region nobody has changes nothing");
        assert_eq!(pick(None).await.as_deref(), Some("far"));

        let mut c = config.as_ref().clone();
        c.nodes[1].api = "http://127.0.0.1:1".into();
        let h = Hosting::new(c);
        let config = h.config();
        assert_eq!(
            h.pick_node(&config, &[], None, Some("eu")).await.map(|n| n.name.clone()).as_deref(),
            Some("far"),
            "a silent node in the region beat one that answered"
        );
    }

    #[tokio::test]
    async fn an_account_that_was_not_given_a_class_cannot_ask_for_it() {
        let mut c = config();
        c.nodes = vec![classed("big", Some("gold"))];
        let h = Hosting::new(c);
        let ask = |class: &str| DeployRequest {
            game_id: "sven-coop".into(),
            name: "x".into(),
            max_players: 8,
            map: None,
            class: Some(class.into()),
            region: None,
            bots: None,
            restart_on_crash: false,
            options: Default::default(),
        };
        for class in ["gold", "does-not-exist", "BAD CLASS"] {
            let err = h
                .deploy(&AccountId("aa".repeat(16)), &ask(class), std::time::SystemTime::now())
                .await
                .unwrap_err()
                .to_string();
            assert_eq!(err, "this account cannot place servers in that class", "{class}");
        }
    }

    #[test]
    fn class_labels_are_data_not_text_to_run() {
        for good in ["gold", "eu-1", "home_2", "a"] {
            assert!(valid_class(good), "{good}");
        }
        for bad in ["", "Gold", "a b", "a;b", "../x", &"x".repeat(33)] {
            assert!(!valid_class(bad), "{bad}");
        }
    }

    #[test]
    fn a_backup_cap_in_the_file_becomes_the_policy_and_a_zero_is_refused() {
        let hash = "b7f20b85976ca7b7b6c56a883992fb9a";
        let ok = hosting_file(
            "gpp-index-backupcap-ok",
            &format!("games = [\"x\"]\n[quota]\nmax_backups_per_account = 5\n[quota.accounts.{hash}]\nmax_backups = 12\n"),
        );
        let config = Hosting::load(&ok).unwrap();
        let q = Quotas::new(QuotaPolicy::from(&config.quota));
        assert_eq!(q.max_backups_for(&AccountId(hash.into())), Some(12));
        assert_eq!(q.max_backups_for(&AccountId("c".repeat(32))), Some(5));
        let _ = std::fs::remove_dir_all(ok.parent().unwrap());

        for (dir, body) in [
            ("gpp-index-backupcap-zero", "games = [\"x\"]\n[quota]\nmax_backups_per_account = 0\n".to_string()),
            ("gpp-index-backupcap-zero-account", format!("[quota.accounts.{hash}]\nmax_backups = 0\n")),
        ] {
            let bad = hosting_file(dir, &body);
            assert!(format!("{:#}", Hosting::load(&bad).unwrap_err()).contains("at least 1"), "{dir}");
            let _ = std::fs::remove_dir_all(bad.parent().unwrap());
        }
    }

    #[test]
    fn classes_in_the_file_reach_the_node_and_the_account_and_a_bad_one_is_refused() {
        let hash = "b7f20b85976ca7b7b6c56a883992fb9a";
        let ok = hosting_file(
            "gpp-index-classes-ok",
            &format!(
                "games = [\"x\"]\n[[nodes]]\nname = \"big\"\napi = \"http://127.0.0.1:1\"\nclass = \"gold\"\n[quota.accounts.{hash}]\nclasses = [\"gold\"]\n"
            ),
        );
        let config = Hosting::load(&ok).unwrap();
        assert_eq!(config.nodes[0].class.as_deref(), Some("gold"));
        let q = Quotas::new(QuotaPolicy::from(&config.quota));
        assert!(q.may_use_class(&AccountId(hash.into()), "gold"));
        let _ = std::fs::remove_dir_all(ok.parent().unwrap());

        let bad_node = hosting_file(
            "gpp-index-classes-bad-node",
            "games = [\"x\"]\n[[nodes]]\nname = \"big\"\napi = \"http://127.0.0.1:1\"\nclass = \"Gold Plated\"\n",
        );
        assert!(format!("{:#}", Hosting::load(&bad_node).unwrap_err()).contains("not a valid label"));
        let _ = std::fs::remove_dir_all(bad_node.parent().unwrap());

        let bad_account = hosting_file(
            "gpp-index-classes-bad-account",
            &format!("[quota.accounts.{hash}]\nclasses = [\"x y\"]\n"),
        );
        assert!(format!("{:#}", Hosting::load(&bad_account).unwrap_err()).contains("not a valid label"));
        let _ = std::fs::remove_dir_all(bad_account.parent().unwrap());
    }

    /// A loopback node with one server, owned by `aa`, that records each request
    /// it is asked for. Enough to see whether a call reached the node at all.
    async fn recording_node() -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::extract::{Path, Query, State};
        use axum::routing::{delete, get, post};
        use axum::{Json, Router};

        type Hits = Arc<std::sync::Mutex<Vec<String>>>;
        async fn list() -> Json<serde_json::Value> {
            Json(serde_json::json!([{
                "instance_id": "srv", "game_id": "sven-coop", "name": "srv", "state": "running",
                "port": null, "ports": [], "owner": "aa".repeat(16), "uptime_secs": 5,
                "players_now": 0, "stopped_secs": null,
            }]))
        }
        async fn logs(State(h): State<Hits>, Path(id): Path<String>, Query(_q): Query<serde_json::Value>) -> String {
            h.lock().unwrap().push(format!("logs {id}"));
            "a line\n".to_string()
        }
        async fn backups_list(State(h): State<Hits>, Path(id): Path<String>) -> Json<serde_json::Value> {
            h.lock().unwrap().push(format!("list {id}"));
            Json(serde_json::json!([]))
        }
        async fn backups_make(State(h): State<Hits>, Path(id): Path<String>) -> Json<serde_json::Value> {
            h.lock().unwrap().push(format!("make {id}"));
            Json(serde_json::json!({"name": "20261022-000000.tar.gz", "bytes": 1, "created_unix": 0}))
        }
        async fn restore(State(h): State<Hits>, Path((id, name)): Path<(String, String)>) -> &'static str {
            h.lock().unwrap().push(format!("restore {id} {name}"));
            "ok"
        }
        async fn remove(State(h): State<Hits>, Path((id, name)): Path<(String, String)>) -> &'static str {
            h.lock().unwrap().push(format!("delete {id} {name}"));
            "ok"
        }

        let hits: Hits = Default::default();
        let app = Router::new()
            .route("/instances", get(list))
            .route("/instances/:id/logs", get(logs))
            .route("/instances/:id/backups", get(backups_list).post(backups_make))
            .route("/instances/:id/backups/:name/restore", post(restore))
            .route("/instances/:id/backups/:name", delete(remove))
            .with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, hits)
    }

    fn one_node_at(addr: std::net::SocketAddr) -> HostingConfig {
        let mut c = config();
        c.nodes[0].api = format!("http://{addr}");
        c
    }

    /// A server's mesh address reaches the owner through the index as its node
    /// reported it, and a node that reports none leaves it absent: a controller
    /// page needs it to show the address and make an invite.
    #[tokio::test]
    async fn a_servers_mesh_address_comes_through_the_index_as_the_node_said() {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/instances",
            get(|| async {
                axum::Json(serde_json::json!([
                    {"instance_id": "up", "game_id": "sven-coop", "name": "up", "state": "running", "port": 27100,
                     "ports": [], "owner": "aa".repeat(16), "mesh_destination": "e322bda3b175b989de75f399a6abfba1"},
                    {"instance_id": "down", "game_id": "sven-coop", "name": "down", "state": "stopped", "port": 27101,
                     "ports": [], "owner": "aa".repeat(16)},
                ]))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let rows = Hosting::new(one_node_at(addr)).all_instances().await.unwrap();
        let by = |id: &str| rows.iter().find(|r| r.instance_id == id).unwrap().mesh_destination.clone();
        assert_eq!(by("up").as_deref(), Some("e322bda3b175b989de75f399a6abfba1"));
        assert_eq!(by("down"), None);
    }

    /// A loopback worker holding `servers` as `(id, owner, backups)`, recording
    /// what it is asked.
    async fn worker_with(servers: Vec<(&'static str, String, usize)>) -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::extract::{Path, State};
        use axum::routing::get;
        use axum::{Json, Router};

        type Held = Arc<(Vec<(&'static str, String, usize)>, std::sync::Mutex<Vec<String>>)>;
        async fn list(State(h): State<Held>) -> Json<serde_json::Value> {
            Json(serde_json::Value::Array(
                h.0.iter()
                    .map(|(id, owner, _)| {
                        serde_json::json!({
                            "instance_id": id, "game_id": "sven-coop", "name": id, "state": "running",
                            "port": null, "ports": [], "owner": owner, "uptime_secs": 5,
                            "players_now": 0, "stopped_secs": null,
                        })
                    })
                    .collect(),
            ))
        }
        async fn backups(State(h): State<Held>, Path(id): Path<String>) -> Json<serde_json::Value> {
            h.1.lock().unwrap().push(format!("list {id}"));
            let n = h.0.iter().find(|(i, ..)| *i == id).map_or(0, |(.., n)| *n);
            Json(serde_json::Value::Array(
                (0..n).map(|k| serde_json::json!({"name": format!("20261022-0000{k:02}.tar.gz"), "bytes": 1, "created_unix": 0})).collect(),
            ))
        }
        async fn make(State(h): State<Held>, Path(id): Path<String>) -> Json<serde_json::Value> {
            h.1.lock().unwrap().push(format!("make {id}"));
            Json(serde_json::json!({"name": "20261022-000099.tar.gz", "bytes": 1, "created_unix": 0}))
        }
        let state: Held = Arc::new((servers, Default::default()));
        let app = Router::new()
            .route("/instances", get(list))
            .route("/instances/:id/backups", get(backups).post(make))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        // The recorded hits live in the shared state; hand out a view of them.
        let hits: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let (view, source) = (hits.clone(), state.clone());
        tokio::spawn(async move {
            loop {
                *view.lock().unwrap() = source.1.lock().unwrap().clone();
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
        (addr, hits)
    }

    /// A worker whose servers and backups can be removed, recording the order
    /// it was asked in.
    async fn erasable_worker(
        servers: Vec<(&'static str, String, Vec<&'static str>)>,
    ) -> (std::net::SocketAddr, Arc<std::sync::Mutex<(Vec<(&'static str, String, Vec<&'static str>)>, Vec<String>)>>) {
        use axum::extract::{Path, State};
        use axum::routing::{delete, get};
        use axum::{Json, Router};

        type Held = Arc<std::sync::Mutex<(Vec<(&'static str, String, Vec<&'static str>)>, Vec<String>)>>;
        async fn list(State(h): State<Held>) -> Json<serde_json::Value> {
            let held = h.lock().unwrap();
            Json(serde_json::Value::Array(
                held.0
                    .iter()
                    .map(|(id, owner, _)| {
                        serde_json::json!({
                            "instance_id": id, "game_id": "sven-coop", "name": id, "state": "stopped",
                            "port": null, "ports": [], "owner": owner,
                        })
                    })
                    .collect(),
            ))
        }
        async fn backups(State(h): State<Held>, Path(id): Path<String>) -> Json<serde_json::Value> {
            let held = h.lock().unwrap();
            let names = held.0.iter().find(|(i, ..)| *i == id).map(|(.., b)| b.clone()).unwrap_or_default();
            Json(serde_json::Value::Array(
                names.iter().map(|n| serde_json::json!({"name": n, "bytes": 1, "created_unix": 0})).collect(),
            ))
        }
        async fn remove_backup(State(h): State<Held>, Path((id, name)): Path<(String, String)>) -> &'static str {
            let mut held = h.lock().unwrap();
            if let Some(s) = held.0.iter_mut().find(|(i, ..)| *i == id) {
                s.2.retain(|b| *b != name);
            }
            held.1.push(format!("delete {id} {name}"));
            "ok"
        }
        async fn remove(State(h): State<Held>, Path(id): Path<String>) -> &'static str {
            let mut held = h.lock().unwrap();
            held.0.retain(|(i, ..)| *i != id);
            held.1.push(format!("remove {id}"));
            "ok"
        }
        let state: Held = Arc::new(std::sync::Mutex::new((servers, Vec::new())));
        let app = Router::new()
            .route("/instances", get(list))
            .route("/instances/:id", delete(remove))
            .route("/instances/:id/backups", get(backups))
            .route("/instances/:id/backups/:name", delete(remove_backup))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, state)
    }

    /// An account's servers are erased with their backups first, and never a
    /// stranger's. With a worker that cannot be asked it is refused and nothing
    /// is touched: an erase that skipped a worker would read as complete and not
    /// be.
    #[tokio::test]
    async fn an_account_erases_its_servers_and_backups_and_nobody_elses() {
        let me = AccountId("aa".repeat(16));
        let stranger = "bb".repeat(16);
        let (addr, held) = erasable_worker(vec![
            ("mine-1", me.0.clone(), vec!["20261022-000001.tar.gz", "20261022-000002.tar.gz"]),
            ("mine-2", me.0.clone(), vec![]),
            ("theirs", stranger.clone(), vec!["20261022-000003.tar.gz"]),
        ])
        .await;

        let mut silent = one_node_at(addr);
        let mut down = silent.nodes[0].clone();
        down.name = "down".into();
        down.api = "http://127.0.0.1:9".into();
        silent.nodes.push(down);
        assert!(Hosting::new(silent).erase_account(&me).await.is_err());
        assert!(held.lock().unwrap().1.is_empty(), "something was removed with a worker silent");

        let h = Hosting::new(one_node_at(addr));
        let erased = h.erase_account(&me).await.unwrap();
        assert_eq!((erased.backups_deleted, erased.servers_removed.len()), (2, 2));
        let asked = held.lock().unwrap().1.clone();
        let removed_at = asked.iter().position(|a| a == "remove mine-1").unwrap();
        assert!(asked[..removed_at].iter().filter(|a| a.starts_with("delete mine-1")).count() == 2, "{asked:?}");
        assert!(!asked.iter().any(|a| a.contains("theirs")), "a stranger's server was touched: {asked:?}");
        assert_eq!(held.lock().unwrap().0.len(), 1);
    }

    /// The cap counts an account's backups across **all** its servers on all
    /// its workers, and never anybody else's. At the cap a backup is refused
    /// before any worker is asked to make one; under it, it is made; with no
    /// cap the index does not even count. A worker that cannot be asked makes
    /// the count unknown, and unknown is refused, never zero.
    #[tokio::test]
    async fn a_backup_is_refused_at_the_accounts_cap_across_its_workers_and_never_counts_a_strangers() {
        let me = "aa".repeat(16);
        let stranger = "bb".repeat(16);
        let (a, _) = worker_with(vec![("one", me.clone(), 2), ("theirs", stranger.clone(), 9)]).await;
        let (b, hits_b) = worker_with(vec![("two", me.clone(), 1)]).await;
        let settle = || tokio::time::sleep(std::time::Duration::from_millis(40));
        let mine = AccountId(me.clone());

        // No cap written: nothing is counted, the backup is made.
        let h = Hosting::new(two_nodes_at(a, b));
        h.backup_create(&mine, "two").await.expect("no cap, no count");
        settle().await;
        assert_eq!(*hits_b.lock().unwrap(), vec!["make two".to_string()], "an uncapped create counted anyway");

        // Three held (2 + 1), cap 3: refused, and nothing was made.
        let capped = |cap: u32, nodes: HostingConfig| {
            let mut c = nodes;
            c.quota.max_backups_per_account = Some(cap);
            Hosting::new(c)
        };
        let (a, hits_a2) = worker_with(vec![("one", me.clone(), 2), ("theirs", stranger.clone(), 9)]).await;
        let (b, hits_b2) = worker_with(vec![("two", me.clone(), 1)]).await;
        let h = capped(3, two_nodes_at(a, b));
        let err = h.backup_create(&mine, "one").await.unwrap_err().to_string();
        assert_eq!(err, "you already hold 3 of your 3 backups; delete one before making another");
        settle().await;
        assert!(!hits_a2.lock().unwrap().iter().any(|x| x.starts_with("make")), "a refused backup was made");
        assert!(!hits_b2.lock().unwrap().iter().any(|x| x.starts_with("make")));
        assert!(
            !hits_a2.lock().unwrap().iter().any(|x| x.contains("theirs")),
            "a stranger's backups were counted: {:?}",
            hits_a2.lock().unwrap()
        );

        // Cap 4: the same account may make one more, on the right worker.
        let (a, hits_a3) = worker_with(vec![("one", me.clone(), 2), ("theirs", stranger.clone(), 9)]).await;
        let (b, _) = worker_with(vec![("two", me.clone(), 1)]).await;
        let h = capped(4, two_nodes_at(a, b));
        h.backup_create(&mine, "one").await.expect("under the cap");
        settle().await;
        assert!(hits_a3.lock().unwrap().contains(&"make one".to_string()));

        // A worker that is down: the count is unknown, so it is refused.
        let (a, _) = worker_with(vec![("one", me.clone(), 0)]).await;
        let dead: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
        let h = capped(5, two_nodes_at(a, dead));
        let err = format!("{:#}", h.backup_create(&mine, "one").await.unwrap_err());
        assert!(err.contains("could not ask b for your backups"), "an unreachable worker was counted as none: {err}");
    }

    /// Someone else's server is "no such instance" for logs and backups too, and
    /// the node is never asked. Otherwise the answer would tell them it exists.
    #[tokio::test]
    async fn another_accounts_server_logs_and_backups_are_not_found_and_never_asked_for() {
        let (addr, hits) = recording_node().await;
        let h = Hosting::new(one_node_at(addr));
        let stranger = AccountId("bb".repeat(16));
        assert_eq!(h.logs(&stranger, "srv", 10).await.unwrap_err().to_string(), "no such instance");
        assert_eq!(h.backups(&stranger, "srv").await.unwrap_err().to_string(), "no such instance");
        assert_eq!(h.backup_create(&stranger, "srv").await.unwrap_err().to_string(), "no such instance");
        assert_eq!(
            h.backup_delete(&stranger, "srv", "20261022-000000.tar.gz").await.unwrap_err().to_string(),
            "no such instance"
        );
        assert!(hits.lock().unwrap().is_empty(), "the node was asked: {:?}", hits.lock().unwrap());
    }

    #[tokio::test]
    async fn the_owner_reaches_their_server_on_the_node_that_holds_it() {
        let (addr, hits) = recording_node().await;
        let h = Hosting::new(one_node_at(addr));
        let owner = AccountId("aa".repeat(16));
        assert_eq!(h.logs(&owner, "srv", 10).await.unwrap().text, "a line\n");
        h.backup_create(&owner, "srv").await.unwrap();
        h.backup_restore(&owner, "srv", "20261022-000000.tar.gz").await.unwrap();
        assert_eq!(
            *hits.lock().unwrap(),
            vec!["logs srv", "make srv", "restore srv 20261022-000000.tar.gz"]
        );
    }

    /// A backup name becomes part of a node URL on the loopback path, so it is
    /// refused before any request, the same as a name the node would refuse.
    #[tokio::test]
    async fn a_backup_name_the_platform_did_not_make_never_reaches_a_node() {
        let (addr, hits) = recording_node().await;
        let h = Hosting::new(one_node_at(addr));
        let owner = AccountId("aa".repeat(16));
        for name in ["../../instances", "20261022-000000.tar.gz/../x", "x.tar.gz", ""] {
            let err = h.backup_delete(&owner, "srv", name).await.unwrap_err().to_string();
            assert!(err.contains("not a backup name"), "{name}: {err}");
        }
        assert!(hits.lock().unwrap().is_empty());
    }

    /// A loopback node that keeps its servers in memory and records what it was
    /// asked, in order, for the move tests. `holds_srv` starts it with one
    /// running server, `srv`, owned by `aa`; `fail_restore` makes every restore
    /// fail, as a node refusing a backup would.
    async fn moving_node(holds_srv: bool, fail_restore: bool) -> (std::net::SocketAddr, Arc<std::sync::Mutex<FakeNode>>) {
        use axum::body::Bytes;
        use axum::extract::{Path, State};
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use axum::{Json, Router};

        type Node = Arc<std::sync::Mutex<FakeNode>>;
        fn row(id: &str, state: &str, owner: &str) -> serde_json::Value {
            serde_json::json!({
                "instance_id": id, "game_id": "sven-coop", "name": "Moving", "state": state,
                "port": null, "ports": [], "owner": owner, "uptime_secs": 5, "players_now": 0,
            })
        }
        async fn list(State(n): State<Node>) -> Json<serde_json::Value> {
            Json(serde_json::Value::Array(n.lock().unwrap().servers.clone()))
        }
        async fn capacity() -> Json<serde_json::Value> {
            Json(serde_json::json!({"max_instances": 10, "running": 0, "port_range_start": 1, "port_range_end": 2}))
        }
        async fn create(State(n): State<Node>, Json(spec): Json<serde_json::Value>) -> Json<serde_json::Value> {
            let mut n = n.lock().unwrap();
            let id = spec["instance_id"].as_str().unwrap().to_string();
            n.hits.push(format!("create {} port={} owner={}", spec["name"].as_str().unwrap(), spec["port"], spec["owner"].as_str().unwrap()));
            n.servers.push(row(&id, "running", spec["owner"].as_str().unwrap()));
            Json(serde_json::json!({"state": "running", "port": 27015}))
        }
        fn set_state(n: &Node, id: &str, state: &str) {
            let mut n = n.lock().unwrap();
            n.hits.push(format!("{state} {id}"));
            for r in n.servers.iter_mut().filter(|r| r["instance_id"] == id) {
                r["state"] = state.into();
            }
        }
        async fn stop(State(n): State<Node>, Path(id): Path<String>) -> &'static str {
            set_state(&n, &id, "stopped");
            "ok"
        }
        async fn start(State(n): State<Node>, Path(id): Path<String>) -> &'static str {
            set_state(&n, &id, "running");
            "ok"
        }
        async fn remove(State(n): State<Node>, Path(id): Path<String>) -> &'static str {
            let mut n = n.lock().unwrap();
            n.hits.push(format!("remove {id}"));
            n.servers.retain(|r| r["instance_id"] != id.as_str());
            "ok"
        }
        async fn spec(State(n): State<Node>, Path(id): Path<String>) -> Json<serde_json::Value> {
            n.lock().unwrap().hits.push(format!("spec {id}"));
            Json(serde_json::json!({
                "instance_id": id, "game_id": "sven-coop", "name": "Moving", "max_players": 12,
                "port": 27015, "extra_ports": {"1": 27016}, "map": "svencoop1",
                "restart_on_crash": true, "owner": "aa".repeat(16),
            }))
        }
        async fn make(State(n): State<Node>, Path(id): Path<String>) -> Json<serde_json::Value> {
            let mut n = n.lock().unwrap();
            n.hits.push(format!("backup {id}"));
            n.made += 1;
            Json(serde_json::json!({"name": "20261022-000000.tar.gz", "bytes": 18, "created_unix": 0, "started_unix": 1000 * n.made}))
        }
        async fn delta(State(n): State<Node>, Path(id): Path<String>, Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
            let mut n = n.lock().unwrap();
            n.hits.push(format!("delta {id} since={}", body["since"]));
            n.made += 1;
            Json(serde_json::json!({"name": "20261022-000001.tar.gz", "bytes": 18, "created_unix": 0, "started_unix": 1000 * n.made}))
        }
        async fn merge(State(n): State<Node>, Path((id, name)): Path<(String, String)>) -> Json<serde_json::Value> {
            n.lock().unwrap().hits.push(format!("merge {id} {name}"));
            Json(serde_json::json!({}))
        }
        async fn drop_backup(State(n): State<Node>, Path((id, _name)): Path<(String, String)>) -> Json<serde_json::Value> {
            n.lock().unwrap().hits.push(format!("backup-delete {id}"));
            Json(serde_json::json!({}))
        }
        async fn read_identity(State(n): State<Node>, Path(id): Path<String>) -> Json<serde_json::Value> {
            n.lock().unwrap().hits.push(format!("identity {id}"));
            Json(serde_json::json!({"identity": "ab".repeat(64)}))
        }
        async fn set_identity(State(n): State<Node>, Path(id): Path<String>, Json(body): Json<serde_json::Value>) -> (StatusCode, Json<serde_json::Value>) {
            let mut n = n.lock().unwrap();
            n.hits.push(format!("identity-set {id}"));
            if n.fail_identity {
                return (StatusCode::CONFLICT, Json(serde_json::json!({"error": "that identity does not fit here"})));
            }
            n.identity = body["identity"].as_str().unwrap_or_default().to_string();
            (StatusCode::OK, Json(serde_json::json!({})))
        }
        async fn file(State(n): State<Node>, Path((id, _name)): Path<(String, String)>) -> &'static [u8] {
            n.lock().unwrap().hits.push(format!("file {id}"));
            b"the server's files"
        }
        async fn import(State(n): State<Node>, Path(id): Path<String>, body: Bytes) -> Json<serde_json::Value> {
            let mut n = n.lock().unwrap();
            n.hits.push(format!("import {id}"));
            n.imported = body.to_vec();
            Json(serde_json::json!({"name": "20261023-000000.tar.gz", "bytes": body.len(), "created_unix": 0}))
        }
        async fn restore(State(n): State<Node>, Path((id, name)): Path<(String, String)>) -> (StatusCode, Json<serde_json::Value>) {
            let mut n = n.lock().unwrap();
            n.hits.push(format!("restore {id} {name}"));
            if n.fail_restore {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "that backup does not restore here"})));
            }
            (StatusCode::OK, Json(serde_json::json!({})))
        }

        let node: Node = Arc::new(std::sync::Mutex::new(FakeNode {
            servers: if holds_srv { vec![row("srv", "running", &"aa".repeat(16))] } else { Vec::new() },
            fail_restore,
            ..Default::default()
        }));
        let app = Router::new()
            .route("/instances", get(list).post(create))
            .route("/capacity", get(capacity))
            .route("/instances/:id", axum::routing::delete(remove))
            .route("/instances/:id/stop", post(stop))
            .route("/instances/:id/start", post(start))
            .route("/instances/:id/spec", get(spec))
            .route("/instances/:id/backups", post(make))
            .route("/instances/:id/delta", post(delta))
            .route("/instances/:id/backups/:name", axum::routing::delete(drop_backup))
            .route("/instances/:id/backups/:name/merge", post(merge))
            .route("/instances/:id/identity", get(read_identity).put(set_identity))
            .route("/instances/:id/backups/import", post(import))
            .route("/instances/:id/backups/:name/file", get(file))
            .route("/instances/:id/backups/:name/restore", post(restore))
            .with_state(node.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, node)
    }

    /// A game's own choices (a Minecraft build, its heap, Bedrock's settings,
    /// the EULA tick, bots, crash restart) used to stop at the index: the create it sent a node
    /// carried none of them, so a hosted Minecraft server was refused for its
    /// missing EULA. They reach the node, and the EULA time is the index's own
    /// clock, never what the caller wrote.
    #[tokio::test]
    async fn a_games_own_choices_reach_the_node_and_the_eula_time_is_the_indexs() {
        use axum::extract::State;
        use axum::routing::post;
        use axum::{Json, Router};

        type Seen = Arc<std::sync::Mutex<Vec<serde_json::Value>>>;
        async fn create(State(seen): State<Seen>, Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
            seen.lock().unwrap().push(body);
            Json(serde_json::json!({ "state": "running", "port": 27100, "ports": [] }))
        }
        async fn none() -> Json<serde_json::Value> {
            Json(serde_json::json!([]))
        }
        let seen: Seen = Default::default();
        let app = Router::new()
            .route("/instances", post(create).get(none))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut c = config();
        c.nodes[0].api = format!("http://{addr}");
        let h = Hosting::new(c);
        let request: DeployRequest = serde_json::from_value(serde_json::json!({
            "game_id": "sven-coop",
            "name": "mine",
            "content_version": "paper-26.3",
            "memory": { "min_mb": 1024, "max_mb": 2048 },
            "eula_accepted_at": 1,
            "minecraft": { "pvp": false },
            "bots": 4,
            "restart_on_crash": true,
        }))
        .unwrap();
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
        h.deploy(&AccountId("aa".repeat(16)), &request, now).await.unwrap();

        let body = seen.lock().unwrap()[0].clone();
        assert_eq!(body["content_version"], "paper-26.3");
        assert_eq!(body["memory"]["max_mb"], 2048);
        assert_eq!(body["minecraft"]["pvp"], false);
        assert_eq!(body["eula_accepted_at"], 1_800_000_000u64, "the caller's own time was kept");
        assert_eq!(body["owner"], "aa".repeat(16));
        assert_eq!(body["bots"], 4, "the bots asked for were dropped");
        assert_eq!(body["restart_on_crash"], true, "the crash restart asked for was dropped");

        // No tick, no stamp: the node refuses a pack that needs one.
        let plain: DeployRequest =
            serde_json::from_value(serde_json::json!({ "game_id": "sven-coop", "name": "plain" })).unwrap();
        h.deploy(&AccountId("aa".repeat(16)), &plain, now).await.unwrap();
        let bad: DeployRequest =
            serde_json::from_value(serde_json::json!({ "game_id": "sven-coop", "name": "x", "region": "Mars Base" })).unwrap();
        let err = h.deploy(&AccountId("aa".repeat(16)), &bad, now).await.unwrap_err().to_string();
        assert_eq!(err, "that is not a region name");
        let nowhere: DeployRequest =
            serde_json::from_value(serde_json::json!({ "game_id": "sven-coop", "name": "y", "region": "mars" })).unwrap();
        h.deploy(&AccountId("aa".repeat(16)), &nowhere, now).await.expect("a region nobody has still places");

        let seen = seen.lock().unwrap();
        assert!(seen.len() == 3 && seen[1].get("eula_accepted_at").is_none());
        assert!(seen[1]["bots"].is_null() && seen[1]["restart_on_crash"] == false, "nothing invented");
    }

    #[derive(Default)]
    struct FakeNode {
        servers: Vec<serde_json::Value>,
        hits: Vec<String>,
        imported: Vec<u8>,
        fail_restore: bool,
        made: i64,
        identity: String,
        fail_identity: bool,
    }

    fn two_nodes_at(a: std::net::SocketAddr, b: std::net::SocketAddr) -> HostingConfig {
        let mut c = config();
        c.nodes[0].name = "a".into();
        c.nodes[0].api = format!("http://{a}");
        let mut second = c.nodes[0].clone();
        second.name = "b".into();
        second.api = format!("http://{b}");
        c.nodes.push(second);
        c
    }

    /// A move makes the same server on the other node with the old one's files,
    /// starts it because the old one was running, and only then removes the
    /// old one. The new node picks the ports; the rest of the spec travels.
    #[tokio::test]
    async fn a_server_moves_with_its_files_and_the_old_one_goes_last() {
        let (a, from) = moving_node(true, false).await;
        let (b, to) = moving_node(false, false).await;
        let h = Hosting::new(two_nodes_at(a, b));
        let owner = AccountId("aa".repeat(16));

        let stranger = AccountId("bb".repeat(16));
        assert_eq!(h.relocate(&stranger, "srv").await.unwrap_err().to_string(), "no such instance");
        assert!(from.lock().unwrap().hits.is_empty(), "a stranger's move reached the node");

        let moved = h.relocate(&owner, "srv").await.unwrap();
        assert_eq!(moved.node, "b");
        assert_ne!(moved.instance_id, "srv");
        assert_eq!(moved.state, "running");
        assert_eq!(moved.port, Some(27015), "the reply should name the port the new node picked");
        let new = &moved.instance_id;
        // The old server is stopped only after a full copy and a small live
        // delta, then one last delta; its identity is read after that.
        assert_eq!(
            from.lock().unwrap().hits,
            vec![
                "spec srv", "backup srv", "file srv", "backup-delete srv",
                "delta srv since=1000", "file srv", "backup-delete srv",
                "stopped srv",
                "delta srv since=2000", "file srv", "backup-delete srv",
                "identity srv", "remove srv",
            ],
        );
        let merged = "20261023-000000.tar.gz";
        assert_eq!(
            to.lock().unwrap().hits,
            vec![
                format!("create Moving port=null owner={}", "aa".repeat(16)),
                format!("stopped {new}"),
                format!("import {new}"),
                format!("restore {new} {merged}"),
                format!("backup-delete {new}"),
                format!("import {new}"),
                format!("merge {new} {merged}"),
                format!("import {new}"),
                format!("merge {new} {merged}"),
                format!("identity-set {new}"),
                format!("running {new}"),
            ],
        );
        assert_eq!(to.lock().unwrap().imported, b"the server's files");
        assert_eq!(to.lock().unwrap().identity, "ab".repeat(64), "the address did not travel");
        assert!(from.lock().unwrap().servers.is_empty());
    }

    /// A move that fails on the new node removes what it made there. The old
    /// server was never removed, and never even stopped.
    #[tokio::test]
    async fn a_move_that_fails_while_copying_never_stops_the_old_server() {
        let (a, from) = moving_node(true, false).await;
        let (b, to) = moving_node(false, true).await;
        let h = Hosting::new(two_nodes_at(a, b));
        let err = h.relocate(&AccountId("aa".repeat(16)), "srv").await.unwrap_err();
        assert!(format!("{err:#}").contains("does not restore here"), "{err:#}");
        let from = from.lock().unwrap();
        // The first copy failed to land, so the old server was never stopped.
        assert!(!from.hits.iter().any(|h| h.starts_with("stopped")), "the old server was stopped: {:?}", from.hits);
        assert!(!from.hits.iter().any(|h| h.starts_with("remove")), "the old server was removed: {:?}", from.hits);
        let to = to.lock().unwrap();
        assert!(to.hits.last().is_some_and(|h| h.starts_with("remove ")), "{:?}", to.hits);
        assert!(to.servers.is_empty());
    }

    /// A failure after the old server is stopped removes the new copy and
    /// starts the old one again; it was never removed.
    #[tokio::test]
    async fn a_move_that_fails_after_the_stop_starts_the_old_server_again() {
        let (a, from) = moving_node(true, false).await;
        let (b, to) = moving_node(false, false).await;
        to.lock().unwrap().fail_identity = true;
        let h = Hosting::new(two_nodes_at(a, b));
        let err = h.relocate(&AccountId("aa".repeat(16)), "srv").await.unwrap_err();
        assert!(format!("{err:#}").contains("does not fit here"), "{err:#}");
        let from = from.lock().unwrap();
        assert!(from.hits.iter().any(|h| h == "stopped srv"), "{:?}", from.hits);
        assert_eq!(from.hits.last().map(String::as_str), Some("running srv"), "{:?}", from.hits);
        assert!(!from.hits.iter().any(|h| h.starts_with("remove")), "the old server was removed: {:?}", from.hits);
        let to = to.lock().unwrap();
        assert!(to.hits.last().is_some_and(|h| h.starts_with("remove ")), "{:?}", to.hits);
        assert!(to.servers.is_empty());
    }

    fn nodes_in(nodes: &[(std::net::SocketAddr, &str, &str)]) -> HostingConfig {
        let mut c = config();
        c.nodes = nodes
            .iter()
            .map(|(addr, name, region)| NodeConfig {
                region: Some(region.to_string()),
                api: format!("http://{addr}"),
                ..classed(name, None)
            })
            .collect();
        c
    }

    /// The owner picks the worker, from those in the server's own region. A
    /// worker in another region, the one it is on, or one that does not exist
    /// is refused before anything is touched; the automatic choice never
    /// leaves the region either.
    #[tokio::test]
    async fn a_move_goes_to_the_chosen_worker_and_never_leaves_its_region_unasked() {
        let owner = AccountId("aa".repeat(16));
        let (a, from) = moving_node(true, false).await;
        let (b, to) = moving_node(false, false).await;
        let (c, other) = moving_node(false, false).await;
        let h = Hosting::new(nodes_in(&[(a, "a", "il"), (b, "b", "il"), (c, "c", "eu")]));

        let targets = h.move_targets(&owner, "srv").await.unwrap();
        assert_eq!((targets.worker.as_str(), targets.region.as_deref()), ("a", Some("il")));
        assert_eq!(targets.workers.iter().map(|w| (w.name.as_str(), w.room.as_str())).collect::<Vec<_>>(), vec![("b", "room")]);
        assert_eq!(targets.regions, vec!["eu".to_string()]);
        assert!(h.move_targets(&AccountId("bb".repeat(16)), "srv").await.is_err(), "a stranger sees nothing");

        for (worker, why) in [
            ("c", "that worker is in another region; ask an admin to move it there"),
            ("a", "it is already on that worker"),
            ("nope", "no such worker"),
        ] {
            let err = h.relocate_to(&owner, "srv", MoveTarget::Worker(worker)).await.unwrap_err().to_string();
            assert_eq!(err, why, "{worker}");
        }
        assert!(from.lock().unwrap().hits.is_empty(), "a refused move touched the server: {:?}", from.lock().unwrap().hits);

        let moved = h.relocate_to(&owner, "srv", MoveTarget::Worker("b")).await.unwrap();
        assert_eq!(moved.node, "b");
        assert!(!to.lock().unwrap().hits.is_empty());
        assert!(other.lock().unwrap().hits.is_empty(), "the other region's worker was used");

        // Alone in its region, the automatic move goes nowhere.
        let (a, from) = moving_node(true, false).await;
        let (c, _) = moving_node(false, false).await;
        let h = Hosting::new(nodes_in(&[(a, "a", "il"), (c, "c", "eu")]));
        let err = h.relocate(&owner, "srv").await.unwrap_err().to_string();
        assert!(err.contains("ask an admin"), "{err}");
        assert!(from.lock().unwrap().hits.is_empty());
    }

    /// Another region is asked for and decided by an admin. The owner cannot
    /// decide their own request; an approval moves the server there and makes
    /// that its home, so going back is a new request; a denial moves nothing.
    #[tokio::test]
    async fn a_region_move_needs_an_admin_and_its_new_region_becomes_home() {
        let owner = AccountId("aa".repeat(16));
        let admin = AccountId("ad".repeat(16));
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
        let (a, from) = moving_node(true, false).await;
        let (c, to) = moving_node(false, false).await;
        let mut cfg = nodes_in(&[(a, "a", "il"), (c, "c", "eu")]);
        cfg.admins = vec![admin.0.clone()];
        let h = Hosting::new(cfg);

        assert_eq!(h.request_region(&owner, "srv", "il", now).await.unwrap_err().to_string(), "this server is already in region il");
        assert_eq!(h.request_region(&owner, "srv", "mars", now).await.unwrap_err().to_string(), "no worker in region mars takes this server");
        assert_eq!(h.request_region(&owner, "srv", "EU West", now).await.unwrap_err().to_string(), "that is not a region name");
        let asked = h.request_region(&owner, "srv", "eu", now).await.unwrap();
        assert_eq!((asked.from_region.as_deref(), asked.to_region.as_str()), (Some("il"), "eu"));
        assert!(h.request_region(&owner, "srv", "eu", now).await.is_err(), "a second pending request");
        assert_eq!(h.region_requests_for(&owner).len(), 1);

        let refused = h.approve_region_request(&owner, &asked.id, now).await.unwrap_err().to_string();
        assert_eq!(refused, "only an admin of this index can decide region requests");
        assert!(h.all_region_requests(&owner).is_err());
        assert!(from.lock().unwrap().hits.is_empty(), "a refused approval touched the server");

        let approved = h.approve_region_request(&admin, &asked.id, now).await.unwrap();
        assert_eq!(approved.state, crate::regions::RequestState::Approved);
        assert_eq!(approved.decided_by.as_deref(), Some(admin.0.as_str()));
        let new_id = approved.moved_to.clone().unwrap();
        assert!(!to.lock().unwrap().hits.is_empty());
        let mine = h.instances_for(&owner).await.unwrap();
        let moved = mine.iter().find(|i| i.instance_id == new_id).expect("the moved server");
        assert_eq!((moved.node.as_str(), moved.region.as_deref()), ("c", Some("eu")), "its new region is its home");
        assert!(h.approve_region_request(&admin, &asked.id, now).await.is_err(), "approved twice");

        // Going back is a request again, and a denial moves nothing.
        let back = h.request_region(&owner, &new_id, "il", now).await.unwrap();
        let hits_before = to.lock().unwrap().hits.len();
        let denied = h.deny_region_request(&admin, &back.id, Some("not this month".into()), now).unwrap();
        assert_eq!(denied.state, crate::regions::RequestState::Denied);
        assert_eq!(to.lock().unwrap().hits.len(), hits_before, "a denial touched the server");
    }

    /// Becoming an admin of the index: anyone may ask, only an admin decides; an
    /// approval makes them one at once; a file admin is the file's and the last
    /// admin stays; the workers view is an admin's.
    #[tokio::test]
    async fn an_account_becomes_an_index_admin_only_by_an_admin() {
        let root = AccountId("ad".repeat(16));
        let asker = AccountId("aa".repeat(16));
        let stranger = AccountId("bb".repeat(16));
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
        let mut cfg = config();
        cfg.admins = vec![root.0.clone()];
        let h = Hosting::new(cfg);

        assert!(h.request_admin(&root, None, now).is_err(), "an admin has nothing to ask for");
        let asked = h.request_admin(&asker, Some("I run the eu workers".into()), now).unwrap();
        assert!(h.request_admin(&asker, None, now).is_err(), "one pending request");
        assert_eq!(h.admin_status(&asker), (false, Some(asked.clone())));
        assert!(h.approve_admin_request(&stranger, &asked.id, now).is_err(), "only an admin decides");
        assert!(h.admin_requests(&asker).is_err());
        assert!(h.workers(&asker).await.is_err(), "the workers view is an admin's");

        h.approve_admin_request(&root, &asked.id, now).unwrap();
        assert!(h.is_admin(&asker), "an approval makes them one at once");
        assert!(h.workers(&asker).await.is_ok());
        let admins = h.admins(&asker).unwrap();
        assert_eq!(admins.iter().map(|a| a.source.as_str()).collect::<Vec<_>>(), vec!["file", "granted"]);

        assert_eq!(
            h.revoke_admin(&asker, &root.0).unwrap_err().to_string(),
            "that admin is written in the hosting file; remove it there"
        );
        h.revoke_admin(&root, &asker.0).unwrap();
        assert!(!h.is_admin(&asker));

        // Granted admins only: the last one stays.
        let other = AccountId("cc".repeat(16));
        let h = Hosting::new(config());
        let r = h.request_admin(&other, None, now).unwrap();
        assert!(h.approve_admin_request(&other, &r.id, now).is_err(), "no admin at all yet: nobody can grant");
    }

    /// With nowhere to go, nothing is touched: the server is not even stopped.
    #[tokio::test]
    async fn a_move_with_no_other_node_touches_nothing() {
        let (a, from) = moving_node(true, false).await;
        let h = Hosting::new(one_node_at(a));
        let err = h.relocate(&AccountId("aa".repeat(16)), "srv").await.unwrap_err();
        assert_eq!(err.to_string(), "no other worker in this server's region has room; ask an admin to move it to another region");
        assert!(from.lock().unwrap().hits.is_empty(), "{:?}", from.lock().unwrap().hits);
    }
}
