# Build plan — four roles

Concrete plan for the product as stated: **a server browser over Reticulum**,
where a user picks any one of four roles. Companion to `DESIGN.md`
(architecture), `GAMES.md` (per-game variation), `MODES.md` (transport modes).
This file is the *ordering and the measured budgets*; those three are the
authority on everything else.

Baseline: `svencoop-prns-clone` at **v0.1.10** (`1efb4a5`), GitHub
`idan2025/Svencoop-Prns`.

## 0. What this is, and is not

**Is:** a server browser. Announce-driven discovery, filter by game and other
criteria, join by opening a Reticulum Link. Closest prior art is the old
GoldSrc/Quake server list, plus Hamachi/ZeroTier for `MODES.md` Mode 3.

**Is not:** matchmaking. Matchmaking needs a shared queue — a global view of who
is waiting, a matcher deciding pairings, then server allocation. A queue is a
single source of truth and therefore load-bearing centralization, which
`DESIGN.md` §0 forbids. A server browser degrades to raw announce listening when
every index dies; a queue does not degrade, it just breaks.

Ranked/skill-based matchmaking additionally needs a globally consistent rating,
i.e. consensus. Do not build it. The games in scope (Sven, Minecraft, Terraria,
Valheim, Factorio) want a server list, not a queue — their players pick a server
by name and stay. Games that genuinely want matchmaking are mostly Mode 4
(Steam Datagram Relay, EOS P2P), already marked unsupported.

Cloud-shaped deploy (`POST /instances`) stays as `DESIGN.md` §2.4 describes it:
a convenience, never a dependency.

## 1. The four roles

| Role | User-facing | Status at v0.1.10 |
| --- | --- | --- |
| **Play** | join a server, game launches | built — `connect_and_launch()` |
| **Host** | run a server others find | built for Sven (Mode 1/2) |
| **Browse** | list/filter every server on the mesh | partial — announces heard, no structure |
| **Relay** | donate transit, carry others' traffic | **already on, unconditionally — see §4** |

## 2. Measured budgets (read out of the vendored engine, not the README)

| Quantity | Value | Source |
| --- | --- | --- |
| Link plaintext cap | **1967 B — only on our fork**; upstream is 431. Bridge uses `MAX_CHUNK = 1900` | `src/framing.rs:24`, `ENGINE.md` |
| Group plaintext cap | **383 B, fire-and-forget** | `MAX_SEND_GROUP_PLAINTEXT_LEN` |
| **Announce `app_data` cap** | **316 B** (284 if ratcheted) | derived below |

```
HEADER_MAX_LEN          = 2 + 16*2 + 1                = 35
BROADCAST_MDU           = BROADCAST_MTU(500) - 35 - IFAC_MIN_LEN(1) = 464
ANNOUNCE_FIXED_FIELDS   = pubkeys(64) + name_hash(10) + announce_id(10) + sig(64) = 148
MAX_ANNOUNCE_APP_DATA_LEN = 464 - 148 = 316
MAX_RATCHETED_...         = 316 - 32  = 284
```

316 bytes is the entire server-browser row, per server, per announce. Everything
that does not fit is a Link query to the server itself, or an index lookup.

## 3. Role: Browse

### 3.1 The constraint that decides the design

`Diagnostic::AnnounceHeard` (`prns-runtime/core/src/runtime/event.rs:97`)
exposes exactly four things:

```rust
AnnounceHeard { destination: DestinationHash, hops: u8,
                source_interface: InterfaceId, app_data: AnnounceAppDataBytes }
```

**No aspect string and no identity.** The destination hash is one-way —
`sha256("app.aspect1.aspect2")` truncated to 10 bytes, mixed with the identity
hash by `derive_destination_hash`. You therefore **cannot** tell that an announce
is a Minecraft server by looking at its hash.

Consequence, non-negotiable: **the game id must be carried in `app_data`.** The
alternative (precompute `derive_destination_hash` per known identity per game)
does not scale and cannot discover a game the launcher has never heard of.

### 3.2 Metadata authenticity is already free

The announce's own Ed25519 signature is computed over `write_signed_material`,
which includes `app_data` (`prns-core/src/routing/announce/mod.rs:384`). A server
therefore **cannot lie about another server's metadata, and an index cannot alter
what it relays** — it can only decline to list it. No second signature, no 64 B
cost. `DESIGN.md` §0's "metadata is signed by the server's identity" is satisfied
by the wire format at zero budget.

### 3.3 The announce record (fits 316 B with room to spare)

Replace `announce_app_data_to_name` (`src/relay.rs:800`) and
`server_announce_app_data` (`:827`) with a length-prefixed record:

```
byte 0        record version (1)         # 0x00-0x7F reserved; a bare UTF-8
                                         # name never starts with a low byte,
                                         # which is how the fallback is detected
byte 1        protocol version           # framing v1 vs v2 (channel ids)
byte 2        flags: passworded | allowlisted | dedicated/listen | mode 1-3
byte 3        min_link_class (tier 1-3, GAMES.md §4)
byte 4        players
byte 5        max_players
byte 6        len(game_id)  + game_id    # <= 24, ASCII, matches pack `id`
then          len(name)     + name       # <= 48 UTF-8
then          len(map)      + map        # <= 32, empty allowed
then          optional TLVs              # unknown types skipped, not fatal
```

Worst case above: 6 + 25 + 49 + 33 = 113 B, leaving ~200 B of TLV headroom
inside 316 (~170 if ratcheted). Budget TLVs against 284, not 316.

Rules:
- **Fallback decode stays.** If byte 0 is not a known record version, decode the
  whole payload as a UTF-8 display name exactly as today, so deployed v0.1.x Sven
  servers keep listing. This is why the version byte must be low.
- **Unknown TLVs are skipped**, never an error — forward compatibility.
- Player counts in an announce are stale by up to one announce interval. Label
  them as such in the UI; the live number comes from the probe over a Link.

**One consequence this section did not state, settled 2026-08-30 when the record
was built.** The fallback is only specified in one direction — a platform
browser reading a deployed peer. In the other direction a deployed v0.1.10
*client* decodes `app_data` as a bare UTF-8 name, so a platform server
announcing a record appears in that client's list under a garbled name. It still
**joins**: the destination hash is unaffected, and joining is what §5 requires.
The trade is accepted, because without the game id in `app_data` there is no
browser at all (§3.1). `ServerArgs::announce_format` carries a `Legacy` escape
hatch for a server that would rather look right to deployed Sven clients than be
filterable, at the cost of being an unattributed row in the platform browser.

### 3.4 Browser UI

- Filter by: game id, tier, has-players, not-full, not-passworded, mode.
- Sort default by `hops` — it is free from the announce and is the honest proxy
  for "will this feel bad". Then by players.
- Show mesh distance and interface, not a fake ping.
- Detail view opens a Link and queries the server for the expensive fields
  (player list, mods, full config). Never put those in an announce.

## 4. Role: Relay

**Every node already relays, unconditionally.** This is the opposite of what an
early reading of the code suggested, and it changes what the work is.

- `src/relay.rs:231` (server) and `src/relay.rs:446` (client) both pass
  `transport_identity: Some(identity)` into `PrnsNodeRecipe`.
- `configure_assembled_node` takes that secret, holds the identity, and calls
  `set_transport_identity` on it
  (`prns-runtime/core/src/runtime/node/assembly.rs:256-262`).
- `set_transport_identity` (`prns-core/src/engine/registration.rs:124`) sets
  `TransportState::Identified { network: NetworkTransport::Enabled }`.
- Path requests then forward recursively
  (`routing/ingress/path_requests.rs:225`) and relayed packets carry
  `PropagationType::Transport`.

So a player who installs this to join one server is, today, also forwarding
strangers' traffic across whatever connection they are on — with no consent
prompt, no visibility, no rate cap, and no off switch. On a metered or mobile
connection that is a defect, not a feature.

The Relay work is therefore about **control and consent**, not about enabling
anything:

1. **An off switch.** `transport_identity` becomes `Option`-driven by config
   rather than always `Some`. Default for the *client* role should be off, or at
   minimum a first-run prompt. Default for a *server* role node can stay on — it
   is already a host volunteering resources.
2. **A standalone Relay role.** A third `Role` variant beside `Server` and
   `Client` in `src/config.rs`, so a person can donate transit **without** running
   a game server or binding a game port. Today the only way to relay is to run one
   of the two game roles, which is why nobody does it deliberately.
3. **Visibility.** Bytes forwarded, peers seen, paths held. Without a counter the
   user cannot tell donating transit from a broken connection.
4. **A rate cap**, enforced and visible.

**Built 2026-08-30, and 3 and 4 did not survive contact with the engine.**
Items 1 and 2 landed as specified: `transport_identity` is now
`Option`-driven per role (`None` leaves the engine at
`TransportState::Unidentified`, forwarding nothing), the client defaults off,
and `BridgeConfig::Relay` is a node with interfaces, a transport identity, no
game and no announced destination.

Item 3 is **half available**. `InterfaceSnapshot`
(`prns-core/src/interfaces/status/mod.rs:67`) exposes `transported_links` per
interface, which is exactly "links I carry for other people" — that part is
honest. It also exposes `rx_bytes`/`tx_bytes`, but those count *everything* on
the interface, this node's own game traffic included. `Diagnostic` has no
forwarded-packet variant at all
(`prns-runtime/core/src/runtime/event.rs`), so **bytes attributable to transit
cannot be reported without patching the engine.** A UI must label the byte
figures as total interface throughput, never as "bandwidth you donated".

Item 4 is **not implementable as stated.** A rate cap needs an enforcement
point in the forwarding path, and the runtime exposes none — the only lever is
tearing down an interface, which would kill this node's own game traffic along
with the transit. Either it becomes a third fork patch (an engine-side cap,
which is the honest place for it) or the Relay role ships with observation and
an off switch but no cap. That is a decision, not an oversight; it is not
resolved here.

Note the standalone role needs no destination and announces nothing — it is a
node with interfaces and a transport identity, and that is all. It is the
smallest of the four roles to build, but it is platform code, so it is built in
this repo as part of Phase 1 rather than bolted onto `svencoop-prns` first
(§5, §7).

Two things the UI must say plainly:

- **A relay cannot read what it carries.** Links are end-to-end encrypted; a
  transport node forwards ciphertext. This is the argument for asking strangers
  to donate transit — say it, do not bury it.
- **It costs bandwidth.** Opt-in, capped, counted. Otherwise "supporting relay"
  becomes "my connection died" and the user disables it permanently.

Interfaces are attached by `attach_interfaces` (`src/relay.rs:832`), which today
takes only `--tcp <host:port>` and `--auto`. A `0.0.0.0` host binds a
`TcpServer`; any other host attaches a `TcpClientInterface`. A public relay wants
the former. The upstream `rnsd` the current deployment leans on must stay a
`TCPServerInterface` (many peers), never a point-to-point `UDPInterface`.

## 5. Relationship to `svencoop-prns` — extract, never couple

Decided 2026-08-30, and it constrains everything below.

**`svencoop-prns` stays a standalone product.** It keeps its own repo, its own
releases, and its own users. It is not being migrated, deprecated, or turned into
a subdirectory of the platform. Sven Co-op becomes *one game option* in the
platform, and the platform is not a prerequisite for running the standalone app.

**Extraction is one-directional: platform copies from Sven, never the reverse.**

- The platform's `game-bridge` starts as a copy of `src/relay.rs` + `src/framing.rs`
  with the Sven-specific parts parametrized. It does **not** become a crate that
  `svencoop-prns` then depends on.
- A reverse dependency would couple a shipped, working app to an unreleased
  platform's churn. That is exactly how "don't break the standalone" gets
  violated by accident.
- The cost is real and accepted: **a fix in shared logic must be applied twice.**
  Anything landing in one repo's relay or framing code gets a deliberate decision
  about porting it to the other. Cheaper than the coupling.

**Never break the standalone.** No platform requirement justifies a change to
`svencoop-prns` that regresses it on its own. Its `AGENTS.md` gotchas
(process-group kill, settings.json interface dedup, WebView2Loader.dll) stay
authoritative for that repo.

**Therefore wire compatibility is a hard requirement, not a nicety.** A platform
launcher must be able to join a standalone `svencoop-prns` server at v0.1.10, and
a standalone client must keep joining its own servers. Two rules follow, both
already in this plan and now non-negotiable:

- The §3.3 announce record **must** keep the bare-UTF-8 fallback decode, or
  deployed Sven servers vanish from the platform browser.
- Framing channel ids **must** stay gated behind announce-advertised version
  negotiation with channel 0 frozen as today's format. `Reassembler::push` masks
  only `FLAG_FINAL` and ignores the other header bits, so an ungated change
  silently corrupts streams for every deployed peer.

**One defect is shared and should be fixed in both**, separately: the
unconditional relaying in §4. It affects installed v0.1.10 clients today, and
only a change in `svencoop-prns` reaches them. Small, independent of the
platform, and its own release decision.

## 6. Roles Play and Host — carry forward, do not rewrite

Already built for Sven. What generalizing them needs is in `DESIGN.md` §2.1/§2.2:
parametrized aspect (`SC_ASPECT_SERVER` is hardcoded at `src/relay.rs:189,207`),
`StreamRelay` for TCP games (**built 2026-08-31**, `stream.rs`), framing v2
channel ids for multi-port games (**built 2026-08-31**, `GAMES.md` §3), and
`game-pack` manifests.

**The link allowlist — corrected 2026-08-30, it is not free.** This section
claimed v0.1.9 (`c9ec90b`) already captured the peer identity at accept, so
gating would be "a rejection before that insert, not new plumbing". Wrong, and
the source says so: `LinkRequestPolicy`
(`prns-core/src/routing/upstream_app_destinations/core.rs:24`) has exactly two
values, `AcceptAll` and `AcceptNone`. There is no per-request callback and no
identity at accept time. The identity appears only if the peer *volunteers* it
via `identify()` **after** the link is established, arriving as
`Diagnostic::PeerIdentified` — which is what `src/relay.rs:282` actually
handles.

So enforcement is: accept the link, start no relay, and buffer the peer's data
in its channel until it identifies. Allowed peers then get the relay started
having lost no datagram; peers identifying as someone else are closed; and
**peers that never identify must be closed on a timer**, or the allowlist is
bypassed by staying silent. That timeout is the part that was invisible in the
original estimate, and it is not optional. Implemented in
`crates/game-bridge/src/relay.rs` (`parse_allowlist` carries the reasoning).

Carry forward verbatim from the reference repo, per its `AGENTS.md`: process-group
spawn with `process_group(0)` and stop via direct `libc::kill(-pid, SIGKILL)` —
never a shelled-out `kill`, which does not exist in the slim image. It broke
three releases in a row.

## 7. Engine dependency — fork Prns, depend on the fork

Decided 2026-08-30, **done** — pinned rev, patch inventory and rebase procedure
are in `ENGINE.md`.

Corrected 2026-08-30: this section originally opened "Prns is not published on
crates.io". That is false — `personal-rns`, `prns-core` and `prns-runtime` are on
crates.io, `0.3.7` included. The decision is unchanged, because it never rested
on that premise:

**We already patch the engine, and that decides this.** The field the entire
server browser rests on — `Diagnostic::AnnounceHeard.app_data`
(`prns-runtime/core/src/runtime/event.rs:97`) — is a *local addition* made in
`svencoop-prns` commit `c9ec90b`, not upstream. `tracing_events.rs` carries a
matching line. Any option that assumes we consume upstream unmodified is
therefore wrong on arrival.

Upstream: `https://github.com/KenAKAFrosty/Prns`, dual MIT / Apache-2.0. The
vendored copy in `svencoop-prns` is version **0.3.7** — 2876 tracked files, ~13 MB
in git, ~53 MB on disk, copied rather than submoduled and with no `.git` of its
own, so its local edits are invisible as history.

**Decision: fork to `idan2025/prns`, land our patches there as real commits, and
depend on the fork by pinned rev.**

Why this over the alternatives:

- **vs. vendoring another copy.** Vendoring works and is hermetic, but the
  patches stay invisible edits inside a blob, upgrading means a manual re-copy
  plus re-applying changes nobody wrote down, and once the platform exists there
  are two copies to patch. The fork turns each engine change into a reviewable
  commit that can be rebased onto a new Prns release, with conflicts shown rather
  than silently lost.
- **vs. submodule or a git dep on upstream.** Both point at somebody else's repo,
  so neither has anywhere to put our patches — each needs a fork underneath
  anyway. Submodules additionally cost `--recursive` clones and detached HEADs.

**This does not violate §5.** Both repos depending on the same third-party fork
is not `svencoop-prns` depending on the platform. `svencoop-prns` may keep its
`vendor/` copy indefinitely; migrating it to the fork is optional and its own
decision.

**Consequences to handle:**

- A pinned rev needs the network on a first build. That is awkward for a project
  selling offline mesh operation — plan a vendored or cached build for releases,
  and keep `cargo vendor` output reproducible.
- Every patch we carry is a merge cost against upstream forever. Keep them
  minimal and upstream them where they are generally useful — the `app_data`
  field plausibly is.
- Record which rev of the fork this repo pins, and why, next to the dependency.
  Done: `ENGINE.md`, pointed at from `Cargo.toml`.

## 8. Ordering

### v0.1.0 is tagged and released (2026-08-31)

`v0.1.0` on `main`, with a GitHub release carrying four artifacts: the launcher
(`.deb`/`.rpm`/`.AppImage` on Linux, `.dmg` on macOS, an installer on Windows),
and `game-bridge` / `platform-agent` / `platform-index` per target.
`RELEASE.md` has the mechanics and `.github/workflows/release.yml` builds what
this machine cannot.

Two things about that release are worth keeping in view rather than in a
changelog:

- **`game-bridge` had to be written for it.** Two of the four roles in §1 —
  Host and Relay — had no artifact at all: the launcher joins and browses, the
  agent orchestrates containers. A platform whose whole premise is "host a
  server without infrastructure" shipped no way to host from a terminal. That is
  the kind of gap only packaging finds.
- **Two checklist items are unverified**, both needing hardware this was built
  without: a two-machine test over a shared interface, and a deployed
  `svencoop-prns` v0.1.10 peer listing and joining. The second is §5's promise to
  people who already installed something, so it is the more important of the two.

### Where this stands, and the next three things (2026-08-31)

Phases 1-3 are done. Phase 4 is built through multi-node. Pack distribution
(§11) has its first three content drivers. What is left, in the order this
document's own reasoning implies:

1. ~~**`StreamRelay` — TCP in the relay.**~~ **Built 2026-08-31**
   (`crates/game-bridge/src/stream.rs`). A pack declaring `transport = "tcp"`
   now runs instead of being refused at load. It is a splice, not a protocol:
   the engine's link channel is already reliable and in-order
   (`prns-core/src/routing/links/channel/mod.rs:27`) and the tokio runtime
   exposes it as a byte stream with an EOF flag
   (`prns-runtime/impls/tokio/src/runtime/node_facade/byte_stream/mod.rs:214`),
   so `stream.rs` copies a TCP socket against that and propagates each close.
   `framing.rs` is not on this path — chunking and ordering belong to the
   channel. `tests/stream_relay.rs` pins it over a real loopback mesh.
2. ~~**Phase 5's second game — the GoldSrc sibling (app 90).**~~ **Done
   2026-08-31**: `packs/half-life.toml` and `packs/counter-strike-16.toml`, no
   Rust change, which is exactly what `GAMES.md` §7 step 1 exists to prove. The
   next rung was Source/TF2, and blocker B (one destination, several ports) plus
   framing v2 is **done 2026-08-31** — a pack's `[[extra_ports]]`, UDP extras on
   framing channel ids, TCP extras on their own stream ids, and the version gate
   on the peer's announce (`GAMES.md` §3, `tests/multi_port.rs`). TF2 itself now
   needs a runtime and a pack, not transport work — the node-side half (a port
   set per instance) is done too, same day.
3. **§11.3 signing with expiry, then §11.4 trust tiers.** Worth doing once
   packs are worth sharing, which is after (1) and (2) widen what a pack can
   describe. `oci` is the last content driver and can wait for a game that
   needs it.

Deferred on purpose, none of them blocking: the two uplink follow-ups (capacity
push, agent auto-discovery) later in this section, and Mode 3 (`MODES.md`),
which stays conditional on Modes 1-2 having users.

Refines `DESIGN.md` §5 by folding the four roles in. The Relay role is **not** a
separate early phase: it would have to be built inside `svencoop-prns` to ship
before Phase 1, and §5 forbids putting platform code there. It folds into Phase 1
instead, where it is written once, in the right repo, against a parametrized
bridge.

- **Phase 0 — prove the ceiling.** Benchmark concurrent players and concurrent
  instances over Reticulum on one node with today's binaries. Every promise about
  player counts is conditional on this. Measure tier 1 for real; each new tier
  needs its own measurement before it ships. Runs against `svencoop-prns` as-is —
  no code changes anywhere.
- **Phase 1 — stand up the platform bridge.** The first code in this repo, in
  order:
  1. **Fork Prns and depend on the fork** (§7). Nothing else in this phase
     compiles first.
  2. **Copy `relay.rs` + `framing.rs` into `crates/game-bridge/`** and parametrize
     the Sven-specific parts: aspect (`SC_ASPECT_SERVER`/`SC_ASPECT_CLIENT` are
     hardcoded at `src/relay.rs:189,207,397,434`), app name, port.
  3. **The §3.3 announce record**, with the bare-UTF-8 fallback decode that keeps
     deployed Sven servers listable.
  4. **Link allowlist** at accept time — cheap now, see §6.
  5. **The Relay role** (§4): `transport_identity` becomes config-driven with the
     client defaulting off, plus a standalone `Relay` variant that donates transit
     with no game and no announced destination, a rate cap, and byte counters.
  6. **`game-pack` manifests**, with Sven Co-op as pack #1.

  `svencoop-prns` is untouched by all of this and keeps working exactly as it does
  today.
- **Phase 2 — Browse role.** Launcher lists and filters from **announces alone** —
  no index, no account, no internet. This is the zero-infrastructure baseline and
  it ships before any central service, so the central service can never quietly
  become load-bearing.

  **The bridge half is built (2026-08-30).** `BridgeConfig::Browse` is a node
  that binds no game port, registers no destination, announces nothing and holds
  no identity — so it cannot forward for anyone even by accident — and
  `crates/game-bridge/src/browse.rs` filters and sorts what it heard.
  `tests/browse_discovery.rs` proves the baseline end to end on loopback: a
  browse node lists a server with its game id, players, map and tier, and lists
  a *legacy* announce by name with no game id, in about a second.

  **The detail probe is built too.** `BridgeSession::probe_details` opens a Link
  to one server and asks it, over the engine's request-endpoint mechanism, at
  `/game-bridge/details/1`. Three things it settled:

  - A node with **no destination and no identity** can still initiate a Link and
    make a request, so browsing and probing stay the cheap passive role rather
    than requiring the join machinery.
  - The response carries a `StatsSource` flag and a `stats_age_secs`. Live
    numbers come from a background A2S poll, not from the request handler — a
    handler that blocked on a 2-second UDP query would stall the node's event
    loop for every other peer. So "live" honestly means "read this recently",
    and a game with no query protocol answers with its announced numbers,
    flagged as announced. `GameProfile::query` / the pack's `query` field says
    which is which; only GoldSrc and Source can be asked today.
  - **An allowlisted server refuses a probe from an unlisted identity.** The
    allowlist decides who may play; who may see who is playing is the same
    question, and answering it more freely leaks what the allowlist exists to
    protect. The server stays *listed* — discoverable but private.

  **The launcher is built**, and phase 2 with it. `crates/launcher-core` holds
  the logic and the JSON shapes the UI consumes; `launcher/src-tauri` is a thin
  Tauri v2 shell whose every command forwards to it, so the part worth testing
  needs no webview, display server or platform toolchain to test. `§9`'s
  `webviewInstallMode: offlineInstaller` is set from the start rather than
  "before shipping".

  Four UI rules follow from the design rather than from taste, and they are
  enforced in the frontend rather than left to a style guide:
  - **No ping and nothing that reads like one** — no latency figure, no
    quality dot, no signal bars derived from hops. Hops is shown as a hop count,
    with the interface it was heard on.
  - **A row's player count is stale by construction**, so it is shown with when
    it was heard, and the live number appears only in the detail pane.
  - **A legacy row's unknowns render as unknown, never as zero.** "0/0 players"
    for a server nobody has a count for is a lie told on that server's behalf.
  - **When a filter hides legacy rows, the UI says so**, with a control to clear
    it, so a user never concludes a server left the mesh when it was filtered.

  Joining starts a client bridge and tells the player where to point their game.
  It deliberately does not launch anything — a pack cannot name a command, and
  §10 still holds open whether packs may ever be trusted with argv.

  Note what the browse node does *not* have: a ping. The list sorts by `hops`,
  which is free in every announce. Measuring latency would mean opening a Link
  to every row — precisely the traffic a browser anyone can run must not
  generate.
- **Phase 3 — one node, many servers.** `platform-agent`, Docker orchestration,
  shared read-only content store plus per-instance writable overlay (a 2.74 GB
  copy per instance does not scale), port allocation. Agent has a local API; still
  no central service.

  **Built 2026-08-30**, with one correction and one finding, and **extended to
  port sets 2026-08-31** so a multi-port game (`GAMES.md` §3) can be hosted:
  `ports.rs::acquire` takes one host port per port the pack declares or none at
  all, `PublishedPort` keeps the container-side number (the pack's) apart from
  the host-side one (the node's), and the whole set is recorded on the container
  in `PORTS_LABEL` — Docker reports published ports but not which is the game and
  which is RCON, and a restarted agent must not guess.

  *The overlay is not an overlay.* Mounting overlayfs inside a container needs
  `CAP_SYS_ADMIN`, and a game server is the last process that should have it. So
  it is a read-only bind of the shared content plus one writable bind per path
  the pack declares, nested inside it. That is why `writable_paths` exists on a
  pack at all.

  *And nesting a writable bind inside a read-only one only works if the
  mountpoint already exists in the read-only source.* Found by an integration
  test against a real daemon, not by reasoning: runc reports it as
  `mkdirat ... read-only file system`, which says nothing about game content.
  `InstancePlan::required_content_dirs` and `Agent::plan_and_check` turn it into
  the name of the directory the install is missing.

  Two boundaries the code is built around, both worth keeping in later phases:

  - **A pack cannot say what runs.** It could already not name a command; it also
    cannot name a container image, since an image name selects the code a node
    executes. Images, limits, ports and the data root are agent config, written
    by the node's operator. An unconfigured game cannot start, and no default
    image is invented for it. This keeps §10's "are community packs allowed"
    genuinely open.
  - **The agent only touches containers it created**, by inspected label, never
    by name prefix. A node that runs game servers is a node someone uses for
    other things.

  The local API is **loopback-only and the config refuses otherwise**: every
  route creates or destroys containers and there is no authentication at all.
  Identity challenge/response is phase 4's job (`DESIGN.md` §2.4); until it
  exists the honest boundary is "you are already on this host".
- **Phase 4 — index + optional hosting.** Announce indexer served over **both** a
  Reticulum destination and HTTPS, identity challenge/response auth, hosted-deploy
  API, agent uplink over Reticulum, multi-node. Per-account quotas and idle reaping
  from day one — public deploy plus anonymous identities is free compute.
  **Hosted deploy is built (2026-08-30), and the legal question is answered by
  not answering it.** The platform ships **no list of hostable games**. Which
  titles an operator may run for other people varies by jurisdiction and by
  agreements this project cannot see (§10, `GAMES.md` §5), so
  `HostingConfig.games` is empty until an operator writes it and an empty list
  means hosting is off — the same shape as the agent making the container image
  an operator's choice.

  Ownership lives on the container as a label; the index reconstructs who owns
  what by asking the node, so there is no index-side instance table to drift out
  of step with reality. "Not found" and "not yours" are the same answer, so the
  API cannot be used to enumerate other people's servers.

  **Multi-node is built (2026-08-30).** The agent's control surface now runs over
  Reticulum as a `platform-agent.control` destination, so an agent needs no
  inbound port and no public IP — the same property the platform sells to its
  users. The index, already a Reticulum node, becomes a client of the agent over
  a Link: challenge/response auth (`platform_auth`, audience = the agent's
  identity), then create/stop/remove/list. Authorization is the operator's
  `trusted_indexes` allowlist, re-checked on every op, so an index removed from
  the list is refused on its next request, not its next login. The loopback HTTP
  API stays unchanged for local use. A Docker-gated two-node round-trip test
  (`crates/platform-index/tests/uplink_roundtrip.rs`) pins create/list/stop/remove
  end to end and the untrusted-index refusal.

  Two uplink follow-ups are deliberately not built and stay on the phase-4 list:
  **capacity push** (agent announces capacity to the index, vs the current
  pull-only `/capacity` over an authenticated link) and **agent auto-discovery**
  (an index learns agents from `platform-agent.control` announces, vs the current
  static `NodeConfig.agent` hash). Both are conveniences; neither blocks
  multi-node, and neither changes the auth model.

- **Pack distribution — after phase 4, before it matters.** §11: `[content]`
  drivers, signing with expiry, trust tiers. **`manual`, `archive` and
  `steamcmd` drivers, signing, and the trust tiers are all built (2026-08-31)** —
  the agent gates deploy on the operator's `[pack_trust]` policy and the
  launcher shows the tier beside Join. `oci` is the one piece left, and §11.2
  parks it until a game needs it. Turns "write a TOML yourself" into "import one
  somebody curated". The bigger lever for breadth was `StreamRelay` (TCP) —
  signing a pack for a game the bridge cannot run helps nobody — and that is
  built, so a pack can now describe a TCP game.

- **Phase 5 — more games.** The ladder in `GAMES.md` §7: GoldSrc sibling (app 90)
  → Source/TF2 → Minetest → Minecraft Java. Each step exercises exactly one new
  axis. Never let the second game be the hard game. **Rungs 1 and 2 are done
  (2026-08-31)** and both were pure data: `packs/half-life.toml`,
  `packs/counter-strike-16.toml`, and `packs/team-fortress-2.toml`, the last
  being the first pack to use `[[extra_ports]]` and so the first to announce
  framing generation 2. TF2 still needs a Source dedicated-server *image* in the
  node's config before it runs anywhere, because a pack cannot name what runs.
  **Minetest is the next rung**, and it is the first that is not free: it has no
  A2S probe, which forces the query protocol and the content source apart
  (`a2s.rs` is one implementation, not yet a `GameQuery` trait — one
  implementation does not tell you where the seam goes).
- **Later, conditionally — Mode 3 virtual LAN.** Only after Modes 1-2 have users;
  it changes the installer, the signing story, and the support burden
  (`MODES.md`).

## 9. Launcher stack — stay on Tauri v2

Decided 2026-08-30. The launcher generalizes from `svencoop-prns-clone/gui/`,
which is Tauri v2 with a vanilla-JS frontend (1489 lines across
`dist/{index.html,app.js,style.css}`, no framework).

**Stay.** Three reasons, in order of weight:

1. **The core is Rust and Tauri links it directly.** `gui/src-tauri/Cargo.toml`
   depends on `sc-rns-controller` and `sc-rns-bridge` as plain path deps — no IPC,
   no FFI, no serialization boundary between the UI and the relay it supervises.
   An Electron-class shell would have to drive a Rust sidecar process instead,
   which is strictly worse for an app whose entire job is process supervision.
2. **Tauri v2 ships mobile.** `MODES.md` identifies Android `VpnService` and iOS
   `NetworkExtension` as the *cleanest* Mode 3 virtual-LAN path — cleaner than
   desktop, which needs a Wintun driver, admin install, and code signing. egui,
   iced, and Slint have a weak-to-absent mobile story. Leaving Tauri forecloses
   the best version of Mode 3.
3. **There is nothing to escape.** The frontend is vanilla JS with no framework
   lock-in, and the server browser is a filterable list. The Windows gotchas
   (WebView2Loader.dll, `inline-assets.py`) are already solved and recorded.

**The one real weakness, fixable in config.** Tauri needs WebView2 on Windows. A
genuinely offline mesh machine that lacks it cannot start the launcher and cannot
download the runtime — precisely the scenario this platform advertises. WebView2
ships with Windows 11 and evergreen Windows 10, so it is an edge case, but it is
*our* edge case. `gui/src-tauri/tauri.conf.json` has no `windows` bundle block
today; add one before shipping to non-technical users:

```json
"bundle": {
  "windows": {
    "webviewInstallMode": { "type": "offlineInstaller" }
  }
}
```

`offlineInstaller` embeds the bootstrapper (a ~130 MB installer);
`fixedRuntime` pins an exact runtime shipped alongside the binary. Either removes
the network dependency at install time.

**Revisit only if** the launcher must become a single static binary with zero
system runtime — then egui, paying a frontend rewrite and the loss of mobile. Do
not pre-emptively switch; wait for WebView2 to actually bite in testing.

## 10. Decisions this plan does not make

Carried from `DESIGN.md` §7, still open, listed here so they are not lost:
node supply (user-contributed is the default under the decentralization rule, so
agents are untrusted), whether Mode 3 justifies a privileged installer, and
monetization.

Two have since been decided:

- **Which games are legally hostable** — decided 2026-08-30 by not deciding it
  centrally. The platform ships no list; an index operator configures what their
  nodes will host (§8 phase 4, `hosting.example.toml`).
- **Whether community packs are allowed at all** — decided 2026-08-30: **yes,
  tiered**, and §11 is how.

## 11. Pack distribution — drivers, signing, trust tiers

Decided 2026-08-30. Answers `DESIGN.md` §7's "signed packs only, or first-party
only" as **both, tiered**, and makes a downloadable pack safe to *execute*
rather than merely safe to read.

### 11.1 The joint that makes it safe

The premise everything else rests on: **Rust ships the bricks, a pack picks a
brick and hands it bounded parameters.** Never free-form, never argv. The
codebase already does this twice — `QueryProtocol::A2s` (the pack names an enum,
the code implements it) and the container image (the operator chooses, the pack
cannot). Pack distribution is the same seam extended, not a new architecture.

That is what lets a stranger's file be run rather than only read. A pack has no
field to put a command in, so a hostile one has nothing to say.

### 11.2 `[content]` — the missing brick

Today content provisioning is **manual**: an operator puts the game into
`<data_root>/content/<game>/<version>/` themselves and the agent checks the
declared `writable_paths` exist inside it before starting. That is the
`manual` driver, implicit rather than named. Touch-and-go needs the named ones:

```toml
[content]
driver = "steamcmd"
app_id = 276060          # a number, not a command line
```

```toml
[content]
driver = "archive"
url = "https://example.org/minetest-server-5.9.0.tar.xz"
sha256 = "9f2c..."       # the safety is here, not in the URL
strip_components = 1
```

Drivers to build, in order: `manual` (name what already happens), `archive`
(fetch, **verify the digest**, extract — a hijacked mirror gets nothing),
`steamcmd` (anonymous app ids only), `oci` (pull an image the *operator*
allowlisted). Each takes typed fields and nothing else.

**`manual`, `archive`, and `steamcmd` are built (2026-08-31).** The schema is
`crates/game-bridge/src/content.rs`; the half that touches a disk is
`crates/platform-agent/src/content.rs`. Absent `[content]` means `manual`, so
every pack written before the field keeps its meaning. Three properties carry
the safety, each pinned by a test: the digest decides rather than the URL and
mismatched bytes never reach the content tree; archive entry paths are rejected
rather than repaired, links included, because a symlink to `/` is a path escape
with extra steps; and extraction stages beside the destination and renames into
place, so an interrupted run cannot leave a partial tree that `plan_and_check`
reads as a complete install.

Two node-operator decisions sit on top of it. Installing is its own step
(`POST /content/:game`), not a side effect of create — a create request that
silently became a gigabyte download would time out. And fetching is **off
unless the operator turns it on** (`allow_content_fetch`): the digest keeps the
bytes honest, but until §11.3 exists nothing says the operator wanted those
bytes at all.

`steamcmd` follows the same seam one level up: the pack supplies `app_id`, a
number, and the agent builds the whole command line — `+login anonymous` is not
negotiable, so an app needing credentials stays `manual`. There is no `login`
field, because a pack is a file that gets shared and a field for credentials is
a field people put credentials in. Which steamcmd runs is `steamcmd_image` in
the agent config, and none configured disables the driver — the same rule as
`GameRuntime.image`. A failed run discards its staging directory, so a partial
download never becomes an install. `oci` is the one variant left; a pack naming
it today fails to parse, loudly.

Only anonymous-steamcmd titles can be fetched unattended (`GAMES.md` §5).
Anything needing credentials stays `manual`, which is the honest answer rather
than a worse one.

### 11.3 Signing, and why expiry rather than revocation

A curated repository's safety is a signing key plus **a person who reviews
submissions and revokes bad ones**. That is an ongoing commitment, not a feature
that ships once; say so before promising anyone a "safe" repo.

Revocation is the hard part on a mesh: nothing guarantees a node ever fetches a
revocation list. So **signed packs carry a validity window and go stale**. An
unrefreshed node fails closed instead of trusting a compromised pack forever,
and it degrades correctly offline — which a CRL does not.

**Built (2026-08-31):** `crates/game-bridge/src/signing.rs` — detached Ed25519
pack signatures with `not_before`/`not_after` inside the signed material,
`PackTrust` tiers, `TrustPolicy`, and `GamePack::load_verified`/
`load_dir_verified`. The load-bearing rule ("a signature that does not verify is
an error, never a downgrade") is pinned by
`an_expired_signature_is_an_error_not_an_unsigned_pack`.

**Wired into the node (2026-08-31):** `crates/platform-agent/src/packs.rs`
reads the pack directory under the operator's policy and hands the agent only
what that policy will deploy; the policy comes from a `[pack_trust]` section in
the agent config. The agent has no import step — every pack it reads it reads in
order to run — so §11.4's import/deploy split collapses there and the gate sits
at load. A refused pack is logged with its tier and the config key that would
change the answer.

**A missing `[pack_trust]` section means every readable pack deploys**, and the
agent warns at startup when there is none. That is not the safe default; it is
the honest one while this project has no first-party key and every shipped pack
is unsigned. A strict default would leave an upgraded node unable to start
anything, and the operator's fix would be to delete the section rather than to
sign a pack. Inside a section that exists, `allow_unsigned` defaults to false.

### 11.4 Trust tiers, shown rather than buried

1. **First-party** — signed by the project key.
2. **Signed community** — signed by a key the operator trusts.
3. **Unsigned local** — a file someone wrote. Refused for deploy unless the
   operator explicitly allowed unsigned packs.

The tier is surfaced at import and at deploy, in those words. A user importing a
community pack gets told what that means at the moment they do it, not in a
document they will not read.

### 11.5 What this does not change

A pack still cannot name a command, an image, or an executable, at any tier.
Signing raises confidence in *who wrote a description*; it does not turn a
description into code. If a future pack format ever needs to carry executable
intent, that is a new decision and this section is the argument against it.

## 12. Bug sweep — 2026-08-31

Scope: the signing work (`crates/game-bridge/src/signing.rs`, `pack.rs`
`load_verified`/`load_dir_verified`, `lib.rs` module export). All three findings
are fixed; the entry stays because finding 1 is the shape of mistake this design
invites again.

1. **The deploy gate was inert.** `load_verified`, `load_dir_verified`,
   `TrustPolicy::may_deploy`, and `verify_pack` had no caller: every pack-load
   path was still the unsigned one, so a forged or stale `.sig` changed nothing.
   A trust module that nothing calls tests green and enforces nothing — the
   tests exercise the checker, not the path a node takes. **Fixed** by
   `platform-agent/src/packs.rs` and the `[pack_trust]` config section; the
   test that would catch a regression to inert is
   `a_strict_policy_refuses_the_same_pack_and_says_what_to_write`, which fails
   if `strict()` and `allowing_unsigned()` ever again behave identically on a
   node. `a_forged_signature_is_not_retried_as_an_unsigned_pack` covers the
   other half: a failed signature must not fall back to the unsigned path.
2. **`PackSignature::remaining_secs` ignored `not_before`**, returning
   `not_after - now` for a window that had not opened — a launcher would show a
   future-dated signature as valid for a long time. **Fixed**: 0 before the
   window opens.
3. **`sign` truncates `valid_for` to whole seconds and saturates.** Neither is
   exploitable (an `EmptyWindow` or a far-future window). **Documented** on
   `sign` rather than changed: seconds is the resolution both bounds are signed
   at.

Verified sound, no defect: domain separation (`PACK_SIGNING_DOMAIN` ≠
`platform_auth::AUTH_DOMAIN`, confirmed byte-equal to the constant at
`crates/platform-auth/src/lib.rs:46`); window-inside-the-signature (editing
`not_after` invalidates the signature, pinned by test); signed-material length
prefix prevents boundary shifting; `read_signature_beside` treats
present-but-unreadable as an error, not as unsigned; crypto-checked-before-clock
in `verify` so a forged-and-stale signature reports as forged.

**The launcher shows the tier (2026-08-31).** `GameSummary` carries `trust`,
`trust_detail`, `signer` and `signature_expires_at`, and the detail pane renders
them in a "Game pack" section beside the Join button — the moment a pack stops
being a file and starts deciding how this machine talks to a server.

**The launcher shows and never gates.** A pack there does not put code on a
host; it tells a client where to point. Refusing to load an unsigned one would
stop a user browsing with a file they wrote and protect nothing, so
`from_pack_dir` uses `allowing_unsigned`. What still holds is the rule that a
signature which *fails* is an error: such a pack is skipped, not shown as
unsigned. Pinned by `a_pack_with_a_broken_signature_is_skipped_not_shown_as_unsigned`.

A fifth tier was needed: **`PackTrust::BuiltIn`**, for `GamePack::sven_coop()`.
§11.4's three tiers all describe a pack that arrived from somewhere, and the
built-in one did not — it is part of the binary. "Unsigned local" would invent a
provenance question about a file that does not exist, and a policy refusing it
would be the program refusing itself, so `may_deploy` allows it unconditionally.
