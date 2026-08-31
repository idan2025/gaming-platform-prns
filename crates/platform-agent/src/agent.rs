//! The daemon: one node, many instances.
//!
//! `PLAN.md` §8 phase 3. Everything here is local — the agent takes commands
//! from whoever is on this machine and answers to nobody else. There is no
//! central service in this phase and, per `DESIGN.md` §0, there must never be
//! one this depends on: an index or a deploy API is a convenience layered on
//! top of an agent that already works alone.
//!
//! # State lives in Docker, not in a file the agent keeps
//!
//! The agent deliberately holds no on-disk instance database. What is running is
//! whatever carries the managed label, and the agent asks Docker rather than
//! remembering. A local file would be a second source of truth that drifts the
//! moment somebody runs `docker rm` by hand — and they will, because it is their
//! machine.
//!
//! The one thing that must be rebuilt at startup is the port allocator, which is
//! seeded from the ports of containers already running. Without that, an agent
//! that restarted would cheerfully hand a live instance's port to a new one.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use game_bridge::content::PackContent;
use game_bridge::GamePack;
use tokio::sync::Mutex;

use crate::config::AgentConfig;
use crate::content::{ProvisionError, Provisioned, Provisioner};
use crate::docker::DockerRuntime;
use crate::instance::{InstanceSpec, InstanceState, InstanceStatus};
use crate::ports::PortAllocator;
use crate::store::{ContentRef, InstancePlan, StoreLayout};

pub struct Agent {
    config: AgentConfig,
    layout: StoreLayout,
    content: Provisioner,
    docker: DockerRuntime,
    ports: Mutex<PortAllocator>,
    packs: BTreeMap<String, GamePack>,
    /// What the agent was asked for, keyed by instance id. Docker remains the
    /// authority on what is *running*; this only remembers the display name and
    /// requested size, which Docker has no place to keep.
    specs: Mutex<BTreeMap<String, InstanceSpec>>,
}

impl Agent {
    pub async fn new(config: AgentConfig, packs: Vec<GamePack>) -> Result<Self> {
        config.validate().map_err(|e| anyhow!("{e}"))?;
        let docker = DockerRuntime::connect()?;
        docker.ping().await?;

        // Seed the allocator from what is already running, or the agent will
        // hand out a port a live instance is using.
        let running = docker.list_managed().await?;
        let reserved: Vec<u16> = running.iter().filter_map(|c| c.port).collect();
        let ports = PortAllocator::with_reserved(config.port_range, &reserved);

        let layout = StoreLayout::new(config.data_root.clone());
        let content = Provisioner::new(
            layout.clone(),
            config.allow_content_fetch,
            config.steamcmd_image.clone(),
        );
        let packs = packs.into_iter().map(|p| (p.id.clone(), p)).collect();

        Ok(Self {
            config,
            layout,
            content,
            docker,
            ports: Mutex::new(ports),
            packs,
            specs: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Plan an instance's layout and refuse early if the shared content copy
    /// cannot host it.
    ///
    /// The mountpoint check is the interesting half. A writable bind nested in a
    /// read-only bind cannot have its mountpoint created by the container
    /// runtime, so a missing `svencoop/logs` in the content copy surfaces as a
    /// Docker 500 quoting `mkdirat ... read-only file system`. That tells an
    /// operator nothing. Checking here turns it into the name of the directory
    /// their game install is missing.
    pub fn plan_and_check(&self, spec: &InstanceSpec) -> Result<InstancePlan> {
        spec.validate().map_err(|e| anyhow!("{e}"))?;

        let pack = self
            .packs
            .get(&spec.game_id)
            .ok_or_else(|| anyhow!("no game pack installed for {:?}", spec.game_id))?;
        let runtime = self.config.runtime_for(&spec.game_id).ok_or_else(|| {
            anyhow!(
                "this node has no runtime configured for {:?}. A pack describes a game; \
                 the operator decides what image runs it",
                spec.game_id
            )
        })?;

        let content = ContentRef {
            game_id: spec.game_id.clone(),
            version: runtime.content_version.clone(),
        };
        let plan = self
            .layout
            .plan_instance(
                &spec.instance_id,
                &content,
                &runtime.content_root,
                &pack.writable_paths,
            )
            .map_err(|e| anyhow!("cannot plan instance layout: {e}"))?;

        let content_dir = self
            .layout
            .content_dir(&content)
            .map_err(|e| anyhow!("{e}"))?;
        if !content_dir.is_dir() {
            return Err(anyhow!(
                "game content for {} version {} is not installed at {}. {}",
                content.game_id,
                content.version,
                content_dir.display(),
                self.how_to_install(&spec.game_id, &pack.content)
            ));
        }

        let missing = StoreLayout::missing_content_dirs(&plan);
        if !missing.is_empty() {
            let names: Vec<String> = missing.iter().map(|p| p.display().to_string()).collect();
            return Err(anyhow!(
                "the game content copy is missing directories this pack declares writable, \
                 and a writable mount cannot create its own mountpoint under a read-only one: {}",
                names.join(", ")
            ));
        }
        Ok(plan)
    }

    /// What an operator should do about content that is not there.
    ///
    /// The sentence differs by driver, because the action differs: a `manual`
    /// pack needs a human with the game files, an `archive` pack needs one API
    /// call, and an `archive` pack on a node that has not opted in needs a
    /// decision about the pack first.
    fn how_to_install(&self, game_id: &str, content: &PackContent) -> String {
        match (content.is_automatic(), self.config.allow_content_fetch) {
            (true, true) => format!(
                "This pack's \"{}\" driver can fetch it: POST /content/{game_id}",
                content.driver_name()
            ),
            (true, false) => format!(
                "This pack's \"{}\" driver could fetch it, but this node has                  allow_content_fetch off — install it by hand or turn fetching on once you                  trust the pack",
                content.driver_name()
            ),
            (false, _) => "This pack's content driver is \"manual\": install the game there                            yourself"
                .to_string(),
        }
    }

    /// Install a game's content if it is not already there.
    ///
    /// Deliberately **not** part of `create`. Fetching a game is gigabytes over
    /// somebody's home uplink; a create request that silently turned into a
    /// half-hour download would time out, and the second attempt would find a
    /// staging directory it could not explain. So provisioning is its own
    /// explicit step, run once per game and version, and `create` keeps failing
    /// fast with a sentence naming this one.
    pub async fn ensure_content(&self, game_id: &str) -> Result<Provisioned, ProvisionError> {
        let pack = self.packs.get(game_id).ok_or_else(|| {
            ProvisionError::Io(std::io::Error::other(format!(
                "no game pack installed for {game_id:?}"
            )))
        })?;
        let runtime = self.config.runtime_for(game_id).ok_or_else(|| {
            ProvisionError::Io(std::io::Error::other(format!(
                "this node has no runtime configured for {game_id:?}, so there is no                  content_version to install into"
            )))
        })?;
        let content = ContentRef {
            game_id: game_id.to_string(),
            version: runtime.content_version.clone(),
        };
        self.content
            .ensure(&content, &pack.content, Some(&self.docker))
            .await
    }

    /// Create and start an instance.
    pub async fn create(&self, spec: InstanceSpec) -> Result<InstanceStatus> {
        let existing = self.docker.list_managed().await?;
        if existing.iter().any(|c| c.instance_id == spec.instance_id) {
            return Err(anyhow!("instance {:?} already exists", spec.instance_id));
        }
        if existing.len() >= self.config.max_instances {
            return Err(anyhow!(
                "this node is at its limit of {} instances",
                self.config.max_instances
            ));
        }

        let plan = self.plan_and_check(&spec)?;
        for dir in &plan.host_dirs_to_create {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }

        let runtime = self
            .config
            .runtime_for(&spec.game_id)
            .ok_or_else(|| anyhow!("no runtime for {:?}", spec.game_id))?;

        let port = {
            let mut ports = self.ports.lock().await;
            match spec.port {
                Some(p) => {
                    ports.reserve(p).map_err(|e| anyhow!("{e}"))?;
                    p
                }
                None => ports.allocate().map_err(|e| anyhow!("{e}"))?,
            }
        };

        let started = self
            .docker
            .create_and_start(&spec, runtime, &plan.mounts, port)
            .await;
        let container_id = match started {
            Ok(id) => id,
            Err(e) => {
                // Give the port back, or a failed start leaks one every attempt.
                self.ports.lock().await.release(port);
                return Err(e);
            }
        };

        self.specs
            .lock()
            .await
            .insert(spec.instance_id.clone(), spec.clone());

        Ok(InstanceStatus {
            instance_id: spec.instance_id,
            game_id: spec.game_id,
            name: spec.name,
            state: InstanceState::Running,
            port: Some(port),
            container_id: Some(container_id),
            uptime_secs: None,
            owner: spec.owner,
        })
    }

    pub async fn stop(&self, instance_id: &str) -> Result<()> {
        self.docker.stop(instance_id).await
    }

    /// Stop and remove, returning the port to the pool.
    pub async fn remove(&self, instance_id: &str) -> Result<()> {
        let port = self
            .docker
            .list_managed()
            .await?
            .into_iter()
            .find(|c| c.instance_id == instance_id)
            .and_then(|c| c.port);
        // Stopping first is politeness, not correctness: `remove` forces. But a
        // game server that is killed outright loses whatever it was flushing.
        let _ = self.docker.stop(instance_id).await;
        self.docker.remove(instance_id).await?;
        if let Some(p) = port {
            self.ports.lock().await.release(p);
        }
        self.specs.lock().await.remove(instance_id);
        Ok(())
    }

    /// Everything this agent manages, as Docker currently sees it.
    pub async fn list(&self) -> Result<Vec<InstanceStatus>> {
        let containers = self.docker.list_managed().await?;
        let specs = self.specs.lock().await;
        Ok(containers
            .into_iter()
            .map(|c| {
                let spec = specs.get(&c.instance_id);
                InstanceStatus {
                    // Read off the container rather than this run's memory: an
                    // index restarts, and the label is what survives.
                    owner: c.owner.clone(),
                    game_id: c
                        .game_id
                        .clone()
                        .or_else(|| spec.map(|s| s.game_id.clone()))
                        .unwrap_or_default(),
                    // An instance the agent did not start this run still shows
                    // up — it is running on this node, and hiding it would make
                    // the list a lie about the machine.
                    name: spec
                        .map(|s| s.name.clone())
                        .unwrap_or_else(|| c.instance_id.clone()),
                    instance_id: c.instance_id,
                    state: c.state,
                    port: c.port,
                    container_id: Some(c.container_id),
                    uptime_secs: None,
                }
            })
            .collect())
    }

    /// Instance directories with no corresponding container.
    ///
    /// Returns them rather than deleting them. Reclaiming an instance's writable
    /// state destroys whatever a player built there, and an agent that did it
    /// automatically would eventually do it to something someone wanted.
    pub async fn orphan_dirs(&self) -> Result<Vec<std::path::PathBuf>> {
        let live: Vec<String> = self
            .docker
            .list_managed()
            .await?
            .into_iter()
            .map(|c| c.instance_id)
            .collect();
        let instances_root = self.config.data_root.join("instances");
        let entries = match std::fs::read_dir(&instances_root) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect::<Vec<_>>(),
            // No instances directory yet is not a problem worth reporting.
            Err(_) => Vec::new(),
        };
        Ok(self.layout.orphan_instance_dirs(&live, &entries))
    }
}
