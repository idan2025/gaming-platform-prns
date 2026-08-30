# Reticulum Gaming Platform — Design

A **decentralized** platform for easy access to game servers over Reticulum,
built on [Prns](https://github.com/KenAKAFrosty/Prns).

Generalizes `/home/pi/svencoop-prns-clone` (one box, one Sven Co-op server) into
something that runs any game that can bind to a LAN or a direct IP, across many
nodes, with no party in the middle.

Companion docs: `GAMES.md` (multi-game abstraction), `MODES.md` (how non-dedicated
games reach the mesh).

## 0. The architectural rule

**Decentralized by default. Centralized only as a convenience, never as a
dependency.**

The zero-infrastructure baseline must always work: two people with the launcher
and any shared Reticulum interface hear each other's announces and play. No
account, no index, no platform, no internet. Everything the "platform" adds is a
cache or a convenience layer on top of that, and every piece of it is something
anyone can run.

```
   Launcher ──announce listening──> the mesh  (works with zero infrastructure)
       │                               ^
       │ optional, for convenience     │ announces
       v                               │
   Index node(s) ──────────────────────┘   (anyone can run one; several coexist)
       ^
       │ optional
   Node agent ──spawn──> Instance ───── Reticulum Link (E2E) ─────> Launcher
                                            game traffic, peer to peer
```

Consequences that are not negotiable:

- **Discovery is announces**, not a database. An index is a *cache of the mesh*,
  never the source of truth. A server the index has never heard of still joins.
- **The index is itself a Reticulum destination**, not only an HTTPS endpoint —
  reachable from a mesh with no internet. Multiple indexes coexist; the launcher
  can query any it knows and fall back to raw announce listening.
- **Identity is the account.** A Reticulum identity signs a challenge; its hash is
  the account key. No password database exists anywhere, so there is nothing
  central to lose or be locked out of. A server's owner is whoever announces it.
- **Metadata is signed by the server's identity**, so an index cannot lie about a
  server's name, game, or player count — only fail to list it.
- **Game traffic never transits any platform component**, ever.

If every index and every hosted node disappears, players who know a destination
hash keep playing and neighbours on a mesh keep finding each other. Only
convenience degrades. That property is the product; "easy access" is the UX layer
over it, not a reason to centralize.

## 1. What already exists (reuse, don't rewrite)

From `svencoop-prns-clone`:

| Piece | Path | Platform role |
| --- | --- | --- |
| UDP↔Reticulum relay + framing | `src/relay.rs`, `src/framing.rs` | becomes the generic game transport |
| `BridgeController` | `controller/src/controller.rs` | becomes the **per-instance** control surface |
| DS lifecycle (find/pull/start/stop/changelevel) | `controller/src/ds.rs` | becomes one *game pack* driver |
| steamcmd runner + progress parse | `controller/src/steamcmd.rs` | generic content fetcher |
| A2S query | `controller/src/a2s.rs` | one *status probe* implementation |
| Announce-based server browser | `controller/src/server_entry.rs` | seed of the directory |
| REST surface (21 commands) | `controller/src/bin/web.rs` | becomes the **agent-facing** instance API |
| Docker image | `Dockerfile`, `docker-compose.yml` | becomes the instance runtime image |

The existing web binary is already, structurally, a single-instance node agent
with a UI bolted on. The platform work is mostly *above* it.

## 2. Four new components

### 2.1 `game-bridge` — generalized transport (Rust crate)

Fork of `sc-rns-bridge` with the Sven-specific parts pulled out:

- Destination aspects become config, not constants: `game.<game_id>.server` /
  `.client` instead of hardcoded `SC_ASPECT_SERVER`.
- Structured announce `app_data` — replace the bare UTF-8 name with a compact
  encoded record (fits `MAX_ANNOUNCE_APP_DATA_LEN`):
  `{name, game_id, map, players, max_players, flags}`. Keep bare-string decode
  as a fallback so v0.1.x Sven servers still list.
- **Link allowlist at accept time** — optional set of permitted client identity
  hashes. Required for private/paid servers; the current bridge accepts any link.
- **`StreamRelay` for TCP games** alongside the datagram relay — link per TCP
  connection, no message boundaries, close propagates both ways. A separate code
  path, not a flag on the existing one.
- **Framing v2: channel id in header bits 1–3**, so one destination fronts a
  port set (game + query + rcon). Not silently compatible — `Reassembler::push`
  ignores non-`FINAL` bits, so gate it behind an announce-advertised protocol
  version. See `GAMES.md` §2–§3.

### 2.2 `game-pack` — declarative game definitions

Today `ds.rs` hardcodes Sven Co-op. Replace with a manifest so adding a game is
data, not code:

```toml
id            = "svencoop"
name          = "Sven Co-op"
transport     = "udp"
default_port  = 27015
content       = { source = "steamcmd", app_id = 276060, arch = "i386" }
launch        = { bin = "svends_run", args = ["-port", "{port}", "+maxplayers", "{max_players}", "+map", "{map}"] }
stop          = { signal = "SIGKILL", process_group = true }   # see supervision note
probe         = "a2s"
console       = { stdin = true, changelevel = "changelevel {map}" }
```

A pack must cover all nine axes of cross-game variation (content source,
runtime, transport, port set, probe, admin channel, config format, content size,
client join) plus `min_link_class`, `content.auth`, and pre-start gates like EULA
acceptance. **`GAMES.md` is the authority on that schema** — read it before
changing the manifest.

### 2.3 `platform-agent` — per-node daemon

Runs on every host that will carry game servers. Responsibilities:

- Register with central; report CPU/RAM/disk/port capacity.
- Receive instance specs; create/start/stop/destroy containers via the Docker API.
- Allocate a UDP port and a Reticulum identity per instance.
- Poll instance status (`/api/state`, probe) and push it upward.
- Manage the shared game-content store (see §4).

**The agent's uplink should itself run over Reticulum.** The agent then needs no
inbound ports and no public IP — same property the platform sells to its users.
Fall back to an outbound WebSocket to central where that's simpler.

### 2.4 `index` + `platform-api` — optional convenience layer

Not the authority. A cache with a nice front door.

- **Index node**: runs a `prnsd` transport node, listens for `game.*.server`
  announces mesh-wide, verifies their signed metadata, and serves the result over
  **both** a Reticulum destination and HTTPS. Lists servers nobody deployed
  through the platform — the browser is a view of the mesh, not a view of a
  database. Anyone can run one; the launcher treats indexes as a list, not a
  singleton.
- **Deploy API** (for people who want hosting rather than self-hosting):
  `POST /instances {game_id, node, name, max_players, visibility}` → picks a node
  → agent provisions → returns the destination hash. Purely optional; the same
  agent takes local commands with no index involved.
- **Auth**: challenge/response against a Reticulum identity. The identity hash is
  the key for ownership and for private-server allowlists — one primitive end to
  end.
- Postgres behind the index for its own cache and for hosted-instance bookkeeping.
  Losing it costs the cache, not the network.

## 3. Player side

The existing Tauri GUI generalizes into the platform launcher:

1. Browse the directory (central HTTP when online; **local announce listening
   when offline** — the mesh case must keep working).
2. Click Join → launcher starts a `game-bridge` client on a local port and runs
   the game with the right connect string. `connect_and_launch()` already does
   exactly this for Sven Co-op.
3. Ship game packs to the launcher too, so the connect/launch step is per-game data.

## 4. Hard problems, with positions

**Game content size.** ~2.74 GB per Sven Co-op install. One copy per instance is
unworkable. Store content once per node, read-only; give each instance a writable
overlay (overlayfs, or a Docker volume seeded from a shared image layer) for
config/maps/logs. Do this before the second instance ever runs on a node.

**One RNS stack per container is wasteful.** Prns supports a shared instance
(`prnsd`). Run one `prnsd` per node and have instance containers attach as
clients, rather than each container standing up its own interfaces. Cuts per-instance
memory and gives the node a single place to configure uplinks/IFAC.

**Throughput is the real ceiling.** Framing is 1 byte + ≤`MAX_CHUNK` (1900)
bytes per link packet (`src/framing.rs:24`), so a typical ~1400-byte GoldSrc
datagram rides in a single chunk. Sustained *rate* is the constraint, not packet
size, and it varies by two orders of magnitude across games — see the viability
tiers in `GAMES.md`. Benchmark players-per-server and servers-per-node over
Reticulum *before* publishing any capacity numbers. Measure in phase 0, not phase 3.

**Process supervision.** `svends_run` spawns the real binary as a plain child, so
killing the tracked PID orphans the game server and wedges the port. The fix is
already in `ds.rs`: spawn with `process_group(0)`, stop with a direct
`libc::kill(-pid, SIGKILL)` — **not** a shelled-out `kill`, which doesn't exist in
the slim image. Carry this code forward verbatim; it broke three releases in a row.

**Self-healing config.** Established pattern in the reference repo: when a config
file can drift (stock bad `mapcycle.txt`, duplicated interfaces in `settings.json`),
repair on load instead of fixing instances by hand. Keep it for instance config.

**Abuse.** Public deploy + anonymous identities = free compute. Need per-account
instance quotas and idle reaping (no players for N minutes → stop) from day one.

## 5. Build order

- **Phase 0 — prove the ceiling.** Benchmark concurrent players and concurrent
  instances over Reticulum on one node using today's binaries. Everything else is
  conditional on this.
- **Phase 1 — generalize.** Extract `game-bridge` (parametrized aspect, structured
  announce, link allowlist) + `game-pack` manifests. Sven Co-op becomes pack #1;
  the existing app must still work.
- **Phase 2 — one node, many servers.** `platform-agent` + Docker orchestration +
  shared content store + port allocation. No central service yet; agent has a local
  API.
- **Phase 3 — launcher + mesh discovery.** Generalized launcher that finds and
  joins servers from **announces alone**, no index, no account. This is the
  zero-infrastructure baseline and it ships before any central service, so the
  central service can never quietly become load-bearing.
- **Phase 4 — index + optional hosting.** Announce indexer served over RNS and
  HTTPS, identity auth, hosted-deploy API, agent uplink over Reticulum, multi-node.
- **Phase 5 — more games.** The ladder in `GAMES.md` §7: GoldSrc sibling
  (app 90) → Source/TF2 → Minetest → Minecraft Java. Each step exercises exactly
  one new axis. Never let the second game be the hard game.

## 6. Repo layout (proposed)

```
gaming-platform-prns/
  crates/
    game-bridge/      # generalized UDP/TCP <-> Reticulum relay
    game-pack/        # manifest schema + loader + drivers (steamcmd, a2s, ...)
    platform-agent/   # node daemon: Docker orchestration, content store
    platform-index/   # optional: announce indexer, served over RNS + HTTPS
    platform-api/     # optional: hosted-deploy API, identity auth
  packs/
    svencoop.toml
  GAMES.md            # multi-game abstraction: variation axes, tiers, ladder
  MODES.md            # dedicated / listen-server / virtual-LAN / unsupported
  web/                # browser UI (directory + deploy)
  launcher/           # player app (Tauri, from svencoop-prns gui/)
  deploy/             # compose / k8s manifests for the central service
```

## 7. Open questions

- Node supply: platform-owned hosts only, or user-contributed nodes? Given the
  decentralized-first rule, user-contributed is the default and platform-owned is
  the convenience — but that means untrusted agents, which shapes pack signing
  (`GAMES.md` §6) and any billing model.
- Does Mode 3 (virtual LAN, `MODES.md`) justify shipping a privileged installer?
  It unlocks the LAN-party back catalogue but changes signing, install, and
  support. Decide only after Modes 1–2 have users.
- Do central-deployed servers get a platform-run Reticulum uplink by default, or
  must the operator supply one?
- Monetization — free/open, paid instances, or bring-your-own-node with a paid
  directory? Affects whether quotas are cosmetic or load-bearing.
- Which games can central actually host? Only anonymous-steamcmd titles are
  legally hostable by the platform; everything else is bring-your-own-node with
  operator-supplied credentials (`GAMES.md` §5). Decide before phase 3.
- Community-contributed packs? A pack is argv on a node — signed packs only, or
  first-party packs only (`GAMES.md` §6).
