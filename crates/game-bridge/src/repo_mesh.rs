//! A pack repository served over Reticulum (`game-bridge repo seed`).
//!
//! A mirror (`repo::mirror`) is a folder of signed files; this puts one on
//! the mesh, so packs reach launchers and nodes with no internet at all. The
//! seed announces what it holds and answers requests for its files over a
//! Link. A reader addresses it as `rns://<destination>/` and reads it through
//! the same `repo::fetch` and the same checks as an https repository: the
//! list's signature and every digest, so a seed serves the official packs
//! exactly or not at all.
//!
//! Rules a later change could quietly break:
//! - **The announce is never a server row.** Its first byte is `0xFE`, which
//!   is neither the §3.3 record's version nor valid UTF-8, so every Lanthorn
//!   build's `announce::decode` refuses it and no browser lists it. Changing
//!   the first byte to anything printable would list every seed as a server.
//! - **A seed serves only names a repository can hold** — the list, its
//!   signature, and bare `.toml` names with theirs (`repo::valid_file_name`),
//!   from its own folder. A request never names a path.
//! - **What a seed says about itself is a hint.** The signer in its announce
//!   only decides what is shown; trust is the list's signature, checked on
//!   every read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use personal_rns::prelude::*;
use prns_core::identity::{IdentityHash, IdentitySigner};
use prns_core::identity::in_memory::InMemoryNodeIdentity;
use prns_core::routing::links::LinkId;
use prns_core::routing::request_handlers::RequestPathHash;
use tracing::{debug, info};

use crate::config::BridgeRole;
use crate::relay::{attach_interfaces, funnel_event, load_identity, spawn_bridge_node, BridgeEvent};
use crate::repo::{valid_file_name, INDEX_FILE};
use crate::signing::SIGNATURE_SUFFIX;
use crate::BridgeSession;

/// The seed's destination: its own app name, so it is never a game's.
pub const APP_NAME: &str = "lanthorn-packs";
pub const ASPECT: &str = "repository";

/// The one request a seed answers: a chunk of one of its files.
pub const FILE_ENDPOINT_ID: &str = "/lanthorn/repository/file/1";

/// Bytes of a file per response: one link packet, with room for the header.
pub const CHUNK: usize = 1400;

/// The announce's first bytes. `0xFE` is invalid UTF-8 and not a record
/// version, so no browser lists a seed as a server.
const MAGIC: [u8; 4] = [0xFE, b'L', b'P', b'R'];
const ANNOUNCE_VERSION: u8 = 1;

/// How often a seed announces itself.
pub const ANNOUNCE_EVERY: Duration = Duration::from_secs(30 * 60);

/// How long a reader keeps a link to a seed open between files.
const LINK_KEPT: Duration = Duration::from_secs(60);

/// What a seed announces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoAnnounce {
    /// Who signed the list it holds, as it says. A hint for display only.
    pub signer: Option<IdentityHash>,
    pub packs: u16,
    pub name: String,
}

impl RepoAnnounce {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.push(ANNOUNCE_VERSION);
        match &self.signer {
            Some(s) => {
                out.push(1);
                out.extend_from_slice(s.as_bytes());
            }
            None => out.push(0),
        }
        out.extend_from_slice(&self.packs.to_be_bytes());
        let name: String = self.name.chars().filter(|c| !c.is_control()).take(64).collect();
        let name = &name.as_bytes()[..name.len().min(200)];
        out.push(name.len() as u8);
        out.extend_from_slice(name);
        out
    }

    pub fn decode(app_data: &[u8]) -> Option<Self> {
        let rest = app_data.strip_prefix(&MAGIC)?;
        let (&version, rest) = rest.split_first()?;
        if version != ANNOUNCE_VERSION {
            return None;
        }
        let (&has_signer, mut rest) = rest.split_first()?;
        let signer = if has_signer == 1 {
            let b: [u8; 16] = rest.get(..16)?.try_into().ok()?;
            rest = &rest[16..];
            Some(IdentityHash::new(b))
        } else {
            None
        };
        let packs = u16::from_be_bytes(rest.get(..2)?.try_into().ok()?);
        let n = *rest.get(2)? as usize;
        let name = std::str::from_utf8(rest.get(3..3 + n)?).ok()?.to_string();
        Some(Self { signer, packs, name })
    }
}

/// A seed heard on the mesh.
#[derive(Debug, Clone)]
pub struct HeardRepo {
    pub destination: DestinationHash,
    pub announce: RepoAnnounce,
    pub hops: u8,
    pub last_seen: Instant,
}

impl HeardRepo {
    /// The address a reader adds: `rns://<destination>/`.
    pub fn address(&self) -> String {
        format!("rns://{}/", hex::encode(self.destination.as_bytes()))
    }
}

static HEARD: Mutex<Vec<HeardRepo>> = Mutex::new(Vec::new());
static MESH: Mutex<Option<PrnsNodeHandle>> = Mutex::new(None);
/// Links kept open to seeds, by destination, with when each was opened.
type OpenLinks = HashMap<[u8; 16], (LinkId, Instant)>;
static LINKS: Mutex<Option<OpenLinks>> = Mutex::new(None);

/// Seeds heard on the mesh by any node in this process, newest first.
pub fn heard() -> Vec<HeardRepo> {
    let mut v = HEARD.lock().unwrap_or_else(|e| e.into_inner()).clone();
    v.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    v
}

/// Remember a seed's announce. Called from the event funnel of every node,
/// before the announce is offered to the server list (which refuses it).
pub(crate) fn note_heard(destination: DestinationHash, hops: u8, app_data: &[u8]) -> bool {
    let Some(announce) = RepoAnnounce::decode(app_data) else { return false };
    let mut heard = HEARD.lock().unwrap_or_else(|e| e.into_inner());
    heard.retain(|h| h.destination != destination);
    heard.push(HeardRepo { destination, announce, hops, last_seen: Instant::now() });
    true
}

/// The node `rns://` repositories are read through: the launcher's browse
/// node, or a node's hub. Set once one is up.
pub fn set_mesh_handle(handle: Option<PrnsNodeHandle>) {
    *MESH.lock().unwrap_or_else(|e| e.into_inner()) = handle;
    *LINKS.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// `rns://<32 hex>/<file>` split into the destination and the file.
pub fn parse_address(url: &str) -> Option<(DestinationHash, &str)> {
    let rest = url.strip_prefix("rns://")?;
    let (hash, file) = rest.split_once('/').unwrap_or((rest, ""));
    let bytes = hex::decode(hash).ok()?;
    Some((DestinationHash::from_slice(&bytes)?, file))
}

fn request_body(name: &str, offset: u32) -> Vec<u8> {
    let mut b = vec![name.len() as u8];
    b.extend_from_slice(name.as_bytes());
    b.extend_from_slice(&offset.to_be_bytes());
    b
}

async fn link_to(handle: &PrnsNodeHandle, destination: DestinationHash) -> Result<LinkId> {
    let key = *destination.as_bytes();
    if let Some((link, at)) =
        LINKS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).get(&key).copied()
    {
        if at.elapsed() < LINK_KEPT {
            return Ok(link);
        }
        let _ = handle.close_link(link);
    }
    let link = match handle.establish_link(destination).await {
        Ok(l) => l,
        Err(_) => {
            handle.request_path(destination).await.map_err(|e| anyhow!("no path to the seed: {e:?}"))?;
            handle.establish_link(destination).await.map_err(|e| anyhow!("could not reach the seed: {e:?}"))?
        }
    };
    LINKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(key, (link, Instant::now()));
    Ok(link)
}

fn forget_link(destination: DestinationHash) {
    if let Some(m) = LINKS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.remove(destination.as_bytes());
    }
}

/// Read `url` (`rns://<destination>/<file>`) from a seed, chunk by chunk.
/// `Ok(None)` is "the seed does not have it".
pub async fn fetch_over_mesh(url: &str, max: usize) -> Result<Option<Vec<u8>>> {
    let (destination, name) = parse_address(url).ok_or_else(|| anyhow!("{url:?} is not an rns:// address"))?;
    let handle = MESH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| anyhow!("this program is not on the mesh yet, so it cannot read {url}"))?;
    let mut out: Vec<u8> = Vec::new();
    let mut retried = false;
    loop {
        let link = link_to(&handle, destination).await?;
        let body = request_body(name, out.len() as u32);
        let reply = match handle.request(link, RequestPathHash::of(FILE_ENDPOINT_ID), &body).await {
            Ok((reply, _)) => reply,
            Err(e) if !retried => {
                debug!(error = ?e, "a seed request failed; trying a new link once");
                forget_link(destination);
                let _ = handle.close_link(link);
                retried = true;
                continue;
            }
            Err(e) => return Err(anyhow!("the seed did not answer for {name}: {e:?}")),
        };
        let (&status, rest) = reply.split_first().ok_or_else(|| anyhow!("an empty answer for {name}"))?;
        match status {
            0 => {}
            1 => return Ok(None),
            _ => return Err(anyhow!("the seed refused {name:?}")),
        }
        let total = u32::from_be_bytes(rest.get(..4).ok_or_else(|| anyhow!("a short answer"))?.try_into()?) as usize;
        if total > max {
            return Err(anyhow!("{name} is over {max} bytes"));
        }
        let chunk = &rest[4..];
        if chunk.is_empty() && out.len() < total {
            return Err(anyhow!("the seed stopped sending {name}"));
        }
        out.extend_from_slice(chunk);
        if out.len() >= total {
            out.truncate(total);
            return Ok(Some(out));
        }
    }
}

/// Whether a seed may serve `name` from its folder.
pub fn servable(name: &str) -> bool {
    if name == INDEX_FILE || name == format!("{INDEX_FILE}{SIGNATURE_SUFFIX}") {
        return true;
    }
    let base = name.strip_suffix(SIGNATURE_SUFFIX).unwrap_or(name);
    valid_file_name(base)
}

/// The answer to one request, as the seed builds it.
pub fn answer(dir: &Path, body: &[u8]) -> Vec<u8> {
    let parsed = (|| {
        let (&n, rest) = body.split_first()?;
        let name = std::str::from_utf8(rest.get(..n as usize)?).ok()?;
        let offset = u32::from_be_bytes(rest.get(n as usize..n as usize + 4)?.try_into().ok()?);
        Some((name, offset as usize))
    })();
    let Some((name, offset)) = parsed.filter(|(n, _)| servable(n)) else { return vec![2] };
    let Ok(bytes) = std::fs::read(dir.join(name)) else { return vec![1] };
    let end = (offset + CHUNK).min(bytes.len());
    let mut out = vec![0];
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    if offset < end {
        out.extend_from_slice(&bytes[offset..end]);
    }
    out
}

/// The seed's state: the folder it serves.
#[derive(Clone)]
pub struct SeedState {
    dir: Arc<PathBuf>,
}

struct FileEndpoint;

impl RequestEndpoint<SeedState> for FileEndpoint {
    const ENDPOINT_ID: &'static str = FILE_ENDPOINT_ID;
    const POLICY: RequestEndpointPolicy = RequestEndpointPolicy::AllowAll;

    async fn handle(
        mut cx: RequestContext<'_, SeedState>,
        _node: &impl personal_rns::PrnsNodeApi,
    ) -> Result<(), Decline> {
        let reply = answer(&cx.state.dir, cx.data);
        cx.respond(reply)
    }
}

/// What a seed's folder says about itself, for its announce.
pub fn describe(dir: &Path) -> RepoAnnounce {
    let index = std::fs::read(dir.join(INDEX_FILE)).ok();
    let parsed = index.as_deref().and_then(|b| crate::repo::RepoIndex::parse(b).ok());
    let signer = index.as_deref().and_then(|b| {
        let text = std::fs::read_to_string(dir.join(format!("{INDEX_FILE}{SIGNATURE_SUFFIX}"))).ok()?;
        crate::signing::PackSignature::parse(&text).ok()?.verify_index(b, std::time::SystemTime::now()).ok()
    });
    RepoAnnounce {
        signer,
        packs: parsed.as_ref().map_or(0, |i| i.packs.len().min(u16::MAX as usize) as u16),
        name: parsed.map(|i| i.name).unwrap_or_else(|| "Pack repository".to_string()),
    }
}

/// How a seed reaches the mesh.
#[derive(Debug, Clone)]
pub struct SeedArgs {
    pub dir: PathBuf,
    pub identity: PathBuf,
    pub tcp: Option<String>,
    pub auto: bool,
    /// Reach the mesh through this machine's shared instance on this
    /// loopback port, as a node's game servers do, instead of interfaces of
    /// its own.
    pub shared_instance: Option<u16>,
    pub announce_every: Duration,
}

/// Put the repository in `args.dir` on the mesh. Re-announces on its timer
/// and whenever `announce_now` is asked, describing the folder as it is then,
/// so a re-mirror shows.
pub async fn start_seed(args: SeedArgs) -> Result<BridgeSession> {
    let secret = load_identity(&args.identity)?;
    let shared_instance = args.shared_instance;
    let destination_of = |secret| PreConfiguredDestination::Single {
        app_name: APP_NAME,
        aspects: &[ASPECT],
        identity: secret,
        announce_app_data: b"",
        proof: ProofStrategy::ProveAll,
        link_requests: LinkRequestPolicy::AcceptAll,
        ratchet: RatchetPolicy::NoRatchets,
        resource_strategy: ResourceStrategy::AcceptNone,
        maximum_request_bytes: Default::default(),
        request_endpoints: ServeMyRequestEndpoints::Yes,
    };
    let own = destination_of(secret.clone()).destination_hash().map_err(|e| anyhow!("{e:?}"))?;
    let me = InMemoryNodeIdentity::from_secret_key_bytes(&secret).identity_hash();
    let (announce_tx, mut announce_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut session = spawn_bridge_node(BridgeRole::Server, Some(own), false, move |_discovered, _| async move {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<BridgeEvent>();
        let dir = args.dir.clone();
        let node = PrnsNode::new(PrnsNodeRecipe {
            transport_identity: None,
            remote_control: personal_rns::runtime::RemoteControlNodeSetup::new(
                personal_rns::remote_control::RemoteControlService::Unavailable,
            ),
            pre_configured_destinations: [destination_of(secret)],
            app_state: SeedState { dir: Arc::new(dir.clone()) },
            storage: GrowableHeap,
            request_endpoints: request_endpoints![FileEndpoint],
            on_event: {
                let event_tx = event_tx.clone();
                move |event, _state| funnel_event(event, &event_tx)
            },
            interfaces: |node: &PrnsNodeHandle| attach_interfaces(node, args.tcp.as_deref(), args.auto),
            persistence: NoPersistence,
        });
        let handle = node.handle();
        // Nothing the seed hears matters to it.
        tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
        let announcer = handle.clone();
        tokio::spawn(async move {
            let mut early = crate::relay::EARLY_ANNOUNCE_DELAYS_SECS.iter().copied();
            loop {
                match early.next() {
                    Some(d) => tokio::time::sleep(Duration::from_secs(d)).await,
                    None => {
                        tokio::select! {
                            _ = tokio::time::sleep(args.announce_every) => {}
                            got = announce_rx.recv() => if got.is_none() { return },
                        }
                    }
                }
                let what = describe(&dir);
                let app_data = AnnounceAppData::Data(
                    prns_core::routing::announce::emit::AnnounceAppDataBytes::from_slice(&what.encode())
                        .unwrap_or_default(),
                );
                if announcer
                    .issue(PrnsCommand::AnnounceNow(AnnounceNow {
                        destination: own,
                        target: AnnounceTarget::AllInterfaces,
                        app_data,
                    }))
                    .is_none()
                {
                    return;
                }
                debug!(packs = what.packs, "seed announced");
            }
        });
        info!(destination = %hex::encode(own.as_bytes()), identity = %hex::encode(me.as_bytes()), "pack repository on the mesh");
        Ok((handle, async move { let _ = node.run().await; }))
    })
    .await?;
    session.set_announce_tx(announce_tx);
    if let Some(port) = shared_instance {
        session.join_shared_instance(port).await?;
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The announce round-trips, and no Lanthorn browser reads it as a
    /// server: the decoder every build carries refuses it.
    #[test]
    fn a_seed_announce_is_never_a_server_row() {
        let a = RepoAnnounce { signer: Some(IdentityHash::new([7; 16])), packs: 21, name: "Official".into() };
        let bytes = a.encode();
        assert_eq!(RepoAnnounce::decode(&bytes), Some(a));
        assert!(crate::announce::decode(&bytes).is_err(), "a browser would list it as a server");
        assert!(std::str::from_utf8(&bytes).is_err());
        assert_eq!(RepoAnnounce::decode(b"Sven server"), None);
    }

    /// A seed serves its list and packs, by name, chunk by chunk, and never a
    /// path.
    #[test]
    fn a_seed_serves_only_repository_names_from_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.json"), vec![b'x'; CHUNK + 10]).unwrap();
        std::fs::write(dir.path().join("a.toml"), b"pack").unwrap();
        let first = answer(dir.path(), &request_body("index.json", 0));
        assert_eq!(first[0], 0);
        assert_eq!(u32::from_be_bytes(first[1..5].try_into().unwrap()) as usize, CHUNK + 10);
        assert_eq!(first.len() - 5, CHUNK);
        let second = answer(dir.path(), &request_body("index.json", CHUNK as u32));
        assert_eq!(second.len() - 5, 10);
        assert_eq!(answer(dir.path(), &request_body("a.toml", 0))[5..], *b"pack");
        assert_eq!(answer(dir.path(), &request_body("b.toml", 0)), vec![1], "not there");
        for bad in ["../etc/passwd", "/etc/passwd", "x/a.toml", "a.txt", ".hidden.toml"] {
            assert_eq!(answer(dir.path(), &request_body(bad, 0)), vec![2], "{bad}");
        }
    }
}
