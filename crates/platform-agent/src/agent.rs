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
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use game_bridge::content::PackContent;
use game_bridge::profile::GameProfile;
use game_bridge::GamePack;
use tokio::sync::{Mutex, RwLock};

use crate::config::AgentConfig;
use crate::content::{ProvisionError, Provisioned, Provisioner};
use crate::docker::{DockerRuntime, PublishedPort};
use crate::instance::{InstancePort, InstanceSpec, InstanceState, InstanceStatus};
use crate::ports::PortAllocator;
use crate::store::{ContentRef, InstancePlan, StoreLayout};
use crate::uplink_wire::CapacityResp;

pub struct Agent {
    config: AgentConfig,
    layout: StoreLayout,
    content: Provisioner,
    docker: DockerRuntime,
    ports: Mutex<PortAllocator>,
    /// Behind a lock because a pack can now be imported into a *running* node
    /// (`pack_import.rs`): "add a game" that needed a restart would not be one
    /// click. Reloading goes through `reload_packs`, which re-runs the
    /// operator's trust policy — there is still exactly one place that decides
    /// which packs this node will run.
    packs: RwLock<BTreeMap<String, GamePack>>,
    /// What the agent was asked for, keyed by instance id. Docker remains the
    /// authority on what is *running*; this only remembers the display name and
    /// requested size, which Docker has no place to keep.
    specs: Mutex<BTreeMap<String, InstanceSpec>>,
    /// One mesh bridge per running instance. Empty and inert on a node with no
    /// `[mesh]` section, which is the LAN-only behaviour that came before.
    mesh: Arc<crate::mesh::MeshBridges>,
}

impl Agent {
    pub async fn new(config: AgentConfig, packs: Vec<GamePack>) -> Result<Self> {
        config.validate().map_err(|e| anyhow!("{e}"))?;
        let docker = DockerRuntime::connect()?;
        docker.ping().await?;

        // Seed the allocator from what is already running, or the agent will
        // hand out a port a live instance is using.
        let running = docker.list_managed().await?;
        // Every port of every live instance, not just its game port: an RCON
        // port handed to a second instance is the same collision as a game port
        // handed twice.
        let reserved: Vec<u16> = running
            .iter()
            .flat_map(|c| {
                let mut ports: Vec<u16> = c.ports.iter().map(|p| p.host_port).collect();
                if ports.is_empty() {
                    ports.extend(c.port);
                }
                ports
            })
            .collect();
        let ports = PortAllocator::with_reserved(config.port_range, &reserved);

        let layout = StoreLayout::new(config.data_root.clone());
        let content = Provisioner::new(
            layout.clone(),
            config.allow_content_fetch,
            config.steamcmd_image.clone(),
        );
        let packs = RwLock::new(packs.into_iter().map(|p| (p.id.clone(), p)).collect());

        let mesh = crate::mesh::MeshBridges::new(config.mesh.clone());
        Ok(Self {
            config,
            layout,
            content,
            docker,
            ports: Mutex::new(ports),
            packs,
            specs: Mutex::new(BTreeMap::new()),
            mesh,
        })
    }

    /// The packs this agent will run, by game id.
    ///
    /// A clone rather than a borrow, because the set can change under an import.
    pub async fn packs(&self) -> BTreeMap<String, GamePack> {
        self.packs.read().await.clone()
    }

    /// Re-read a pack directory under the operator's trust policy and adopt the
    /// result.
    ///
    /// **Goes through `packs::load_deployable`, never around it.** An import
    /// route that installed straight into this map would be a route that
    /// bypasses `[pack_trust]`, which is the one thing the gate exists to stop.
    /// Returns what was refused so a caller can say why a game it just imported
    /// did not appear.
    pub async fn reload_packs(
        &self,
        dir: &std::path::Path,
        now: SystemTime,
    ) -> Result<crate::packs::DeployablePacks> {
        let policy = self.config.pack_trust_policy();
        let loaded = crate::packs::load_deployable(dir, &policy, now)
            .map_err(|e| anyhow!("{e}"))?;
        let mut packs = self.packs.write().await;
        *packs = loaded.packs.iter().map(|p| (p.id.clone(), p.clone())).collect();
        Ok(loaded)
    }

    /// Where this node can actually reach a running instance's game.
    ///
    /// The container's own address on the Docker network and the port the game
    /// binds **inside** it — not `127.0.0.1:<host_port>`. The agent is meant to
    /// run in a container, and there that loopback is its own namespace: a mesh
    /// bridge pointed at it announces a destination, accepts a link, and then
    /// forwards every packet into nothing. The game is up, the announce is
    /// real, and joining does not work, which is the worst of the three
    /// possible failures because it looks like it should be fine.
    ///
    /// Same reasoning as `player_count`. Both learned it the same way.
    async fn game_address(
        &self,
        instance_id: &str,
        profile: &GameProfile,
    ) -> Option<(String, u16)> {
        let containers = self.docker.list_managed().await.ok()?;
        let container = containers.iter().find(|c| c.instance_id == instance_id)?;
        match &container.ip {
            Some(ip) if !ip.is_empty() => Some((ip.clone(), profile.default_port)),
            // No network address means nothing can reach it, the agent
            // included. Better to announce nothing than a destination that
            // forwards into the void.
            _ => None,
        }
    }

    /// Put every already-running instance back on the mesh.
    ///
    /// Containers outlive the agent — that is the point of `--restart` and of
    /// the agent keeping no database — so on startup there may be servers
    /// running that nothing is announcing. Without this an agent restart takes
    /// every hosted server off the mesh until somebody recreates it, which is a
    /// silent outage: the game is still up and still reachable on the LAN.
    ///
    /// Each identity is the one already beside that instance, so a restored
    /// server keeps the destination players had.
    pub async fn restore_mesh_bridges(&self) {
        if !self.mesh.enabled() {
            return;
        }
        let instances = match self.list().await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "could not list instances to restore mesh bridges");
                return;
            }
        };
        let packs = self.packs.read().await;
        for instance in instances.iter().filter(|i| i.state == InstanceState::Running) {
            let Some(pack) = packs.get(&instance.game_id) else { continue };
            let Ok(profile) = pack.to_profile() else { continue };
            let Some(addr) = self.game_address(&instance.instance_id, &profile).await else {
                continue;
            };
            let identity = crate::mesh::identity_path(&self.config.data_root, &instance.instance_id);
            let max_players = self
                .specs
                .lock()
                .await
                .get(&instance.instance_id)
                .map(|s| s.max_players)
                // The spec is this run's memory and a restart has none, so fall
                // back to the pack's own idea rather than refusing to restore.
                .unwrap_or(16);
            if let Err(e) = self
                .mesh
                .start(
                    &instance.instance_id,
                    &profile,
                    &identity,
                    (&addr.0, addr.1),
                    &instance.name,
                    max_players,
                )
                .await
            {
                tracing::warn!(
                    instance = %instance.instance_id,
                    error = %format!("{e:#}"),
                    "could not put this running server back on the mesh"
                );
            }
        }
    }

    /// The mesh bridges this node is running, for the API and the web UI.
    pub fn mesh(&self) -> &Arc<crate::mesh::MeshBridges> {
        &self.mesh
    }

    /// The daemon handle, for a caller that needs to ask Docker something the
    /// agent itself does not model.
    pub fn docker(&self) -> &DockerRuntime {
        &self.docker
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
    pub async fn plan_and_check(&self, spec: &InstanceSpec) -> Result<InstancePlan> {
        spec.validate().map_err(|e| anyhow!("{e}"))?;

        let packs = self.packs.read().await;
        let pack = packs
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
        let packs = self.packs.read().await;
        let pack = packs.get(game_id).ok_or_else(|| {
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
        let provisioned = self
            .content
            .ensure(&content, &pack.content, Some(&self.docker))
            .await?;
        self.create_writable_mountpoints(&content, &pack.writable_paths)?;
        Ok(provisioned)
    }

    /// Make sure every path the pack declares writable exists inside the shared
    /// content copy.
    ///
    /// **A fresh install cannot start a server without this.** Each writable
    /// path becomes a bind mount nested inside the read-only content mount, and
    /// a container runtime cannot create a mountpoint inside a read-only bind —
    /// runc reports it as `mkdirat ... read-only file system`. Sven Co-op is
    /// the live example: steamcmd creates `svencoop/maps` and
    /// `svencoop/scripts` but not `svencoop/logs`, so a node that had just
    /// downloaded 2.6 GB successfully still refused to start anything.
    ///
    /// Install is the right moment and instance start is not.
    /// `missing_content_dirs` deliberately only reads: it runs per instance, on
    /// shared state, and a directory appearing because somebody pressed Start is
    /// a surprise. Here the tree is this function's to prepare, once, for the
    /// pack it was installed for.
    ///
    /// The paths are validated before use even though they came from a parsed
    /// pack, because this is a `mkdir` under a directory the node owns and
    /// "it was validated somewhere else" is how a traversal gets in.
    fn create_writable_mountpoints(
        &self,
        content: &ContentRef,
        writable_paths: &[String],
    ) -> Result<(), ProvisionError> {
        let root = self
            .layout
            .content_dir(content)
            .map_err(|e| ProvisionError::Io(std::io::Error::other(format!("{e}"))))?;
        for path in writable_paths {
            crate::store::validate_writable_path(path)
                .map_err(|e| ProvisionError::Io(std::io::Error::other(format!("{e}"))))?;
            let target = root.join(path);
            // Belt and braces after validation: a symlink inside the content
            // tree could still point out of it, and `starts_with` on the
            // canonical parent is what catches that.
            if !target.starts_with(&root) {
                return Err(ProvisionError::Io(std::io::Error::other(format!(
                    "writable path {path:?} escapes the content directory"
                ))));
            }
            if !target.is_dir() {
                std::fs::create_dir_all(&target)?;
                tracing::info!(
                    path = %target.display(),
                    "created a writable mountpoint the game install did not"
                );
            }
        }
        Ok(())
    }

    /// Whether this game's files are missing from the shared content tree.
    ///
    /// Asked before turning a failed create into a download, so that only a
    /// *missing content* failure does so. A mistyped game id or an
    /// unconfigured runtime is the caller's to fix, and starting a multi-gigabyte
    /// download because somebody fat-fingered a name would be worse than the
    /// error they got.
    pub async fn content_is_missing(&self, game_id: &str) -> bool {
        let Some(runtime) = self.config.runtime_for(game_id) else {
            return false;
        };
        if !self.packs.read().await.contains_key(game_id) {
            return false;
        }
        let content = ContentRef {
            game_id: game_id.to_string(),
            version: runtime.content_version.clone(),
        };
        match self.layout.content_dir(&content) {
            Ok(dir) => !dir.is_dir(),
            Err(_) => false,
        }
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

        let plan = self.plan_and_check(&spec).await?;
        for dir in &plan.host_dirs_to_create {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }

        let runtime = self
            .config
            .runtime_for(&spec.game_id)
            .ok_or_else(|| anyhow!("no runtime for {:?}", spec.game_id))?;
        let packs = self.packs.read().await;
        let pack = packs
            .get(&spec.game_id)
            .ok_or_else(|| anyhow!("no game pack installed for {:?}", spec.game_id))?;

        // What this game wants reachable: channel 0 from `default_port`, plus
        // whatever the pack declares as extra ports (`GAMES.md` §3). The pack
        // decides which ports exist; the node decides what they are reachable
        // as.
        let profile = pack.to_profile().map_err(|e| anyhow!("invalid game pack: {e}"))?;
        let wanted = profile.ports();
        let requested: Vec<Option<u16>> = wanted
            .iter()
            .map(|p| {
                if p.channel == game_bridge::framing::CHANNEL_GAME {
                    spec.port
                } else {
                    spec.extra_ports.get(&p.channel).copied()
                }
            })
            .collect();
        let host_ports = {
            let mut ports = self.ports.lock().await;
            ports.acquire(&requested).map_err(|e| anyhow!("{e}"))?
        };
        let published: Vec<PublishedPort> = wanted
            .iter()
            .zip(&host_ports)
            .map(|(p, &host_port)| PublishedPort {
                channel: p.channel,
                container_port: p.port,
                host_port,
                transport: p.transport,
            })
            .collect();

        let started = self
            .docker
            .create_and_start(&spec, runtime, &plan.mounts, &published)
            .await;
        let container_id = match started {
            Ok(id) => id,
            Err(e) => {
                // Give the whole set back, or a failed start leaks a port per
                // channel on every attempt.
                self.ports.lock().await.release_all(&host_ports);
                return Err(e);
            }
        };

        self.specs
            .lock()
            .await
            .insert(spec.instance_id.clone(), spec.clone());

        // Put it on the mesh. A bridge that will not start leaves a server that
        // is reachable on the LAN and not the mesh, which is degraded; failing
        // the create here would leave one that is not reachable at all, which
        // is broken. So this is logged and the instance stands.
        let mut mesh_destination_now = None;
        if let Some(addr) = self.game_address(&spec.instance_id, &profile).await {
            let identity = crate::mesh::identity_path(&self.config.data_root, &spec.instance_id);
            match self
                .mesh
                .start(
                    &spec.instance_id,
                    &profile,
                    &identity,
                    (&addr.0, addr.1),
                    &spec.name,
                    spec.max_players,
                )
                .await
            {
                Ok(status) => mesh_destination_now = status.and_then(|s| s.destination),
                Err(e) => tracing::warn!(
                    instance = %spec.instance_id,
                    error = %format!("{e:#}"),
                    "this server is running but is not on the mesh"
                ),
            }
        }

        Ok(InstanceStatus {
            instance_id: spec.instance_id,
            game_id: spec.game_id,
            name: spec.name,
            state: InstanceState::Running,
            port: published
                .iter()
                .find(|p| p.channel == game_bridge::framing::CHANNEL_GAME)
                .map(|p| p.host_port),
            ports: published
                .iter()
                .map(|p| InstancePort {
                    channel: p.channel,
                    host_port: p.host_port,
                    transport: p.transport,
                })
                .collect(),
            container_id: Some(container_id),
            // Read back by the next `list`; not worth an inspect here.
            container_ip: None,
            mesh_destination: mesh_destination_now,
            // A container created a moment ago. Both are answered by the next
            // `list`, and neither is worth a Docker round trip here.
            uptime_secs: Some(0),
            owner: spec.owner,
            players_now: None,
        })
    }

    pub async fn stop(&self, instance_id: &str) -> Result<()> {
        // Off the mesh first. A bridge still announcing a server whose game has
        // stopped advertises a destination that accepts a link and then relays
        // to a closed port — worse than not being listed at all.
        self.mesh.stop(instance_id).await;
        self.docker.stop(instance_id).await
    }

    /// Restart an instance in place, and put it back on the mesh.
    ///
    /// The container is restarted rather than recreated, so the instance keeps
    /// its ports and its identity — a player's bookmark still works afterwards.
    /// The mesh bridge is torn down first and rebuilt after: the game's UDP
    /// socket goes away across a restart, and a bridge left announcing through
    /// it would relay links into a closed port for as long as the game took to
    /// come back.
    pub async fn restart(&self, instance_id: &str) -> Result<()> {
        self.mesh.stop(instance_id).await;
        self.docker.restart(instance_id).await?;

        let instances = self.list().await?;
        let Some(instance) = instances.iter().find(|i| i.instance_id == instance_id) else {
            return Ok(());
        };
        let packs = self.packs.read().await;
        let Some(pack) = packs.get(&instance.game_id) else { return Ok(()) };
        let profile = pack.to_profile().map_err(|e| anyhow!("{e}"))?;
        let Some(addr) = self.game_address(instance_id, &profile).await else { return Ok(()) };
        let max_players = self
            .specs
            .lock()
            .await
            .get(instance_id)
            .map(|s| s.max_players)
            .unwrap_or(16);
        let identity = crate::mesh::identity_path(&self.config.data_root, instance_id);
        if let Err(e) = self
            .mesh
            .start(
                instance_id,
                &profile,
                &identity,
                (&addr.0, addr.1),
                &instance.name,
                max_players,
            )
            .await
        {
            // Same rule as create: a server on the LAN and not the mesh is
            // degraded, and failing the restart would leave it stopped.
            tracing::warn!(
                instance = %instance_id,
                error = %format!("{e:#}"),
                "restarted, but this server is not back on the mesh"
            );
        }
        Ok(())
    }

    /// Stop and remove, returning the port to the pool.
    pub async fn remove(&self, instance_id: &str) -> Result<()> {
        let held: Vec<u16> = self
            .docker
            .list_managed()
            .await?
            .into_iter()
            .find(|c| c.instance_id == instance_id)
            .map(|c| {
                let mut ports: Vec<u16> = c.ports.iter().map(|p| p.host_port).collect();
                // A container from a build before the port label still has its
                // game port to give back.
                if ports.is_empty() {
                    ports.extend(c.port);
                }
                ports
            })
            .unwrap_or_default();
        self.mesh.stop(instance_id).await;
        // Stopping first is politeness, not correctness: `remove` forces. But a
        // game server that is killed outright loses whatever it was flushing.
        let _ = self.docker.stop(instance_id).await;
        self.docker.remove(instance_id).await?;
        self.ports.lock().await.release_all(&held);
        self.specs.lock().await.remove(instance_id);
        Ok(())
    }

    /// What this node has room for, as **both** control surfaces report it.
    ///
    /// The wire type is shared with the uplink deliberately: the loopback API
    /// and the Reticulum uplink must not be able to answer the same question
    /// differently, and two structs is how that starts.
    ///
    /// The running count comes from `list()`, so an instance mid-create is not
    /// counted yet. That is honest rather than convenient: the container is the
    /// record here as it is for ownership, and a number derived from anything
    /// else would drift from what the node will actually accept.
    pub async fn capacity(&self) -> CapacityResp {
        let running = self
            .list()
            .await
            .map(|instances| {
                instances.iter().filter(|i| i.state == InstanceState::Running).count()
            })
            .unwrap_or(0);
        CapacityResp {
            max_instances: self.config.max_instances,
            running,
            port_range_start: self.config.port_range.start,
            port_range_end: self.config.port_range.end,
        }
    }

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
                    ports: c.ports,
                    container_id: Some(c.container_id),
                    container_ip: c.ip.clone(),
                    // Filled by `list_detailed`; `list` is on the hot path and
                    // does not take the bridge lock.
                    mesh_destination: None,
                    uptime_secs: c.created_unix.and_then(|created| {
                        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
                        u64::try_from(created).ok().map(|c| now.saturating_sub(c))
                    }),
                    // `list` is on the hot path — capacity asks it before every
                    // placement — so it never queries a game. `list_detailed`
                    // is the one that does.
                    players_now: None,
                }
            })
            .collect())
    }

    /// `list`, plus how many players each running instance actually has.
    ///
    /// Separate from `list` because asking costs a UDP round trip per instance
    /// and `capacity` must stay cheap. The queries run concurrently, so the
    /// whole call costs one query timeout rather than one per instance.
    ///
    /// **A game that cannot be asked reports `None`, never `0`.** The pack says
    /// which query protocol a game speaks, and a game that speaks none — or one
    /// that did not answer this time — is unknown. An index reaping on "no
    /// players" would otherwise stop a busy server the moment a query was
    /// dropped.
    pub async fn list_detailed(&self) -> Result<Vec<InstanceStatus>> {
        let mut instances = self.list().await?;
        // A container can stop without the agent stopping it — somebody ran
        // `docker stop`, or the game crashed. Reconcile here, where the truth
        // was just read, rather than trusting that every exit came through us.
        let live: Vec<String> = instances
            .iter()
            .filter(|i| i.state == InstanceState::Running)
            .map(|i| i.instance_id.clone())
            .collect();
        self.mesh.retain_only(&live).await;
        for instance in instances.iter_mut() {
            instance.mesh_destination = self
                .mesh
                .status_of(&instance.instance_id)
                .await
                .and_then(|s| s.destination);
        }
        let queries = instances.iter().map(|i| self.player_count(i));
        let counts = futures_util::future::join_all(queries).await;
        for (instance, count) in instances.iter_mut().zip(counts) {
            instance.players_now = count;
        }
        Ok(instances)
    }

    /// Ask one instance's game how many players it has, if it can be asked.
    ///
    /// The query is aimed at the published port on this node's own loopback:
    /// the game is a container this agent started, not a stranger on the
    /// network.
    async fn player_count(&self, instance: &InstanceStatus) -> Option<u32> {
        if instance.state != InstanceState::Running {
            return None;
        }
        let profile = self.packs.read().await.get(&instance.game_id)?.to_profile().ok()?;
        // The pack names an enum this build implements — never a command. A
        // game declaring no query is unknown, which is the honest answer.
        match profile.query? {
            game_bridge::profile::QueryProtocol::A2s => {
                // The container's own address and the port the game binds
                // *inside* it. Not `127.0.0.1:<host_port>`: this agent is meant
                // to run in a container, where that loopback is its own
                // namespace and the query reaches nothing — every instance
                // would report "could not ask", and reaping would then never
                // touch anything because unknown is deliberately never treated
                // as empty.
                let ip: std::net::IpAddr = instance.container_ip.as_ref()?.parse().ok()?;
                let addr = std::net::SocketAddr::new(ip, profile.default_port);
                game_bridge::a2s::query(addr).await.ok().map(|s| u32::from(s.info.players))
            }
        }
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
