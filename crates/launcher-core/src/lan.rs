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
    /// A room can start: the helper is there and has a way to its privilege.
    pub ready: bool,
    /// How the helper gets its privilege when a room starts:
    /// `"granted"` (Linux, permission granted once — no prompt),
    /// `"per-room"` (Linux, a password prompt each room, nothing installed),
    /// `"prompt"` (Windows' elevation prompt), or `"none"`.
    pub mode: String,
    /// The launcher can grant the permission once, so rooms stop prompting
    /// (Linux, installed — never portable or AppImage).
    pub can_grant: bool,
    /// The permission is granted and the launcher can take it back.
    pub can_revoke: bool,
    /// One line for a person: what happens, or what is missing and what to do.
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
    /// Whether this machine's firewall is keeping the room out.
    pub firewall: RoomFirewallView,
    /// What the room's own filter refused because the game's pack does not
    /// list it — a missing port, named.
    pub pack_gaps: RoomPackGapsView,
}

/// Traffic the room filter refused because the game's pack does not list its
/// port (`game_bridge::lan_filter::RefusedLog`). Not empty is the likely
/// reason a game can be seen and not joined, and the fix is the pack's.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RoomPackGapsView {
    /// The ports the pack most likely needs, as `"UDP 3660"`.
    pub ports: Vec<String>,
    /// What was refused, one line each, as a player would paste it.
    pub seen: Vec<String>,
    /// The whole finding as one paragraph, for a bug report.
    pub report: Option<String>,
    /// Of `ports`, those the Allow button would open: none below 1024, no
    /// remote-access or database service (`lan_filter::is_allowable`), none
    /// already allowed.
    pub can_allow: Vec<String>,
    /// Ports this machine already allows for this game on top of its pack's,
    /// which Undo takes back.
    pub allowed: Vec<String>,
    /// The last-resort switch is on: every port above 1024 is open to room
    /// members for this game, on this machine.
    pub wide_open: bool,
}

/// `"udp/3660"`, as settings keep an allowed port.
fn port_key(p: &game_bridge::lan_filter::LanPort) -> String {
    match p.proto {
        game_bridge::lan_filter::LanProto::Udp => format!("udp/{}", p.port),
        game_bridge::lan_filter::LanProto::Tcp => format!("tcp/{}", p.port),
    }
}

/// `"UDP 3660"`, as the pane names a port.
fn port_label(p: &game_bridge::lan_filter::LanPort) -> String {
    match p.proto {
        game_bridge::lan_filter::LanProto::Udp => format!("UDP {}", p.port),
        game_bridge::lan_filter::LanProto::Tcp => format!("TCP {}", p.port),
    }
}

/// A settings key back into a port; anything else is ignored, so a hand-edited
/// settings file cannot stop a room starting.
pub(crate) fn parse_port_key(key: &str) -> Option<game_bridge::lan_filter::LanPort> {
    use game_bridge::lan_filter::{is_allowable, LanPort, LanProto};
    let (proto, port) = key.split_once('/')?;
    let proto = match proto {
        "udp" => LanProto::Udp,
        "tcp" => LanProto::Tcp,
        _ => return None,
    };
    let port: u16 = port.parse().ok()?;
    is_allowable(port).then_some(LanPort { proto, port })
}

/// The ports a refused-traffic log says a pack is missing, by its fixed end.
fn suggested_ports(refused: &[game_bridge::lan_filter::Refused]) -> Vec<game_bridge::lan_filter::LanPort> {
    let mut out: Vec<game_bridge::lan_filter::LanPort> = Vec::new();
    for r in refused {
        let p = game_bridge::lan_filter::LanPort { proto: r.proto, port: r.suggested_port() };
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

impl RoomPackGapsView {
    fn from_refused(
        refused: &[game_bridge::lan_filter::Refused],
        allowed: &[game_bridge::lan_filter::LanPort],
    ) -> Self {
        let suggested = suggested_ports(refused);
        Self {
            ports: suggested.iter().map(port_label).collect(),
            seen: refused.iter().map(|r| r.describe()).collect(),
            report: (!refused.is_empty())
                .then(|| game_bridge::lan_check::missing_ports_finding(refused)),
            can_allow: suggested
                .iter()
                .filter(|p| game_bridge::lan_filter::is_allowable(p.port))
                .filter(|p| !allowed.contains(p))
                .map(port_label)
                .collect(),
            allowed: allowed.iter().map(port_label).collect(),
            wide_open: false,
        }
    }
}

/// This machine's firewall, as far as the room can tell (`game_bridge::lan_firewall`).
///
/// The room's helper opens the firewall itself where it runs with full
/// privilege (Windows, and a Linux room through `pkexec`), so most players
/// never see this. What is left for the UI: a **Fix** button where the helper
/// could not ([`Launcher::fix_room_firewall`]), Windows' own Block rules for a
/// program ([`Launcher::unblock_room_programs`]), and third-party firewalls,
/// which only the player can open.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RoomFirewallView {
    /// Members' connections that reached this machine and that nothing here
    /// answered, in the last ten minutes. Not empty means a firewall here is
    /// dropping the room's connections — the "can see it, cannot join" fault.
    pub dropped: Vec<RoomDroppedView>,
    /// What to do about it, once something was dropped; `command` is the
    /// thing to run.
    pub advice: Option<String>,
    /// A warning before anything was dropped, for a firewall whose default is
    /// known to refuse incoming connections (ufw, firewalld); `command` again.
    pub heads_up: Option<String>,
    /// The command that opens this machine's firewall to the room adapter, when
    /// the firewall is one it knows: the by-hand way, for whoever wants it.
    pub command: Option<String>,
    /// Whether the Fix button can do it for the player — one password or
    /// administrator prompt.
    pub can_fix: bool,
    /// Windows Block rules for a program, which beat any allow: what Windows
    /// made when someone clicked "Cancel" on that program's firewall prompt.
    /// Read once something was dropped.
    pub blocking_programs: Vec<RoomBlockView>,
    /// Third-party firewalls Windows reports as on. They ignore Windows
    /// Firewall's rules: only the player can open them.
    pub other_firewalls: Vec<String>,
}

/// One Windows Block rule, as the room pane names it.
#[derive(Debug, Clone, Serialize)]
pub struct RoomBlockView {
    /// The program's file name, e.g. `speed2.exe`.
    pub program: String,
    /// Where it is.
    pub path: String,
}

/// One connection nothing here answered.
#[derive(Debug, Clone, Serialize)]
pub struct RoomDroppedView {
    pub port: u16,
    /// Always `"tcp"`: only a connection has an answer to miss.
    pub transport: &'static str,
    /// The member that tried.
    pub from: String,
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
            firewall: RoomFirewallView::default(),
            pack_gaps: RoomPackGapsView::default(),
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

/// What the room check found (`game_bridge::lan_check`), as the UI shows it.
#[derive(Debug, Clone, Serialize)]
pub struct RoomCheckView {
    /// Every member heard this machine's broadcasts and answered.
    pub ok: bool,
    /// This machine's address in the room.
    pub address: String,
    /// The game's UDP port the probes went to.
    pub port: u16,
    /// What went wrong, in words a player can act on — or that nothing did.
    /// The first line is the verdict.
    pub findings: Vec<String>,
    pub members: Vec<RoomCheckMemberView>,
}

/// One other member, as the check saw it.
#[derive(Debug, Clone, Serialize)]
pub struct RoomCheckMemberView {
    pub address: String,
    pub ok: bool,
    /// Fastest answer there and back, when it answered at all.
    pub round_trip_ms: Option<u64>,
    /// The game's declared TCP ports on that member that nothing answered.
    pub tcp_unanswered: Vec<u16>,
}

impl From<game_bridge::lan_check::CheckReport> for RoomCheckView {
    fn from(r: game_bridge::lan_check::CheckReport) -> Self {
        Self {
            ok: r.ok(),
            address: r.address.to_string(),
            port: r.port,
            findings: r.findings(),
            members: r
                .members
                .iter()
                .map(|m| RoomCheckMemberView {
                    address: m.address.to_string(),
                    ok: m.ok(),
                    round_trip_ms: m.round_trip.map(|d| d.as_millis() as u64),
                    tcp_unanswered: m.tcp_unanswered(),
                })
                .collect(),
        }
    }
}

/// A running room, held by the launcher.
pub(crate) struct RoomState {
    role: &'static str,
    game_id: String,
    name: Option<String>,
    session: Arc<LanSession>,
    policy: game_bridge::lan_filter::LanPolicy,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<()>>,
    phase: watch::Receiver<AdapterPhase>,
    /// Which firewall this machine runs, read once when the room opens.
    firewall: game_bridge::lan_firewall::LocalFirewall,
    /// The room's helper runs with full privilege (Windows; Linux through
    /// `pkexec`), so it opened the firewall itself.
    helper_opened_firewall: bool,
    /// Windows' firewall facts, read once something is dropped.
    facts: Arc<std::sync::Mutex<FactsState>>,
}

/// [`RoomState::facts`]: read in the background, since PowerShell takes a
/// second or two and the room pane polls.
#[derive(Debug, Default)]
pub(crate) enum FactsState {
    #[default]
    Unread,
    Reading,
    Read(game_bridge::lan_firewall::WindowsFacts),
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

/// What [`Launcher::lan_helper`] found out, before deciding anything.
#[derive(Debug, Clone)]
pub(crate) struct HelperFacts {
    pub supported: bool,
    pub windows: bool,
    pub path: Option<PathBuf>,
    pub exists: bool,
    /// The helper's own `check`: its capability on Linux, Wintun on Windows.
    pub check: Option<Result<(), String>>,
    pub pkexec: bool,
    pub setcap: bool,
    pub portable: bool,
    pub appimage: bool,
}

/// The pure decision behind [`Launcher::lan_helper`], testable without a
/// helper on the machine.
pub(crate) fn helper_view(f: HelperFacts) -> LanHelperView {
    let path = f.path.as_ref().map(|p| p.display().to_string());
    let view = |ready: bool, mode: &str, can_grant: bool, can_revoke: bool, detail: String| {
        LanHelperView {
            supported: f.supported,
            path: path.clone(),
            ready,
            mode: mode.to_string(),
            can_grant,
            can_revoke,
            detail,
        }
    };
    if !f.supported {
        return view(
            false,
            "none",
            false,
            false,
            "LAN rooms need a room adapter, which this platform does not have yet (Linux and Windows do)."
                .to_string(),
        );
    }
    if !f.exists {
        return view(
            false,
            "none",
            false,
            false,
            "lan-helper is not beside this launcher, so it cannot make a room adapter. Reinstall, or \
             unpack the portable download again."
                .to_string(),
        );
    }
    let check_ok = matches!(f.check, Some(Ok(())));
    if f.windows {
        return if check_ok {
            view(
                true,
                "prompt",
                false,
                false,
                if f.portable {
                    "Ready. Windows asks for administrator rights when a room starts, for the helper \
                     only; when the room ends the Wintun driver is removed again."
                        .to_string()
                } else {
                    "Ready. Windows asks for administrator rights when a room starts, for the helper only."
                        .to_string()
                },
            )
        } else {
            view(
                false,
                "none",
                false,
                false,
                format!("lan-helper cannot run: {}", err_text(&f.check)),
            )
        };
    }
    // Linux. A capability can only live on an installed helper: an AppImage's
    // mount ignores it, and portable mode installs nothing by definition.
    let installed = !f.portable && !f.appimage;
    if check_ok {
        return view(
            true,
            "granted",
            false,
            installed,
            "Ready. lan-helper has its network permission, so rooms start without asking. Revoke it to \
             be asked each time instead."
                .to_string(),
        );
    }
    if f.pkexec {
        let can_grant = installed && f.setcap;
        return view(
            true,
            "per-room",
            can_grant,
            false,
            if can_grant {
                "Ready. Your password is asked each time a room starts, and nothing is installed or left \
                 behind. Grant the permission once to stop being asked."
                    .to_string()
            } else {
                "Ready. Your password is asked each time a room starts, and nothing is installed or left \
                 behind."
                    .to_string()
            },
        );
    }
    view(
        false,
        "none",
        false,
        false,
        format!(
            "LAN rooms need a way to ask for your password (polkit's pkexec), or a permission granted \
             once: sudo setcap cap_net_admin+ep {}",
            path.as_deref().unwrap_or("lan-helper")
        ),
    )
}

fn err_text(check: &Option<Result<(), String>>) -> String {
    match check {
        Some(Err(e)) => e.clone(),
        _ => "it could not be asked".to_string(),
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
        let portable = self.portable;
        tokio::task::spawn_blocking(move || {
            let path = helper_path();
            let exists = path.as_ref().is_some_and(|p| p.is_file());
            let supported = cfg!(any(target_os = "linux", windows));
            helper_view(HelperFacts {
                supported,
                windows: cfg!(windows),
                check: (supported && exists).then(|| run_check(path.as_ref().expect("exists"))),
                path,
                exists,
                pkexec: on_path("pkexec"),
                setcap: on_path("setcap"),
                portable,
                appimage: crate::portable::is_appimage(),
            })
        })
        .await
        .expect("the helper check does not panic")
    }

    /// Grant the helper its permission once, so rooms stop prompting (Linux,
    /// installed only). `pkexec` shows the desktop's own authentication dialog;
    /// the command it runs is fixed here and names only the helper beside this
    /// launcher.
    pub async fn grant_lan_helper(&self) -> Result<LanHelperView> {
        if !self.lan_helper().await.can_grant {
            return Err(anyhow!("this launcher cannot grant the helper a permission (portable, an AppImage, or not Linux)"));
        }
        // The helper grants itself and opens the firewall to the rooms, under
        // one password prompt. Run in place: the capability lands on this file.
        self.run_helper_elevated(vec!["grant".to_string()], true).await?;
        self.mark_room_firewall_opened().await;
        Ok(self.lan_helper().await)
    }

    /// Open this machine's firewall to LAN rooms, for good: the room pane's
    /// Fix. One password (Linux) or administrator (Windows) prompt.
    pub async fn fix_room_firewall(&self) -> Result<RoomView> {
        if self.portable {
            return Err(anyhow!("a portable launcher changes nothing outside its folder; its rooms open the firewall for themselves"));
        }
        self.run_helper_elevated(vec!["allow-rooms".to_string()], false).await?;
        self.mark_room_firewall_opened().await;
        self.forget_firewall_findings().await;
        Ok(self.room_status().await)
    }

    /// Disable the Windows Block rules the room pane listed — which beat any
    /// allow — through one administrator prompt.
    pub async fn unblock_room_programs(&self) -> Result<RoomView> {
        let names: Vec<String> = {
            let inner = self.inner.lock().await;
            let room = inner.room.as_ref().ok_or_else(|| anyhow!("not in a LAN room"))?;
            let facts = room.facts.lock().expect("facts lock");
            match &*facts {
                FactsState::Read(f) => f.blocks.iter().map(|b| hex::encode(b.name.as_bytes())).collect(),
                _ => Vec::new(),
            }
        };
        if names.is_empty() {
            return Err(anyhow!("Windows reports no program blocked"));
        }
        let mut args = vec!["unblock".to_string()];
        args.extend(names);
        self.run_helper_elevated(args, false).await?;
        self.forget_firewall_findings().await;
        Ok(self.room_status().await)
    }

    /// Let the ports the room named as missing through, for this game, on this
    /// machine, from now on: the room pane's Allow. Never a port below 1024.
    pub async fn allow_room_ports(&self) -> Result<RoomView> {
        let (game_id, ports) = {
            let inner = self.inner.lock().await;
            let room = inner.room.as_ref().ok_or_else(|| anyhow!("not in a LAN room"))?;
            let mut ports = room.session.extra_ports().ports();
            let new: Vec<_> = suggested_ports(&room.session.refused_log().refused(std::time::Instant::now()))
                .into_iter()
                .filter(|p| game_bridge::lan_filter::is_allowable(p.port))
                .filter(|p| !ports.contains(p))
                .collect();
            if new.is_empty() {
                return Err(anyhow!("the room has no missing port to allow"));
            }
            room.session.refused_log().forget(&new);
            ports.extend(new);
            room.session
                .extra_ports()
                .set(ports.clone())
                .map_err(|p| anyhow!("port {p} is a system or remote-access port and is never allowed"))?;
            (room.game_id.clone(), ports)
        };
        self.save_extra_ports(&game_id, &ports).await?;
        self.open_os_firewall_if_needed().await
    }

    /// The last resort, for a join that fails with no missing port named: let
    /// every port above 1024 through for this room's game on this machine (or
    /// stop). Never a system port, never the OS's own chatter; remembered per
    /// game. Turning it on also opens this computer's firewall to the room if
    /// it is not open yet, for whichever firewall it runs.
    pub async fn set_room_wide_open(&self, on: bool) -> Result<RoomView> {
        let game_id = {
            let inner = self.inner.lock().await;
            let room = inner.room.as_ref().ok_or_else(|| anyhow!("not in a LAN room"))?;
            room.session.extra_ports().set_any_high(on);
            room.game_id.clone()
        };
        {
            let mut settings = self.settings.lock().await;
            if on {
                settings.lan_wide_open.insert(game_id);
            } else {
                settings.lan_wide_open.remove(&game_id);
            }
            self.persist(&settings)?;
        }
        if on {
            return self.open_os_firewall_if_needed().await;
        }
        Ok(self.room_status().await)
    }

    /// The operating system's firewall is the second gate after the room's
    /// filter. Its room rule covers every port — ufw and firewalld trust the
    /// room adapter, Windows' rule the room range — so it needs opening only
    /// if it is not open yet: one password or administrator prompt.
    async fn open_os_firewall_if_needed(&self) -> Result<RoomView> {
        let view = self.room_status().await;
        let f = &view.firewall;
        if f.can_fix && (f.heads_up.is_some() || !f.dropped.is_empty()) {
            self.fix_room_firewall().await.map_err(|e| {
                anyhow!("allowed in the room, but this computer's firewall was not opened: {e}")
            })?;
            return Ok(self.room_status().await);
        }
        Ok(view)
    }

    /// Take back every port allowed for this room's game: the pack's own
    /// ports only, again.
    pub async fn reset_room_ports(&self) -> Result<RoomView> {
        let game_id = {
            let inner = self.inner.lock().await;
            let room = inner.room.as_ref().ok_or_else(|| anyhow!("not in a LAN room"))?;
            let _ = room.session.extra_ports().set(Vec::new());
            room.game_id.clone()
        };
        self.save_extra_ports(&game_id, &[]).await?;
        Ok(self.room_status().await)
    }

    async fn save_extra_ports(
        &self,
        game_id: &str,
        ports: &[game_bridge::lan_filter::LanPort],
    ) -> Result<()> {
        let mut settings = self.settings.lock().await;
        if ports.is_empty() {
            settings.lan_extra_ports.remove(game_id);
        } else {
            settings.lan_extra_ports.insert(game_id.to_string(), ports.iter().map(port_key).collect());
        }
        self.persist(&settings)
    }

    async fn run_helper_elevated(&self, args: Vec<String>, in_place: bool) -> Result<String> {
        #[cfg(any(target_os = "linux", windows))]
        {
            let path = helper_path()
                .filter(|p| p.is_file())
                .ok_or_else(|| anyhow!("lan-helper is not beside this launcher"))?;
            tokio::task::spawn_blocking(move || {
                game_bridge::lan_adapter::run_helper_elevated(&path, &args, in_place)
            })
            .await?
            .map_err(|e| anyhow!("{e}"))
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = (args, in_place);
            Err(anyhow!("LAN rooms are not built for this platform yet"))
        }
    }

    async fn mark_room_firewall_opened(&self) {
        let mut settings = self.settings.lock().await;
        if !settings.room_firewall_opened {
            settings.room_firewall_opened = true;
            if let Err(e) = self.persist(&settings) {
                tracing::debug!(error = %e, "remembering that the firewall was opened");
            }
        }
    }

    /// The firewall just changed: drop the old warnings and read Windows'
    /// facts again next time something is dropped.
    async fn forget_firewall_findings(&self) {
        let inner = self.inner.lock().await;
        if let Some(room) = &inner.room {
            room.session.connect_watch().forget();
            *room.facts.lock().expect("facts lock") = FactsState::Unread;
        }
    }

    /// Take the granted permission back: rooms prompt again, and nothing of
    /// the grant is left on the helper.
    pub async fn revoke_lan_helper(&self) -> Result<LanHelperView> {
        if !self.lan_helper().await.can_revoke {
            return Err(anyhow!("the helper holds no permission this launcher granted"));
        }
        self.setcap(&["-r"]).await?;
        Ok(self.lan_helper().await)
    }

    async fn setcap(&self, args: &'static [&'static str]) -> Result<()> {
        let path = helper_path()
            .filter(|p| p.is_file())
            .ok_or_else(|| anyhow!("lan-helper is not installed beside this launcher"))?;
        let status = tokio::task::spawn_blocking(move || {
            std::process::Command::new("pkexec").arg("setcap").args(args).arg(&path).status()
        })
        .await?
        .context("running pkexec")?;
        if !status.success() {
            return Err(anyhow!("the change was not made ({status})"));
        }
        Ok(())
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
        // Ports this player allowed for this game before, on top of its pack's.
        let extra: Vec<_> = self
            .settings
            .lock()
            .await
            .lan_extra_ports
            .get(&game_id)
            .map(|keys| keys.iter().filter_map(|k| parse_port_key(k)).collect())
            .unwrap_or_default();
        let _ = session.extra_ports().set(extra);
        if self.settings.lock().await.lan_wide_open.contains(&game_id) {
            session.extra_ports().set_any_high(true);
        }
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (phase_tx, phase_rx) = watch::channel(AdapterPhase::WaitingForSeat);
        let task = spawn_room(
            session.clone(),
            lan.policy(),
            helper_path,
            self.portable,
            stop_rx,
            phase_tx,
        );
        let firewall = tokio::task::spawn_blocking(game_bridge::lan_firewall::detect)
            .await
            .unwrap_or(game_bridge::lan_firewall::LocalFirewall::Unknown);
        let helper_opened_firewall = helper.mode != "granted";
        inner.room = Some(RoomState {
            role,
            game_id,
            name,
            session,
            policy: lan.policy(),
            stop: Some(stop_tx),
            task,
            phase: phase_rx,
            firewall,
            helper_opened_firewall,
            facts: Default::default(),
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
        // session can be stopped and its node's thread joined. A room check
        // still running holds one too, for a few seconds at most.
        let mut session = room.session;
        let deadline = tokio::time::Instant::now() + LEAVE_TIMEOUT;
        loop {
            match Arc::try_unwrap(session) {
                Ok(mut s) => {
                    s.stop().await;
                    break;
                }
                Err(still_shared) if tokio::time::Instant::now() < deadline => {
                    session = still_shared;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(_) => break,
            }
        }
        Ok(())
    }

    /// Check the room from this machine: a real broadcast on the game's own
    /// port, which every member's launcher answers (`game_bridge::lan_check`).
    /// Takes a few seconds; the room stays usable meanwhile.
    pub async fn check_room(&self) -> Result<RoomCheckView> {
        let (session, policy) = {
            let inner = self.inner.lock().await;
            let room = inner.room.as_ref().ok_or_else(|| anyhow!("not in a LAN room"))?;
            if !matches!(*room.phase.borrow(), AdapterPhase::Up { .. }) {
                return Err(anyhow!("the room's network adapter is not up yet"));
            }
            let mut policy = room.policy.clone();
            for p in room.session.extra_ports().ports() {
                if !policy.ports.contains(&p) {
                    policy.ports.push(p);
                }
            }
            (room.session.clone(), policy)
        };
        let report = game_bridge::lan_check::check_room(&session, &policy, ADAPTER_NAME).await?;
        Ok(report.into())
    }

    /// The room this launcher is in, as the UI shows it.
    pub async fn room_status(&self) -> RoomView {
        let inner_flag = self.settings.lock().await.room_firewall_opened;
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
            firewall: firewall_view(room, self.portable, inner_flag),
            pack_gaps: RoomPackGapsView {
                wide_open: room.session.extra_ports().any_high(),
                ..RoomPackGapsView::from_refused(
                    &room.session.refused_log().refused(std::time::Instant::now()),
                    &room.session.extra_ports().ports(),
                )
            },
        }
    }

    /// The room identity: its own file beside the settings, like the client's.
    fn lan_identity_path(&self) -> PathBuf {
        self.client_identity_path().with_file_name("lan.identity")
    }
}

/// Whether the Fix button can open the firewall, and whether to warn before
/// anything is dropped.
///
/// A portable launcher installs nothing, so its Fix would be the per-room
/// helper's job — which already ran. The heads-up is only for a room whose
/// helper could not open the firewall itself: a capability-granted Linux
/// helper cannot run ufw. Once the player has opened it, it stops.
pub(crate) fn fix_and_heads_up(
    firewall: game_bridge::lan_firewall::LocalFirewall,
    portable: bool,
    helper_present: bool,
    helper_opened_firewall: bool,
    opened_before: bool,
) -> (bool, bool) {
    let can_fix =
        firewall != game_bridge::lan_firewall::LocalFirewall::Unknown && !portable && helper_present;
    (can_fix, can_fix && !helper_opened_firewall && !opened_before)
}

/// What the room's firewall watch has seen, and what to tell the player.
fn firewall_view(room: &RoomState, portable: bool, opened_before: bool) -> RoomFirewallView {
    use game_bridge::lan_firewall::{advice, fix_command, heads_up, LocalFirewall};
    let dropped: Vec<RoomDroppedView> = room
        .session
        .connect_watch()
        .dropped(std::time::Instant::now())
        .into_iter()
        .map(|d| RoomDroppedView { port: d.port, transport: "tcp", from: d.from.to_string() })
        .collect();
    let (can_fix, warn_first) = fix_and_heads_up(
        room.firewall,
        portable,
        helper_path().is_some_and(|p| p.is_file()),
        room.helper_opened_firewall,
        opened_before,
    );
    let mut view = RoomFirewallView {
        advice: (!dropped.is_empty()).then(|| advice(room.firewall, ADAPTER_NAME)),
        heads_up: warn_first.then(|| heads_up(room.firewall)).flatten(),
        command: fix_command(room.firewall, ADAPTER_NAME),
        can_fix,
        blocking_programs: Vec::new(),
        other_firewalls: Vec::new(),
        dropped,
    };
    if room.firewall == LocalFirewall::Windows && !view.dropped.is_empty() {
        let mut facts = room.facts.lock().expect("facts lock");
        match &*facts {
            FactsState::Unread => {
                *facts = FactsState::Reading;
                let slot = room.facts.clone();
                tokio::task::spawn_blocking(move || {
                    let read = game_bridge::lan_firewall::windows_facts(&game_bridge::lan_firewall::System)
                        .unwrap_or_default();
                    *slot.lock().expect("facts lock") = FactsState::Read(read);
                });
            }
            FactsState::Reading => {}
            FactsState::Read(f) => {
                view.blocking_programs = f
                    .blocks
                    .iter()
                    .map(|b| RoomBlockView {
                        program: b
                            .program
                            .rsplit(['\\', '/'])
                            .next()
                            .unwrap_or(&b.program)
                            .to_string(),
                        path: b.program.clone(),
                    })
                    .collect();
                view.other_firewalls = f.other_firewalls.clone();
            }
        }
    }
    view
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
    portable: bool,
    stop: oneshot::Receiver<()>,
    phase: watch::Sender<AdapterPhase>,
) -> tokio::task::JoinHandle<Result<()>> {
    use game_bridge::lan_adapter::{run_room_on_adapter_reporting, AdapterSetup};
    tokio::spawn(run_room_on_adapter_reporting(
        session,
        policy,
        ADAPTER_NAME.to_string(),
        AdapterSetup::Helper { path: helper, portable },
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
    _portable: bool,
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
                "firewall",
                "game_id",
                "members",
                "name",
                "pack_gaps",
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
        let f = serde_json::to_value(RoomFirewallView {
            dropped: vec![RoomDroppedView { port: 9900, transport: "tcp", from: "198.19.0.2".into() }],
            advice: Some("ufw is refusing incoming connections here.".into()),
            heads_up: None,
            command: Some("sudo ufw allow in on gbl0".into()),
            can_fix: true,
            blocking_programs: vec![RoomBlockView {
                program: "speed2.exe".into(),
                path: "C:\\Games\\speed2.exe".into(),
            }],
            other_firewalls: vec!["Norton Firewall".into()],
        })
        .unwrap();
        assert_eq!(
            keys(&f),
            [
                "advice",
                "blocking_programs",
                "can_fix",
                "command",
                "dropped",
                "heads_up",
                "other_firewalls"
            ]
        );
        assert_eq!(keys(&f["dropped"][0]), ["from", "port", "transport"]);
        assert_eq!(keys(&f["blocking_programs"][0]), ["path", "program"]);
        let g = serde_json::to_value(RoomPackGapsView::default()).unwrap();
        assert_eq!(keys(&g), ["allowed", "can_allow", "ports", "report", "seen", "wide_open"]);
    }

    fn facts() -> HelperFacts {
        HelperFacts {
            supported: true,
            windows: false,
            path: Some(PathBuf::from("/usr/bin/lan-helper")),
            exists: true,
            check: Some(Err("no CAP_NET_ADMIN".into())),
            pkexec: true,
            setcap: true,
            portable: false,
            appimage: false,
        }
    }

    #[test]
    fn room_check_json_keys_are_the_frontend_contract() {
        let report = game_bridge::lan_check::CheckReport {
            address: "198.19.0.1".parse().unwrap(),
            port: 9999,
            members: vec![game_bridge::lan_check::MemberCheck {
                address: "198.19.0.2".parse().unwrap(),
                heard_limited_broadcast: true,
                heard_subnet_broadcast: true,
                answered_unicast: true,
                round_trip: Some(std::time::Duration::from_millis(12)),
                tcp: vec![game_bridge::lan_check::TcpCheck {
                    port: 9900,
                    state: game_bridge::lan_check::TcpState::NoAnswer,
                }],
            }],
            limited_broadcast_left: true,
            subnet_broadcast_left: true,
            answers_blocked_here: false,
            dropped_here: Vec::new(),
            firewall_advice: None,
            refused_here: Vec::new(),
        };
        let v = serde_json::to_value(RoomCheckView::from(report)).unwrap();
        assert_eq!(keys(&v), ["address", "findings", "members", "ok", "port"]);
        assert_eq!(keys(&v["members"][0]), ["address", "ok", "round_trip_ms", "tcp_unanswered"]);
        assert_eq!(v["ok"], false, "an unanswered TCP port fails the member");
        assert_eq!(v["members"][0]["tcp_unanswered"][0], 9900);
        assert_eq!(v["members"][0]["round_trip_ms"], 12);
    }

    #[test]
    fn a_pack_gap_names_the_port_once_and_keeps_the_report() {
        use game_bridge::lan_filter::{DropDirection, RefusedLog};
        let log = RefusedLog::default();
        let now = std::time::Instant::now();
        let udp = |sport: u16, dport: u16| {
            let mut p = vec![0u8; 28];
            p[0] = 0x45;
            p[9] = 17;
            p[12..16].copy_from_slice(&[198, 19, 1, 1]);
            p[16..20].copy_from_slice(&[198, 19, 4, 2]);
            p[20..22].copy_from_slice(&sport.to_be_bytes());
            p[22..24].copy_from_slice(&dport.to_be_bytes());
            p
        };
        // Two kinds of packet that need the same port.
        log.saw(DropDirection::Inbound, &udp(3660, 3660), now);
        log.saw(DropDirection::Inbound, &udp(3660, 50000), now);
        // A probe at a system port is listed but never offered.
        let mut ssh = udp(40000, 22);
        ssh[9] = 6;
        log.saw(DropDirection::Inbound, &ssh, now);
        // So is a member knocking on Remote Desktop, which is above 1024.
        let mut rdp = udp(40001, 3389);
        rdp[9] = 6;
        log.saw(DropDirection::Inbound, &rdp, now);
        let v = RoomPackGapsView::from_refused(&log.refused(now), &[]);
        assert_eq!(v.ports.iter().filter(|p| *p == "UDP 3660").count(), 1);
        assert!(v.ports.contains(&"TCP 22".to_string()), "{:?}", v.ports);
        assert!(v.ports.contains(&"TCP 3389".to_string()), "{:?}", v.ports);
        assert_eq!(v.can_allow, ["UDP 3660"], "never a system or remote-access port");
        assert_eq!(v.seen.len(), 4);
        assert!(v.report.unwrap().contains("Please report this"));
        let allowed = [game_bridge::lan_filter::LanPort {
            proto: game_bridge::lan_filter::LanProto::Udp,
            port: 3660,
        }];
        let after = RoomPackGapsView::from_refused(&[], &allowed);
        assert!(after.report.is_none() && after.can_allow.is_empty());
        assert_eq!(after.allowed, ["UDP 3660"]);
    }

    #[test]
    fn an_allowed_port_round_trips_through_settings_and_a_system_port_never_does() {
        let p = parse_port_key("udp/3660").unwrap();
        assert_eq!(port_key(&p), "udp/3660");
        assert!(parse_port_key("tcp/22").is_none());
        assert!(parse_port_key("tcp/3389").is_none(), "a hand-edited RDP entry is ignored");
        assert!(parse_port_key("icmp/3660").is_none());
        assert!(parse_port_key("udp/notaport").is_none());
    }

    /// The player who granted the helper before the launcher opened firewalls
    /// is the one a heads-up is for; nobody whose room already did it.
    #[test]
    fn a_heads_up_is_only_for_a_room_whose_helper_could_not_open_the_firewall() {
        use game_bridge::lan_firewall::LocalFirewall::{Ufw, Unknown, Windows};
        assert_eq!(fix_and_heads_up(Ufw, false, true, false, false), (true, true), "granted, never opened");
        assert_eq!(fix_and_heads_up(Ufw, false, true, true, false), (true, false), "per-room: the helper did it");
        assert_eq!(fix_and_heads_up(Ufw, false, true, false, true), (true, false), "opened before");
        assert_eq!(fix_and_heads_up(Ufw, true, true, false, false), (false, false), "portable installs nothing");
        assert_eq!(fix_and_heads_up(Unknown, false, true, false, false), (false, false));
        assert_eq!(fix_and_heads_up(Windows, false, false, true, false), (false, false), "no helper");
    }

    #[tokio::test]
    async fn checking_outside_a_room_says_so() {
        let launcher = Launcher::new(shipped_packs());
        let err = launcher.check_room().await.unwrap_err().to_string();
        assert!(err.contains("not in a LAN room"), "{err}");
    }

    #[test]
    fn helper_view_json_keys_are_the_frontend_contract() {
        let v = serde_json::to_value(helper_view(facts())).unwrap();
        assert_eq!(
            keys(&v),
            ["can_grant", "can_revoke", "detail", "mode", "path", "ready", "supported"]
        );
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

    /// Each state a person can be in gets a sentence that says what happens.
    #[test]
    fn an_installed_linux_helper_prompts_per_room_until_granted_and_can_be_revoked() {
        let per_room = helper_view(facts());
        assert!(per_room.ready, "pkexec is enough: a room can start");
        assert_eq!(per_room.mode, "per-room");
        assert!(per_room.can_grant && !per_room.can_revoke);
        assert!(per_room.detail.contains("nothing is installed"));

        let granted = helper_view(HelperFacts { check: Some(Ok(())), ..facts() });
        assert_eq!(granted.mode, "granted");
        assert!(granted.ready && granted.can_revoke && !granted.can_grant);

        let stuck = helper_view(HelperFacts { pkexec: false, ..facts() });
        assert!(
            !stuck.ready && stuck.detail.contains("setcap cap_net_admin+ep /usr/bin/lan-helper")
        );
    }

    /// Portable and AppImage install nothing: never offer a grant, never a
    /// revoke, always the per-room prompt.
    #[test]
    fn a_portable_or_appimage_launcher_never_offers_to_install_anything() {
        for f in
            [HelperFacts { portable: true, ..facts() }, HelperFacts { appimage: true, ..facts() }]
        {
            let v = helper_view(f);
            assert!(v.ready && v.mode == "per-room" && !v.can_grant && !v.can_revoke, "{v:?}");
        }
        let portable_granted =
            helper_view(HelperFacts { portable: true, check: Some(Ok(())), ..facts() });
        assert!(!portable_granted.can_revoke, "a portable launcher granted nothing to revoke");
    }

    #[test]
    fn windows_asks_at_room_start_and_portable_mode_says_the_driver_goes() {
        let win = HelperFacts {
            windows: true,
            check: Some(Ok(())),
            pkexec: false,
            setcap: false,
            ..facts()
        };
        let v = helper_view(win.clone());
        assert!(v.ready && v.mode == "prompt" && !v.can_grant && !v.can_revoke);
        let p = helper_view(HelperFacts { portable: true, ..win.clone() });
        assert!(p.detail.contains("driver is removed"));
        let no_dll = helper_view(HelperFacts { check: Some(Err("no wintun.dll".into())), ..win });
        assert!(!no_dll.ready && no_dll.detail.contains("wintun.dll"));
    }

    #[test]
    fn a_missing_or_unsupported_helper_says_so() {
        assert!(!helper_view(HelperFacts { exists: false, ..facts() }).ready);
        assert!(!helper_view(HelperFacts { supported: false, ..facts() }).supported);
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
