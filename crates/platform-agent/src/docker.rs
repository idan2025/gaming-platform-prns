//! Container orchestration, over the local Docker socket.
//!
//! # The rule this module exists to enforce
//!
//! **The agent only ever touches containers it created.** Every mutating call
//! goes through `assert_managed`, which inspects the container and refuses
//! unless it carries `MANAGED_LABEL`. Listing is filtered by the same label.
//!
//! That is not defensive tidiness. A node that runs game servers is a node
//! somebody uses for other things — the machine this was written on was already
//! running an unrelated `svencoop-prns-host` container and a `nexus-pillar` —
//! and an agent that stopped, removed or reaped by name prefix alone would
//! eventually destroy a stranger's service because two names collided. Label,
//! inspect, then act.
//!
//! Nothing here removes images or volumes. Reclaiming disk is an operator's
//! decision about their own machine, and an agent that pruned would eventually
//! prune something it did not put there.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    StopContainerOptions,
};
use bollard::models::{HostConfig, PortBinding};
use bollard::Docker;

use crate::config::{GameRuntime, INSTANCE_LABEL, MANAGED_LABEL};
use crate::instance::{InstanceSpec, InstanceState};
use crate::store::Mount;

/// How long a container gets to exit on its own before Docker kills it.
///
/// A game server flushes config and logs on shutdown; killing it immediately
/// loses that. Ten seconds is generous for a shutdown that should take one.
const STOP_TIMEOUT_SECS: i64 = 10;

pub struct DockerRuntime {
    docker: Docker,
}

/// What Docker says about one managed container.
#[derive(Debug, Clone)]
pub struct ManagedContainer {
    pub instance_id: String,
    pub container_id: String,
    pub state: InstanceState,
    pub port: Option<u16>,
}

impl DockerRuntime {
    pub fn connect() -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .context("connecting to the local Docker socket")?;
        Ok(Self { docker })
    }

    pub async fn ping(&self) -> Result<()> {
        self.docker.ping().await.context("Docker did not answer a ping")?;
        Ok(())
    }

    /// Every container this agent manages, running or not.
    ///
    /// Filtered by label rather than by name, because a name prefix is a
    /// convention anyone can adopt by accident and a label is something only
    /// this agent sets.
    pub async fn list_managed(&self) -> Result<Vec<ManagedContainer>> {
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![MANAGED_LABEL.to_string()]);
        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions { all: true, filters, ..Default::default() }))
            .await
            .context("listing managed containers")?;

        Ok(containers
            .into_iter()
            .filter_map(|c| {
                let labels = c.labels.unwrap_or_default();
                let instance_id = labels.get(INSTANCE_LABEL)?.clone();
                let state = match c.state.as_deref() {
                    Some("running") => InstanceState::Running,
                    Some("created") => InstanceState::Creating,
                    Some("exited") | Some("dead") | Some("paused") => InstanceState::Stopped,
                    // A state string this build does not recognise is not
                    // "stopped": saying so would report a running server as down.
                    _ => InstanceState::Unknown,
                };
                let port = c.ports.as_ref().and_then(|ports| {
                    ports.iter().find_map(|p| p.public_port)
                });
                Some(ManagedContainer {
                    instance_id,
                    container_id: c.id.unwrap_or_default(),
                    state,
                    port,
                })
            })
            .collect())
    }

    /// Refuse to act on anything this agent did not create.
    async fn assert_managed(&self, name: &str) -> Result<()> {
        let info = self
            .docker
            .inspect_container(name, None)
            .await
            .with_context(|| format!("inspecting container {name}"))?;
        let managed = info
            .config
            .and_then(|c| c.labels)
            .map(|l| l.contains_key(MANAGED_LABEL))
            .unwrap_or(false);
        if !managed {
            return Err(anyhow!(
                "container {name} is not managed by this agent; refusing to touch it"
            ));
        }
        Ok(())
    }

    /// Create and start a container for `spec`.
    ///
    /// `mounts` must come from the store planner: the read-only content mount
    /// first, writable ones nested inside it. Every writable mountpoint has to
    /// already exist inside the shared content directory — a writable bind
    /// nested in a read-only bind cannot have its mountpoint created by the
    /// runtime. `plan_and_check` in `agent.rs` is what enforces that; calling
    /// this directly with a plan you have not checked gets you a runc error
    /// about `mkdirat ... read-only file system` instead.
    pub async fn create_and_start(
        &self,
        spec: &InstanceSpec,
        runtime: &GameRuntime,
        mounts: &[Mount],
        host_port: u16,
    ) -> Result<String> {
        let name = spec.container_name();

        let mut labels = HashMap::new();
        labels.insert(MANAGED_LABEL.to_string(), "1".to_string());
        labels.insert(INSTANCE_LABEL.to_string(), spec.instance_id.clone());
        labels.insert("org.idan2025.gaming-platform-prns.game".to_string(), spec.game_id.clone());

        // Order matters: the read-only content mount comes first and the
        // writable ones are nested inside it. See the store planner.
        let binds: Vec<String> = mounts
            .iter()
            .map(|m| {
                format!(
                    "{}:{}:{}",
                    m.host_path.display(),
                    m.container_path.display(),
                    if m.read_only { "ro" } else { "rw" }
                )
            })
            .collect();

        let container_port = format!("{}/udp", spec.port.unwrap_or(host_port));
        let mut port_bindings = HashMap::new();
        port_bindings.insert(
            container_port.clone(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(host_port.to_string()),
            }]),
        );

        let env: Vec<String> = runtime
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();

        let host_config = HostConfig {
            binds: Some(binds),
            port_bindings: Some(port_bindings),
            memory: runtime.memory_limit_bytes,
            nano_cpus: runtime.cpus.map(|c| (c * 1_000_000_000.0) as i64),
            // No restart policy on purpose: the agent decides what runs, and a
            // container that resurrects itself behind the agent's back is a
            // container the agent's state no longer describes.
            ..Default::default()
        };

        let config = Config {
            image: Some(runtime.image.clone()),
            labels: Some(labels),
            env: if env.is_empty() { None } else { Some(env) },
            exposed_ports: Some(HashMap::from([(container_port, HashMap::new())])),
            host_config: Some(host_config),
            ..Default::default()
        };

        let created = self
            .docker
            .create_container(
                Some(CreateContainerOptions { name: name.clone(), platform: None }),
                config,
            )
            .await
            .with_context(|| format!("creating container {name}"))?;

        self.docker
            .start_container::<String>(&name, None)
            .await
            .with_context(|| format!("starting container {name}"))?;

        Ok(created.id)
    }

    pub async fn stop(&self, spec_id: &str) -> Result<()> {
        let name = format!("{}{}", crate::config::CONTAINER_PREFIX, spec_id);
        self.assert_managed(&name).await?;
        self.docker
            .stop_container(&name, Some(StopContainerOptions { t: STOP_TIMEOUT_SECS }))
            .await
            .with_context(|| format!("stopping container {name}"))?;
        Ok(())
    }

    /// Remove the container. Never removes volumes — `v: false` is deliberate:
    /// an instance's writable state lives in bind-mounted host directories the
    /// agent owns, and reaping those is a separate, explicit decision.
    pub async fn remove(&self, spec_id: &str) -> Result<()> {
        let name = format!("{}{}", crate::config::CONTAINER_PREFIX, spec_id);
        self.assert_managed(&name).await?;
        self.docker
            .remove_container(
                &name,
                Some(RemoveContainerOptions { force: true, v: false, ..Default::default() }),
            )
            .await
            .with_context(|| format!("removing container {name}"))?;
        Ok(())
    }

    pub async fn state_of(&self, spec_id: &str) -> Result<InstanceState> {
        let name = format!("{}{}", crate::config::CONTAINER_PREFIX, spec_id);
        match self.docker.inspect_container(&name, None).await {
            Ok(info) => {
                let managed = info
                    .config
                    .and_then(|c| c.labels)
                    .map(|l| l.contains_key(MANAGED_LABEL))
                    .unwrap_or(false);
                if !managed {
                    // Something else owns this name. Not ours to report on.
                    return Ok(InstanceState::Unknown);
                }
                Ok(match info.state.and_then(|s| s.running) {
                    Some(true) => InstanceState::Running,
                    Some(false) => InstanceState::Stopped,
                    None => InstanceState::Unknown,
                })
            }
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => {
                Ok(InstanceState::Missing)
            }
            Err(e) => Err(e).with_context(|| format!("inspecting container {name}")),
        }
    }
}
