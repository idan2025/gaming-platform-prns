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
`StreamRelay` for TCP games, framing v2 channel ids for multi-port games, and
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
- **Phase 3 — one node, many servers.** `platform-agent`, Docker orchestration,
  shared read-only content store plus per-instance writable overlay (a 2.74 GB
  copy per instance does not scale), port allocation. Agent has a local API; still
  no central service.
- **Phase 4 — index + optional hosting.** Announce indexer served over **both** a
  Reticulum destination and HTTPS, identity challenge/response auth, hosted-deploy
  API, agent uplink over Reticulum, multi-node. Per-account quotas and idle reaping
  from day one — public deploy plus anonymous identities is free compute.
- **Phase 5 — more games.** The ladder in `GAMES.md` §7: GoldSrc sibling (app 90)
  → Source/TF2 → Minetest → Minecraft Java. Each step exercises exactly one new
  axis. Never let the second game be the hard game.
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
agents are untrusted), whether Mode 3 justifies a privileged installer,
monetization, which games are legally central-hostable (only anonymous-steamcmd
titles — decide before phase 4), and whether community packs are allowed at all
given a pack is argv on a node.
