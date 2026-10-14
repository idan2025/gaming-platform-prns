//! Hosting a server from the launcher (`PLAN.md` §13.3 step 2).
//!
//! A player runs a game server on this computer — a dedicated server, or a
//! game's own "host a game" — and the launcher puts it on the mesh: the same
//! `BridgeSession::start_server` the `game-bridge server` command runs, so
//! hosting needs no terminal. Nothing here starts the game server itself:
//! which program runs is the player's, as on a node it is the operator's.
//!
//! Rules a later change could quietly break:
//! - **One hosted server at a time**, on its own identity
//!   (`server.identity` beside the settings), so its address stays the same
//!   from one evening to the next and is never the client's.
//! - **The interfaces are the browse node's**, as for a join
//!   (`join_interfaces`): a server attached to fewer interfaces than the
//!   launcher browses with is one nobody else can reach.
//! - **The name is data, not a console line**: it only reaches the announce,
//!   trimmed and held to the announce's length, and is refused if it is not
//!   one line of text.

use anyhow::{anyhow, bail, Result};
use game_bridge::config::ServerArgs;
use game_bridge::BridgeSession;
use serde::{Deserialize, Serialize};

use crate::{join_interfaces, Launcher};

/// What the player asks for.
#[derive(Debug, Clone, Deserialize)]
pub struct HostRequest {
    pub game_id: String,
    pub name: String,
    pub max_players: u8,
    /// The port the game server listens on here. `None` is the pack's default.
    pub game_port: Option<u16>,
    #[serde(default)]
    pub passworded: bool,
}

/// The server this launcher hosts, as the UI shows it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HostView {
    pub active: bool,
    pub game_id: Option<String>,
    pub name: Option<String>,
    pub game_port: Option<u16>,
    pub max_players: Option<u8>,
    /// Its mesh address, hex.
    pub destination_hash: Option<String>,
    /// A link that selects it in another launcher.
    pub invite: Option<String>,
    /// Players linked to it now.
    pub clients: usize,
}

pub(crate) struct HostState {
    pub(crate) session: BridgeSession,
    request: HostRequest,
}

impl Launcher {
    /// Put the game server on this computer's `port` on the mesh.
    pub async fn host_server(&self, request: HostRequest) -> Result<HostView> {
        let name = request.name.trim().to_string();
        if name.is_empty() || name.chars().any(char::is_control) {
            bail!("a server name is one line of text");
        }
        if name.len() > game_bridge::announce::MAX_NAME_LEN {
            bail!("a server name is at most {} characters", game_bridge::announce::MAX_NAME_LEN);
        }
        if request.max_players == 0 {
            bail!("a server needs room for at least one player");
        }
        let profile = self
            .profile_for(&request.game_id)
            .ok_or_else(|| anyhow!("no game pack installed for {:?}", request.game_id))?;
        let saved = self.saved_browse_opts().await;
        let identity = self.client_identity_path().with_file_name("server.identity");
        if let Some(parent) = identity.parent() {
            std::fs::create_dir_all(parent)?;
        }

        self.stop_hosting().await;
        let mut inner = self.inner.lock().await;
        let opts = join_interfaces(inner.browse_opts.as_ref(), &saved);
        if opts.tcp.is_none() && !opts.auto {
            bail!(
                "this launcher has no mesh interface, so nobody could reach the server. \
                 Add a hub or turn on looking on this network first"
            );
        }
        let mut args = ServerArgs::new(profile);
        if let Some(port) = request.game_port {
            args.game_port = port;
        }
        args.identity = identity;
        args.tcp = opts.tcp.clone();
        args.auto = opts.auto;
        args.name = Some(name.clone());
        args.max_players = request.max_players;
        args.passworded = request.passworded;
        // A player's own machine: a listen server as often as a dedicated one,
        // and never volunteered as a transit node for strangers.
        args.dedicated = false;
        args.relay_transit = false;
        let session = BridgeSession::start_server(args).await?;
        inner.host = Some(HostState { session, request: HostRequest { name, ..request } });
        drop(inner);
        Ok(self.hosting().await)
    }

    /// Take the hosted server off the mesh. The game server itself keeps
    /// running; it was never the launcher's.
    pub async fn stop_hosting(&self) {
        let host = self.inner.lock().await.host.take();
        if let Some(mut host) = host {
            host.session.stop().await;
        }
    }

    pub async fn hosting(&self) -> HostView {
        let inner = self.inner.lock().await;
        let Some(host) = &inner.host else { return HostView::default() };
        let hash = host.session.own_hash().map(|h| hex::encode(h.as_bytes()));
        let clients = host.session.connected_clients().await.len();
        let r = &host.request;
        HostView {
            active: true,
            game_id: Some(r.game_id.clone()),
            name: Some(r.name.clone()),
            game_port: r.game_port,
            max_players: Some(r.max_players),
            invite: hash
                .as_deref()
                .map(|h| crate::invite::link(h, Some(&r.game_id), Some(&r.name), false)),
            destination_hash: hash,
            clients,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_bridge::GamePack;

    fn req(name: &str, max: u8) -> HostRequest {
        HostRequest {
            game_id: "sven-coop".into(),
            name: name.into(),
            max_players: max,
            game_port: Some(1),
            passworded: false,
        }
    }

    /// Refused before anything starts: a name that is not one line, no room,
    /// an unknown game, and no way onto the mesh.
    #[tokio::test]
    async fn hosting_refuses_what_it_could_not_announce() {
        let dir = tempfile::tempdir().unwrap();
        let l = Launcher::new(vec![GamePack::sven_coop()])
            .with_settings_file(dir.path().join("launcher.json"));
        assert!(l.host_server(req("two\nlines", 8)).await.is_err());
        assert!(l.host_server(req(&"x".repeat(200), 8)).await.is_err());
        assert!(l.host_server(req("ok", 0)).await.is_err());
        let mut other = req("ok", 8);
        other.game_id = "nope".into();
        assert!(l.host_server(other).await.is_err());
        let e = l.host_server(req("Friday", 8)).await.unwrap_err().to_string();
        assert!(e.contains("no mesh interface"), "{e}");
        assert!(!l.hosting().await.active);
        let keys = serde_json::to_value(HostView::default()).unwrap();
        for k in [
            "active",
            "game_id",
            "name",
            "game_port",
            "max_players",
            "destination_hash",
            "invite",
            "clients",
        ] {
            assert!(keys.get(k).is_some(), "the frontend reads {k}");
        }
    }

    /// With a way onto the mesh it goes up: an address, an invite that
    /// selects it, and off again with Stop.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hosted_server_has_an_address_and_an_invite_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let l = Launcher::new(vec![GamePack::sven_coop()])
            .with_settings_file(dir.path().join("launcher.json"));
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        l.settings
            .lock()
            .await
            .interfaces
            .push(crate::settings::LauncherInterface::Tcp { addr: format!("0.0.0.0:{port}") });
        let view = l.host_server(req("Friday night", 8)).await.unwrap();
        assert!(view.active);
        let hash = view.destination_hash.clone().expect("an address");
        let invite = crate::invite::parse(view.invite.as_deref().unwrap()).unwrap();
        assert_eq!(invite.destination_hash, hash);
        assert_eq!(invite.name.as_deref(), Some("Friday night"));
        assert!(
            dir.path().join("server.identity").exists(),
            "its own identity, beside the settings"
        );
        assert_eq!(l.keeps_running().await, Some("server"));
        l.stop_hosting().await;
        assert!(!l.hosting().await.active);
    }
}
