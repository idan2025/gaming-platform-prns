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

use std::collections::HashMap;
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
    /// Session tokens by who they act as ([`Acting::key`]), held in memory
    /// only. One is dropped on a refusal, so the next request logs in again.
    tokens: Mutex<HashMap<String, String>>,
    /// The last server list of each identity and when it came. Held while a
    /// list is fetched, so pages that poll together wait for one request
    /// rather than send one each; dropped by any change sent through the
    /// controller.
    lists: Mutex<HashMap<String, (Instant, Forwarded)>>,
}

/// Who a request through the controller acts as at the index.
///
/// The node's admins act as the node, as they always have: its servers, and
/// whatever the index lets the node do as an admin there. Each account with
/// the user role acts as **its own identity**, derived from the node's key and
/// the account ([`account_secret`]), so the index keeps every account's
/// servers apart by the same owner check it applies to anyone, and its quotas
/// count each account on its own. Before this every account was the node at
/// the index, and any user could stop, remove or restore another's server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acting {
    Node,
    Account { username: String, salt: String },
}

impl Acting {
    fn key(&self) -> String {
        match self {
            Acting::Node => String::new(),
            Acting::Account { username, .. } => format!("account:{username}"),
        }
    }
}

/// An account's identity secret at a controller: two HMAC-SHA256 blocks keyed
/// by the node's own secret over a fixed label, the account's name and its
/// salt. Only the node can make it, it is the same every time for the same
/// account, and a removed account's name taken again gets a new salt and so a
/// new identity. Never written anywhere.
pub fn account_secret(node: &[u8], username: &str, salt: &str) -> Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]> {
    let mut out = Zeroizing::new([0u8; IDENTITY_SECRET_KEY_LEN]);
    for (i, chunk) in out.chunks_mut(32).enumerate() {
        let mut msg = Vec::with_capacity(64 + username.len() + salt.len());
        msg.extend_from_slice(b"lanthorn/controller-account/v1\0");
        msg.extend_from_slice(username.as_bytes());
        msg.push(0);
        msg.extend_from_slice(salt.as_bytes());
        msg.push(i as u8 + 1);
        chunk.copy_from_slice(&hmac_sha256(node, &msg)[..chunk.len()]);
    }
    out
}

/// HMAC-SHA256 (RFC 2104).
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
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
            tokens: Mutex::new(HashMap::new()),
            lists: Mutex::new(HashMap::new()),
        })
    }

    /// Send one request to the controller as the node. See [`forward_as`].
    pub async fn forward(&self, method: &Method, path: &str, body: Option<&[u8]>) -> Result<Forwarded> {
        self.forward_as(&Acting::Node, method, path, body).await
    }

    /// Send one request to the controller as `acting`, logging in first if
    /// needed. Each identity's server list is shared by its pages for
    /// [`LIST_FRESH`]; any other request that is not a read drops that list,
    /// before and after, so the list a page sees next includes the change.
    pub async fn forward_as(&self, acting: &Acting, method: &Method, path: &str, body: Option<&[u8]>) -> Result<Forwarded> {
        let key = acting.key();
        if *method == Method::GET && path == "/instances" {
            let mut lists = self.lists.lock().await;
            if let Some((at, reply)) = lists.get(&key) {
                if at.elapsed() < LIST_FRESH {
                    return Ok(reply.clone());
                }
            }
            let reply = self.forward_now(acting, method, path, body).await?;
            if reply.status == 200 {
                lists.insert(key, (Instant::now(), reply.clone()));
            } else {
                lists.remove(&key);
            }
            return Ok(reply);
        }
        if *method == Method::GET {
            return self.forward_now(acting, method, path, body).await;
        }
        self.lists.lock().await.remove(&key);
        let reply = self.forward_now(acting, method, path, body).await;
        self.lists.lock().await.remove(&key);
        reply
    }

    /// One request, no cache. A refusal that looks like an expired session is
    /// retried once after a fresh login.
    async fn forward_now(&self, acting: &Acting, method: &Method, path: &str, body: Option<&[u8]>) -> Result<Forwarded> {
        let token = self.token_or_login(acting).await?;
        let first = self.send(method, path, body, &token).await?;
        if first.status != 401 {
            return Ok(first);
        }
        self.tokens.lock().await.remove(&acting.key());
        let token = self.login(acting).await?;
        self.send(method, path, body, &token).await
    }

    async fn token_or_login(&self, acting: &Acting) -> Result<String> {
        if let Some(t) = self.tokens.lock().await.get(&acting.key()).cloned() {
            return Ok(t);
        }
        self.login(acting).await
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
        if path.ends_with("/relocate") || path.ends_with("/approve") {
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
    async fn login(&self, acting: &Acting) -> Result<String> {
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
        let identity = PrivateIdentityMaterial::from_slice(&self.secret_of(acting)[..])
            .map_err(|e| anyhow!("the identity key: {e:?}"))?;
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
        self.tokens.lock().await.insert(acting.key(), session.token.clone());
        Ok(session.token)
    }

    fn secret_of(&self, acting: &Acting) -> Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]> {
        match acting {
            Acting::Node => self.secret.clone(),
            Acting::Account { username, salt } => account_secret(&self.secret[..], username, salt),
        }
    }

    /// The identity hash `acting` has at the index, hex: what an admin there
    /// names to give an account its own limits.
    pub fn identity_of(&self, acting: &Acting) -> Result<String> {
        let identity = PrivateIdentityMaterial::from_slice(&self.secret_of(acting)[..])
            .map_err(|e| anyhow!("the identity key: {e:?}"))?;
        Ok(hex::encode(identity.identity_hash().as_bytes()))
    }

    /// Have the index remove everything it keeps about an account: its
    /// servers, their backups and its records (`DELETE /me`). Refused for the
    /// node itself, whose servers are its admins'. The index refuses when one
    /// of its workers cannot answer, and that refusal is this one's.
    pub async fn erase(&self, acting: &Acting) -> Result<serde_json::Value> {
        if *acting == Acting::Node {
            return Err(anyhow!("the node's own servers are its admins', not one account's"));
        }
        let body = serde_json::to_vec(&serde_json::json!({ "confirm": self.identity_of(acting)? }))?;
        let reply = self.forward_now(acting, &Method::DELETE, "/me", Some(&body)).await?;
        if reply.status != 200 {
            let why = serde_json::from_slice::<serde_json::Value>(&reply.body)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| format!("the controller answered {}", reply.status));
            return Err(anyhow!("{why}"));
        }
        self.forget(acting).await;
        serde_json::from_slice(&reply.body).context("reading the controller's answer")
    }

    /// Forget an identity's session and list: its account was removed.
    pub async fn forget(&self, acting: &Acting) {
        self.tokens.lock().await.remove(&acting.key());
        self.lists.lock().await.remove(&acting.key());
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
        // What the index hosts and where (regions), read-only: the create form
        // offers a region from it.
        ["hosting"] if *method == Method::GET && no_query() => Some("/hosting".into()),
        // Where a server could move, and asking an admin for another region.
        ["instances", id, "move-targets"] if valid_id(id) && *method == Method::GET && no_query() => {
            Some(format!("/instances/{id}/move-targets"))
        }
        ["instances", id, "region-request"] if valid_id(id) && *method == Method::POST && no_query() => {
            Some(format!("/instances/{id}/region-request"))
        }
        ["region-requests"] if *method == Method::GET && no_query() => Some("/region-requests".into()),
        ["region-requests", rid] if valid_request_id(rid) && *method == Method::DELETE && no_query() => {
            Some(format!("/region-requests/{rid}"))
        }
        // An admin of the index deciding them. The index checks who is admin.
        ["admin", "region-requests"] if *method == Method::GET && no_query() => Some("/admin/region-requests".into()),
        ["admin", "region-requests", rid, action @ ("approve" | "deny")]
            if valid_request_id(rid) && *method == Method::POST && no_query() =>
        {
            Some(format!("/admin/region-requests/{rid}/{action}"))
        }
        // Who this node (or this account) is to the index, and whether it is an
        // admin there. Erasing an account is not here: removing the account
        // does that (`Controller::erase`).
        ["me"] if *method == Method::GET && no_query() => Some("/me".into()),
        // Asking to become an admin of the index, and an admin deciding; the
        // admins, and the workers. The index checks who is admin.
        ["admin-requests"] if *method == Method::POST && no_query() => Some("/admin-requests".into()),
        ["admin", "admin-requests"] if *method == Method::GET && no_query() => Some("/admin/admin-requests".into()),
        ["admin", "admin-requests", rid, action @ ("approve" | "deny")]
            if valid_request_id(rid) && *method == Method::POST && no_query() =>
        {
            Some(format!("/admin/admin-requests/{rid}/{action}"))
        }
        ["admin", "admins"] if *method == Method::GET && no_query() => Some("/admin/admins".into()),
        ["admin", "admins", identity] if valid_identity(identity) && *method == Method::DELETE && no_query() => {
            Some(format!("/admin/admins/{identity}"))
        }
        ["admin", "workers"] if *method == Method::GET && no_query() => Some("/admin/workers".into()),
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

/// An identity hash: thirty-two lower-case hex characters.
fn valid_identity(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A region request id the index made: sixteen lower-case hex characters.
/// Checked before it is put in a path, like a server id.
fn valid_request_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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
        // What the index hosts and in which regions, for the create form: a
        // read and nothing else.
        assert_eq!(allowed_path(&Method::GET, "hosting").as_deref(), Some("/hosting"));
        assert_eq!(allowed_path(&Method::POST, "hosting"), None);
        assert_eq!(allowed_path(&Method::GET, "hosting?x=1"), None);
        // Region moves: asking, and an admin deciding. The index says who is admin.
        let rid = "0123456789abcdef";
        for (method, rest, want) in [
            (Method::GET, "instances/sven-coop-1/move-targets".to_string(), "/instances/sven-coop-1/move-targets".to_string()),
            (Method::POST, "instances/sven-coop-1/region-request".to_string(), "/instances/sven-coop-1/region-request".to_string()),
            (Method::GET, "region-requests".to_string(), "/region-requests".to_string()),
            (Method::DELETE, format!("region-requests/{rid}"), format!("/region-requests/{rid}")),
            (Method::GET, "admin/region-requests".to_string(), "/admin/region-requests".to_string()),
            (Method::POST, format!("admin/region-requests/{rid}/approve"), format!("/admin/region-requests/{rid}/approve")),
            (Method::POST, format!("admin/region-requests/{rid}/deny"), format!("/admin/region-requests/{rid}/deny")),
        ] {
            assert_eq!(allowed_path(&method, &rest).as_deref(), Some(want.as_str()), "{method} {rest}");
        }
        let who = "ab".repeat(16);
        for (method, rest, want) in [
            (Method::GET, "me".to_string(), "/me".to_string()),
            (Method::POST, "admin-requests".to_string(), "/admin-requests".to_string()),
            (Method::GET, "admin/admin-requests".to_string(), "/admin/admin-requests".to_string()),
            (Method::POST, format!("admin/admin-requests/{rid}/approve"), format!("/admin/admin-requests/{rid}/approve")),
            (Method::GET, "admin/admins".to_string(), "/admin/admins".to_string()),
            (Method::DELETE, format!("admin/admins/{who}"), format!("/admin/admins/{who}")),
            (Method::GET, "admin/workers".to_string(), "/admin/workers".to_string()),
        ] {
            assert_eq!(allowed_path(&method, &rest).as_deref(), Some(want.as_str()), "{method} {rest}");
        }
        assert_eq!(allowed_path(&Method::DELETE, "admin/admins/not-an-identity"), None);
        for bad in ["admin/region-requests/../x/approve", "admin/region-requests/ABCDEF0123456789/approve", "admin/users"] {
            assert_eq!(allowed_path(&Method::POST, bad), None, "{bad}");
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
