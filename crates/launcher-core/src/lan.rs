//! Mode 3 LAN rooms in the launcher (`PLAN.md` §14, step 5).
//!
//! Hosting, joining and leaving a room, and the one question a room needs
//! answered before it can start: whether this machine's `lan-helper` is there
//! and allowed to make an adapter. The room itself is `game_bridge`'s
//! `LanSession` on `run_room_on_adapter`, exactly as the CLI runs it; this
//! module only holds it for the UI and says what it is doing.
//!
//! # Rules a later change could quietly break
//!
//! - **The launcher never elevates itself.** On Linux the helper holds a file
//!   capability the player granted once (`grant_lan_helper` asks through
//!   `pkexec`, which is the opt-in); on Windows the helper asks for
//!   administrator rights when a room starts. Nothing here runs privileged.
//! - **A game without a `[lan]` block gets no room**, and a room row is never
//!   offered as an ordinary join: its destination speaks the room protocol,
//!   not a game's datagrams.
//! - **The warning is the pack's, shown before joining.** `inbound_any` means
//!   other members can reach every port on this machine while the room is up;
//!   `tested` false means nobody has played this game in a room yet. Both are
//!   facts the player acts on, so both reach the UI unsoftened.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use game_bridge::lan_adapter::AdapterPhase;
use game_bridge::lan_session::{LanHostArgs, LanMemberArgs, LanSession};
use game_bridge::pack::{GamePack, PackLanInbound, PackTransport};
use serde::Serialize;
use tokio::sync::{oneshot, watch};

use crate::{join_interfaces, parse_hash, Launcher};

/// The adapter every room in this launcher uses. One room at a time.
pub const ADAPTER_NAME: &str = "gbl0";

/// How long leaving waits for the adapter to be taken away.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(15);

/// What a game's pack says about playing it in a room, for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct LanSupport {
    /// Somebody has played it in a room. False means untested, and says so.
    pub tested: bool,
    /// Other members can reach every port on this machine while the room is
    /// up. The UI must warn before joining.
    pub inbound_any: bool,
    /// The ports members can reach, as `udp/3979`.
    pub ports: Vec<String>,
}

pub(crate) fn lan_support(pack: &GamePack) -> Option<LanSupport> {
    let lan = pack.lan.as_ref()?;
    Some(LanSupport {
        tested: lan.tested,
        inbound_any: lan.inbound == PackLanInbound::Any,
        ports: lan
            .ports
            .iter()
            .map(|p| {
                let proto = match p.transport {
                    PackTransport::Udp => "udp",
                    PackTransport::Tcp => "tcp",
                };
                format!("{proto}/{}", p.port)
            })
            .collect(),
    })
}

/// Whether this machine can put a room on an adapter.
#[derive(Debug, Clone, Serialize)]
pub struct LanHelperView {
    /// This platform has a room adapter at all (Linux and Windows today).
    pub supported: bool,
    /// Where the launcher expects `lan-helper`.
    pub path: Option<String>,
    /// The helper is there and able to make an adapter.
    pub ready: bool,
    /// The launcher can ask for the permission itself (Linux, via `pkexec`).
    pub can_grant: bool,
    /// One line for a person: what is missing and what to do about it.
    pub detail: String,
}

/// One member of the room, as the UI lists them.
#[derive(Debug, Clone, Serialize)]
pub struct RoomMemberView {
    pub address: String,
    pub is_self: bool,
}

/// The room this launcher is in, if any.
#[derive(Debug, Clone, Serialize)]
pub struct RoomView {
    pub active: bool,
    /// `"host"` or `"member"`.
    pub role: Option<String>,
    pub game_id: Option<String>,
    pub name: Option<String>,
    pub room_hash: Option<String>,
    /// This machine's address in the room, once seated.
    pub address: Option<String>,
    /// The room's subnet, `198.19.0.0/16`.
    pub subnet: Option<String>,
    pub members: Vec<RoomMemberView>,
    /// `"none"`, `"waiting"` (for a seat), `"starting"` (the adapter — on
    /// Windows, the elevation prompt), `"up"`, `"stopped"` or `"failed"`.
    pub adapter: String,
    /// Why the adapter failed, when it did.
    pub error: Option<String>,
    /// Why the room refused this member, when it did.
    pub refused: Option<String>,
}

impl RoomView {
    fn none() -> Self {
        Self {
            active: false,
            role: None,
            game_id: None,
            name: None,
            room_hash: None,
            address: None,
            subnet: None,
            members: Vec::new(),
            adapter: "none".to_string(),
            error: None,
            refused: None,
        }
    }
}

pub(crate) fn phase_label(phase: &AdapterPhase) -> (&'static str, Option<String>) {
    match phase {
        AdapterPhase::WaitingForSeat => ("waiting", None),
        AdapterPhase::Starting => ("starting", None),
        AdapterPhase::Up { .. } => ("up", None),
        AdapterPhase::Stopped => ("stopped", None),
        AdapterPhase::Failed(e) => ("failed", Some(e.clone())),
    }
}

/// A running room, held by the launcher.
pub(crate) struct RoomState {
    role: &'static str,
    game_id: String,
    name: Option<String>,
    session: Arc<LanSession>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<()>>,
    phase: watch::Receiver<AdapterPhase>,
}

/// Where `lan-helper` ships: beside this executable, on every platform the
/// launcher bundles it for.
pub fn helper_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.with_file_name(format!("lan-helper{}", std::env::consts::EXE_SUFFIX)))
}

fn on_path(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&paths)
        .chain([PathBuf::from("/usr/sbin"), PathBuf::from("/sbin")])
        .any(|dir| dir.join(program).is_file())
}

/// The pure decision behind [`Launcher::lan_helper`], from facts already
/// gathered, so it is testable without a helper on the machine.
pub(crate) fn helper_view(
    supported: bool,
    path: Option<PathBuf>,
    exists: bool,
    check: Option<Result<(), String>>,
    grant_tools: bool,
) -> LanHelperView {
    let path_text = path.as_ref().map(|p| p.display().to_string());
    if !supported {
        return LanHelperView {
            supported: false,
            path: path_text,
            ready: false,
            can_grant: false,
            detail: "LAN rooms need a room adapter, which this platform does not have yet \
                     (Linux and Windows do)."
                .to_string(),
        };
    }
    if !exists {
        return LanHelperView {
            supported: true,
            path: path_text,
            ready: false,
            can_grant: false,
            detail: "lan-helper is not installed beside this launcher, so it cannot make a room \
                     adapter. Reinstall from a release that includes it."
                .to_string(),
        };
    }
    // An AppImage mounts itself `nosuid`, where a file capability is ignored:
    // granting would succeed and change nothing.
    let in_appimage = path.as_ref().is_some_and(|p| p.to_string_lossy().contains("/.mount_"));
    match check {
        Some(Ok(())) => LanHelperView {
            supported: true,
            path: path_text,
            ready: true,
            can_grant: false,
            detail: if cfg!(windows) {
                "Ready. Windows will ask for administrator rights when a room starts, for the \
                 helper only."
                    .to_string()
            } else {
                "Ready.".to_string()
            },
        },
        _ if in_appimage => LanHelperView {
            supported: true,
            path: path_text,
            ready: false,
            can_grant: false,
            detail: "The AppImage cannot hold the network permission a room adapter needs. \
                     Install the .deb or .rpm to host or join LAN rooms."
                .to_string(),
        },
        Some(Err(why)) => LanHelperView {
            supported: true,
            ready: false,
            can_grant: cfg!(target_os = "linux") && grant_tools,
            detail: if cfg!(target_os = "linux") {
                format!(
                    "LAN rooms need one-time permission for lan-helper to make a network \
                     adapter (and nothing else). Grant it here, or run: sudo setcap \
                     cap_net_admin+ep {}",
                    path_text.as_deref().unwrap_or("lan-helper")
                )
            } else {
                format!("lan-helper cannot run: {why}")
            },
            path: path_text,
        },
        None => LanHelperView {
            supported: true,
            path: path_text,
            ready: false,
            can_grant: false,
            detail: "lan-helper could not be asked whether it is ready.".to_string(),
        },
    }
}

fn run_check(path: &std::path::Path) -> Result<(), String> {
    let out = std::process::Command::new(path).arg("check").output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

impl Launcher {
    /// Whether this machine can put a room on an adapter, and if not, why.
    pub async fn lan_helper(&self) -> LanHelperView {
        let supported = cfg!(any(target_os = "linux", windows));
        let path = helper_path();
        tokio::task::spawn_blocking(move || {
            let exists = path.as_ref().is_some_and(|p| p.is_file());
            let check = (supported && exists).then(|| run_check(path.as_ref().expect("exists")));
            helper_view(supported, path, exists, check, on_path("pkexec") && on_path("setcap"))
        })
        .await
        .unwrap_or_else(|_| helper_view(supported, None, false, None, false))
    }

    /// Ask for the helper's one permission — the opt-in moment on Linux.
    ///
    /// `pkexec` shows the desktop's own authentication dialog; the command it
    /// runs is fixed here and names only the helper beside this launcher.
    pub async fn grant_lan_helper(&self) -> Result<LanHelperView> {
        if !cfg!(target_os = "linux") {
            return Err(anyhow!(
                "only Linux grants the helper a permission; Windows asks when a room starts"
            ));
        }
        let path = helper_path()
            .filter(|p| p.is_file())
            .ok_or_else(|| anyhow!("lan-helper is not installed beside this launcher"))?;
        let status = tokio::task::spawn_blocking(move || {
            std::process::Command::new("pkexec")
                .args(["setcap", "cap_net_admin+ep"])
                .arg(&path)
                .status()
        })
        .await?
        .context("running pkexec")?;
        if !status.success() {
            return Err(anyhow!("the permission was not granted ({status})"));
        }
        Ok(self.lan_helper().await)
    }

    /// Open a room for `game_id` and put it on this machine's adapter.
    pub async fn host_room(&self, game_id: &str, name: Option<String>) -> Result<RoomView> {
        self.start_room(RoomRequest::Host { game_id: game_id.to_string(), name }).await
    }

    /// Join the room at `destination_hash` and put it on this machine's adapter.
    pub async fn join_room(&self, destination_hash: &str, game_id: &str) -> Result<RoomView> {
        parse_hash(destination_hash)?;
        self.start_room(RoomRequest::Join {
            hash: destination_hash.to_string(),
            game_id: game_id.to_string(),
        })
        .await
    }

    async fn start_room(&self, request: RoomRequest) -> Result<RoomView> {
        let game_id = match &request {
            RoomRequest::Host { game_id, .. } | RoomRequest::Join { game_id, .. } => {
                game_id.clone()
            }
        };
        let pack = self
            .packs
            .iter()
            .find(|p| p.pack.id == game_id)
            .map(|p| p.pack.clone())
            .ok_or_else(|| anyhow!("no game pack installed for {game_id:?}"))?;
        let lan = pack.lan.clone().ok_or_else(|| {
            anyhow!(
                "{} is not offered as a LAN room: its pack has no [lan] block",
                pack.display_name
            )
        })?;
        let profile =
            pack.to_profile().map_err(|e| anyhow!("pack {game_id:?} is not usable: {e}"))?;

        let helper = self.lan_helper().await;
        if !helper.ready {
            return Err(anyhow!("{}", helper.detail));
        }
        let helper_path = helper_path().ok_or_else(|| anyhow!("cannot locate lan-helper"))?;

        let saved_opts = self.saved_browse_opts().await;
        let identity = self.lan_identity_path();
        if let Some(parent) = identity.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {} for the room identity", parent.display()))?;
        }

        // One room at a time: leave any room before starting another.
        self.leave_room().await?;

        let mut inner = self.inner.lock().await;
        let opts = join_interfaces(inner.browse_opts.as_ref(), &saved_opts);
        if opts.tcp.is_none() && !opts.auto {
            return Err(anyhow!(
                "this launcher has no mesh interface, so a room could not reach anyone. \
                 Add a TCP peer or turn on LAN auto-discovery first"
            ));
        }
        let (role, name, session) = match request {
            RoomRequest::Host { name, .. } => {
                let mut args = LanHostArgs::new(profile);
                args.identity = identity;
                args.tcp = opts.tcp.clone();
                args.auto = opts.auto;
                args.name = name.clone();
                ("host", name, LanSession::host(args).await?)
            }
            RoomRequest::Join { hash, .. } => {
                let mut args = LanMemberArgs::new(profile);
                args.identity = identity;
                args.tcp = opts.tcp.clone();
                args.auto = opts.auto;
                args.room_hash = Some(hash);
                ("member", None, LanSession::join(args).await?)
            }
        };
        let session = Arc::new(session);
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (phase_tx, phase_rx) = watch::channel(AdapterPhase::WaitingForSeat);
        let task = spawn_room(session.clone(), lan.policy(), helper_path, stop_rx, phase_tx);
        inner.room = Some(RoomState {
            role,
            game_id,
            name,
            session,
            stop: Some(stop_tx),
            task,
            phase: phase_rx,
        });
        drop(inner);
        Ok(self.room_status().await)
    }

    /// Leave the room, taking the adapter away. Not being in one is not an error.
    pub async fn leave_room(&self) -> Result<()> {
        let room = self.inner.lock().await.room.take();
        let Some(mut room) = room else { return Ok(()) };
        if let Some(stop) = room.stop.take() {
            let _ = stop.send(());
        }
        let _ = tokio::time::timeout(LEAVE_TIMEOUT, &mut room.task).await;
        room.task.abort();
        // The runner held the only other reference; with it finished, the
        // session can be stopped and its node's thread joined.
        if let Ok(mut session) = Arc::try_unwrap(room.session) {
            session.stop().await;
        }
        Ok(())
    }

    /// The room this launcher is in, as the UI shows it.
    pub async fn room_status(&self) -> RoomView {
        let inner = self.inner.lock().await;
        let Some(room) = &inner.room else { return RoomView::none() };
        let view = room.session.view();
        let (adapter, error) = phase_label(&room.phase.borrow());
        RoomView {
            active: true,
            role: Some(room.role.to_string()),
            game_id: Some(room.game_id.clone()),
            name: room.name.clone(),
            room_hash: room.session.room_hash().map(|h| hex::encode(h.as_bytes())),
            address: view.own_address.map(|a| a.to_string()),
            subnet: Some(format!("{}/{}", view.subnet.prefix, view.subnet.prefix_len)),
            members: view
                .members
                .iter()
                .map(|m| RoomMemberView {
                    address: m.address.to_string(),
                    is_self: m.identity == view.own_identity,
                })
                .collect(),
            adapter: adapter.to_string(),
            error,
            refused: view.refused.map(|r| r.to_string()),
        }
    }

    /// The room identity: its own file beside the settings, like the client's.
    fn lan_identity_path(&self) -> PathBuf {
        self.client_identity_path().with_file_name("lan.identity")
    }
}

enum RoomRequest {
    Host { game_id: String, name: Option<String> },
    Join { hash: String, game_id: String },
}

#[cfg(any(target_os = "linux", windows))]
fn spawn_room(
    session: Arc<LanSession>,
    policy: game_bridge::lan_filter::LanPolicy,
    helper: PathBuf,
    stop: oneshot::Receiver<()>,
    phase: watch::Sender<AdapterPhase>,
) -> tokio::task::JoinHandle<Result<()>> {
    use game_bridge::lan_adapter::{run_room_on_adapter_reporting, AdapterSetup};
    tokio::spawn(run_room_on_adapter_reporting(
        session,
        policy,
        ADAPTER_NAME.to_string(),
        AdapterSetup::Helper(helper),
        async move {
            let _ = stop.await;
        },
        phase,
    ))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn spawn_room(
    _session: Arc<LanSession>,
    _policy: game_bridge::lan_filter::LanPolicy,
    _helper: PathBuf,
    _stop: oneshot::Receiver<()>,
    phase: watch::Sender<AdapterPhase>,
) -> tokio::task::JoinHandle<Result<()>> {
    let _ = phase.send(AdapterPhase::Failed("this platform has no room adapter".to_string()));
    tokio::spawn(async { Err(anyhow!("this platform has no room adapter")) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(v: &serde_json::Value) -> Vec<&str> {
        let mut k: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        k.sort();
        k
    }

    #[test]
    fn room_view_json_keys_are_the_frontend_contract() {
        let v = serde_json::to_value(RoomView::none()).unwrap();
        assert_eq!(
            keys(&v),
            [
                "active",
                "adapter",
                "address",
                "error",
                "game_id",
                "members",
                "name",
                "refused",
                "role",
                "room_hash",
                "subnet"
            ]
        );
        assert_eq!(v["adapter"], "none");
        let m =
            serde_json::to_value(RoomMemberView { address: "198.19.0.1".into(), is_self: true })
                .unwrap();
        assert_eq!(keys(&m), ["address", "is_self"]);
    }

    #[test]
    fn helper_view_json_keys_are_the_frontend_contract() {
        let v = serde_json::to_value(helper_view(true, None, false, None, false)).unwrap();
        assert_eq!(keys(&v), ["can_grant", "detail", "path", "ready", "supported"]);
    }

    #[test]
    fn lan_support_json_keys_are_the_frontend_contract() {
        let v = serde_json::to_value(LanSupport {
            tested: false,
            inbound_any: true,
            ports: vec!["udp/1".into()],
        })
        .unwrap();
        assert_eq!(keys(&v), ["inbound_any", "ports", "tested"]);
    }

    #[test]
    fn every_adapter_phase_has_a_label_the_ui_knows() {
        for (phase, label) in [
            (AdapterPhase::WaitingForSeat, "waiting"),
            (AdapterPhase::Starting, "starting"),
            (AdapterPhase::Up { address: std::net::Ipv4Addr::new(198, 19, 0, 1) }, "up"),
            (AdapterPhase::Stopped, "stopped"),
            (AdapterPhase::Failed("x".into()), "failed"),
        ] {
            assert_eq!(phase_label(&phase).0, label);
        }
        assert_eq!(phase_label(&AdapterPhase::Failed("why".into())).1.as_deref(), Some("why"));
    }

    /// Each state a person can be in gets a sentence that says what to do.
    #[test]
    fn the_helper_view_says_what_is_missing() {
        let p = Some(PathBuf::from("/usr/bin/lan-helper"));
        let unsupported = helper_view(false, p.clone(), true, None, true);
        assert!(!unsupported.supported && !unsupported.ready);

        let missing = helper_view(true, p.clone(), false, None, true);
        assert!(!missing.ready && missing.detail.contains("not installed"));

        let ready = helper_view(true, p.clone(), true, Some(Ok(())), true);
        assert!(ready.ready && !ready.can_grant);

        let needs = helper_view(true, p.clone(), true, Some(Err("no cap".into())), true);
        assert!(!needs.ready);
        if cfg!(target_os = "linux") {
            assert!(needs.can_grant, "pkexec and setcap present: the launcher can ask");
            assert!(needs.detail.contains("setcap cap_net_admin+ep /usr/bin/lan-helper"));
            let no_tools = helper_view(true, p.clone(), true, Some(Err("no cap".into())), false);
            assert!(!no_tools.can_grant, "without pkexec the command is shown instead");
        }

        // An AppImage's mount ignores file capabilities: granting would change
        // nothing, so it is not offered.
        let appimage = PathBuf::from("/tmp/.mount_MeshGaXYZ/usr/bin/lan-helper");
        let v = helper_view(true, Some(appimage), true, Some(Err("no cap".into())), true);
        assert!(!v.can_grant && v.detail.contains(".deb"));
    }

    fn shipped_packs() -> Vec<GamePack> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
        GamePack::load_dir(&dir).unwrap().packs
    }

    #[tokio::test]
    async fn a_game_without_a_lan_block_cannot_host_a_room() {
        let launcher = Launcher::new(shipped_packs());
        let err = launcher.host_room("sven-coop", None).await.unwrap_err().to_string();
        assert!(err.contains("[lan]"), "{err}");
        assert!(!launcher.room_status().await.active);
    }

    /// The test binary has no `lan-helper` beside it — exactly a broken
    /// install — so a room must refuse with the helper's own explanation rather
    /// than start a node it cannot put anywhere.
    #[tokio::test]
    async fn a_room_is_refused_with_the_reason_when_the_helper_is_not_ready() {
        let launcher = Launcher::new(shipped_packs());
        let lan_game = shipped_packs().into_iter().find(|p| p.lan.is_some()).unwrap().id;
        let helper = launcher.lan_helper().await;
        assert!(!helper.ready);
        let err = launcher.host_room(&lan_game, None).await.unwrap_err().to_string();
        assert_eq!(err, helper.detail);
        assert!(!launcher.room_status().await.active, "nothing was started");
    }

    #[tokio::test]
    async fn leaving_when_not_in_a_room_is_not_an_error() {
        let launcher = Launcher::new(shipped_packs());
        launcher.leave_room().await.unwrap();
        assert_eq!(launcher.room_status().await.adapter, "none");
    }

    #[test]
    fn a_pack_without_a_lan_block_offers_no_room() {
        assert!(lan_support(&GamePack::sven_coop()).is_none());
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
        let loaded = GamePack::load_dir(&dir).unwrap();
        let with_lan: Vec<_> = loaded.packs.iter().filter_map(lan_support).collect();
        assert!(!with_lan.is_empty(), "some shipped pack offers a room");
        for s in with_lan {
            assert!(s.inbound_any || !s.ports.is_empty());
        }
    }
}
