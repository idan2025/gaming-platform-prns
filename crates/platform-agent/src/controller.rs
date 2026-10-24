//! The node's own page placing servers through a controller (`ROADMAP.md`
//! §13.5).
//!
//! A worker's page is a view of one machine. With a `[controller]` section the
//! same page creates and lists servers through an index instead, so an operator
//! who knows that page places servers across workers and sees all of them. The
//! index picks the node and drives it, exactly as it does for any other request.
//!
//! The node is a client here, not an authority. It logs in to the index with
//! its uplink identity, and the login's signature is bound to the index's
//! identity hash, so a server that is not that index cannot replay it.
//!
//! **The allowlist is the rule.** [`allowed_path`] is the whole of what a node's
//! page may do through a controller. Console, map change, bots, chat, players,
//! world upload and backup download are not on it, and they stay on the node:
//! the controller never gains a console route (`CLAUDE.md`).

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use axum::http::Method;
use personal_rns::load_or_create_identity_secret;
use personal_rns::prelude::{Zeroizing, IDENTITY_SECRET_KEY_LEN};
use platform_auth::{answer_challenge, Challenge, ChallengeResponse, Session};
use prns_core::identity::{IdentityHash, PrivateIdentityMaterial};
use tokio::sync::Mutex;

use crate::backups::valid_name;
use crate::config::{controller_url_is_acceptable, ControllerConfig};

/// The most a reply from the controller may be before this node refuses it.
const MAX_REPLY: usize = 4 * 1024 * 1024;

/// How long a move through the controller may take (`POST
/// /instances/:id/relocate`): the index's own loopback transfer allows 30
/// minutes, and this waits on it.
const MOVE_TIMEOUT: Duration = Duration::from_secs(35 * 60);

/// How long one server list from the controller answers every page open on
/// this node. Under the page's own poll (5 s), so a page sees each change on
/// its next poll; without it every open page cost the index one listing per
/// poll, and a listing asks every worker.
const LIST_FRESH: Duration = Duration::from_secs(4);

/// What the controller answered, for the node to pass on unchanged.
#[derive(Clone)]
pub struct Forwarded {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

pub struct Controller {
    base: String,
    index: IdentityHash,
    secret: Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>,
    http: reqwest::Client,
    /// The session token, held in memory only. Dropped on a refusal, so the
    /// next request logs in again.
    token: Mutex<Option<String>>,
    /// The last server list and when it came. Held while a list is fetched, so
    /// pages that poll together wait for one request rather than send one
    /// each; dropped by any change sent through the controller.
    list: Mutex<Option<(Instant, Forwarded)>>,
}

impl Controller {
    /// The index's address, as the operator wrote it. Shown on the page, never
    /// the token.
    pub fn address(&self) -> &str {
        &self.base
    }

    /// Read the node's identity (made on first use, like the uplink's) and pin
    /// the index. Refuses an address `config::controller_url_is_acceptable` does
    /// not allow, so a caller that skipped validation still cannot send a token
    /// in the clear.
    pub fn new(config: &ControllerConfig, identity_secret_path: &Path) -> Result<Self> {
        if !controller_url_is_acceptable(&config.url) {
            return Err(anyhow!("controller url {:?} is not acceptable", config.url));
        }
        let index = parse_identity(&config.identity)?;
        let secret = load_or_create_identity_secret(identity_secret_path)
            .map_err(|e| anyhow!("loading the node's identity: {e}"))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("building the controller client")?;
        Ok(Self {
            base: config.url.trim_end_matches('/').to_string(),
            index,
            secret,
            http,
            token: Mutex::new(None),
            list: Mutex::new(None),
        })
    }

    /// Send one request to the controller, logging in first if needed. The
    /// server list is shared by every page for [`LIST_FRESH`]; any other
    /// request that is not a read drops it, before and after, so the list a
    /// page sees next includes the change.
    pub async fn forward(&self, method: &Method, path: &str, body: Option<&[u8]>) -> Result<Forwarded> {
        if *method == Method::GET && path == "/instances" {
            let mut list = self.list.lock().await;
            if let Some((at, reply)) = list.as_ref() {
                if at.elapsed() < LIST_FRESH {
                    return Ok(reply.clone());
                }
            }
            let reply = self.forward_now(method, path, body).await?;
            *list = (reply.status == 200).then(|| (Instant::now(), reply.clone()));
            return Ok(reply);
        }
        if *method == Method::GET {
            return self.forward_now(method, path, body).await;
        }
        *self.list.lock().await = None;
        let reply = self.forward_now(method, path, body).await;
        *self.list.lock().await = None;
        reply
    }

    /// One request, no cache. A refusal that looks like an expired session is
    /// retried once after a fresh login.
    async fn forward_now(&self, method: &Method, path: &str, body: Option<&[u8]>) -> Result<Forwarded> {
        let token = self.token_or_login().await?;
        let first = self.send(method, path, body, &token).await?;
        if first.status != 401 {
            return Ok(first);
        }
        *self.token.lock().await = None;
        let token = self.login().await?;
        self.send(method, path, body, &token).await
    }

    async fn token_or_login(&self) -> Result<String> {
        if let Some(t) = self.token.lock().await.clone() {
            return Ok(t);
        }
        self.login().await
    }

    async fn send(&self, method: &Method, path: &str, body: Option<&[u8]>, token: &str) -> Result<Forwarded> {
        let mut request = self
            .http
            .request(method.clone(), format!("{}{}", self.base, path))
            .bearer_auth(token);
        if let Some(bytes) = body {
            request = request.header("Content-Type", "application/json").body(bytes.to_vec());
        }
        // A move carries a server's files between workers and takes as long
        // as that takes; everything else is one quick reply.
        if path.ends_with("/relocate") {
            request = request.timeout(MOVE_TIMEOUT);
        }
        let response = request.send().await.context("asking the controller")?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response.bytes().await.context("reading the controller's reply")?;
        if body.len() > MAX_REPLY {
            return Err(anyhow!("the controller's reply is {} bytes, over the cap", body.len()));
        }
        Ok(Forwarded { status, content_type, body: body.to_vec() })
    }

    /// Challenge, sign bound to the pinned index, verify, and keep the token.
    async fn login(&self) -> Result<String> {
        let challenge: Challenge = self
            .http
            .post(format!("{}/auth/challenge", self.base))
            .send()
            .await
            .context("asking the controller for a challenge")?
            .error_for_status()
            .context("the controller refused a challenge")?
            .json()
            .await
            .context("reading the controller's challenge")?;
        let identity = PrivateIdentityMaterial::from_slice(&self.secret[..])
            .map_err(|e| anyhow!("the node's identity key: {e:?}"))?;
        let response: ChallengeResponse =
            answer_challenge(&challenge, &self.index, &identity).map_err(|e| anyhow!("{e}"))?;
        let session: Session = self
            .http
            .post(format!("{}/auth/verify", self.base))
            .json(&response)
            .send()
            .await
            .context("sending the login to the controller")?
            .error_for_status()
            .context("the controller refused the login")?
            .json()
            .await
            .context("reading the controller's session")?;
        *self.token.lock().await = Some(session.token.clone());
        Ok(session.token)
    }
}

fn parse_identity(hex_str: &str) -> Result<IdentityHash> {
    let bytes = hex::decode(hex_str).map_err(|e| anyhow!("bad controller identity: {e}"))?;
    let arr: [u8; 16] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("controller identity is not 16 bytes"))?;
    Ok(IdentityHash::new(arr))
}

/// The index path a node's page may reach through a controller, or `None`.
///
/// `rest` is the part of the request path after `/controller/`, with its query
/// if any. Ids and backup names are checked as data before they are put into a
/// path, so `..` or a slash inside one cannot reach another index route.
pub fn allowed_path(method: &Method, rest: &str) -> Option<String> {
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    let segments: Vec<&str> = path.split('/').collect();
    let no_query = || query.is_none();
    match segments.as_slice() {
        ["instances"] => match *method {
            Method::GET | Method::POST if no_query() => Some("/instances".into()),
            _ => None,
        },
        ["instances", id] if valid_id(id) => match *method {
            Method::GET | Method::DELETE if no_query() => Some(format!("/instances/{id}")),
            _ => None,
        },
        ["instances", id, action @ ("stop" | "start" | "relocate")] if valid_id(id) && *method == Method::POST && no_query() => {
            Some(format!("/instances/{id}/{action}"))
        }
        ["instances", id, "logs"] if valid_id(id) && *method == Method::GET => {
            let lines = match query {
                None => None,
                Some(q) => Some(q.strip_prefix("lines=").filter(|n| is_small_number(n))?),
            };
            Some(match lines {
                Some(n) => format!("/instances/{id}/logs?lines={n}"),
                None => format!("/instances/{id}/logs"),
            })
        }
        ["instances", id, "backups"] if valid_id(id) && no_query() => match *method {
            Method::GET | Method::POST => Some(format!("/instances/{id}/backups")),
            _ => None,
        },
        ["instances", id, "backups", name] if valid_id(id) && no_query() && valid_name(name) => {
            (*method == Method::DELETE).then(|| format!("/instances/{id}/backups/{name}"))
        }
        ["instances", id, "backups", name, "restore"]
            if valid_id(id) && no_query() && valid_name(name) && *method == Method::POST =>
        {
            Some(format!("/instances/{id}/backups/{name}/restore"))
        }
        _ => None,
    }
}

/// A server id the platform made: lower-case letters, digits and dashes. Checked
/// before it is put in a path, so it cannot carry a slash, a dot or a query.
fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn is_small_number(n: &str) -> bool {
    (1..=4).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_servers_page_actions_are_reachable_through_the_controller() {
        let id = "sven-coop-1a2b3c";
        let cases = [
            (Method::GET, "instances".to_string(), "/instances"),
            (Method::POST, "instances".to_string(), "/instances"),
            (Method::GET, format!("instances/{id}"), "/instances/sven-coop-1a2b3c"),
            (Method::DELETE, format!("instances/{id}"), "/instances/sven-coop-1a2b3c"),
            (Method::POST, format!("instances/{id}/stop"), "/instances/sven-coop-1a2b3c/stop"),
            (Method::POST, format!("instances/{id}/start"), "/instances/sven-coop-1a2b3c/start"),
            (Method::POST, format!("instances/{id}/relocate"), "/instances/sven-coop-1a2b3c/relocate"),
            (Method::GET, format!("instances/{id}/logs?lines=50"), "/instances/sven-coop-1a2b3c/logs?lines=50"),
            (Method::GET, format!("instances/{id}/backups"), "/instances/sven-coop-1a2b3c/backups"),
            (Method::POST, format!("instances/{id}/backups"), "/instances/sven-coop-1a2b3c/backups"),
            (
                Method::POST,
                format!("instances/{id}/backups/20261022-101112.tar.gz/restore"),
                "/instances/sven-coop-1a2b3c/backups/20261022-101112.tar.gz/restore",
            ),
            (
                Method::DELETE,
                format!("instances/{id}/backups/20261022-101112.tar.gz"),
                "/instances/sven-coop-1a2b3c/backups/20261022-101112.tar.gz",
            ),
        ];
        for (method, rest, want) in cases {
            assert_eq!(allowed_path(&method, &rest).as_deref(), Some(want), "{method} {rest}");
        }
    }

    /// The point of the list: nothing that types at a server, reads a player's
    /// data or moves a file in or out is reachable. Each of these would be a
    /// console route or a file route through the controller.
    #[test]
    fn nothing_that_types_at_a_server_or_moves_its_files_is_reachable() {
        let id = "sven-coop-1a2b3c";
        for (method, rest) in [
            (Method::POST, format!("instances/{id}/console")),
            (Method::POST, format!("instances/{id}/map")),
            (Method::POST, format!("instances/{id}/bots")),
            (Method::POST, format!("instances/{id}/chat")),
            (Method::GET, format!("instances/{id}/players")),
            (Method::GET, format!("instances/{id}/backups/20261022-101112.tar.gz/file")),
            (Method::POST, format!("instances/{id}/backups/import")),
            (Method::PUT, format!("instances/{id}/backup-schedule")),
            (Method::POST, format!("instances/{id}/restart")),
            (Method::POST, format!("instances/{id}/world")),
            (Method::POST, format!("instances/{id}/restart-on-crash")),
        ] {
            assert_eq!(allowed_path(&method, &rest), None, "{method} {rest} must not reach the controller");
        }
    }

    /// A path the platform did not make cannot be turned into another index route
    /// by the id or name in it.
    #[test]
    fn an_id_or_name_that_could_change_the_route_is_refused() {
        for rest in [
            "instances/../auth/verify",
            "instances/a/../../health",
            "instances/a%2F..%2Fx",
            "instances/A-UPPER",
            "instances/a.b",
            "instances/a?x=1",
        ] {
            assert_eq!(allowed_path(&Method::GET, rest), None, "{rest}");
        }
        assert_eq!(
            allowed_path(&Method::DELETE, "instances/sven-coop-1/backups/../../../x"),
            None,
            "a backup name is the agent's own shape, not a path"
        );
        assert_eq!(
            allowed_path(&Method::GET, "instances/sven-coop-1/logs?lines=1000000000"),
            None,
            "a line count is small or refused"
        );
    }

    #[test]
    fn a_controller_identity_must_be_sixteen_bytes() {
        assert!(parse_identity("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6").is_ok());
        assert!(parse_identity("a1b2").is_err());
        assert!(parse_identity("zz").is_err());
    }
}
