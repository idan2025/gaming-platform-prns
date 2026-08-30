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

use anyhow::{anyhow, Context, Result};
use personal_rns::prelude::DestinationHash;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::agent_client::AgentClient;
use crate::quota::{AccountId, InstanceRecord, QuotaPolicy, Quotas};

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

/// One hosted instance, as the index reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedInstance {
    pub instance_id: String,
    pub node: String,
    pub game_id: String,
    pub name: String,
    pub state: String,
    pub port: Option<u16>,
    pub owner: Option<String>,
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
                    owner: r.owner,
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
                owner: r["owner"].as_str().map(str::to_string),
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
        let records: Vec<InstanceRecord> = existing
            .iter()
            .map(|i| InstanceRecord {
                instance_id: i.instance_id.clone(),
                account: AccountId(i.owner.clone().unwrap_or_default()),
                // The node does not report creation time yet, so quota
                // admission sees every instance as old enough to have escaped
                // the create cooldown. That makes the cooldown ineffective
                // rather than wrong; the instance-count limits, which are the
                // ones that actually bound resource use, are exact.
                created_at: now,
                last_player_seen: None,
                players_now: 0,
                exempt_from_reaping: false,
            })
            .collect();
        self.quotas
            .admit(account, &records, now)
            .map_err(|e| anyhow!("{e}"))?;

        let node = self
            .pick_node(&existing)
            .ok_or_else(|| anyhow!("no node available"))?;

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
                port: None,
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
                owner: Some(account.0.clone()),
            });
        }

        let url = format!("{}/instances", node.api.trim_end_matches('/'));
        let body = serde_json::json!({
            "instance_id": instance_id,
            "game_id": request.game_id,
            "name": request.name,
            "max_players": request.max_players,
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
            owner: Some(account.0.clone()),
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

    /// Least-loaded node wins. Crude, and enough for a handful of nodes; real
    /// placement needs the capacity reporting that arrives with the agent
    /// uplink.
    fn pick_node(&self, existing: &[HostedInstance]) -> Option<&NodeConfig> {
        self.config.nodes.iter().min_by_key(|n| {
            existing.iter().filter(|i| i.node == n.name).count()
        })
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

    #[test]
    fn a_node_is_picked_by_load() {
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
            owner: None,
        }];
        assert_eq!(h.pick_node(&existing).unwrap().name, "second");
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
