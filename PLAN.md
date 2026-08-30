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
| Link plaintext cap | ~1967 B; bridge uses `MAX_CHUNK = 1900` | `src/framing.rs:24` |
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
node with interfaces and a transport identity, and that is all. That makes it the
smallest of the four roles to build, which is why it lands early (§6).

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

## 5. Roles Play and Host — carry forward, do not rewrite

Already built for Sven. What generalizing them needs is in `DESIGN.md` §2.1/§2.2:
parametrized aspect (`SC_ASPECT_SERVER` is hardcoded at `src/relay.rs:189,207`),
`StreamRelay` for TCP games, framing v2 channel ids for multi-port games, and
`game-pack` manifests.

One item got cheaper since `DESIGN.md` was written: the **link allowlist**.
v0.1.9 (`c9ec90b`) already captures the peer identity at accept —
`src/relay.rs:282` inserts `(link_id, identity)` into `ConnectedClients`, `:366`
removes it on close. Gating is a rejection before that insert, not new plumbing.

Carry forward verbatim from the reference repo, per its `AGENTS.md`: process-group
spawn with `process_group(0)` and stop via direct `libc::kill(-pid, SIGKILL)` —
never a shelled-out `kill`, which does not exist in the slim image. It broke
three releases in a row.

## 6. Ordering

Refines `DESIGN.md` §5 by folding the four roles in. Rationale for the one
change: **Relay moves early** because it is the smallest new work of the four and
it is what makes the mesh actually reach — a browser listing three servers on one
LAN does not demonstrate anything.

- **Phase 0 — prove the ceiling.** Benchmark concurrent players and concurrent
  instances over Reticulum on one node with today's binaries. Every promise about
  player counts is conditional on this. Measure tier 1 for real; each new tier
  needs its own measurement before it ships.
- **Phase 0.5 — Relay role.** Not "enable transport" — it is already on for every
  node (§4). Add the off switch, the standalone Relay role, interface config, a
  rate cap, a byte counter, and the "cannot read your traffic" copy. Independent
  of everything else; ship it whenever. Treat the current always-on relaying as a
  defect to fix, not a feature to keep.
- **Phase 1 — generalize the bridge.** Parametrized aspect, the §3.3 announce
  record with fallback decode, link allowlist, `game-pack` manifests. Sven becomes
  pack #1 and the existing app must still work.
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

## 7. Launcher stack — stay on Tauri v2

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

## 8. Decisions this plan does not make

Carried from `DESIGN.md` §7, still open, listed here so they are not lost:
node supply (user-contributed is the default under the decentralization rule, so
agents are untrusted), whether Mode 3 justifies a privileged installer,
monetization, which games are legally central-hostable (only anonymous-steamcmd
titles — decide before phase 4), and whether community packs are allowed at all
given a pack is argv on a node.
