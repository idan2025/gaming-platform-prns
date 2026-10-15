//! Chat on a server (`ROADMAP.md` §6.1, `game_bridge::server_chat`): the
//! server this launcher joined, and the one it hosts.
//!
//! Rules a later change could quietly break:
//! - **Joining a server never waits on its chat.** The chat link is opened in
//!   the background after the join, from a [`ChatOpener`] taken out of the
//!   session, so no lock is held across its round trips.
//! - **Only a server whose announce offers chat is asked**; the bridge decides
//!   that from the server's own announce, and a server without it shows as
//!   "unavailable", never as an error.
//! - **The host decides who is muted, for the session only.** A joined
//!   player can only read and write.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use game_bridge::lan::valid_name;
use game_bridge::server_chat::ChatClient;
use game_bridge::ChatOpener;
use prns_core::identity::IdentityHash;
use serde::Serialize;

use crate::Launcher;

/// What a chat card shows about its chat.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ServerChatView {
    /// `"none"` (no server), `"opening"`, `"open"`, `"unavailable"` (the
    /// server has no chat) or `"closed"` (it ended).
    pub state: String,
    /// Why it is unavailable or closed, in words.
    pub detail: Option<String>,
    pub members: Vec<ServerChatMemberView>,
    /// Whether this is the chat of the server this launcher hosts, where the
    /// host can switch chat and mute.
    pub hosting: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerChatMemberView {
    /// The identity hash, hex: what a mute names.
    pub identity: String,
    /// What they call themselves. Data from another player: shown, never
    /// used to tell people apart.
    pub name: String,
    pub is_self: bool,
    pub is_host: bool,
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerChatLineView {
    pub seq: u64,
    pub identity: String,
    pub name: Option<String>,
    pub is_self: bool,
    pub is_host: bool,
    pub text: String,
    pub at_unix: u64,
}

pub(crate) enum JoinedChatState {
    Opening,
    Open(Arc<ChatClient>),
    Unavailable(String),
}

/// The chat of the server this launcher joined.
pub(crate) struct JoinedChat {
    me: IdentityHash,
    state: Arc<Mutex<JoinedChatState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for JoinedChat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A name a server will take: the player's, else a plain one.
pub(crate) fn chat_name(player_name: Option<String>) -> String {
    player_name.filter(|n| valid_name(n)).unwrap_or_else(|| "Player".to_string())
}

impl JoinedChat {
    /// Open `server`'s chat in the background.
    pub(crate) fn start(
        opener: ChatOpener,
        server: personal_rns::prelude::DestinationHash,
        name: String,
    ) -> Self {
        let state = Arc::new(Mutex::new(JoinedChatState::Opening));
        let me = opener.identity();
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let next = match opener.open(server, &name).await {
                Ok(client) => JoinedChatState::Open(Arc::new(client)),
                Err(e) => JoinedChatState::Unavailable(e.to_string()),
            };
            *task_state.lock().unwrap_or_else(|e| e.into_inner()) = next;
        });
        Self { me, state, task }
    }

    fn client(&self) -> Option<Arc<ChatClient>> {
        match &*self.state.lock().unwrap_or_else(|e| e.into_inner()) {
            JoinedChatState::Open(c) => Some(c.clone()),
            _ => None,
        }
    }
}

fn hex_of(id: &IdentityHash) -> String {
    hex::encode(id.as_bytes())
}

fn parse_identity(hex_id: &str) -> Result<IdentityHash> {
    let bytes: [u8; 16] = hex::decode(hex_id.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| anyhow!("{hex_id:?} is not an identity hash"))?;
    Ok(IdentityHash::new(bytes))
}

fn line_views(
    lines: Vec<game_bridge::server_chat::ChatLine>,
    names: &[(IdentityHash, String)],
    me: Option<IdentityHash>,
    host: Option<IdentityHash>,
) -> Vec<ServerChatLineView> {
    lines
        .into_iter()
        .map(|l| ServerChatLineView {
            seq: l.seq,
            identity: hex_of(&l.from),
            name: names.iter().find(|(id, _)| *id == l.from).map(|(_, n)| n.clone()),
            is_self: Some(l.from) == me,
            is_host: Some(l.from) == host,
            text: l.text,
            at_unix: l.at_unix,
        })
        .collect()
}

impl Launcher {
    /// The chat of the server this launcher joined.
    pub async fn server_chat_view(&self) -> ServerChatView {
        let inner = self.inner.lock().await;
        let Some(chat) = &inner.joined_chat else {
            return ServerChatView { state: "none".into(), ..Default::default() };
        };
        let state = chat.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            JoinedChatState::Opening => {
                ServerChatView { state: "opening".into(), ..Default::default() }
            }
            JoinedChatState::Unavailable(why) => ServerChatView {
                state: "unavailable".into(),
                detail: Some(why.clone()),
                ..Default::default()
            },
            JoinedChatState::Open(client) => {
                let names = client.names();
                // The server lists its host first.
                let host = names.first().map(|(id, _)| *id);
                ServerChatView {
                    state: if client.is_open() { "open" } else { "closed" }.into(),
                    detail: (!client.is_open()).then(|| "the server ended the chat".to_string()),
                    members: names
                        .iter()
                        .map(|(id, name)| ServerChatMemberView {
                            identity: hex_of(id),
                            name: name.clone(),
                            is_self: *id == chat.me,
                            is_host: Some(*id) == host,
                            muted: false,
                        })
                        .collect(),
                    hosting: false,
                }
            }
        }
    }

    /// Lines of the joined server's chat newer than `since`.
    pub async fn server_chat(&self, since: u64) -> Vec<ServerChatLineView> {
        let inner = self.inner.lock().await;
        let Some(chat) = &inner.joined_chat else { return Vec::new() };
        let Some(client) = chat.client() else { return Vec::new() };
        let names = client.names();
        let host = names.first().map(|(id, _)| *id);
        line_views(client.lines_since(since), &names, Some(chat.me), host)
    }

    /// Say one line in the joined server's chat.
    pub async fn send_server_chat(&self, text: &str) -> Result<()> {
        let inner = self.inner.lock().await;
        let client = inner
            .joined_chat
            .as_ref()
            .and_then(JoinedChat::client)
            .ok_or_else(|| anyhow!("this server's chat is not open"))?;
        client.say(text.trim()).map_err(|e| anyhow!("{e}"))
    }

    /// The chat of the server this launcher hosts.
    pub async fn hosted_chat_view(&self) -> ServerChatView {
        let inner = self.inner.lock().await;
        let Some(hub) = inner.host.as_ref().and_then(|h| h.session.chat()) else {
            return ServerChatView { state: "none".into(), ..Default::default() };
        };
        if !hub.enabled() {
            return ServerChatView {
                state: "unavailable".into(),
                detail: Some("chat is off for this server".into()),
                hosting: true,
                ..Default::default()
            };
        }
        let muted = hub.muted();
        let host = hub.host_identity();
        ServerChatView {
            state: "open".into(),
            detail: None,
            members: hub
                .names()
                .iter()
                .map(|(id, name)| ServerChatMemberView {
                    identity: hex_of(id),
                    name: name.clone(),
                    is_self: *id == host,
                    is_host: *id == host,
                    muted: muted.contains(id),
                })
                .collect(),
            hosting: true,
        }
    }

    /// Lines of the hosted server's chat newer than `since`.
    pub async fn hosted_chat(&self, since: u64) -> Vec<ServerChatLineView> {
        let inner = self.inner.lock().await;
        let Some(hub) = inner.host.as_ref().and_then(|h| h.session.chat()) else {
            return Vec::new();
        };
        let host = hub.host_identity();
        line_views(hub.lines_since(since), &hub.names(), Some(host), Some(host))
    }

    /// Say one line, as the host, in the hosted server's chat.
    pub async fn send_hosted_chat(&self, text: &str) -> Result<()> {
        let inner = self.inner.lock().await;
        let hub = inner
            .host
            .as_ref()
            .and_then(|h| h.session.chat())
            .ok_or_else(|| anyhow!("not hosting a server"))?;
        hub.host_say(text.trim()).map(|_| ()).map_err(|e| anyhow!("{e}"))
    }

    /// Turn the hosted server's chat on or off.
    pub async fn set_hosted_chat(&self, on: bool) -> Result<()> {
        let inner = self.inner.lock().await;
        let host = inner.host.as_ref().ok_or_else(|| anyhow!("not hosting a server"))?;
        host.session.set_chat(on);
        Ok(())
    }

    /// Mute or unmute someone in the hosted server's chat, until it stops.
    pub async fn mute_in_hosted_chat(&self, identity: &str, muted: bool) -> Result<()> {
        let id = parse_identity(identity)?;
        let inner = self.inner.lock().await;
        let hub = inner
            .host
            .as_ref()
            .and_then(|h| h.session.chat())
            .ok_or_else(|| anyhow!("not hosting a server"))?;
        if id == hub.host_identity() {
            return Err(anyhow!("the host cannot mute themselves"));
        }
        hub.set_muted(id, muted);
        Ok(())
    }

    /// The host's new name, in the hosted server's chat.
    pub(crate) async fn rename_in_hosted_chat(&self, name: &str) {
        let inner = self.inner.lock().await;
        if let Some(hub) = inner.host.as_ref().and_then(|h| h.session.chat()) {
            hub.set_host_name(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_view_key_the_frontend_reads_is_there() {
        let v = serde_json::to_value(ServerChatView {
            state: "open".into(),
            detail: None,
            members: vec![ServerChatMemberView {
                identity: "00".into(),
                name: "Ada".into(),
                is_self: true,
                is_host: false,
                muted: false,
            }],
            hosting: false,
        })
        .unwrap();
        for k in ["state", "detail", "members", "hosting"] {
            assert!(v.get(k).is_some(), "the frontend reads {k}");
        }
        for k in ["identity", "name", "is_self", "is_host", "muted"] {
            assert!(v["members"][0].get(k).is_some(), "the frontend reads members[].{k}");
        }
        let l = serde_json::to_value(ServerChatLineView {
            seq: 1,
            identity: "00".into(),
            name: None,
            is_self: false,
            is_host: true,
            text: "hi".into(),
            at_unix: 0,
        })
        .unwrap();
        for k in ["seq", "identity", "name", "is_self", "is_host", "text", "at_unix"] {
            assert!(l.get(k).is_some(), "the frontend reads {k}");
        }
        assert_eq!(chat_name(Some("two\nlines".into())), "Player");
        assert_eq!(chat_name(Some("Ada".into())), "Ada");
    }
}
