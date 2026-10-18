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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use game_bridge::content::PackContent;
use game_bridge::console::{BotProtocol, ConsoleProtocol};
use game_bridge::GamePack;
use tokio::sync::{Mutex, RwLock};

use crate::config::AgentConfig;
use crate::content::{ProvisionError, Provisioned, Provisioner};
use crate::docker::{DockerRuntime, PublishedPort};
use crate::instance::{InstancePort, InstanceSpec, InstanceState, InstanceStatus};
use crate::ports::PortAllocator;
use crate::store::{ContentRef, InstancePlan, StoreLayout};
use crate::uplink_wire::CapacityResp;

/// How long to wait between attempts to give a freshly created server its
/// bots, and how many times to try. A GoldSrc map load is a second or two on a
/// warm cache and longer on a cold one; the console accepts the lines any time
/// after that, so this is a short poll rather than a deadline.
const BOT_APPLY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
const BOT_APPLY_ATTEMPTS: u32 = 10;

/// Free and total bytes on the filesystem holding `path`.
// statvfs's field types differ by platform (u64 on Linux, u32 and c_ulong on
// macOS), so the conversions are real somewhere even where they are not here.
#[cfg(unix)]
#[allow(clippy::useless_conversion)]
pub fn disk_space(path: &std::path::Path) -> Result<Storage> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .context("the path contains a NUL byte")?;
    // SAFETY: an all-zero statvfs is a valid value of a plain C struct.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a valid, writable
    // statvfs for the call to fill.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("reading free space at {}", path.display()));
    }
    let frsize = u64::from(st.f_frsize);
    Ok(Storage {
        path: path.display().to_string(),
        // f_bavail, not f_bfree: what an unprivileged writer can use.
        free_bytes: u64::from(st.f_bavail) * frsize,
        total_bytes: u64::from(st.f_blocks) * frsize,
    })
}

/// A node runs on Linux (Docker's socket); the agent still builds elsewhere,
/// and there free space is simply not reported. The page hides the line.
#[cfg(not(unix))]
pub fn disk_space(path: &std::path::Path) -> Result<Storage> {
    Err(anyhow!("free space is not read on this platform ({})", path.display()))
}

/// How often the agent looks for update checks that are due. The interval
/// itself is the operator's (`updates.rs`); this is only how soon after it
/// passes a check starts.
pub const UPDATE_CHECK_TICK: Duration = Duration::from_secs(60);

/// Bytes as the page writes them, for sentences an operator reads.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (n as f64, 0);
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

/// How long a backup waits for a running world to finish saving.
const WORLD_SAVE_WAIT: Duration = Duration::from_secs(30);

pub(crate) fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Most log lines one request may ask for.
pub const MAX_LOG_LINES: usize = 2000;

/// Disk space where this node keeps game content.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Storage {
    pub path: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

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
    /// Restart-on-crash bookkeeping (`check_crashes`).
    crashes: Mutex<CrashWatch>,
    /// Sizes of installed copies, keyed by directory and its modification
    /// time. A copy is never changed in place (`content.rs`), so a walk of a
    /// 20 GB tree is done once, not on every poll of the page.
    content_sizes: std::sync::Mutex<HashMap<std::path::PathBuf, (SystemTime, u64)>>,
    /// Which copy new servers use once an update has moved it (`active.rs`).
    active: crate::active::ActiveVersions,
    /// Update checks, snoozes and the disk reserve (`updates.rs`).
    updates: crate::updates::UpdateChecks,
    /// One check at a time: they share steamcmd's `$HOME`.
    check_lock: Mutex<()>,
    /// Downloads, queued one at a time (`install.rs`). Here rather than in
    /// the API's state so an automatic download waits in the same queue as
    /// the operator's.
    installs: Arc<crate::install::Installs>,
    /// The official pack repository's passes (`official.rs`).
    official: Arc<crate::official::Official>,
    /// Moves of servers onto a new copy (`move_instance`): one at a time, the
    /// servers and copies in one now, what happened last, and how long a move
    /// watches and warns.
    move_lock: Mutex<()>,
    moving: std::sync::Mutex<HashMap<String, MoveInFlight>>,
    move_log: std::sync::Mutex<BTreeMap<String, MoveRecord>>,
    move_timing: std::sync::Mutex<MoveTiming>,
    /// Why a scheduled restart is waiting, by server (`scheduled_restarts`).
    restart_waits: std::sync::Mutex<BTreeMap<String, String>>,
    /// Minecraft builds the publishers list, cached (`minecraft.rs`).
    minecraft: crate::minecraft::Catalog,
}

/// A server being moved, and both its copies: neither is deleted while the
/// move is under way, since for a moment no container mounts either.
struct MoveInFlight {
    game_id: String,
    from: String,
    to: String,
}

/// Takes a server out of `Agent::moving` however its move ends.
struct Unpin<'a> {
    moving: &'a std::sync::Mutex<HashMap<String, MoveInFlight>>,
    instance_id: String,
}

impl Drop for Unpin<'_> {
    fn drop(&mut self) {
        self.moving.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.instance_id);
    }
}

/// How long a moved server must keep running before the move counts, and how
/// long players get between the warning and a forced move.
#[derive(Debug, Clone, Copy)]
pub struct MoveTiming {
    pub settle: Duration,
    pub warn: Duration,
}

impl Default for MoveTiming {
    fn default() -> Self {
        Self { settle: Duration::from_secs(30), warn: Duration::from_secs(60) }
    }
}

/// The in-game warning before a forced move. A constant, held to the server
/// name rule by `ConsoleProtocol::say` all the same.
pub const MOVE_WARNING: &str = "This server restarts in about a minute for a game update. It keeps its name and address";

/// Where a move of one server has got to, for the Installed page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MoveRecord {
    pub instance_id: String,
    pub game_id: String,
    /// The copy it is on, and the copy new servers use.
    pub from: String,
    pub to: String,
    #[serde(flatten)]
    pub state: MoveState,
    pub at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MoveState {
    /// Not yet, and why.
    Waiting { why: String },
    /// Players were told; the move follows.
    Warned,
    Moving,
    Moved,
    /// The server did not stay up on the new copy and is back on the old one.
    RolledBack { why: String },
    /// Nothing was touched, and why; or, if `lost`, the server could be put
    /// back on neither copy.
    Failed {
        why: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        lost: bool,
    },
}

/// How a move ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved { from: String, to: String },
    RolledBack { from: String, to: String, why: String },
}

/// The in-game warning before a scheduled restart with players on.
pub const RESTART_WARNING: &str = "This server restarts in about a minute. It keeps its name and address";

/// How often the agent looks at servers' daily restart windows.
pub const SCHEDULE_CHECK_EVERY: Duration = Duration::from_secs(60);

/// One server's backups, for the page.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupSet {
    pub instance_id: String,
    /// False for a server that was removed; its backups stayed.
    pub server_exists: bool,
    pub backups: Vec<crate::backups::Backup>,
}

/// How often the agent looks for backups that are due.
pub const BACKUP_CHECK_EVERY: Duration = Duration::from_secs(60);

/// A server's daily restart, for the page.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScheduleStatus {
    #[serde(flatten)]
    pub stored: crate::schedule::Stored,
    /// Why it has not restarted yet in a window that is open now.
    pub waiting: Option<String>,
}

/// How often the agent looks for servers to move onto a new copy.
pub const MOVE_CHECK_EVERY: Duration = Duration::from_secs(60);

/// How often the agent looks for servers that crashed.
pub const CRASH_CHECK_EVERY: Duration = Duration::from_secs(10);

/// How often servers' own files are measured against their disk limit.
pub const DISK_LIMIT_CHECK_EVERY: Duration = Duration::from_secs(60);
/// Crash restarts allowed per server per hour. A server that dies at boot is
/// started five times and then left stopped, with its last exit on the page.
pub const MAX_CRASH_RESTARTS_PER_HOUR: usize = 5;
/// An orphaned instance directory touched this recently is left alone: a create
/// in progress makes its directories before its container exists, and would
/// otherwise lose them to a cleanup that ran in between.
pub const ORPHAN_MIN_AGE: Duration = Duration::from_secs(600);
/// The marker beside an instance that asks for restart-on-crash. Beside
/// `mesh.identity`, never mounted into the container.
const RESTART_MARKER: &str = "restart-on-crash";

/// What the crash watch remembers. In memory on purpose: an agent restart
/// forgets its counts, and it also starts by marking every exit already on the
/// node as seen, so it never restarts a server that stopped before it ran.
#[derive(Default)]
struct CrashWatch {
    /// Seeded on the first pass; until then nothing is restarted.
    seeded: bool,
    /// The exit already looked at, per instance, by its `finished_at`.
    seen: HashMap<String, String>,
    /// Stopped through this agent since its last start: a stop, never a crash,
    /// whatever its exit code.
    stopped_here: HashSet<String>,
    /// When each crash restart happened.
    restarts: HashMap<String, Vec<Instant>>,
}

impl CrashWatch {
    fn recent(&mut self, id: &str) -> usize {
        let hour = Duration::from_secs(3600);
        let list = self.restarts.entry(id.to_string()).or_default();
        list.retain(|t| t.elapsed() < hour);
        list.len()
    }
}

/// One player, as the game itself reports it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlayerRow {
    pub name: String,
    pub score: i32,
    pub connected_secs: u32,
}

/// What a cleanup of leftover instance directories did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OrphanReport {
    pub count: usize,
    pub bytes: u64,
    /// Too new to touch (`ORPHAN_MIN_AGE`).
    pub skipped_recent: usize,
    /// Could not be removed; the first error says why.
    pub failed: usize,
    pub error: Option<String>,
}

/// One installed copy of a game's files, for the Installed page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InstalledCopy {
    pub game_id: String,
    pub version: String,
    pub bytes: u64,
    /// When the directory was last modified, in unix seconds.
    pub modified_unix: Option<i64>,
    /// `agent.toml` names this version as the game's `content_version`, so
    /// new servers of the game use it.
    pub configured: bool,
    /// Servers whose container mounts this copy, running or stopped.
    pub used_by: Vec<String>,
    /// Neither configured nor mounted: space nothing on this node will use.
    pub unused: bool,
    /// The copy new servers use, of a game this node can update (`steamcmd`).
    pub updatable: bool,
}

/// Where one game's files stand (`Agent::content_state`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ContentState {
    /// The pack's `[content]` driver: `manual`, `archive` or `steamcmd`.
    pub driver: String,
    /// The copy new servers use is on disk.
    pub installed: bool,
    pub version: String,
    /// Where a person puts a `manual` game's files, as this agent sees its
    /// disk. Only while it is not there.
    pub install_dir: Option<String>,
    /// Folders a `manual` copy must have inside it: the pack's writable paths
    /// and the operator's, which servers mount over and cannot create.
    pub must_contain: Vec<String>,
    /// Of those, the ones an installed copy lacks. A server will not start
    /// until they exist.
    pub missing: Vec<String>,
    /// The pack's own words on where to get the files. Shown as text only.
    pub note: Option<String>,
    /// This node may download it by itself (`allow_content_fetch`).
    pub fetch_allowed: bool,
}

/// What an update did, for the Installed page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpdateReport {
    pub from_version: String,
    /// The new version directory, or `None` when there was nothing newer.
    pub to_version: Option<String>,
    pub from_build: Option<String>,
    pub to_build: Option<String>,
    /// What happened to the old copy: `kept`, `removed`, or `waiting` — asked
    /// to be removed while servers still use it, and removed once they are.
    pub old: String,
    /// The servers the old copy is waiting for.
    pub old_used_by: Vec<String>,
}

/// How a check of a copy's files against Valve's ended (`verify_content`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VerifyReport {
    /// The copy that was checked: the one new servers use.
    pub version: String,
    /// Where the repaired copy was kept, when anything differed. Not made the
    /// copy servers use: that is `use_copy`, the operator's call.
    pub repaired_version: Option<String>,
    /// The files that differed, at most [`VERIFY_REPORT_FILES`] of them.
    pub changed: Vec<String>,
    pub changed_count: usize,
    pub from_build: Option<String>,
    pub to_build: Option<String>,
}

/// How many differing files a verify report names; the count is always whole.
pub const VERIFY_REPORT_FILES: usize = 50;

/// Why a copy was not deleted.
#[derive(Debug)]
pub enum DeleteContentError {
    /// The game or version is not an id this node would ever have made a
    /// directory from.
    BadId(crate::store::PlanError),
    NotInstalled,
    /// These servers mount the copy. A stopped server restarts from the same
    /// mount, so stopped ones count.
    InUse(Vec<String>),
    Other(anyhow::Error),
}

impl std::fmt::Display for DeleteContentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadId(e) => write!(f, "not a game or version id: {e}"),
            Self::NotInstalled => write!(f, "that copy is not installed"),
            Self::InUse(ids) => write!(
                f,
                "{} using these files: {}. Remove {} first",
                if ids.len() == 1 { "a server is" } else { "servers are" },
                ids.join(", "),
                if ids.len() == 1 { "it" } else { "them" },
            ),
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

/// Bytes under a directory, not following symlinks.
fn dir_size(path: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(path) else { return 0 };
    rd.filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Year, month and day of a day count since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`), so naming a version needs no date crate.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Every `<game>/<version>` directory under the content root.
fn list_content_dirs(root: &std::path::Path) -> Vec<(String, String)> {
    let names = |dir: &std::path::Path| -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for game in names(root) {
        for version in names(&root.join(&game)) {
            out.push((game.clone(), version));
        }
    }
    out
}

fn recently_touched(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < ORPHAN_MIN_AGE)
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

        // Runtime interfaces live beside the node's other state, so adding a
        // relay from the UI survives a restart.
        let mesh = crate::mesh::MeshBridges::with_store(
            config.mesh.clone(),
            config.data_root.join("mesh-interfaces.json"),
        );
        let config_data_root = config.data_root.clone();
        Ok(Self {
            config,
            layout,
            content,
            docker,
            ports: Mutex::new(ports),
            packs,
            specs: Mutex::new(BTreeMap::new()),
            mesh,
            crashes: Mutex::new(CrashWatch::default()),
            content_sizes: std::sync::Mutex::new(HashMap::new()),
            active: crate::active::ActiveVersions::load(&config_data_root),
            updates: crate::updates::UpdateChecks::load(&config_data_root),
            check_lock: Mutex::new(()),
            installs: Arc::new(crate::install::Installs::new()),
            official: Arc::new(crate::official::Official::new(&config_data_root)),
            move_lock: Mutex::new(()),
            moving: std::sync::Mutex::new(HashMap::new()),
            move_log: std::sync::Mutex::new(BTreeMap::new()),
            move_timing: std::sync::Mutex::new(MoveTiming::default()),
            restart_waits: std::sync::Mutex::new(BTreeMap::new()),
            minecraft: crate::minecraft::Catalog::default(),
        })
    }

    /// The official pack repository on this node.
    pub fn official(&self) -> Arc<crate::official::Official> {
        self.official.clone()
    }

    /// Point the official repository elsewhere: a test's own signed folder.
    #[doc(hidden)]
    pub fn set_official(&mut self, official: crate::official::Official) {
        self.official = Arc::new(official);
    }

    /// This node's downloads and their queue.
    pub fn installs(&self) -> Arc<crate::install::Installs> {
        self.installs.clone()
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

    /// Where this node can actually reach a running instance's game: the games
    /// network's gateway and the game's published host ports
    /// ([`GameReach`](crate::mesh::GameReach)).
    ///
    /// Never `127.0.0.1:<host_port>`: the agent is meant to run in a container,
    /// where that loopback is its own namespace, and a mesh bridge pointed at
    /// it announces a destination, accepts a link, and forwards every packet
    /// into nothing. And never the container's own address any more: game
    /// containers are kept apart on `GAMES_NETWORK`, where nothing but the
    /// host reaches a container's address. The gateway is this host's own
    /// address, reachable from the host and from any container
    /// (`DockerRuntime::games_gateway`).
    async fn game_address(&self, instance_id: &str) -> Option<crate::mesh::GameReach> {
        let containers = self.docker.list_managed().await.ok()?;
        let container = containers.iter().find(|c| c.instance_id == instance_id)?;
        let host = match self.docker.games_gateway().await {
            Ok(host) => host,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "cannot reach game servers: no games network");
                return None;
            }
        };
        crate::mesh::GameReach::from_ports(host, &container.ports, container.port)
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
            let Some(addr) = self.game_address(&instance.instance_id).await else {
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
            // Only a seed for the first announce; the bridge's A2S poller
            // replaces it with the map the game is really on. A restarted agent
            // has no spec and so seeds nothing, which is correct — it should
            // report what it observes, not what it once asked for.
            let seed_map = self
                .specs
                .lock()
                .await
                .get(&instance.instance_id)
                .and_then(|s| s.map.clone());
            if let Err(e) = self
                .mesh
                .start(
                    &instance.instance_id,
                    &profile,
                    &identity,
                    &addr,
                    crate::mesh::MeshAnnounce {
                        name: &instance.name,
                        max_players,
                        map: seed_map.as_deref(),
                    },
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
        self.plan_and_check_on(spec, None).await
    }

    /// [`plan_and_check`](Self::plan_and_check) on a given copy of the game
    /// rather than the one new servers use. A move plans on the new copy and,
    /// if it has to roll back, on the old one.
    async fn plan_and_check_on(&self, spec: &InstanceSpec, version: Option<&str>) -> Result<InstancePlan> {
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

        check_spec_against_pack(spec, pack)?;
        let content = ContentRef {
            game_id: spec.game_id.clone(),
            version: match version {
                Some(v) => v.to_string(),
                None => self.version_for(&spec.game_id, spec.options.content_version.as_deref(), &runtime.content_version),
            },
        };
        let plan = self
            .layout
            .plan_instance(
                &spec.instance_id,
                &content,
                &runtime.content_root,
                &self.writable_paths_for(&spec.game_id, pack),
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
                "This pack's \"{}\" driver could fetch it, but this node has allow_content_fetch off — install it by hand or turn fetching on once you trust the pack",
                content.driver_name()
            ),
            (false, _) => "This pack's content driver is \"manual\": install the game there yourself"
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
        self.ensure_content_tracked(game_id, None, None).await
    }

    /// [`ensure_content`](Self::ensure_content), reporting progress and
    /// honouring a cancel (`install.rs`).
    pub async fn ensure_content_tracked(
        &self,
        game_id: &str,
        version: Option<&str>,
        progress: Option<&crate::content::InstallProgress>,
    ) -> Result<Provisioned, ProvisionError> {
        let packs = self.packs.read().await;
        let pack = packs.get(game_id).ok_or_else(|| {
            ProvisionError::Io(std::io::Error::other(format!(
                "no game pack installed for {game_id:?}"
            )))
        })?;
        let runtime = self.config.runtime_for(game_id).ok_or_else(|| {
            ProvisionError::Io(std::io::Error::other(format!(
                "this node has no runtime configured for {game_id:?}, so there is no content_version to install into"
            )))
        })?;
        if version.is_some() && !matches!(pack.content, PackContent::Minecraft {}) {
            return Err(ProvisionError::Listing(format!("{game_id} has one copy per node; a version cannot be picked")));
        }
        let content = ContentRef {
            game_id: game_id.to_string(),
            version: self.version_for(game_id, version, &runtime.content_version),
        };
        let provisioned = self
            .content
            .ensure_tracked(&content, &pack.content, Some(&self.docker), progress)
            .await?;
        self.create_writable_mountpoints(&content, &self.writable_paths_for(game_id, pack))?;
        Ok(provisioned)
    }

    /// Every path this node makes writable inside a game's content: the pack's,
    /// plus whatever the operator added.
    ///
    /// The operator's exist because a pack cannot know what a node installed
    /// into its own copy of a game — YaPB caching its pathfinding under
    /// `addons/yapb/data` is the case that motivated it. Both lists are
    /// validated identically further down; this only decides what is on the
    /// list.
    fn writable_paths_for(&self, game_id: &str, pack: &GamePack) -> Vec<String> {
        let mut paths = pack.writable_paths.clone();
        if let Some(runtime) = self.config.runtime_for(game_id) {
            for extra in &runtime.writable_paths {
                if !paths.contains(extra) {
                    paths.push(extra.clone());
                }
            }
        }
        paths
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
    pub async fn content_is_missing(&self, game_id: &str, version: Option<&str>) -> bool {
        let Some(runtime) = self.config.runtime_for(game_id) else {
            return false;
        };
        if !self.packs.read().await.contains_key(game_id) {
            return false;
        }
        let content = ContentRef {
            game_id: game_id.to_string(),
            version: self.version_for(game_id, version, &runtime.content_version),
        };
        match self.layout.content_dir(&content) {
            Ok(dir) => !dir.is_dir(),
            Err(_) => false,
        }
    }

    /// Create and start an instance.
    pub async fn create(&self, spec: InstanceSpec) -> Result<InstanceStatus> {
        self.create_on(spec, None, false).await
    }

    /// Create on a given copy (`None`: the one new servers use). With
    /// `ports_held`, every port in the spec is already this instance's in the
    /// allocator, kept across a move's recreate so nothing else can take it in
    /// between, and a failure does not give them back.
    async fn create_on(&self, spec: InstanceSpec, version: Option<&str>, ports_held: bool) -> Result<InstanceStatus> {
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

        let plan = self.plan_and_check_on(&spec, version).await?;
        let verdict = self.resource_check(&spec.game_id, spec.options.memory, spec.options.cpus).await;
        if verdict.level == crate::resources::ResourceLevel::Refuse {
            return Err(anyhow!("{}", verdict.messages.join(" ")));
        }
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
        let host_ports = if ports_held {
            requested
                .iter()
                .map(|p| p.ok_or_else(|| anyhow!("this server has no port for every channel its game now declares")))
                .collect::<Result<Vec<u16>>>()?
        } else {
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
                // channel on every attempt. Not a move's: it tries the old
                // copy on the same ports next.
                if !ports_held {
                    self.ports.lock().await.release_all(&host_ports);
                }
                return Err(e);
            }
        };

        self.specs
            .lock()
            .await
            .insert(spec.instance_id.clone(), spec.clone());

        // Bots the spec asked for, applied over the console rather than the
        // command line.
        //
        // **Not a shortcut — the command line does not work.** A GoldSrc server
        // started with `+bot_quota 4 +bot_join_after_player 0` comes up with no
        // bots: the bot manager is not there to take those cvars when the
        // command line is parsed. So the number has to be sent once the map is
        // loaded, which is seconds after this function returns.
        //
        // In the background, and never fatal: the server is running and
        // joinable either way, and a create that failed because bots were slow
        // would be a worse outcome than a server that starts empty. The retries
        // exist because "the map is loaded" is the condition, and nothing here
        // can observe it directly.
        if spec.options.stopped {
            return Ok(self.created_status(spec, published, container_id, InstanceState::Stopped, None));
        }
        self.apply_bans_later(&spec.instance_id, &spec.game_id).await;

        if let Some(count) = spec.bots {
            // The lines are built here, where the pack is already in hand, so
            // the background task carries data and not a pack lookup — and a
            // count this build would refuse fails the create rather than a
            // task nobody is watching.
            let bots = self.bot_protocol(&spec.game_id).await;
            if let Some(bots) = bots {
                let lines = bots.quota_lines(count).map_err(|e| anyhow!("{e}"))?;
                let docker = self.docker.clone();
                let instance_id = spec.instance_id.clone();
                tokio::spawn(async move {
                    for attempt in 1..=BOT_APPLY_ATTEMPTS {
                        tokio::time::sleep(BOT_APPLY_INTERVAL).await;
                        let mut sent = true;
                        for line in &lines {
                            if let Err(e) = docker.send_console_line(&instance_id, line).await {
                                sent = false;
                                if attempt == BOT_APPLY_ATTEMPTS {
                                    tracing::warn!(
                                        instance = %instance_id,
                                        error = %format!("{e:#}"),
                                        "this server is running but never took its bots"
                                    );
                                }
                                break;
                            }
                        }
                        if sent {
                            return;
                        }
                    }
                });
            }
        }

        // Put it on the mesh. A bridge that will not start leaves a server that
        // is reachable on the LAN and not the mesh, which is degraded; failing
        // the create here would leave one that is not reachable at all, which
        // is broken. So this is logged and the instance stands.
        let mut mesh_destination_now = None;
        if let Some(addr) = self.game_address(&spec.instance_id).await {
            let identity = crate::mesh::identity_path(&self.config.data_root, &spec.instance_id);
            match self
                .mesh
                .start(
                    &spec.instance_id,
                    &profile,
                    &identity,
                    &addr,
                    crate::mesh::MeshAnnounce {
                        name: &spec.name,
                        max_players: spec.max_players,
                        map: spec.map.as_deref(),
                    },
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

        self.crashes.lock().await.stopped_here.remove(&spec.instance_id);
        if spec.restart_on_crash {
            if let Err(e) = self.set_restart_on_crash(&spec.instance_id, true).await {
                tracing::warn!(instance = %spec.instance_id, error = %format!("{e:#}"), "restart-on-crash could not be turned on");
            }
        }

        Ok(self.created_status(spec, published, container_id, InstanceState::Running, mesh_destination_now))
    }

    /// What `create` reports for a container it just made.
    fn created_status(
        &self,
        spec: InstanceSpec,
        published: Vec<PublishedPort>,
        container_id: String,
        state: InstanceState,
        mesh_destination_now: Option<String>,
    ) -> InstanceStatus {
        InstanceStatus {
            instance_id: spec.instance_id,
            game_id: spec.game_id,
            name: spec.name,
            state,
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
            max_players_now: None,
            last_exit: None,
            restart_on_crash: None,
            crash_restarts: None,
            map_now: None,
            bots: spec.bots,
        }
    }

    pub async fn stop(&self, instance_id: &str) -> Result<()> {
        // Off the mesh first. A bridge still announcing a server whose game has
        // stopped advertises a destination that accepts a link and then relays
        // to a closed port — worse than not being listed at all.
        self.mesh.stop(instance_id).await;
        self.docker.stop(instance_id).await?;
        self.crashes.lock().await.stopped_here.insert(instance_id.to_string());
        Ok(())
    }

    /// The maps this node actually has for a game, read off the content copy.
    ///
    /// Two reasons this is the node's answer and not the pack's: a pack lists
    /// no maps (it says only which *directory* they live in), and what is
    /// installed is a property of this machine — a node with partial content,
    /// or with maps an operator added by hand, must report what is there rather
    /// than what a manifest claims.
    ///
    /// **The pack's `maps_dir` is validated before anything is read**, with the
    /// same rule that governs `writable_paths`: a relative path with no `..`,
    /// no absolute root, no backslash. So a pack can point this at a directory
    /// inside the game's own install and nowhere else, and the worst it can do
    /// is name a directory that does not exist.
    ///
    /// An empty list is a fine answer — no content yet, or a game whose maps
    /// are not files. Absence of `maps_dir` is an error, because a caller that
    /// asked deserves to know the difference between "no maps" and "this game
    /// cannot be asked".
    pub async fn available_maps(&self, game_id: &str) -> Result<Vec<String>> {
        let packs = self.packs.read().await;
        let pack = packs
            .get(game_id)
            .ok_or_else(|| anyhow!("no game pack installed for {game_id:?}"))?;
        let maps_dir = pack.maps_dir.clone().ok_or_else(|| {
            anyhow!("the {game_id:?} pack does not say where its maps live")
        })?;
        let runtime = self.config.runtime_for(game_id).ok_or_else(|| {
            anyhow!("this node has no runtime configured for {game_id:?}")
        })?;
        let version = self.active_version_of(game_id, &runtime.content_version);
        drop(packs);

        crate::store::validate_writable_path(&maps_dir)
            .map_err(|e| anyhow!("the {game_id:?} pack's maps_dir is not usable: {e}"))?;

        let content = ContentRef { game_id: game_id.to_string(), version };
        let dir = self
            .layout
            .content_dir(&content)
            .map_err(|e| anyhow!("{e}"))?
            .join(&maps_dir);

        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            // Not installed yet is "no maps", not a failure: the UI asks for
            // this before a game has necessarily been downloaded.
            Err(_) => return Ok(Vec::new()),
        };
        let mut maps: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                // A map is its file name without the extension. Case-insensitive
                // because GoldSrc installs are not consistent about it.
                let stem = name.strip_suffix(".bsp").or_else(|| name.strip_suffix(".BSP"))?;
                // The same rule the map name has to pass to be used at all, so
                // the list can never offer something the server would refuse.
                game_bridge::console::validate_map_name(stem).ok()?;
                Some(stem.to_string())
            })
            .collect();
        maps.sort_by_key(|m| m.to_ascii_lowercase());
        maps.dedup();
        Ok(maps)
    }

    /// Change a running server's map, without restarting it.
    ///
    /// The distinction is the whole feature: recreating the container with a
    /// different `GPP_MAP` would also work, and would drop every player and
    /// take the game's ports and mesh destination down with it. `changelevel`
    /// keeps the connections.
    ///
    /// **The pack decides whether this is possible at all.** A game whose pack
    /// declares no `console` is refused by name, because the alternative is
    /// guessing a command and typing it at a stranger's dedicated server. The
    /// pack supplies a protocol; `console.rs` supplies the words
    /// (`crates/game-bridge/src/console.rs`).
    pub async fn change_map(&self, instance_id: &str, map: &str) -> Result<String> {
        let instances = self.list().await?;
        let instance = instances
            .iter()
            .find(|i| i.instance_id == instance_id)
            .ok_or_else(|| anyhow!("no instance {instance_id:?} on this node"))?;

        let packs = self.packs.read().await;
        let pack = packs.get(&instance.game_id).ok_or_else(|| {
            anyhow!(
                "no game pack installed for {:?}, so this node cannot tell its server anything",
                instance.game_id
            )
        })?;
        let console: ConsoleProtocol = pack
            .console
            .ok_or_else(|| {
                anyhow!(
                    "the {:?} pack declares no console, so this node has no way to change its map without restarting the server",
                    instance.game_id
                )
            })?
            .into();
        // The map name is judged before it becomes a line, and a name that does
        // not pass is refused rather than rewritten: a silently corrected map
        // name loads the wrong map and reads as a game bug.
        let line = console.change_map(map).map_err(|e| anyhow!("{e}"))?;
        drop(packs);

        self.docker.send_console_line(instance_id, &line).await?;
        Ok(line)
    }

    /// Send a line the operator typed to a running server's console
    /// (`ROADMAP.md` item 7). Only for a game whose pack declares a console,
    /// so the line reaches something that reads it; held to one line by
    /// `validate_console_line`. Logged, since it is someone typing at a game
    /// server. The uplink has no way to call this: an index never types at a
    /// node's servers.
    pub async fn send_console(&self, instance_id: &str, line: &str) -> Result<()> {
        game_bridge::console::validate_console_line(line).map_err(|e| anyhow!("{e}"))?;
        let instances = self.list().await?;
        let instance = instances
            .iter()
            .find(|i| i.instance_id == instance_id)
            .ok_or_else(|| anyhow!("no instance {instance_id:?} on this node"))?;
        let has_console = self.packs.read().await.get(&instance.game_id).is_some_and(|p| p.console.is_some());
        if !has_console {
            return Err(anyhow!(
                "the {:?} pack declares no console, so this node does not know that its server reads one",
                instance.game_id
            ));
        }
        self.docker.send_console_line(instance_id, line).await?;
        tracing::info!(instance = %instance_id, line = %line, "console line sent by the operator");
        Ok(())
    }

    /// The console protocol of an instance's game, or why there is none.
    async fn console_of(&self, instance_id: &str) -> Result<(String, ConsoleProtocol)> {
        let game_id = self
            .game_of(instance_id)
            .await
            .ok_or_else(|| anyhow!("no instance {instance_id:?} on this node"))?;
        let console = self
            .packs
            .read()
            .await
            .get(&game_id)
            .and_then(|p| p.console)
            .ok_or_else(|| anyhow!("the {game_id:?} pack declares no console, so this node cannot tell its server anything"))?;
        Ok((game_id, console.into()))
    }

    /// Which game an instance runs, read off its container.
    async fn game_of(&self, instance_id: &str) -> Option<String> {
        self.docker
            .list_managed()
            .await
            .ok()?
            .into_iter()
            .find(|c| c.instance_id == instance_id)
            .and_then(|c| c.game_id)
    }

    fn instance_dir(&self, instance_id: &str) -> Result<std::path::PathBuf> {
        self.layout.instance_dir(instance_id).map_err(|e| anyhow!("{e:?}"))
    }

    /// Kick, ban, unban, op or deop a player (`MINECRAFT.md` §4). The words
    /// are the game's console protocol's; the target and reason are judged
    /// there and refused, never rewritten. A ban on a Steam engine is also
    /// written to the node's record, because the game cannot keep it, and an
    /// unban is taken out of it; both work on a stopped server too, and reach
    /// the game at its next start. Loopback API only.
    pub async fn moderate(
        &self,
        instance_id: &str,
        action: game_bridge::console::PlayerAction,
        target: &str,
        reason: Option<&str>,
    ) -> Result<Vec<String>> {
        use game_bridge::console::{BanList, PlayerAction};
        let (_, console) = self.console_of(instance_id).await?;
        let lines = console.moderation_lines(action, target, reason).map_err(|e| anyhow!("{e}"))?;
        let running = self.ensure_instance(instance_id).await?.state == InstanceState::Running;
        if console.ban_list() == BanList::NodeRecord {
            let dir = self.instance_dir(instance_id)?;
            match action {
                PlayerAction::Ban => crate::bans::add(
                    &dir,
                    crate::bans::Ban {
                        target: target.to_string(),
                        reason: reason.map(str::trim).filter(|r| !r.is_empty()).map(str::to_string),
                        at: Some(now_unix() as u64),
                    },
                )?,
                PlayerAction::Unban => crate::bans::remove(&dir, target)?,
                _ => {}
            }
        }
        if running {
            for line in &lines {
                self.docker.send_console_line(instance_id, line).await?;
            }
        } else if console.ban_list() != BanList::NodeRecord || !matches!(action, PlayerAction::Ban | PlayerAction::Unban) {
            return Err(anyhow!("start the server first: it is not running, so it cannot be told anything"));
        }
        tracing::info!(instance = %instance_id, action = action.as_str(), target = %target, "moderation");
        Ok(lines)
    }

    /// What a server's moderation page shows: which actions its game has,
    /// what names a player for each, the ban list, and the operators.
    pub async fn moderation_view(&self, instance_id: &str) -> Result<serde_json::Value> {
        use game_bridge::console::{BanList, PlayerAction};
        let (_, console) = self.console_of(instance_id).await?;
        let dir = self.instance_dir(instance_id)?;
        let server_dir = dir.join("writable").join("server");
        let (banned, ops) = match console.ban_list() {
            BanList::NodeRecord => (crate::bans::read(&dir)?, None),
            BanList::GameFile => (
                crate::bans::read_minecraft_list(&server_dir.join("banned-players.json"))?,
                Some(crate::bans::read_minecraft_list(&server_dir.join("ops.json"))?),
            ),
        };
        let actions: Vec<serde_json::Value> = [
            PlayerAction::Kick,
            PlayerAction::Ban,
            PlayerAction::Unban,
            PlayerAction::Op,
            PlayerAction::Deop,
        ]
        .into_iter()
        .filter(|a| console.supports(*a))
        .map(|a| serde_json::json!({ "action": a, "target": console.target_kind(a) }))
        .collect();
        Ok(serde_json::json!({
            "actions": actions,
            "ban_list": console.ban_list(),
            "banned": banned,
            "ops": ops,
        }))
    }

    /// Send a node-kept ban list to a server once it has started, in the
    /// background, like bots: the console is not listening the moment the
    /// container starts.
    async fn apply_bans_later(&self, instance_id: &str, game_id: &str) {
        let Some(console) = self.packs.read().await.get(game_id).and_then(|p| p.console) else { return };
        let console: ConsoleProtocol = console.into();
        let Ok(dir) = self.instance_dir(instance_id) else { return };
        let ids: Vec<String> = match crate::bans::read(&dir) {
            Ok(bans) => bans.into_iter().map(|b| b.target).collect(),
            Err(e) => {
                tracing::warn!(instance = %instance_id, error = %e, "the ban list could not be read; no bans were sent");
                return;
            }
        };
        let lines = console.reapply_bans(&ids);
        if lines.is_empty() {
            return;
        }
        let docker = self.docker.clone();
        let instance_id = instance_id.to_string();
        tokio::spawn(async move {
            for attempt in 1..=BOT_APPLY_ATTEMPTS {
                tokio::time::sleep(BOT_APPLY_INTERVAL).await;
                let mut sent = true;
                for line in &lines {
                    if docker.send_console_line(&instance_id, line).await.is_err() {
                        sent = false;
                        break;
                    }
                }
                if sent {
                    return;
                }
                if attempt == BOT_APPLY_ATTEMPTS {
                    tracing::warn!(instance = %instance_id, "this server is running but never took its ban list");
                }
            }
        });
    }

    /// Whether a server's memory and CPU fit this host (`resources.rs`).
    pub async fn resource_check(
        &self,
        game_id: &str,
        memory: Option<crate::instance::MemorySpec>,
        cpus: Option<u16>,
    ) -> crate::resources::ResourceVerdict {
        let host = std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| parse_meminfo(&m)).map(|(total, available)| {
            crate::resources::HostResources {
                total_bytes: total,
                available_bytes: available,
                cpus: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1),
            }
        });
        let ceiling = self.config.runtime_for(game_id).and_then(|r| r.memory_limit_bytes);
        crate::resources::judge(host, memory, cpus, ceiling)
    }

    /// Minecraft builds a person can pick for a server type, newest first.
    pub async fn minecraft_versions(
        &self,
        server: game_bridge::minecraft::MinecraftServer,
    ) -> Result<Vec<crate::minecraft::VersionChoice>> {
        self.minecraft.versions(server).await.map_err(|e| anyhow!("{e}"))
    }

    /// Where a world upload for a **stopped** server is written while it
    /// arrives, held to the disk reserve like a backup upload.
    pub async fn world_upload_path(&self, instance_id: &str, bytes: Option<u64>) -> Result<std::path::PathBuf> {
        let c = self.ensure_instance(instance_id).await?;
        if c.state == InstanceState::Running {
            return Err(anyhow!("stop the server first: a world cannot be swapped while it runs"));
        }
        if let Some(bytes) = bytes {
            self.room_for(bytes.saturating_mul(2)).await.map_err(|e| anyhow!("{e}"))?;
        }
        let dir = self.config.data_root.join("staging");
        std::fs::create_dir_all(&dir)?;
        let mut seed = [0u8; 8];
        getrandom::getrandom(&mut seed).map_err(|e| anyhow!("no randomness for an upload name: {e}"))?;
        Ok(dir.join(format!("world-upload-{}.partial", hex::encode(seed))))
    }

    /// Swap an uploaded world in (`world.rs`), after backing up what the
    /// server has: no world is ever deleted by an import. The upload is
    /// removed either way.
    pub async fn import_world(&self, instance_id: &str, upload: &std::path::Path) -> Result<serde_json::Value> {
        let result = self.import_world_inner(instance_id, upload).await;
        let _ = std::fs::remove_file(upload);
        result
    }

    async fn import_world_inner(&self, instance_id: &str, upload: &std::path::Path) -> Result<serde_json::Value> {
        let c = self.ensure_instance(instance_id).await?;
        if c.state == InstanceState::Running {
            return Err(anyhow!("stop the server first: a world cannot be swapped while it runs"));
        }
        let is_minecraft = self
            .packs
            .read()
            .await
            .get(c.game_id.as_deref().unwrap_or_default())
            .is_some_and(|p| matches!(p.content, PackContent::Minecraft {}));
        if !is_minecraft {
            return Err(anyhow!("only a Minecraft server takes a world"));
        }
        let server_dir = self.instance_dir(instance_id)?.join("writable").join("server");
        let had_world = crate::world::WORLD_DIRS.iter().any(|d| server_dir.join(d).exists());
        let backup = if had_world { Some(self.create_backup(instance_id).await?.name) } else { None };
        let mut seed = [0u8; 8];
        getrandom::getrandom(&mut seed).map_err(|e| anyhow!("no randomness for a staging name: {e}"))?;
        let staging = self.config.data_root.join("staging").join(format!("world-{}", hex::encode(seed)));
        let archive = upload.to_path_buf();
        let placed = tokio::task::spawn_blocking(move || crate::world::import(&archive, &server_dir, &staging))
            .await?
            .map_err(|e| anyhow!("{e}"))?;
        tracing::info!(instance = %instance_id, backup = ?backup, "imported a world");
        Ok(serde_json::json!({ "placed": placed, "backup": backup }))
    }

    /// Which bot implementation this node can drive for a game, if any.
    ///
    /// The node's config wins over the pack. A pack may only name bots the game
    /// ships (`PackBots`), so the two never contradict each other about the
    /// same thing: the pack says "Condition Zero has Z-Bot", the operator says
    /// "this copy of Counter-Strike has YaPB in it", and only the second is a
    /// fact about this machine.
    async fn bot_protocol(&self, game_id: &str) -> Option<BotProtocol> {
        if let Some(bots) = self.config.runtime_for(game_id).and_then(|r| r.bots) {
            return Some(bots);
        }
        let packs = self.packs.read().await;
        packs.get(game_id).and_then(|p| p.bots).map(Into::into)
    }

    /// Set how many bots a running server holds, without restarting it.
    ///
    /// Same shape as [`change_map`](Self::change_map) and refused on the same
    /// two grounds: a pack that declares no bot implementation, and a container
    /// with no console to talk to. What a node sends is a **quota**, so calling
    /// this twice with 4 leaves four bots and calling it with 0 empties the
    /// server — a caller never has to know what is already there.
    ///
    /// The count is a number the console module formats. A pack contributes the
    /// protocol and nothing else, exactly as it does for a map change.
    pub async fn set_bots(&self, instance_id: &str, count: u8) -> Result<Vec<String>> {
        let instances = self.list().await?;
        let instance = instances
            .iter()
            .find(|i| i.instance_id == instance_id)
            .ok_or_else(|| anyhow!("no instance {instance_id:?} on this node"))?;

        let bots = self.bot_protocol(&instance.game_id).await.ok_or_else(|| {
            anyhow!(
                "nothing on this node can add bots to {:?}. A pack names only bots the game \
                 ships — Condition Zero's Z-Bot — and Counter-Strike 1.6 is the case worth \
                 knowing: its server library carries Z-Bot and runs it only as Condition Zero. \
                 A bot you installed yourself, like YaPB, is declared by this node instead: \
                 bots = \"yapb\" in [games.{}]",
                instance.game_id,
                instance.game_id
            )
        })?;
        let packs = self.packs.read().await;
        let pack = packs.get(&instance.game_id).ok_or_else(|| {
            anyhow!(
                "no game pack installed for {:?}, so this node cannot tell its server anything",
                instance.game_id
            )
        })?;
        // A separate refusal from the one above, because it is a different
        // thing for an operator to fix: bots exist, and the pack named no
        // console to ask for them over.
        if pack.console.is_none() {
            return Err(anyhow!(
                "the {:?} pack declares no console, so this node has no way to ask for bots",
                instance.game_id
            ));
        }
        let lines = bots.quota_lines(count).map_err(|e| anyhow!("{e}"))?;
        drop(packs);

        for line in &lines {
            self.docker.send_console_line(instance_id, line).await?;
        }
        // Remembered so a restart, which recreates nothing but does re-read the
        // spec's environment, does not quietly come back with a different
        // number of bots than the operator last asked for.
        if let Some(spec) = self.specs.lock().await.get_mut(instance_id) {
            spec.bots = Some(count);
        }
        Ok(lines)
    }

    /// Why a server may not run, if its own files are over its game's
    /// `writable_limit_bytes`. `None` when under it, when the game has no
    /// limit, and when the server is unknown here.
    async fn over_disk_limit(&self, instance_id: &str) -> Option<String> {
        let game_id = self.docker.list_managed().await.ok()?.into_iter().find(|c| c.instance_id == instance_id)?.game_id?;
        let limit = self.config.runtime_for(&game_id)?.writable_limit_bytes?;
        let writable = self.layout.instance_dir(instance_id).ok()?.join("writable");
        let used = tokio::task::spawn_blocking(move || dir_size(&writable)).await.ok()?;
        (used > limit).then(|| {
            format!(
                "{instance_id:?} keeps {} in its own files, over this node's {} limit for {game_id:?}; \
                 delete some of them, or raise writable_limit_bytes in [games.{game_id}]",
                human_bytes(used),
                human_bytes(limit)
            )
        })
    }

    /// Stop every running server whose own files have grown past its game's
    /// `writable_limit_bytes`, so one server cannot fill the disk the others
    /// need. A stop, never a deletion, and it stays stopped: `restart` and
    /// `start` refuse while it is over, and the crash watch reads it as a
    /// stop. Returns what it stopped.
    pub async fn check_disk_limits(&self) -> Vec<String> {
        let Ok(instances) = self.list().await else { return Vec::new() };
        let mut stopped = Vec::new();
        for instance in instances.iter().filter(|i| i.state == InstanceState::Running) {
            let Some(reason) = self.over_disk_limit(&instance.instance_id).await else { continue };
            match self.stop(&instance.instance_id).await {
                Ok(()) => {
                    tracing::warn!(instance = %instance.instance_id, "{reason}; stopped");
                    stopped.push(instance.instance_id.clone());
                }
                Err(e) => tracing::warn!(instance = %instance.instance_id, error = %format!("{e:#}"), "over its disk limit, and could not be stopped"),
            }
        }
        stopped
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
        // Before anything stops: a server over its disk limit stays as it is.
        if let Some(over) = self.over_disk_limit(instance_id).await {
            return Err(anyhow!("{over}"));
        }
        self.mesh.stop(instance_id).await;
        self.docker.restart(instance_id).await?;
        self.crashes.lock().await.stopped_here.remove(instance_id);
        if let Some(game_id) = self.game_of(instance_id).await {
            self.apply_bans_later(instance_id, &game_id).await;
        }

        let instances = self.list().await?;
        let Some(instance) = instances.iter().find(|i| i.instance_id == instance_id) else {
            return Ok(());
        };
        let packs = self.packs.read().await;
        let Some(pack) = packs.get(&instance.game_id) else { return Ok(()) };
        let profile = pack.to_profile().map_err(|e| anyhow!("{e}"))?;
        let Some(addr) = self.game_address(instance_id).await else { return Ok(()) };
        let max_players = self
            .specs
            .lock()
            .await
            .get(instance_id)
            .map(|s| s.max_players)
            .unwrap_or(16);
        let seed_map = self.specs.lock().await.get(instance_id).and_then(|s| s.map.clone());
        let identity = crate::mesh::identity_path(&self.config.data_root, instance_id);
        if let Err(e) = self
            .mesh
            .start(
                instance_id,
                &profile,
                &identity,
                &addr,
                crate::mesh::MeshAnnounce {
                    name: &instance.name,
                    max_players,
                    map: seed_map.as_deref(),
                },
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

    /// Start a stopped instance again: [`restart`](Self::restart), but refused
    /// while it runs, so a caller who means "turn it back on" can never drop
    /// the players of a server that was already up. Same container, so the
    /// same ports, files and mesh identity.
    pub async fn start(&self, instance_id: &str) -> Result<()> {
        let instances = self.list().await?;
        let instance = instances
            .iter()
            .find(|i| i.instance_id == instance_id)
            .ok_or_else(|| anyhow!("no instance {instance_id:?} on this node"))?;
        if instance.state == InstanceState::Running {
            return Err(anyhow!("{instance_id:?} is already running"));
        }
        self.restart(instance_id).await
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
        // The server may have been the last one holding an old copy the
        // operator asked to remove after an update.
        self.sweep_old_copies().await;
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
        let memory = std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| parse_meminfo(&m));
        CapacityResp {
            max_instances: self.config.max_instances,
            running,
            port_range_start: self.config.port_range.start,
            port_range_end: self.config.port_range.end,
            memory_total_bytes: memory.map(|(total, _)| total),
            memory_available_bytes: memory.map(|(_, available)| available),
            cpus: std::thread::available_parallelism().ok().and_then(|n| u32::try_from(n.get()).ok()),
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
                    max_players_now: None,
                    last_exit: None,
                    restart_on_crash: None,
                    crash_restarts: None,
                    map_now: None,
                    // This run's memory of what was asked for. A restarted
                    // agent has none, exactly like `spec` itself — the
                    // container is the record for ports and ownership, and
                    // nothing writes a bot count on it.
                    bots: spec.and_then(|s| s.bots),
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
        // One query per instance, and it already carries the map — asking
        // twice for two fields of the same A2S reply would double the traffic
        // to every game on the node for nothing.
        let queries = instances.iter().map(|i| self.live_stats(i));
        let stats = futures_util::future::join_all(queries).await;
        for (instance, s) in instances.iter_mut().zip(stats) {
            instance.players_now = s.as_ref().map(|(p, _, _)| *p);
            instance.max_players_now = s.as_ref().map(|(_, m, _)| *m);
            instance.map_now = s.map(|(_, _, m)| m);
        }
        let mut crashes = self.crashes.lock().await;
        for instance in instances.iter_mut() {
            instance.restart_on_crash = Some(self.restart_on_crash(&instance.instance_id));
            instance.crash_restarts = Some(crashes.recent(&instance.instance_id) as u32);
        }
        drop(crashes);
        // Only a stopped server has an ending worth showing, and there are few.
        for instance in instances.iter_mut().filter(|i| i.state == InstanceState::Stopped) {
            if let Some(cid) = instance.container_id.as_deref() {
                instance.last_exit = self.docker.last_exit(cid).await.ok().flatten();
            }
        }
        Ok(instances)
    }

    /// The last lines a server wrote, for a person working out why it will not
    /// start. Capped so a request cannot ask Docker for a game's whole history.
    pub async fn logs(&self, instance_id: &str, lines: usize) -> Result<String> {
        self.docker.logs_tail(instance_id, lines.clamp(1, MAX_LOG_LINES)).await
    }

    /// Free and total space on the filesystem holding `data_root`, where game
    /// content is installed. A full disk otherwise surfaces only as a failed
    /// download, after the time it took to fill it.
    pub fn storage(&self) -> Result<Storage> {
        disk_space(&self.config().data_root)
    }

    /// What every running server on this node is using right now.
    ///
    /// Only running containers are read: a stopped one has no counters worth
    /// showing, and asking Docker for them costs a round trip each. A container
    /// whose stats could not be read is left out rather than reported as idle —
    /// an absent row is "unknown", a row of zeros would be a claim.
    pub async fn usage(&self) -> Result<Vec<crate::instance::InstanceUsage>> {
        let containers = self.docker.list_managed().await?;
        let reads = containers
            .iter()
            .filter(|c| c.state == InstanceState::Running)
            .map(|c| self.docker.usage(&c.instance_id, &c.container_id));
        Ok(futures_util::future::join_all(reads)
            .await
            .into_iter()
            .filter_map(|r| {
                r.map_err(|e| tracing::debug!("container stats unavailable: {e:#}")).ok()
            })
            .collect())
    }

    /// Ask one instance's game what it is doing, if it can be asked: how many
    /// players, and which map.
    ///
    /// The query is aimed at the published port on this node's own loopback:
    /// the game is a container this agent started, not a stranger on the
    /// network.
    async fn live_stats(&self, instance: &InstanceStatus) -> Option<(u32, u32, String)> {
        self.ask_game(instance)
            .await
            .map(|s| (u32::from(s.info.players), u32::from(s.info.max_players), s.info.map))
    }

    /// One query to a running instance's game (A2S or Server List Ping), when
    /// its pack declares one.
    async fn ask_game(&self, instance: &InstanceStatus) -> Option<game_bridge::a2s::A2sStats> {
        if instance.state != InstanceState::Running {
            return None;
        }
        let profile = self.packs.read().await.get(&instance.game_id)?.to_profile().ok()?;
        // The pack names an enum this build implements — never a command. A
        // game declaring no query is unknown, which is the honest answer.
        let protocol = profile.query?;
        // The same place the mesh bridge reaches the game
        // (`game_address`). Not `127.0.0.1:<host_port>`: this agent is
        // meant to run in a container, where that loopback is its own
        // namespace and the query reaches nothing — every instance
        // would report "could not ask", and reaping would then never
        // touch anything because unknown is deliberately never treated
        // as empty.
        let host = self.docker.games_gateway().await.ok()?;
        let reach = crate::mesh::GameReach::from_ports(host, &instance.ports, instance.port)?;
        let ip: std::net::IpAddr = reach.host.parse().ok()?;
        protocol.ask(std::net::SocketAddr::new(ip, reach.port)).await.ok()
    }

    /// Who is on a server right now, as its game reports it: names, scores and
    /// time connected. Bots are in the list; A2S does not tell them apart.
    ///
    /// `Ok(None)` is "the game could not be asked" - not running, no query
    /// protocol in its pack, or no answer - which is not an empty server.
    pub async fn players(&self, instance_id: &str) -> Result<Option<Vec<PlayerRow>>> {
        let instances = self.list().await?;
        let instance = instances
            .iter()
            .find(|i| i.instance_id == instance_id)
            .ok_or_else(|| anyhow!("no instance {instance_id:?} on this node"))?;
        Ok(self.ask_game(instance).await.map(|s| {
            s.players_list
                .into_iter()
                .map(|p| PlayerRow {
                    name: p.name,
                    score: p.score,
                    connected_secs: p.duration_secs.max(0.0) as u32,
                })
                .collect()
        }))
    }

    fn restart_marker(&self, instance_id: &str) -> Result<std::path::PathBuf> {
        Ok(self.layout.instance_dir(instance_id).map_err(|e| anyhow!("{e:?}"))?.join(RESTART_MARKER))
    }

    /// Whether this server is started again when it crashes.
    pub fn restart_on_crash(&self, instance_id: &str) -> bool {
        self.restart_marker(instance_id).map(|p| p.is_file()).unwrap_or(false)
    }

    /// Turn restart-on-crash on or off for a server this agent manages.
    pub async fn set_restart_on_crash(&self, instance_id: &str, on: bool) -> Result<()> {
        if !self.docker.list_managed().await?.iter().any(|c| c.instance_id == instance_id) {
            return Err(anyhow!("no instance {instance_id:?} on this node"));
        }
        let marker = self.restart_marker(instance_id)?;
        if on {
            if let Some(dir) = marker.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&marker, b"")?;
        } else if marker.exists() {
            std::fs::remove_file(&marker)?;
        }
        Ok(())
    }

    /// A server's chat (`ROADMAP.md` §6.1): whether it is on, who is in it,
    /// and what was said after `since`. Loopback API only — the uplink never
    /// gets chat, as it never got the console.
    pub async fn chat(&self, instance_id: &str, since: u64) -> Result<NodeChat> {
        self.ensure_instance(instance_id).await?;
        let dir = self.layout.instance_dir(instance_id).map_err(|e| anyhow!("{e:?}"))?;
        let on = crate::mesh::chat_on(&dir);
        let Some(hub) = self.mesh.chat_hub(instance_id).await else {
            return Ok(NodeChat { on, live: false, members: Vec::new(), lines: Vec::new() });
        };
        let host = hub.host_identity();
        let names = hub.names();
        let muted = hub.muted();
        let hex_of = |id: &prns_core::identity::IdentityHash| hex::encode(id.as_bytes());
        Ok(NodeChat {
            on,
            live: true,
            members: names
                .iter()
                .map(|(id, name)| NodeChatMember {
                    identity: hex_of(id),
                    name: name.clone(),
                    host: *id == host,
                    muted: muted.contains(id),
                })
                .collect(),
            lines: hub
                .lines_since(since)
                .into_iter()
                .map(|l| NodeChatLine {
                    seq: l.seq,
                    identity: hex_of(&l.from),
                    name: names.iter().find(|(id, _)| *id == l.from).map(|(_, n)| n.clone()),
                    host: l.from == host,
                    text: l.text,
                    at_unix: l.at_unix,
                })
                .collect(),
        })
    }

    /// Turn a server's chat on or off, now and from its next start.
    pub async fn set_chat(&self, instance_id: &str, on: bool) -> Result<()> {
        self.ensure_instance(instance_id).await?;
        let dir = self.layout.instance_dir(instance_id).map_err(|e| anyhow!("{e:?}"))?;
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join(crate::mesh::CHAT_FILE),
            serde_json::to_vec(&serde_json::json!({ "on": on }))?,
        )?;
        self.mesh.set_chat(instance_id, on).await;
        tracing::info!(instance = %instance_id, on, "server chat switched");
        Ok(())
    }

    /// Say one line in a server's chat, as its host.
    pub async fn chat_say(&self, instance_id: &str, text: &str) -> Result<()> {
        self.ensure_instance(instance_id).await?;
        let hub = self
            .mesh
            .chat_hub(instance_id)
            .await
            .ok_or_else(|| anyhow!("this server is not on the mesh"))?;
        hub.host_say(text.trim()).map(|_| ()).map_err(|e| anyhow!("{e}"))
    }

    /// Mute or unmute someone in a server's chat until it stops.
    pub async fn chat_mute(&self, instance_id: &str, identity: &str, muted: bool) -> Result<()> {
        self.ensure_instance(instance_id).await?;
        let bytes: [u8; 16] = hex::decode(identity.trim())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| anyhow!("{identity:?} is not an identity hash"))?;
        let hub = self
            .mesh
            .chat_hub(instance_id)
            .await
            .ok_or_else(|| anyhow!("this server is not on the mesh"))?;
        hub.set_muted(prns_core::identity::IdentityHash::new(bytes), muted);
        Ok(())
    }

    /// Where a server's backups are kept: `backups/<id>` under the data root,
    /// outside the instance folder, so they outlive the server's own files.
    fn backups_dir(&self, instance_id: &str) -> Result<std::path::PathBuf> {
        self.layout.instance_dir(instance_id).map_err(|e| anyhow!("{e:?}"))?;
        Ok(self.config.data_root.join(crate::backups::BACKUPS_DIR).join(instance_id))
    }

    async fn ensure_instance(&self, instance_id: &str) -> Result<crate::docker::ManagedContainer> {
        self.docker
            .list_managed()
            .await?
            .into_iter()
            .find(|c| c.instance_id == instance_id)
            .ok_or_else(|| anyhow!("no instance {instance_id:?} on this node"))
    }

    /// A server's backups, newest first (`backups.rs`).
    pub async fn backups(&self, instance_id: &str) -> Result<Vec<crate::backups::Backup>> {
        self.ensure_instance(instance_id).await?;
        Ok(crate::backups::list(&self.backups_dir(instance_id)?))
    }

    /// Back up a server's writable files now. Running or stopped: a running
    /// game may be writing a file at that moment, which the page says. Held
    /// to the disk reserve like a download, by the files' full size.
    pub async fn create_backup(&self, instance_id: &str) -> Result<crate::backups::Backup> {
        let c = self.ensure_instance(instance_id).await?;
        let writable = self.layout.instance_dir(instance_id).map_err(|e| anyhow!("{e:?}"))?.join("writable");
        let dir = self.backups_dir(instance_id)?;
        let need = {
            let w = writable.clone();
            tokio::task::spawn_blocking(move || crate::content::dir_size(&w)).await?
        };
        self.room_for(need).await.map_err(|e| anyhow!("{e}"))?;
        // A running world is flushed and held still while it is copied
        // (`MINECRAFT.md` §7), and let go afterwards whatever happened.
        let console = match c.state {
            InstanceState::Running => self.console_of(instance_id).await.ok().map(|(_, p)| p),
            _ => None,
        };
        if let Some(console) = console.filter(|p| !p.before_backup().is_empty()) {
            self.hold_world(instance_id, console).await;
        }
        let name = crate::backups::new_name(&dir, now_unix());
        let made = tokio::task::spawn_blocking(move || crate::backups::create(&writable, &dir, &name)).await;
        if let Some(console) = console {
            for line in console.after_backup() {
                if let Err(e) = self.docker.send_console_line(instance_id, line).await {
                    tracing::warn!(instance = %instance_id, error = %format!("{e:#}"), "the world was not let go after a backup; send {line} yourself");
                }
            }
        }
        let made = made??;
        tracing::info!(instance = %instance_id, backup = %made.name, bytes = made.bytes, "backed up a server's files");
        Ok(made)
    }

    /// Send a game's before-backup lines and wait, up to `WORLD_SAVE_WAIT`,
    /// for the server to say it saved. A server that never says so is backed
    /// up anyway: a copy taken mid-save is still better than none, and the
    /// log says it happened.
    async fn hold_world(&self, instance_id: &str, console: ConsoleProtocol) {
        let saved = |log: &str| log.matches("Saved the game").count();
        let before = self.logs(instance_id, 200).await.map(|l| saved(&l)).unwrap_or(0);
        for line in console.before_backup() {
            if let Err(e) = self.docker.send_console_line(instance_id, line).await {
                tracing::warn!(instance = %instance_id, error = %format!("{e:#}"), "could not ask the server to save before a backup");
                return;
            }
        }
        let deadline = Instant::now() + WORLD_SAVE_WAIT;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if self.logs(instance_id, 200).await.map(|l| saved(&l)).unwrap_or(0) > before {
                return;
            }
        }
        tracing::warn!(instance = %instance_id, "the server did not say it saved; backing up anyway");
    }

    /// Put a backup back over a **stopped** server's writable files.
    pub async fn restore_backup(&self, instance_id: &str, name: &str) -> Result<()> {
        let c = self.ensure_instance(instance_id).await?;
        if c.state != InstanceState::Stopped {
            return Err(anyhow!("stop the server first: its files cannot be swapped while it runs"));
        }
        if self.is_moving(instance_id) {
            return Err(anyhow!("this server is being moved to a new copy; try again when that is done"));
        }
        let archive = crate::backups::path_of(&self.backups_dir(instance_id)?, name)
            .filter(|p| p.is_file())
            .ok_or_else(|| anyhow!("{instance_id} has no backup {name:?}"))?;
        let writable = self.layout.instance_dir(instance_id).map_err(|e| anyhow!("{e:?}"))?.join("writable");
        let mut seed = [0u8; 8];
        getrandom::getrandom(&mut seed).map_err(|e| anyhow!("no randomness for a staging name: {e}"))?;
        let staging = self.config.data_root.join("staging").join(format!("restore-{}", hex::encode(seed)));
        tokio::task::spawn_blocking(move || crate::backups::restore(&archive, &writable, &staging))
            .await?
            .map_err(|e| anyhow!("{e}"))?;
        tracing::info!(instance = %instance_id, backup = %name, "restored a server's files");
        Ok(())
    }

    /// Delete one backup. The server need not exist any more: a removed
    /// server's backups stay until deleted. The id is still checked as an
    /// instance id and the name against the agent's own shape.
    pub async fn delete_backup(&self, instance_id: &str, name: &str) -> Result<()> {
        let dir = self.backups_dir(instance_id)?;
        let path = crate::backups::path_of(&dir, name)
            .filter(|p| p.is_file())
            .ok_or_else(|| anyhow!("{instance_id} has no backup {name:?}"))?;
        std::fs::remove_file(path)?;
        // An empty folder of a removed server is not worth listing.
        if crate::backups::list(&dir).is_empty() && !self.server_exists(instance_id).await? {
            let _ = std::fs::remove_dir(&dir);
        }
        Ok(())
    }

    /// Where a named backup is, for download. Like delete, the server need
    /// not exist any more.
    pub async fn backup_file(&self, instance_id: &str, name: &str) -> Result<std::path::PathBuf> {
        crate::backups::path_of(&self.backups_dir(instance_id)?, name)
            .filter(|p| p.is_file())
            .ok_or_else(|| anyhow!("{instance_id} has no backup {name:?}"))
    }

    /// Where an upload for `instance_id` is written while it arrives: a
    /// hidden partial file in its backups folder, which `list` never shows.
    /// Refused for a server that does not exist, or when `bytes` (if the
    /// sender said) would go below the disk reserve.
    pub async fn backup_upload_path(&self, instance_id: &str, bytes: Option<u64>) -> Result<std::path::PathBuf> {
        self.ensure_instance(instance_id).await?;
        if let Some(bytes) = bytes {
            self.room_for(bytes).await.map_err(|e| anyhow!("{e}"))?;
        }
        let dir = self.backups_dir(instance_id)?;
        std::fs::create_dir_all(&dir)?;
        let mut seed = [0u8; 8];
        getrandom::getrandom(&mut seed).map_err(|e| anyhow!("no randomness for an upload name: {e}"))?;
        Ok(dir.join(format!(".upload-{}.partial", hex::encode(seed))))
    }

    /// Keep an uploaded backup if it is one (`backups::check_archive`), under
    /// a name the agent makes. The upload is removed either way.
    pub async fn import_backup(&self, instance_id: &str, upload: &std::path::Path) -> Result<crate::backups::Backup> {
        let dir = self.backups_dir(instance_id)?;
        let mut seed = [0u8; 8];
        getrandom::getrandom(&mut seed).map_err(|e| anyhow!("no randomness for a staging name: {e}"))?;
        let staging = self.config.data_root.join("staging").join(format!("import-{}", hex::encode(seed)));
        let archive = upload.to_path_buf();
        let checked = tokio::task::spawn_blocking(move || crate::backups::check_archive(&archive, &staging)).await?;
        if let Err(e) = checked {
            let _ = std::fs::remove_file(upload);
            return Err(anyhow!("{e}"));
        }
        let name = crate::backups::new_name(&dir, now_unix());
        if let Err(e) = std::fs::rename(upload, dir.join(&name)) {
            let _ = std::fs::remove_file(upload);
            return Err(e.into());
        }
        let bytes = std::fs::metadata(dir.join(&name))?.len();
        tracing::info!(instance = %instance_id, backup = %name, bytes, "imported a backup");
        Ok(crate::backups::Backup { name, bytes, created_unix: Some(now_unix()) })
    }

    async fn server_exists(&self, instance_id: &str) -> Result<bool> {
        Ok(self.docker.list_managed().await?.iter().any(|c| c.instance_id == instance_id))
    }

    /// Every server's backups on this node, removed servers' included, by
    /// server id.
    pub async fn all_backups(&self) -> Result<Vec<BackupSet>> {
        let existing: std::collections::BTreeSet<String> =
            self.docker.list_managed().await?.into_iter().map(|c| c.instance_id).collect();
        let root = self.config.data_root.join(crate::backups::BACKUPS_DIR);
        let Ok(entries) = std::fs::read_dir(&root) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let Ok(id) = e.file_name().into_string() else { continue };
            // A folder whose name is not an instance id is not the agent's.
            if self.layout.instance_dir(&id).is_err() || !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let backups = crate::backups::list(&e.path());
            if backups.is_empty() {
                continue;
            }
            out.push(BackupSet { server_exists: existing.contains(&id), instance_id: id, backups });
        }
        out.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        Ok(out)
    }

    /// Delete every backup of a server that no longer exists. Refused for a
    /// server that does: its backups go one at a time.
    pub async fn delete_removed_backups(&self, instance_id: &str) -> Result<usize> {
        let dir = self.backups_dir(instance_id)?;
        if self.server_exists(instance_id).await? {
            return Err(anyhow!("{instance_id} still exists; delete its backups one at a time"));
        }
        let backups = crate::backups::list(&dir);
        if backups.is_empty() {
            return Err(anyhow!("{instance_id} has no backups"));
        }
        for b in &backups {
            std::fs::remove_file(dir.join(&b.name))?;
        }
        let _ = std::fs::remove_dir(&dir);
        tracing::info!(instance = %instance_id, count = backups.len(), "deleted a removed server's backups");
        Ok(backups.len())
    }

    /// A server's backup schedule, if it has one.
    pub async fn backup_schedule(&self, instance_id: &str) -> Result<Option<crate::backups::StoredSchedule>> {
        self.ensure_instance(instance_id).await?;
        let dir = self.layout.instance_dir(instance_id).map_err(|e| anyhow!("{e:?}"))?;
        Ok(crate::backups::load_schedule(&dir))
    }

    /// Back a server up on a schedule, or stop with `None`. Changing the
    /// interval keeps what the schedule made, so it still prunes those.
    pub async fn set_backup_schedule(
        &self,
        instance_id: &str,
        schedule: Option<crate::backups::BackupSchedule>,
    ) -> Result<()> {
        self.ensure_instance(instance_id).await?;
        let dir = self.layout.instance_dir(instance_id).map_err(|e| anyhow!("{e:?}"))?;
        let stored = match schedule {
            Some(schedule) => {
                schedule.validate().map_err(|e| anyhow!("{e}"))?;
                let old = crate::backups::load_schedule(&dir);
                Some(crate::backups::StoredSchedule {
                    schedule,
                    last_unix: old.as_ref().and_then(|o| o.last_unix),
                    made: old.map(|o| o.made).unwrap_or_default(),
                })
            }
            None => None,
        };
        crate::backups::save_schedule(&dir, stored.as_ref())?;
        Ok(())
    }

    /// Back up the running servers whose schedule is due, and delete the
    /// oldest backups each schedule made beyond its `keep`. A stopped server's
    /// files do not change, so it is not backed up again. The attempt is
    /// recorded first, so one that fails (the disk reserve, say) waits for the
    /// next interval rather than retrying every minute.
    pub async fn scheduled_backups(&self) {
        let Ok(containers) = self.docker.list_managed().await else { return };
        for c in containers.iter().filter(|c| c.state == InstanceState::Running) {
            let Ok(dir) = self.layout.instance_dir(&c.instance_id) else { continue };
            let Some(mut stored) = crate::backups::load_schedule(&dir) else { continue };
            let now = now_unix();
            if !stored.schedule.due(now, stored.last_unix) {
                continue;
            }
            stored.last_unix = Some(now);
            if let Err(e) = crate::backups::save_schedule(&dir, Some(&stored)) {
                tracing::warn!(instance = %c.instance_id, error = %e, "not backing up: the schedule could not be saved");
                continue;
            }
            let made = match self.create_backup(&c.instance_id).await {
                Ok(made) => made,
                Err(e) => {
                    tracing::warn!(instance = %c.instance_id, error = %format!("{e:#}"), "the scheduled backup failed");
                    continue;
                }
            };
            let prune = stored.record(made.name);
            if let Err(e) = crate::backups::save_schedule(&dir, Some(&stored)) {
                // Not pruning: a name the file does not remember would never
                // be pruned again, but one it forgot to drop could be deleted
                // twice. Neither is worth deleting on a guess.
                tracing::warn!(instance = %c.instance_id, error = %e, "the backup schedule could not be saved");
                continue;
            }
            let Ok(bdir) = self.backups_dir(&c.instance_id) else { continue };
            for name in prune {
                if let Some(path) = crate::backups::path_of(&bdir, &name) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    /// Every server's daily restart window, by server (`schedule.rs`).
    pub async fn restart_schedules(&self) -> Result<BTreeMap<String, ScheduleStatus>> {
        let waits = self.restart_waits.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut out = BTreeMap::new();
        for c in self.docker.list_managed().await? {
            let Ok(dir) = self.layout.instance_dir(&c.instance_id) else { continue };
            if let Some(stored) = crate::schedule::load(&dir) {
                let waiting = waits.get(&c.instance_id).cloned();
                out.insert(c.instance_id, ScheduleStatus { stored, waiting });
            }
        }
        Ok(out)
    }

    /// Give a server a daily restart window, or take it away with `None`.
    pub async fn set_restart_schedule(
        &self,
        instance_id: &str,
        schedule: Option<crate::schedule::RestartSchedule>,
    ) -> Result<()> {
        if !self.docker.list_managed().await?.iter().any(|c| c.instance_id == instance_id) {
            return Err(anyhow!("no instance {instance_id:?} on this node"));
        }
        let dir = self.layout.instance_dir(instance_id).map_err(|e| anyhow!("{e:?}"))?;
        let stored = match schedule {
            Some(schedule) => {
                schedule.validate().map_err(|e| anyhow!("{e}"))?;
                let last_unix = crate::schedule::load(&dir).and_then(|s| s.last_unix);
                Some(crate::schedule::Stored { schedule, last_unix })
            }
            None => None,
        };
        crate::schedule::save(&dir, stored.as_ref())?;
        self.restart_waits.lock().unwrap_or_else(|e| e.into_inner()).remove(instance_id);
        Ok(())
    }

    /// Restart the running servers whose daily window is open, once each
    /// window (`schedule.rs`): when nobody is playing, or after an in-game
    /// warning where the operator allowed it. A stopped server, and a server
    /// being moved, are left alone. Through `restart`, so each comes back on
    /// the mesh.
    pub async fn scheduled_restarts(&self) {
        let Ok(containers) = self.docker.list_managed().await else { return };
        let Ok(instances) = self.list().await else { return };
        let mut waits = BTreeMap::new();
        for c in containers.iter().filter(|c| c.state == InstanceState::Running) {
            let Ok(dir) = self.layout.instance_dir(&c.instance_id) else { continue };
            let Some(mut stored) = crate::schedule::load(&dir) else { continue };
            if self.is_moving(&c.instance_id) {
                continue;
            }
            let console = match &c.game_id {
                Some(game) => self.packs.read().await.get(game).and_then(|p| p.console).map(ConsoleProtocol::from),
                None => None,
            };
            let humans = match instances.iter().find(|i| i.instance_id == c.instance_id) {
                Some(i) => self.humans_on(i).await,
                None => None,
            };
            let now = now_unix();
            let warn = match stored.schedule.decision(humans, now, console.is_some(), stored.last_unix) {
                crate::schedule::RestartDecision::NotNow => continue,
                crate::schedule::RestartDecision::Wait(why) => {
                    waits.insert(c.instance_id.clone(), why);
                    continue;
                }
                crate::schedule::RestartDecision::Restart => None,
                crate::schedule::RestartDecision::WarnThenRestart => console.and_then(|c| c.say(RESTART_WARNING).ok()),
            };
            // Recorded first, so a restart that fails is not retried every
            // minute for the rest of the window.
            stored.last_unix = Some(now);
            if let Err(e) = crate::schedule::save(&dir, Some(&stored)) {
                tracing::warn!(instance = %c.instance_id, error = %e, "not restarting: the schedule could not be saved");
                continue;
            }
            if let Some(line) = warn {
                if let Err(e) = self.docker.send_console_line(&c.instance_id, &line).await {
                    tracing::warn!(instance = %c.instance_id, error = %format!("{e:#}"), "not restarting: the players could not be warned");
                    continue;
                }
                let wait = self.move_timing.lock().unwrap_or_else(|e| e.into_inner()).warn;
                tokio::time::sleep(wait).await;
            }
            tracing::info!(instance = %c.instance_id, "scheduled restart");
            if let Err(e) = self.restart(&c.instance_id).await {
                tracing::warn!(instance = %c.instance_id, error = %format!("{e:#}"), "the scheduled restart failed");
            }
        }
        *self.restart_waits.lock().unwrap_or_else(|e| e.into_inner()) = waits;
    }

    /// Start again every server that asked for it and crashed since the last
    /// look. Run every `CRASH_CHECK_EVERY` from `main`.
    ///
    /// The agent restarts it, not Docker: a restart policy would bring the
    /// container back behind the agent's back, and nothing would put it on
    /// the mesh again (`docker.rs`, "No restart policy on purpose"). Going
    /// through `restart` rebuilds the bridge like a person's Restart does.
    pub async fn check_crashes(&self) {
        let containers = match self.docker.list_managed().await {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut due = Vec::new();
        {
            let mut watch = self.crashes.lock().await;
            for c in containers.iter().filter(|c| c.state == InstanceState::Stopped) {
                // A move watches its own server and rolls it back itself.
                if self.is_moving(&c.instance_id) {
                    continue;
                }
                let Ok(Some(exit)) = self.docker.last_exit(&c.container_id).await else { continue };
                let key = exit.finished_at.clone().unwrap_or_default();
                if watch.seen.get(&c.instance_id) == Some(&key) {
                    continue;
                }
                watch.seen.insert(c.instance_id.clone(), key);
                if !watch.seeded
                    || watch.stopped_here.contains(&c.instance_id)
                    || !exit.is_crash()
                    || !self.restart_on_crash(&c.instance_id)
                {
                    continue;
                }
                if watch.recent(&c.instance_id) >= MAX_CRASH_RESTARTS_PER_HOUR {
                    tracing::warn!(instance = %c.instance_id, code = exit.code,
                        "crashed again; already restarted {MAX_CRASH_RESTARTS_PER_HOUR} times this hour, leaving it stopped");
                    continue;
                }
                watch.restarts.entry(c.instance_id.clone()).or_default().push(Instant::now());
                due.push((c.instance_id.clone(), exit));
            }
            watch.seeded = true;
        }
        for (id, exit) in due {
            tracing::warn!(instance = %id, code = exit.code, oom_killed = exit.oom_killed, "server crashed; starting it again");
            if let Err(e) = self.restart(&id).await {
                tracing::warn!(instance = %id, error = %format!("{e:#}"), "could not restart a crashed server");
            }
        }
    }

    /// The version new servers of `game` use: the one an update made active
    /// (`active.rs`) while it is installed and `agent.toml` has not moved on,
    /// else the config's.
    /// Where a game's files stand on this node, for the Games page
    /// (`PLAN.md` §13.3 step 3): which driver brings them, whether the copy
    /// new servers use is there, and for a `manual` game exactly where a
    /// person puts it and which folders it must have. `None` for a game this
    /// node has no runtime for: nothing says which version it would be.
    pub async fn content_state(&self, game_id: &str) -> Option<ContentState> {
        let packs = self.packs.read().await;
        let pack = packs.get(game_id)?;
        let version = self.active_version(game_id)?;
        let dir = self
            .layout
            .content_dir(&ContentRef { game_id: game_id.to_string(), version: version.clone() })
            .ok()?;
        let installed = dir.is_dir();
        let manual = !pack.content.is_automatic();
        let missing: Vec<String> = self
            .writable_paths_for(game_id, pack)
            .into_iter()
            .filter(|p| installed && !dir.join(p).is_dir())
            .collect();
        Some(ContentState {
            driver: pack.content.driver_name().to_string(),
            installed,
            version,
            install_dir: (manual && !installed).then(|| dir.display().to_string()),
            must_contain: if manual { self.writable_paths_for(game_id, pack) } else { Vec::new() },
            missing,
            note: match &pack.content {
                PackContent::Manual { note } => note.clone(),
                _ => None,
            },
            fetch_allowed: !manual && self.config.allow_content_fetch,
        })
    }

    pub fn active_version(&self, game_id: &str) -> Option<String> {
        let runtime = self.config.runtime_for(game_id)?;
        Some(self.active_version_of(game_id, &runtime.content_version))
    }

    /// The copy a server runs: the one its spec picked (a Minecraft build),
    /// else the node's own.
    fn version_for(&self, game_id: &str, picked: Option<&str>, config_version: &str) -> String {
        match picked {
            Some(v) => v.to_string(),
            None => self.active_version_of(game_id, config_version),
        }
    }

    fn active_version_of(&self, game_id: &str, config_version: &str) -> String {
        let Some(active) = self.active.active(game_id, config_version) else {
            return config_version.to_string();
        };
        let content = ContentRef { game_id: game_id.to_string(), version: active.clone() };
        match self.layout.content_dir(&content) {
            Ok(dir) if dir.is_dir() => active,
            // Deleted since, or not an id: fall back rather than point new
            // servers at nothing.
            _ => config_version.to_string(),
        }
    }

    /// Fetch the newest build of a game into a new version directory and make
    /// it the one new servers use (`Provisioner::update`). With `remove_old`,
    /// the previous copy is deleted now if nothing mounts it, or queued and
    /// deleted once the servers using it are removed (`sweep_old_copies`).
    ///
    /// Servers already running keep the copy they were created on: a
    /// container's mounts are fixed at create, and swapping files under a
    /// running game is what "existing content is never replaced" forbids.
    pub async fn update_content(
        &self,
        game_id: &str,
        remove_old: bool,
        progress: Option<&crate::content::InstallProgress>,
    ) -> Result<UpdateReport, ProvisionError> {
        let io = |m: String| ProvisionError::Io(std::io::Error::other(m));
        let spec = {
            let packs = self.packs.read().await;
            packs
                .get(game_id)
                .ok_or_else(|| io(format!("no game pack installed for {game_id:?}")))?
                .content
                .clone()
        };
        let runtime = self
            .config
            .runtime_for(game_id)
            .ok_or_else(|| io(format!("this node has no runtime configured for {game_id:?}")))?;
        let base = runtime.content_version.clone();
        let current = self.active_version_of(game_id, &base);
        let from = ContentRef { game_id: game_id.to_string(), version: current.clone() };
        let to_version = self.next_version_name(game_id, &base).map_err(io)?;
        let to = ContentRef { game_id: game_id.to_string(), version: to_version.clone() };

        let updated = self.content.update(&from, &to, &spec, Some(&self.docker), progress).await?;
        let (to_version, from_build, to_build) = match updated {
            crate::content::Updated::UpToDate { build } => {
                return Ok(UpdateReport {
                    from_version: current,
                    to_version: None,
                    from_build: build.clone(),
                    to_build: build,
                    old: "kept".into(),
                    old_used_by: Vec::new(),
                });
            }
            crate::content::Updated::NewBuild { from_build, to_build, .. } => {
                (to_version, from_build, to_build)
            }
        };
        self.active
            .set_active(game_id, &base, &to_version)
            .map_err(|e| io(format!("the new copy is installed, but recording it as active failed: {e}")))?;
        let pack = self.packs.read().await.get(game_id).cloned();
        if let Some(pack) = pack {
            self.create_writable_mountpoints(&to, &self.writable_paths_for(game_id, &pack))?;
        }

        let mut report = UpdateReport {
            from_version: current.clone(),
            to_version: Some(to_version),
            from_build,
            to_build,
            old: "kept".into(),
            old_used_by: Vec::new(),
        };
        if remove_old {
            self.active
                .add_pending(game_id, &current)
                .map_err(|e| io(format!("queueing the old copy for removal: {e}")))?;
            self.sweep_old_copies().await;
            report.old_used_by = self.copy_used_by(game_id, &current).await.unwrap_or_default();
            report.old = if report.old_used_by.is_empty() { "removed" } else { "waiting" }.into();
        }
        Ok(report)
    }

    /// Check the copy new servers use against Valve's files
    /// (`Provisioner::verify`). Nothing that differed means nothing is kept;
    /// otherwise the repaired copy is kept beside it under a new version name
    /// and is **not** made active: `validate` also puts back files the
    /// operator changed on purpose, such as YaPB's `liblist.gam`, so the
    /// report names every file and `use_copy` is a separate step.
    pub async fn verify_content(
        &self,
        game_id: &str,
        progress: Option<&crate::content::InstallProgress>,
    ) -> Result<VerifyReport, ProvisionError> {
        let io = |m: String| ProvisionError::Io(std::io::Error::other(m));
        let spec = {
            let packs = self.packs.read().await;
            packs
                .get(game_id)
                .ok_or_else(|| io(format!("no game pack installed for {game_id:?}")))?
                .content
                .clone()
        };
        let runtime = self
            .config
            .runtime_for(game_id)
            .ok_or_else(|| io(format!("this node has no runtime configured for {game_id:?}")))?;
        let base = runtime.content_version.clone();
        let current = self.active_version_of(game_id, &base);
        let from = ContentRef { game_id: game_id.to_string(), version: current.clone() };
        let to_version = self.next_version_name(game_id, &base).map_err(io)?;
        let to = ContentRef { game_id: game_id.to_string(), version: to_version.clone() };
        match self.content.verify(&from, &to, &spec, Some(&self.docker), progress).await? {
            crate::content::Verified::Intact { build } => Ok(VerifyReport {
                version: current,
                repaired_version: None,
                changed: Vec::new(),
                changed_count: 0,
                from_build: build.clone(),
                to_build: build,
            }),
            crate::content::Verified::Repaired { changed, from_build, to_build, .. } => Ok(VerifyReport {
                version: current,
                repaired_version: Some(to_version),
                changed_count: changed.len(),
                changed: changed.into_iter().take(VERIFY_REPORT_FILES).collect(),
                from_build,
                to_build,
            }),
        }
    }

    /// Make an installed copy the one new servers use, such as a repaired
    /// copy a verify kept. `agent.toml` is not rewritten (`active.rs`), and
    /// servers that exist move by the game's rule, as after an update.
    pub async fn use_copy(&self, game_id: &str, version: &str) -> Result<(), String> {
        let runtime = self
            .config
            .runtime_for(game_id)
            .ok_or_else(|| format!("this node has no runtime configured for {game_id:?}"))?;
        let content = ContentRef { game_id: game_id.to_string(), version: version.to_string() };
        let dir = self.layout.content_dir(&content).map_err(|e| format!("{e}"))?;
        if !dir.is_dir() {
            return Err(format!("{game_id} has no installed copy {version:?}"));
        }
        let pack = self.packs.read().await.get(game_id).cloned();
        if let Some(pack) = pack {
            self.create_writable_mountpoints(&content, &self.writable_paths_for(game_id, &pack))
                .map_err(|e| format!("{e}"))?;
        }
        self.active
            .set_active(game_id, &runtime.content_version, version)
            .map_err(|e| format!("recording the copy new servers use: {e}"))
    }

    /// The operator's update-check and reserve settings (`updates.rs`).
    pub fn update_settings(&self) -> crate::updates::Settings {
        self.updates.settings()
    }

    pub fn set_update_settings(&self, settings: crate::updates::Settings) -> Result<(), String> {
        self.updates.set_settings(settings)
    }

    /// The reserve on this node's disk, and the disk it was worked out from.
    /// `None` where free space cannot be read (`disk_space`), and then there is
    /// no reserve to keep: nothing is refused for a number nobody knows.
    pub fn reserve(&self) -> Option<(Storage, u64)> {
        let storage = self.storage().ok()?;
        let reserve = self.updates.settings().reserve(storage.total_bytes);
        Some((storage, reserve))
    }

    /// Why a download running now has to stop, if the disk has reached the
    /// reserve (`install.rs` asks on every progress tick).
    pub fn disk_reached_reserve(&self) -> Option<String> {
        let (storage, reserve) = self.reserve()?;
        (storage.free_bytes < reserve).then(|| {
            format!(
                "stopped: the disk reached this node's reserve ({} must stay free, {} is). Nothing it \
                 fetched was kept. Free some space, such as unused copies on the Installed page, and \
                 try again",
                human_bytes(reserve),
                human_bytes(storage.free_bytes)
            )
        })
    }

    /// The space an update of `game_id` is assumed to need: its active copy
    /// and the delta (`updates::update_needs`). Zero when there is no copy.
    pub fn update_needs(&self, game_id: &str) -> u64 {
        let Some(version) = self.active_version(game_id) else { return 0 };
        let content = ContentRef { game_id: game_id.to_string(), version };
        let Ok(dir) = self.layout.content_dir(&content) else { return 0 };
        if !dir.is_dir() {
            return 0;
        }
        let modified = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
        crate::updates::update_needs(self.content_size(&dir, modified))
    }

    /// Refuse a download of about `need` bytes that would take the disk below
    /// the reserve, naming what it needs, what is free, and the unused copies
    /// whose deletion would make room. **Nothing is deleted here**: making
    /// room is the operator's call.
    pub async fn room_for(&self, need: u64) -> Result<(), String> {
        let Some((storage, reserve)) = self.reserve() else { return Ok(()) };
        if crate::updates::fits(storage.free_bytes, need, reserve) {
            return Ok(());
        }
        let above = storage.free_bytes.saturating_sub(reserve);
        let mut msg = if need == 0 {
            format!(
                "not enough room: this node keeps {} free and has {} free, so nothing new is downloaded",
                human_bytes(reserve),
                human_bytes(storage.free_bytes)
            )
        } else {
            format!(
                "not enough room: this needs about {}, and {} is free above the {} this node keeps free",
                human_bytes(need),
                human_bytes(above),
                human_bytes(reserve)
            )
        };
        let unused: Vec<InstalledCopy> =
            self.installed().await.unwrap_or_default().into_iter().filter(|c| c.unused).collect();
        let freed: u64 = unused.iter().map(|c| c.bytes).sum();
        if !unused.is_empty() {
            let names: Vec<String> =
                unused.iter().map(|c| format!("{} {} ({})", c.game_id, c.version, human_bytes(c.bytes))).collect();
            let enough = crate::updates::fits(storage.free_bytes.saturating_add(freed), need, reserve);
            msg.push_str(&format!(
                ". Deleting the unused copies ({}) {} free enough",
                names.join(", "),
                if enough { "would" } else { "would still not" }
            ));
        }
        Err(msg)
    }

    /// Ask Steam for `game_id`'s newest public build now, remember the answer,
    /// and return the game's notice (`updates.rs`). Only a steamcmd game whose
    /// active copy is installed is checked: an archive pack's digest pins its
    /// bytes, and a manual game has nothing to ask.
    pub async fn check_for_update(&self, game_id: &str) -> Result<crate::updates::UpdateNotice, String> {
        let app_id = match self.packs.read().await.get(game_id).map(|p| p.content.clone()) {
            None => return Err(format!("no game pack installed for {game_id:?}")),
            Some(PackContent::Steamcmd { app_id, .. }) => app_id,
            Some(other) => {
                return Err(format!(
                    "only a game downloaded with steamcmd has newer builds to check for; this pack's \
                     content driver is {:?}",
                    other.driver_name()
                ))
            }
        };
        let version = self.active_version(game_id).ok_or_else(|| format!("this node has no runtime configured for {game_id:?}"))?;
        let content = ContentRef { game_id: game_id.to_string(), version: version.clone() };
        let dir = self.layout.content_dir(&content).map_err(|e| format!("{e}"))?;
        if !dir.is_dir() {
            return Err(format!("{game_id} is not installed here, so there is nothing to update"));
        }
        let home = self.config.data_root.join("steamcmd-check");
        let outcome = {
            let _one = self.check_lock.lock().await;
            self.content.latest_build(app_id, &self.docker, &home).await.map_err(|e| format!("{e}"))
        };
        if let Err(e) = &outcome {
            tracing::warn!(game = %game_id, error = %e, "update check failed");
        }
        self.updates
            .record(game_id, now_unix(), outcome)
            .map_err(|e| format!("saving the check: {e}"))?;
        Ok(self.notice_for(game_id, &version, app_id, &dir))
    }

    fn notice_for(
        &self,
        game_id: &str,
        version: &str,
        app_id: u32,
        dir: &std::path::Path,
    ) -> crate::updates::UpdateNotice {
        let installed = crate::content::steam_build_id(dir, app_id);
        crate::updates::UpdateNotice::new(
            game_id,
            version,
            installed,
            &self.updates.game(game_id),
            &self.updates.policy(game_id),
            now_unix(),
        )
    }

    /// Every steamcmd game with an installed active copy, and what is known
    /// about its newest build.
    pub async fn update_notices(&self) -> Vec<crate::updates::UpdateNotice> {
        let packs: Vec<(String, u32)> = self
            .packs
            .read()
            .await
            .values()
            .filter_map(|p| match p.content {
                PackContent::Steamcmd { app_id, .. } => Some((p.id.clone(), app_id)),
                _ => None,
            })
            .collect();
        let mut out = Vec::new();
        for (game_id, app_id) in packs {
            let Some(version) = self.active_version(&game_id) else { continue };
            let content = ContentRef { game_id: game_id.clone(), version: version.clone() };
            let Ok(dir) = self.layout.content_dir(&content) else { continue };
            if dir.is_dir() {
                out.push(self.notice_for(&game_id, &version, app_id, &dir));
            }
        }
        out
    }

    /// Run every check that is due under the operator's interval, the node's
    /// or the game's own (nothing while both are off, the default), then start
    /// the downloads a game's
    /// "download it" asks for (§12.3).
    pub async fn check_due_updates(self: &Arc<Self>) {
        let node = self.updates.settings().check_every_hours;
        let now = now_unix();
        for notice in self.update_notices().await {
            let Some(hours) = self.updates.policy(&notice.game_id).check_hours(node) else { continue };
            if self.updates.game(&notice.game_id).due(now, hours) {
                let _ = self.check_for_update(&notice.game_id).await;
            }
        }
        self.start_automatic_downloads().await;
    }

    /// Start an update for every game set to download a found build, whose
    /// copy is behind it, and which has not tried this build since its last
    /// check. Through the same queue and reserve gate as the Update button,
    /// and never removing the old copy: nothing automatic deletes.
    pub async fn start_automatic_downloads(self: &Arc<Self>) {
        let now = now_unix();
        for notice in self.update_notices().await {
            if notice.on_new_build != crate::updates::OnNewBuild::Download || notice.available != Some(true) {
                continue;
            }
            let check = self.updates.game(&notice.game_id);
            if !check.may_auto_download(now) {
                continue;
            }
            let Some(build) = check.latest_build else { continue };
            if self.installs.is_running(&notice.game_id).await {
                continue;
            }
            if let Err(e) = self.updates.record_auto(&notice.game_id, &build, now) {
                tracing::warn!(game = %notice.game_id, error = %e, "not downloading: could not record the attempt");
                continue;
            }
            let started =
                crate::install::spawn_update(self.installs.clone(), self.clone(), notice.game_id.clone(), false).await;
            if started {
                tracing::info!(game = %notice.game_id, build = %build, "downloading a new build automatically");
            }
        }
    }

    /// What `game_id` does when a check finds a newer build.
    pub async fn set_update_policy(&self, game_id: &str, patch: crate::updates::PolicyPatch) -> Result<(), String> {
        match self.packs.read().await.get(game_id).map(|p| &p.content) {
            None => return Err(format!("no game pack installed for {game_id:?}")),
            Some(PackContent::Steamcmd { .. }) => {}
            Some(other) => {
                return Err(format!(
                    "only a game downloaded with steamcmd has newer builds; this pack's content driver is {:?}",
                    other.driver_name()
                ))
            }
        }
        let policy = self.updates.policy(game_id).patched(patch)?;
        self.updates.set_policy(game_id, policy).map_err(|e| format!("saving the choice: {e}"))
    }

    // ---- Moving servers onto a new copy (`ROADMAP.md` §12.4) ----

    /// Change how long a move watches the new server and warns players; the
    /// defaults are 30 s and 60 s. For tests, which cannot wait that long.
    pub fn set_move_timing(&self, timing: MoveTiming) {
        *self.move_timing.lock().unwrap_or_else(|e| e.into_inner()) = timing;
    }

    fn is_moving(&self, instance_id: &str) -> bool {
        self.moving.lock().unwrap_or_else(|e| e.into_inner()).contains_key(instance_id)
    }

    /// The servers being moved off or onto this copy right now.
    fn servers_moving_with(&self, game_id: &str, version: &str) -> Vec<String> {
        self.moving
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, m)| m.game_id == game_id && (m.from == version || m.to == version))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Which copy of its game a container mounts, read off its mounts.
    fn copy_of(&self, game_id: &str, c: &crate::docker::ManagedContainer) -> Option<String> {
        let probe = ContentRef { game_id: game_id.to_string(), version: "probe".to_string() };
        let root = self.layout.content_dir(&probe).ok()?.parent()?.to_path_buf();
        c.mount_sources.iter().find_map(|m| {
            let rest = m.strip_prefix(&root).ok()?;
            rest.components().next()?.as_os_str().to_str().map(str::to_string)
        })
    }

    /// People on a server, bots left out, as its game reports them. `None`
    /// when it could not be asked, which is never "nobody".
    async fn humans_on(&self, instance: &InstanceStatus) -> Option<u32> {
        let stats = self.ask_game(instance).await?;
        Some(u32::from(stats.info.players.saturating_sub(stats.info.bots)))
    }

    fn log_move(&self, instance_id: &str, game_id: &str, from: &str, to: &str, state: MoveState) {
        self.move_log.lock().unwrap_or_else(|e| e.into_inner()).insert(
            instance_id.to_string(),
            MoveRecord {
                instance_id: instance_id.to_string(),
                game_id: game_id.to_string(),
                from: from.to_string(),
                to: to.to_string(),
                state,
                at_unix: now_unix(),
            },
        );
    }

    /// What is known about moves: servers waiting for one, and how the last
    /// one of each server went.
    pub fn moves(&self) -> Vec<MoveRecord> {
        self.move_log.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
    }

    /// Move a running server onto the copy new servers use: recreate its
    /// container there with the **same instance id**, so the same folder,
    /// name, ports and mesh identity (`mesh::identity_path`), and watch it.
    /// If it does not stay up for the settle time it is recreated on the old
    /// copy, and the outcome says why. With `warn`, players are told over the
    /// game's console first and given the warning time.
    ///
    /// One move at a time on a node. Everything that can refuse — the plan on
    /// the new copy, the port set — does so before anything is stopped, and
    /// both copies are pinned against deletion until it ends. Nothing is
    /// deleted here, except through `sweep_old_copies` after a move that
    /// worked, which deletes only what the operator asked to remove.
    pub async fn move_instance(&self, instance_id: &str, warn: bool) -> Result<MoveOutcome, String> {
        let _one = self.move_lock.lock().await;
        let containers = self.docker.list_managed().await.map_err(|e| format!("{e:#}"))?;
        let c = containers
            .iter()
            .find(|c| c.instance_id == instance_id)
            .ok_or_else(|| format!("no server {instance_id:?} on this node"))?;
        if c.state != InstanceState::Running {
            return Err("only a running server is moved; a stopped one stays on its copy until it runs again".into());
        }
        let game_id = c.game_id.clone().ok_or("this server carries no game label")?;
        let from = self.copy_of(&game_id, c).ok_or("which copy of its game this server uses is not known")?;
        let to = self
            .active_version(&game_id)
            .ok_or_else(|| format!("this node has no runtime configured for {game_id:?}"))?;
        if from == to {
            return Err("this server is already on the copy new servers use".into());
        }
        let spec = self.docker.spec_of(instance_id).await.map_err(|e| format!("{e:#}"))?;
        self.plan_and_check_on(&spec, Some(&to)).await.map_err(|e| format!("not moved: {e:#}"))?;
        let console = {
            let packs = self.packs.read().await;
            let pack = packs.get(&game_id).ok_or_else(|| format!("no game pack installed for {game_id:?}"))?;
            let profile = pack.to_profile().map_err(|e| format!("{e}"))?;
            let missing = profile.ports().iter().any(|p| {
                if p.channel == game_bridge::framing::CHANNEL_GAME {
                    spec.port.is_none()
                } else {
                    !spec.extra_ports.contains_key(&p.channel)
                }
            });
            if missing {
                return Err(
                    "not moved: its game now declares a port this server never had; remove it and start it again".into(),
                );
            }
            pack.console.map(ConsoleProtocol::from)
        };
        let warning = if warn {
            let console = console.ok_or("not moved: its game has no console to warn players on")?;
            Some(console.say(MOVE_WARNING).map_err(|e| format!("{e}"))?)
        } else {
            None
        };

        self.moving.lock().unwrap_or_else(|e| e.into_inner()).insert(
            instance_id.to_string(),
            MoveInFlight { game_id: game_id.clone(), from: from.clone(), to: to.clone() },
        );
        let _unpin = Unpin { moving: &self.moving, instance_id: instance_id.to_string() };
        let timing = *self.move_timing.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(line) = warning {
            self.docker
                .send_console_line(instance_id, &line)
                .await
                .map_err(|e| format!("not moved: the players could not be warned: {e:#}"))?;
            self.log_move(instance_id, &game_id, &from, &to, MoveState::Warned);
            tokio::time::sleep(timing.warn).await;
        }

        self.log_move(instance_id, &game_id, &from, &to, MoveState::Moving);
        tracing::info!(instance = %instance_id, from = %from, to = %to, "moving a server onto the new copy");
        self.mesh.stop(instance_id).await;
        let _ = self.docker.stop(instance_id).await;
        if let Err(e) = self.docker.remove(instance_id).await {
            // Still there: put it back as it was.
            let _ = self.restart(instance_id).await;
            return Err(format!("not moved: its container could not be removed: {e:#}"));
        }

        let why = match self.create_on(spec.clone(), Some(&to), true).await {
            Err(e) => Some(format!("it did not start on the new copy: {e:#}")),
            Ok(_) => self.settled(instance_id, timing.settle).await,
        };
        let Some(why) = why else {
            self.log_move(instance_id, &game_id, &from, &to, MoveState::Moved);
            tracing::info!(instance = %instance_id, to = %to, "server moved");
            drop(_unpin);
            self.sweep_old_copies().await;
            return Ok(MoveOutcome::Moved { from, to });
        };

        tracing::warn!(instance = %instance_id, why = %why, "move failed; putting the server back on the old copy");
        self.mesh.stop(instance_id).await;
        let _ = self.docker.remove(instance_id).await;
        match self.create_on(spec.clone(), Some(&from), true).await {
            Ok(_) => {
                self.log_move(instance_id, &game_id, &from, &to, MoveState::RolledBack { why: why.clone() });
                Ok(MoveOutcome::RolledBack { from, to, why })
            }
            Err(e) => {
                let mut held: Vec<u16> = spec.port.into_iter().collect();
                held.extend(spec.extra_ports.values());
                self.ports.lock().await.release_all(&held);
                let why = format!("{why}; putting it back on {from} failed too: {e:#}. Start it again from the Servers page");
                tracing::error!(instance = %instance_id, why = %why, "a moved server is on neither copy");
                self.log_move(instance_id, &game_id, &from, &to, MoveState::Failed { why: why.clone(), lost: true });
                Err(why)
            }
        }
    }

    /// `None` if the server is still running after `settle`, else why not.
    async fn settled(&self, instance_id: &str, settle: Duration) -> Option<String> {
        let until = Instant::now() + settle;
        loop {
            if self.docker.state_of(instance_id).await.ok() != Some(InstanceState::Running) {
                let containers = self.docker.list_managed().await.unwrap_or_default();
                let exit = match containers.iter().find(|c| c.instance_id == instance_id) {
                    Some(c) => self.docker.last_exit(&c.container_id).await.ok().flatten(),
                    None => None,
                };
                return Some(match exit {
                    Some(e) => format!("it stopped on the new copy within {} s (exit {})", settle.as_secs(), e.code),
                    None => format!("it was not running on the new copy within {} s", settle.as_secs()),
                });
            }
            if Instant::now() >= until {
                return None;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Move what each game's choice says to move now, one server at a time
    /// (§12.4). A server whose move onto this copy already failed is not
    /// tried again until there is a newer copy.
    pub async fn move_servers_pass(&self) {
        let Ok(containers) = self.docker.list_managed().await else { return };
        let Ok(instances) = self.list().await else { return };
        {
            let mut log = self.move_log.lock().unwrap_or_else(|e| e.into_inner());
            log.retain(|id, r| {
                containers.iter().any(|c| &c.instance_id == id) && !matches!(r.state, MoveState::Waiting { .. })
            });
        }
        let games: Vec<(String, bool)> =
            self.packs.read().await.values().map(|p| (p.id.clone(), p.console.is_some())).collect();
        for (game_id, can_warn) in games {
            let policy = self.updates.policy(&game_id);
            if policy.move_servers == crate::updates::MoveServers::Never {
                continue;
            }
            let Some(to) = self.active_version(&game_id) else { continue };
            let Ok(dir) = self.layout.content_dir(&ContentRef { game_id: game_id.clone(), version: to.clone() }) else {
                continue;
            };
            if !dir.is_dir() {
                continue;
            }
            for c in containers.iter().filter(|c| c.game_id.as_deref() == Some(game_id.as_str())) {
                let Some(from) = self.copy_of(&game_id, c) else { continue };
                if from == to {
                    continue;
                }
                let tried = self.move_log.lock().unwrap_or_else(|e| e.into_inner()).get(&c.instance_id).is_some_and(|r| {
                    r.to == to && matches!(r.state, MoveState::RolledBack { .. } | MoveState::Failed { .. })
                });
                if tried {
                    continue;
                }
                if c.state != InstanceState::Running {
                    let why = "stopped; it moves once it runs again".to_string();
                    self.log_move(&c.instance_id, &game_id, &from, &to, MoveState::Waiting { why });
                    continue;
                }
                let humans = match instances.iter().find(|i| i.instance_id == c.instance_id) {
                    Some(i) => self.humans_on(i).await,
                    None => None,
                };
                let warn = match policy.move_decision(humans, now_unix(), can_warn) {
                    crate::updates::MoveDecision::Stay => continue,
                    crate::updates::MoveDecision::Wait(why) => {
                        self.log_move(&c.instance_id, &game_id, &from, &to, MoveState::Waiting { why });
                        continue;
                    }
                    crate::updates::MoveDecision::Move => false,
                    crate::updates::MoveDecision::WarnThenMove => true,
                };
                if let Err(why) = self.move_instance(&c.instance_id, warn).await {
                    if !matches!(
                        self.move_log.lock().unwrap_or_else(|e| e.into_inner()).get(&c.instance_id).map(|r| &r.state),
                        Some(MoveState::Failed { .. })
                    ) {
                        self.log_move(&c.instance_id, &game_id, &from, &to, MoveState::Failed { why, lost: false });
                    }
                }
            }
        }
    }

    /// Hide a game's notice for `hours`, or while `skip_build` is its newest
    /// build; neither clears both. `None` and `false` show it again.
    pub fn snooze_update(&self, game_id: &str, hours: Option<u32>, skip_build: bool) -> Result<(), String> {
        let until = hours.map(|h| now_unix() + i64::from(h) * 3600);
        let skip = if skip_build {
            Some(
                self.updates
                    .game(game_id)
                    .latest_build
                    .ok_or_else(|| format!("{game_id} has not been checked yet, so there is no build to skip"))?,
            )
        } else {
            None
        };
        self.updates.snooze(game_id, until, skip).map_err(|e| format!("saving the snooze: {e}"))
    }

    /// `<config version>-<yyyymmdd>`, with `-2`, `-3`… if that exists. Named
    /// from the config's version, not the current one, so names never grow.
    fn next_version_name(&self, game_id: &str, base: &str) -> Result<String, String> {
        let days = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() / 86_400).unwrap_or(0);
        let (y, m, d) = civil_from_days(days as i64);
        let stem = format!("{base}-{y:04}{m:02}{d:02}");
        for n in 1..100 {
            let name = if n == 1 { stem.clone() } else { format!("{stem}-{n}") };
            let content = ContentRef { game_id: game_id.to_string(), version: name.clone() };
            let dir = self.layout.content_dir(&content).map_err(|e| format!("cannot name the new version {name:?}: {e}"))?;
            if !dir.exists() {
                return Ok(name);
            }
        }
        Err(format!("too many updates of {game_id} today"))
    }

    async fn copy_used_by(&self, game_id: &str, version: &str) -> Result<Vec<String>> {
        let content = ContentRef { game_id: game_id.to_string(), version: version.to_string() };
        let dir = self.layout.content_dir(&content).map_err(|e| anyhow!("{e}"))?;
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        Ok(self
            .docker
            .list_managed()
            .await?
            .into_iter()
            .filter(|c| c.mount_sources.iter().any(|m| m.starts_with(&dir)))
            .map(|c| c.instance_id)
            .collect())
    }

    /// Delete old copies the operator asked to remove after an update, once
    /// nothing mounts them. Never the active copy. Run after an update, after
    /// a server is removed, and when the agent starts.
    pub async fn sweep_old_copies(&self) {
        for (game, version) in self.active.pending() {
            if self.active_version(&game).as_deref() == Some(version.as_str()) {
                continue;
            }
            match self.delete_content(&game, &version).await {
                Ok(bytes) => {
                    tracing::info!(game = %game, version = %version, bytes, "removed an old copy after an update");
                    let _ = self.active.remove_pending(&game, &version);
                }
                Err(DeleteContentError::NotInstalled) | Err(DeleteContentError::BadId(_)) => {
                    let _ = self.active.remove_pending(&game, &version);
                }
                Err(DeleteContentError::InUse(_)) => {}
                Err(e) => tracing::warn!(game = %game, version = %version, error = %e, "could not remove an old copy"),
            }
        }
    }

    /// Every installed copy of every game's files on this node, unused ones
    /// first.
    ///
    /// Read from the disk and from Docker, like everything the agent reports:
    /// a copy is a directory `content/<game>/<version>`, and which servers use
    /// it is which containers mount it. A directory whose name is not an id
    /// this node would make is skipped rather than reported, since nothing
    /// here could act on it.
    pub async fn installed(&self) -> Result<Vec<InstalledCopy>> {
        let containers = self.docker.list_managed().await?;
        let packs = self.packs.read().await;
        let root = self.config.data_root.join("content");
        let mut copies = Vec::new();
        for (game, version) in list_content_dirs(&root) {
            let content = ContentRef { game_id: game.clone(), version: version.clone() };
            let Ok(dir) = self.layout.content_dir(&content) else { continue };
            let used_by: Vec<String> = containers
                .iter()
                .filter(|c| c.mount_sources.iter().any(|m| m.starts_with(&dir)))
                .map(|c| c.instance_id.clone())
                .collect();
            let configured = self.active_version(&game).is_some_and(|v| v == version);
            let updatable = configured
                && packs.get(&game).is_some_and(|p| matches!(p.content, PackContent::Steamcmd { .. }));
            let modified = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
            copies.push(InstalledCopy {
                bytes: self.content_size(&dir, modified),
                modified_unix: modified
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64),
                unused: !configured && used_by.is_empty(),
                updatable,
                game_id: game,
                version,
                configured,
                used_by,
            });
        }
        copies.sort_by(|a, b| {
            b.unused.cmp(&a.unused).then_with(|| (&a.game_id, &a.version).cmp(&(&b.game_id, &b.version)))
        });
        Ok(copies)
    }

    fn content_size(&self, dir: &std::path::Path, modified: Option<SystemTime>) -> u64 {
        let mut cache = self.content_sizes.lock().unwrap_or_else(|e| e.into_inner());
        if let (Some(m), Some((seen, bytes))) = (modified, cache.get(dir)) {
            if *seen == m {
                return *bytes;
            }
        }
        let bytes = dir_size(dir);
        if let Some(m) = modified {
            cache.insert(dir.to_path_buf(), (m, bytes));
        }
        bytes
    }

    /// Delete one installed copy, because the operator asked. Returns the
    /// bytes it held.
    ///
    /// **A copy any managed container mounts is never deleted, running or
    /// stopped**: servers bind-mount their game's files, and a stopped server
    /// restarts from the same mount. The caller also holds the game's install
    /// slot (`Installs::exclusive`), so no download can rename its staging
    /// directory into this tree while it goes.
    ///
    /// The path is built by `StoreLayout::content_dir` from validated ids,
    /// never taken from the request; and the copy is first renamed out of
    /// `content/`, so a delete that is interrupted never leaves a partial tree
    /// that `plan_and_check` would read as installed.
    pub async fn delete_content(&self, game_id: &str, version: &str) -> Result<u64, DeleteContentError> {
        let content = ContentRef { game_id: game_id.to_string(), version: version.to_string() };
        let dir = self.layout.content_dir(&content).map_err(DeleteContentError::BadId)?;
        if !dir.is_dir() {
            return Err(DeleteContentError::NotInstalled);
        }
        let containers = self.docker.list_managed().await.map_err(DeleteContentError::Other)?;
        let used_by: Vec<String> = containers
            .into_iter()
            .filter(|c| c.mount_sources.iter().any(|m| m.starts_with(&dir)))
            .map(|c| c.instance_id)
            .collect();
        if !used_by.is_empty() {
            return Err(DeleteContentError::InUse(used_by));
        }
        // After the listing, not before: a move pins its copies before it
        // removes the container, so a listing that no longer shows that
        // container is one taken after the pin.
        let moving = self.servers_moving_with(game_id, version);
        if !moving.is_empty() {
            return Err(DeleteContentError::InUse(moving));
        }
        let modified = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
        let bytes = self.content_size(&dir, modified);
        let mut seed = [0u8; 8];
        getrandom::getrandom(&mut seed)
            .map_err(|e| DeleteContentError::Other(anyhow!("no randomness for a trash name: {e}")))?;
        let trash_root = self.config.data_root.join("staging");
        let trash = trash_root.join(format!("deleting-{}", hex::encode(seed)));
        let moved = std::fs::create_dir_all(&trash_root).and_then(|()| std::fs::rename(&dir, &trash));
        moved.map_err(|e| DeleteContentError::Other(anyhow!("moving {} aside: {e}", dir.display())))?;
        self.content_sizes.lock().unwrap_or_else(|e| e.into_inner()).remove(&dir);
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&trash).map_err(|e| (trash, e)))
            .await
            .map_err(|e| DeleteContentError::Other(anyhow!("{e}")))?
            .map_err(|(trash, e)| {
                DeleteContentError::Other(anyhow!(
                    "the copy is no longer installed, but deleting {} failed: {e}",
                    trash.display()
                ))
            })?;
        // An empty game directory is left by deleting its last copy; it holds
        // nothing and would list as nothing.
        if let Some(parent) = dir.parent() {
            let _ = std::fs::remove_dir(parent);
        }
        Ok(bytes)
    }

    /// Leftover instance directories: how many, and how much they hold.
    pub async fn orphan_report(&self) -> Result<OrphanReport> {
        let dirs = self.orphan_dirs().await?;
        let mut report = OrphanReport::default();
        for d in &dirs {
            if recently_touched(d) {
                report.skipped_recent += 1;
                continue;
            }
            report.count += 1;
            report.bytes += dir_size(d);
        }
        Ok(report)
    }

    /// Delete leftover instance directories, because the operator asked.
    ///
    /// Never on its own (`orphan_dirs` says why). What is deleted is
    /// re-derived here, from the containers that exist now, so a server
    /// created since the page last looked is never touched; and a directory
    /// touched in the last `ORPHAN_MIN_AGE` is skipped, which covers a create
    /// that has made its directories but not yet its container.
    pub async fn clean_orphans(&self) -> Result<OrphanReport> {
        let dirs = self.orphan_dirs().await?;
        let mut report = OrphanReport::default();
        for d in &dirs {
            if recently_touched(d) {
                report.skipped_recent += 1;
                continue;
            }
            let bytes = dir_size(d);
            match std::fs::remove_dir_all(d) {
                Ok(()) => {
                    report.count += 1;
                    report.bytes += bytes;
                }
                Err(e) => {
                    report.failed += 1;
                    report.error.get_or_insert_with(|| format!("{}: {e}", d.display()));
                }
            }
        }
        Ok(report)
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


/// A server's chat, as Lanthorn Host shows it (`Agent::chat`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeChat {
    /// Whether the operator turned it on (`chat.json` in the instance folder).
    pub on: bool,
    /// Whether the server is on the mesh, where chat happens.
    pub live: bool,
    pub members: Vec<NodeChatMember>,
    pub lines: Vec<NodeChatLine>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeChatMember {
    pub identity: String,
    /// A player's own name for themselves: shown, never trusted.
    pub name: String,
    pub host: bool,
    pub muted: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeChatLine {
    pub seq: u64,
    pub identity: String,
    pub name: Option<String>,
    pub host: bool,
    pub text: String,
    pub at_unix: u64,
}

/// What a spec may ask of this pack (`MINECRAFT.md` §8, §9): a game with an
/// `[eula]` needs the owner's acceptance, and a Java heap, Minecraft settings
/// and a picked copy belong to a Minecraft game only. Refused, never ignored:
/// a setting silently dropped is a server that is not what was asked for.
pub fn check_spec_against_pack(spec: &InstanceSpec, pack: &GamePack) -> Result<()> {
    let o = &spec.options;
    if let Some(eula) = &pack.eula {
        if o.eula_accepted_at.is_none() {
            return Err(anyhow!(
                "{} needs its owner to accept the {} ({}) before the server can be created",
                pack.display_name,
                eula.name,
                eula.url
            ));
        }
    }
    let minecraft = matches!(pack.content, PackContent::Minecraft {});
    if !minecraft && (o.memory.is_some() || o.minecraft.is_some() || o.content_version.is_some()) {
        return Err(anyhow!(
            "{} does not take a Java memory size, Minecraft settings or a picked version",
            pack.display_name
        ));
    }
    if let (true, Some(v)) = (minecraft, &o.content_version) {
        game_bridge::minecraft::MinecraftBuild::parse(v).map_err(|e| anyhow!("{e}"))?;
    }
    Ok(())
}

/// `MemTotal` and `MemAvailable` from `/proc/meminfo`, in bytes. Available,
/// not free: the kernel's own estimate of what new work can use without
/// swapping, page cache included.
fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|v| v.trim().strip_suffix("kB"))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
    };
    Some((field("MemTotal")?, field("MemAvailable")?))
}

#[cfg(test)]
mod pack_gate_tests {
    use super::check_spec_against_pack;
    use crate::instance::{InstanceOptions, InstanceSpec, MemorySpec};
    use game_bridge::GamePack;

    fn minecraft_pack() -> GamePack {
        GamePack::parse(
            r#"
schema_version = 1
id = "minecraft-java"
display_name = "Minecraft: Java Edition"
app_name = "minecraft-java"
default_port = 25565
transport = "tcp"
min_link_class = 1
console = "minecraft"
writable_paths = ["server"]
[content]
driver = "minecraft"
[eula]
name = "Minecraft End User License Agreement"
url = "https://aka.ms/MinecraftEULA"
"#,
        )
        .unwrap()
    }

    fn spec(options: InstanceOptions) -> InstanceSpec {
        InstanceSpec {
            instance_id: "mc1".into(),
            game_id: "minecraft-java".into(),
            name: "Friday".into(),
            max_players: 20,
            port: None,
            extra_ports: Default::default(),
            bots: None,
            map: None,
            restart_on_crash: false,
            owner: None,
            options,
        }
    }

    #[test]
    fn a_game_with_an_eula_is_never_created_without_the_owners_acceptance() {
        let pack = minecraft_pack();
        let err = check_spec_against_pack(&spec(InstanceOptions::default()), &pack).unwrap_err();
        assert!(err.to_string().contains("accept"), "{err}");
        let ok = InstanceOptions { eula_accepted_at: Some(1_760_000_000), ..Default::default() };
        assert!(check_spec_against_pack(&spec(ok), &pack).is_ok());
    }

    #[test]
    fn a_picked_build_is_judged_and_java_settings_belong_to_minecraft_only() {
        let pack = minecraft_pack();
        let bad = InstanceOptions {
            eula_accepted_at: Some(1),
            content_version: Some("paper-26.4-snapshot-4".into()),
            ..Default::default()
        };
        assert!(check_spec_against_pack(&spec(bad), &pack).is_err());

        let sven = GamePack::sven_coop();
        let heap = InstanceOptions { memory: Some(MemorySpec { min_mb: 1024, max_mb: 2048 }), ..Default::default() };
        let mut s = spec(heap);
        s.game_id = "sven-coop".into();
        assert!(check_spec_against_pack(&s, &sven).is_err(), "a heap on a game with no JVM is refused, not ignored");
    }
}

#[cfg(test)]
mod meminfo_tests {
    use super::parse_meminfo;

    /// Placement reads the node's memory from the kernel's own figures; a
    /// system that does not report `MemAvailable` reports nothing at all
    /// rather than a guess.
    #[test]
    fn memory_is_read_from_meminfo_and_never_guessed() {
        let text = "MemTotal:       16052940 kB\nMemFree:         7116948 kB\nMemAvailable:   14735260 kB\n";
        assert_eq!(parse_meminfo(text), Some((16052940 * 1024, 14735260 * 1024)));
        assert_eq!(parse_meminfo("MemTotal:       16052940 kB\nMemFree:  1 kB\n"), None);
    }
}

#[cfg(test)]
mod date_tests {
    use super::civil_from_days;

    #[test]
    fn day_counts_become_calendar_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_739), (2026, 10, 13));
    }
}

#[cfg(test)]
mod storage_tests {
    use super::disk_space;

    #[cfg(unix)]
    #[test]
    fn free_space_is_read_and_is_never_more_than_the_disk() {
        let s = disk_space(&std::env::temp_dir()).unwrap();
        assert!(s.total_bytes > 0);
        assert!(s.free_bytes <= s.total_bytes, "{s:?}");
    }

    #[test]
    fn a_missing_path_is_an_error_not_an_empty_disk() {
        assert!(disk_space(std::path::Path::new("/no/such/path/here")).is_err());
    }
}
