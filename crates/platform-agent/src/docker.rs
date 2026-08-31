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
    Config, CreateContainerOptions, ListContainersOptions, LogOutput, LogsOptions,
    RemoveContainerOptions, StopContainerOptions, WaitContainerOptions,
};
use bollard::models::{HostConfig, PortBinding};
use bollard::Docker;

use game_bridge::framing::CHANNEL_GAME;
use game_bridge::profile::GameTransport;

use crate::config::{GameRuntime, GAME_LABEL, INSTANCE_LABEL, MANAGED_LABEL, OWNER_LABEL, PORTS_LABEL};
use crate::instance::{InstancePort, InstanceSpec, InstanceState};
use crate::store::Mount;

/// How long a container gets to exit on its own before Docker kills it.
///
/// A game server flushes config and logs on shutdown; killing it immediately
/// loses that. Ten seconds is generous for a shutdown that should take one.
const STOP_TIMEOUT_SECS: i64 = 10;

/// Label marking a short-lived provisioning container, so one is never mistaken
/// for an instance. It carries `MANAGED_LABEL` too — the agent's guard applies
/// to everything the agent creates — and `list_managed` skips it anyway,
/// because a task container carries no `INSTANCE_LABEL` to report.
pub const TASK_LABEL: &str = "org.idan2025.gaming-platform-prns.task";

/// How much of a failed task's output to keep. Enough to carry steamcmd's
/// actual complaint, short enough not to put a game's whole download log into
/// an HTTP error body.
const TASK_LOG_TAIL_LINES: usize = 40;

/// What a one-shot container did.
#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub exit_code: i64,
    pub output: String,
}

pub struct DockerRuntime {
    docker: Docker,
}

/// One port to publish: the number the game binds *inside* the container, the
/// node's port it appears on outside, and which transport.
///
/// The container-side number comes from the pack (`GameProfile::ports()`) and
/// the host-side one from the node's allocator, which is the whole reason they
/// are separate fields: the game's port numbers are the game's business and the
/// node's range is the operator's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedPort {
    pub channel: u8,
    pub container_port: u16,
    pub host_port: u16,
    pub transport: GameTransport,
}

fn proto(transport: GameTransport) -> &'static str {
    match transport {
        GameTransport::Udp => "udp",
        GameTransport::Tcp => "tcp",
    }
}

/// Render a port set for `PORTS_LABEL`: `channel:host_port/proto`, comma
/// separated.
fn encode_ports_label(ports: &[PublishedPort]) -> String {
    ports
        .iter()
        .map(|p| format!("{}:{}/{}", p.channel, p.host_port, proto(p.transport)))
        .collect::<Vec<_>>()
        .join(",")
}

/// Read a port set back off a container.
///
/// Tolerant on purpose: an entry this build cannot parse is skipped rather than
/// failing the whole listing, because the alternative is an agent that cannot
/// see — and therefore cannot stop — a container it created.
fn decode_ports_label(label: &str) -> Vec<InstancePort> {
    label
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            let (channel, rest) = entry.split_once(':')?;
            let (port, proto) = rest.split_once('/')?;
            let transport = match proto {
                "udp" => GameTransport::Udp,
                "tcp" => GameTransport::Tcp,
                _ => return None,
            };
            Some(InstancePort {
                channel: channel.parse().ok()?,
                host_port: port.parse().ok()?,
                transport,
            })
        })
        .collect()
}

/// What Docker says about one managed container.
#[derive(Debug, Clone)]
pub struct ManagedContainer {
    pub instance_id: String,
    pub container_id: String,
    pub state: InstanceState,
    /// The game's own port — channel 0 of the port set. Falls back to whatever
    /// Docker reports published when the container carries no port label,
    /// because a container created by an older build is still this agent's to
    /// manage.
    pub port: Option<u16>,
    /// The whole port set, off `PORTS_LABEL`. Empty for a container that
    /// predates the label.
    pub ports: Vec<InstancePort>,
    pub owner: Option<String>,
    pub game_id: Option<String>,
    /// Unix seconds the container was created, as Docker reports it.
    ///
    /// Creation rather than start, deliberately: quota admission and reaping
    /// both ask "how long has this existed", and a container that has been
    /// restarted has not thereby become new. `None` for a Docker that did not
    /// report it, which reads downstream as "age unknown" and never as "age
    /// zero".
    pub created_unix: Option<i64>,
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
                let ports = labels
                    .get(PORTS_LABEL)
                    .map(|l| decode_ports_label(l))
                    .unwrap_or_default();
                let port = ports
                    .iter()
                    .find(|p| p.channel == CHANNEL_GAME)
                    .map(|p| p.host_port)
                    .or_else(|| {
                        c.ports
                            .as_ref()
                            .and_then(|ports| ports.iter().find_map(|p| p.public_port))
                    });
                Some(ManagedContainer {
                    instance_id,
                    container_id: c.id.unwrap_or_default(),
                    state,
                    port,
                    ports,
                    created_unix: c.created,
                    owner: labels.get(OWNER_LABEL).cloned(),
                    game_id: labels.get(GAME_LABEL).cloned(),
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
        ports: &[PublishedPort],
    ) -> Result<String> {
        let name = spec.container_name();

        let mut labels = HashMap::new();
        labels.insert(MANAGED_LABEL.to_string(), "1".to_string());
        labels.insert(INSTANCE_LABEL.to_string(), spec.instance_id.clone());
        labels.insert(GAME_LABEL.to_string(), spec.game_id.clone());
        labels.insert(PORTS_LABEL.to_string(), encode_ports_label(ports));
        if let Some(owner) = &spec.owner {
            labels.insert(OWNER_LABEL.to_string(), owner.clone());
        }

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

        // The container side is the pack's own number and the host side is the
        // node's — a Source server binds 27015 inside whichever node it lands
        // on, and the operator's range decides what that is reachable as
        // outside. One binding per declared port, in that port's transport:
        // publishing RCON as UDP would be a port that answers nothing.
        let mut port_bindings = HashMap::new();
        let mut exposed = HashMap::new();
        for p in ports {
            let container_port = format!("{}/{}", p.container_port, proto(p.transport));
            port_bindings.insert(
                container_port.clone(),
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".to_string()),
                    host_port: Some(p.host_port.to_string()),
                }]),
            );
            exposed.insert(container_port, HashMap::new());
        }

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
            exposed_ports: Some(exposed),
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

    /// Run a one-shot container to completion and return its exit code and the
    /// tail of its output.
    ///
    /// This is how content drivers that need a tool the node has not installed
    /// on its host get one — `steamcmd` in particular (`content.rs`). The image
    /// is **operator config**, never pack-supplied, for the same reason a game's
    /// image is: an image selects the code this node executes.
    ///
    /// The container carries `MANAGED_LABEL` so the agent's own guard applies to
    /// it, gets no ports and no network restrictions beyond Docker's default,
    /// and is removed when it exits — a provisioning run that stayed around
    /// would show up in `list_managed` as an instance nobody asked for.
    pub async fn run_to_completion(
        &self,
        name: &str,
        image: &str,
        cmd: Vec<String>,
        mounts: &[Mount],
        memory_limit_bytes: Option<i64>,
    ) -> Result<TaskOutcome> {
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

        let mut labels = HashMap::new();
        labels.insert(MANAGED_LABEL.to_string(), "1".to_string());
        labels.insert(TASK_LABEL.to_string(), "1".to_string());

        let config = Config {
            image: Some(image.to_string()),
            cmd: if cmd.is_empty() { None } else { Some(cmd) },
            labels: Some(labels),
            host_config: Some(HostConfig {
                binds: Some(binds),
                memory: memory_limit_bytes,
                ..Default::default()
            }),
            ..Default::default()
        };

        self.docker
            .create_container(
                Some(CreateContainerOptions { name: name.to_string(), platform: None }),
                config,
            )
            .await
            .with_context(|| format!("creating task container {name} from {image}"))?;

        // Whatever happens next, the container goes away. A provisioning run
        // that failed and left itself behind would collide with the next
        // attempt by name and be much harder to explain than the failure was.
        let result = self.await_task(name).await;
        let _ = self
            .docker
            .remove_container(name, Some(RemoveContainerOptions { force: true, ..Default::default() }))
            .await;
        result
    }

    async fn await_task(&self, name: &str) -> Result<TaskOutcome> {
        use futures_util::StreamExt;

        self.docker
            .start_container::<String>(name, None)
            .await
            .with_context(|| format!("starting task container {name}"))?;

        let mut waits = self
            .docker
            .wait_container(name, None::<WaitContainerOptions<String>>);
        let mut exit_code = 0i64;
        while let Some(next) = waits.next().await {
            match next {
                Ok(response) => exit_code = response.status_code,
                // A non-zero exit arrives as an error carrying the status, which
                // is a result here, not a failure to report.
                Err(bollard::errors::Error::DockerContainerWaitError { code, .. }) => {
                    exit_code = code
                }
                Err(e) => return Err(e).with_context(|| format!("waiting for {name}")),
            }
        }

        let mut logs = self.docker.logs(
            name,
            Some(LogsOptions::<String> {
                stdout: true,
                stderr: true,
                tail: TASK_LOG_TAIL_LINES.to_string(),
                ..Default::default()
            }),
        );
        let mut output = String::new();
        while let Some(next) = logs.next().await {
            match next {
                Ok(LogOutput::StdOut { message }) | Ok(LogOutput::StdErr { message }) => {
                    output.push_str(&String::from_utf8_lossy(&message));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        Ok(TaskOutcome { exit_code, output })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The label is how a restarted agent learns which port is the game and
    /// which is RCON. Docker reports published ports, but not what they mean,
    /// and an agent that guessed would hand a player the admin port.
    #[test]
    fn a_port_set_round_trips_through_its_label() {
        let ports = vec![
            PublishedPort { channel: 0, container_port: 27015, host_port: 27151, transport: GameTransport::Udp },
            PublishedPort { channel: 1, container_port: 27015, host_port: 27152, transport: GameTransport::Tcp },
            PublishedPort { channel: 2, container_port: 27020, host_port: 27153, transport: GameTransport::Udp },
        ];
        let label = encode_ports_label(&ports);
        assert_eq!(label, "0:27151/udp,1:27152/tcp,2:27153/udp");

        let decoded = decode_ports_label(&label);
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], InstancePort { channel: 0, host_port: 27151, transport: GameTransport::Udp });
        assert_eq!(decoded[1].transport, GameTransport::Tcp, "RCON must not come back as UDP");
        assert_eq!(decoded[2].host_port, 27153);
    }

    /// An entry this build cannot read is skipped, not fatal: the alternative
    /// is an agent that cannot see — and therefore cannot stop or free the
    /// ports of — a container it created itself.
    #[test]
    fn an_unreadable_entry_does_not_lose_the_rest_of_the_set() {
        let decoded = decode_ports_label("0:27151/udp,nonsense,9:70000/udp,1:27152/sctp,2:27153/tcp");
        assert_eq!(
            decoded,
            vec![
                InstancePort { channel: 0, host_port: 27151, transport: GameTransport::Udp },
                InstancePort { channel: 2, host_port: 27153, transport: GameTransport::Tcp },
            ]
        );
        assert!(decode_ports_label("").is_empty());
    }
}
