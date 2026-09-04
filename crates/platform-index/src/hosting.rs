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
//! An agent's API is loopback-only and refuses to bind anything else, because it
//! creates containers and has no authentication. So an index can only drive
//! agents it shares a host with. **Multi-node needs the agent uplink over
//! Reticulum** (`PLAN.md` §8 phase 4), which is not built. Telling operators to
//! expose the agent to the network instead would trade the whole authentication
//! story for a shortcut.


use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use personal_rns::prelude::DestinationHash;
use platform_agent::instance::InstancePort;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use platform_agent::uplink_wire::CapacityResp;

use crate::agent_client::AgentClient;
use crate::quota::{AccountId, InstanceRecord, QuotaPolicy, Quotas, ReapReason};

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
}

/// The quota knobs, as an operator writes them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaSettings {
    pub max_instances_per_account: u32,
    pub max_total_instances: u32,
    pub idle_timeout_secs: Option<u64>,
    pub min_lifetime_secs: u64,
    pub create_cooldown_secs: Option<u64>,
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
        }
    }
}

impl HostingConfig {
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
}

fn default_max_players() -> u8 {
    8
}

pub struct Hosting {
    config: HostingConfig,
    quotas: Quotas,
    http: reqwest::Client,
    /// The Reticulum uplink to remote agents. `None` when this index runs no
    /// Reticulum node, in which case nodes with `agent` set are unreachable
    /// (and reported as such) and loopback-HTTP nodes keep working. Set after
    /// construction once `node.rs` has produced a handle (`set_agent_client`).
    agent_client: Mutex<Option<Arc<AgentClient>>>,
}

impl Hosting {
    pub fn new(config: HostingConfig) -> Self {
        let quotas = Quotas::new(QuotaPolicy::from(&config.quota));
        Self {
            config,
            quotas,
            http: reqwest::Client::builder()
                // An agent that has stopped answering must not hold a request
                // open until the caller gives up on the index itself.
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
            agent_client: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &HostingConfig {
        &self.config
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
        let mut out = Vec::new();
        for node in &self.config.nodes {
            match self.node_instances(node).await {
                Ok(mut rows) => out.append(&mut rows),
                // One unreachable node must not blank the whole listing: the
                // other nodes' instances are still running and still the
                // caller's.
                Err(e) => tracing::warn!(node = %node.name, error = %e, "node did not answer"),
            }
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
                    players_now: r.players_now,
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
                players_now: r["players_now"].as_u64().map(|p| p as u32),
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
        if !self.config.enabled() {
            return Err(anyhow!("this index does not offer hosting"));
        }
        if !self.config.hosts_game(&request.game_id) {
            // Named rather than a bare 404: an operator's list is a deliberate
            // choice, and a caller should be able to see what it is.
            return Err(anyhow!(
                "this index does not host {:?}. It hosts: {}",
                request.game_id,
                self.config.games.join(", ")
            ));
        }
        if request.name.trim().is_empty() {
            return Err(anyhow!("a server needs a name"));
        }

        let existing = self.all_instances().await?;
        let records: Vec<InstanceRecord> = existing.iter().map(|i| record_for(i, now)).collect();
        self.quotas
            .admit(account, &records, now)
            .map_err(|e| anyhow!("{e}"))?;

        let node = self.pick_node(&existing).await.ok_or_else(|| {
            anyhow!("no node with room for another instance")
        })?;

        // The index picks the id. See DeployRequest.
        let instance_id = new_instance_id(&request.game_id)?;

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
                owner: None,
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
                players_now: Some(0),
            });
        }

        let url = format!("{}/instances", node.api.trim_end_matches('/'));
        let body = serde_json::json!({
            "instance_id": instance_id,
            "game_id": request.game_id,
            "name": request.name,
            "max_players": request.max_players,
            "map": request.map,
            "owner": account.0,
        });
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
            players_now: Some(0),
        })
    }

    /// Stop and remove one of the caller's own instances.
    ///
    /// Ownership is checked against the node's label, not against anything this
    /// index remembers, so a restarted index still enforces it correctly.
    pub async fn destroy(&self, account: &AccountId, instance_id: &str) -> Result<()> {
        let all = self.all_instances().await?;
        let found = all
            .iter()
            .find(|i| i.instance_id == instance_id)
            // Not found and not yours are the same answer on purpose: a
            // different one would let anyone enumerate other people's
            // instances by watching which ids come back "forbidden".
            .filter(|i| i.owner.as_deref() == Some(account.0.as_str()))
            .ok_or_else(|| anyhow!("no such instance"))?;

        let node = self
            .config
            .nodes
            .iter()
            .find(|n| n.name == found.node)
            .ok_or_else(|| anyhow!("the node holding it is no longer configured"))?;

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
        if self.quotas.policy().idle_timeout.is_none() {
            return Ok(Vec::new());
        }
        let existing = self.all_instances().await?;
        let records: Vec<InstanceRecord> = existing.iter().map(|i| record_for(i, now)).collect();

        let mut reaped = Vec::new();
        for (instance_id, reason) in self.quotas.to_reap(&records, now) {
            let Some(instance) = existing.iter().find(|i| i.instance_id == instance_id) else {
                continue;
            };
            let Some(node) = self.config.nodes.iter().find(|n| n.name == instance.node) else {
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
    async fn pick_node(&self, existing: &[HostedInstance]) -> Option<&NodeConfig> {
        let mut best: Option<(&NodeConfig, (u8, usize))> = None;
        for node in &self.config.nodes {
            let here = existing.iter().filter(|i| i.node == node.name).count();
            let Some(ranked) = rank_node(self.node_capacity(node).await, here) else {
                continue;
            };
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

/// How attractive a node is to place on. Higher is better; `None` means skip.
///
/// The tuple is `(answered, free)` so that **a reported figure always beats a
/// guess**: a node that told us it has one slot left outranks a node that said
/// nothing, however empty the index believes that one to be. Silence is not
/// room, and treating it as room is how one unreachable node collects every
/// create request during an outage.
fn rank_node(capacity: Option<CapacityResp>, own_count: usize) -> Option<(u8, usize)> {
    match capacity {
        // Full is full. Picking it would pass quota admission and then fail the
        // create, after the user has been told a node was chosen.
        Some(cap) if cap.running >= cap.max_instances => None,
        Some(cap) => Some((1, cap.max_instances - cap.running)),
        None => Some((0, usize::MAX - own_count)),
    }
}

/// A random, unguessable instance id, prefixed by the game so `docker ps` on the
/// node reads sensibly.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> HostingConfig {
        HostingConfig {
            games: vec!["sven-coop".to_string()],
            nodes: vec![NodeConfig {
                agent: None,
                name: "local".to_string(),
                api: "http://127.0.0.1:4750".to_string(),
            }],
            quota: QuotaSettings::default(),
        }
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
                },
                std::time::SystemTime::now(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not offer hosting"), "{err}");
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
        c.nodes.push(NodeConfig { name: "second".into(), api: "http://127.0.0.1:4751".into(), agent: None });
        let h = Hosting::new(c);
        let existing = vec![HostedInstance {
            instance_id: "a".into(),
            node: "local".into(),
            game_id: "sven-coop".into(),
            name: "a".into(),
            state: "running".into(),
            port: None,
            ports: Vec::new(),
            owner: None,
            uptime_secs: None,
            players_now: None,
        }];
        // Neither node answers `/capacity` here — nothing is listening — so
        // both fall back to the index's own count and the emptier one wins.
        assert_eq!(h.pick_node(&existing).await.unwrap().name, "second");
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
            owner: Some("acct".into()),
            uptime_secs: uptime,
            players_now: players,
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
        }
    }

    /// The whole reason placement asks: a full node must not be chosen and then
    /// fail the create.
    #[test]
    fn a_full_node_is_skipped_rather_than_picked() {
        assert_eq!(rank_node(Some(cap(4, 4)), 0), None);
        assert_eq!(rank_node(Some(cap(5, 4)), 0), None, "over its own limit is still full");
        assert!(rank_node(Some(cap(3, 4)), 0).is_some());
    }

    /// A node that answered outranks one that did not, even when the index
    /// believes the silent one is emptier. Silence is not room.
    #[test]
    fn a_reported_figure_beats_a_guess() {
        let answered = rank_node(Some(cap(3, 4)), 99).expect("has one slot");
        let silent = rank_node(None, 0).expect("unknown, not full");
        assert!(answered > silent, "{answered:?} should outrank {silent:?}");
    }

    #[test]
    fn among_nodes_that_answered_the_emptiest_wins() {
        assert!(rank_node(Some(cap(1, 8)), 0) > rank_node(Some(cap(6, 8)), 0));
    }

    #[test]
    fn quota_settings_round_trip_into_a_policy() {
        let s = QuotaSettings {
            max_instances_per_account: 2,
            max_total_instances: 5,
            idle_timeout_secs: Some(60),
            min_lifetime_secs: 30,
            create_cooldown_secs: None,
        };
        let p = QuotaPolicy::from(&s);
        assert_eq!(p.max_instances_per_account, 2);
        assert_eq!(p.idle_timeout, Some(std::time::Duration::from_secs(60)));
        assert_eq!(p.create_cooldown, None);
    }
}
