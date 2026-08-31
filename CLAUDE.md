# Working in this repo

## What this is

A **decentralized server browser** for game servers over Reticulum, built on
[Prns](https://github.com/KenAKAFrosty/Prns).

**Phase 1 is underway.** The docs are still the authority on design, but there is
now code: `crates/game-bridge/`, with the engine pin recorded in `ENGINE.md`.
**Phase 1 is done** — all six steps of `PLAN.md` §8: the Prns fork and pin, the
parametrized relay and framing, the §3.3 announce record, the link allowlist,
the Relay role with a transit off switch, and game packs with Sven Co-op as
pack #1. **Phase 2's browse core is built too**: a `Browse` role that binds no
game port and holds no identity, and `browse.rs` filtering and sorting from
announces alone, the §3.4 detail probe over a Link, and a Tauri launcher
(`launcher/`) over `crates/launcher-core`. `cargo test` is 93 tests and green,
clippy clean.

The launcher splits deliberately: `launcher-core` holds every shape the UI
consumes and is tested without a webview; `launcher/src-tauri` only forwards.
Its serde field names **are** the frontend contract — a rename is a silent break,
because JavaScript reads a missing property as `undefined`. Tests pin the keys.

`launcher/src-tauri` is excluded from the workspace, so `cargo test` at the root
does not build it; build it from its own directory.

**Phase 3 is done**: `crates/platform-agent` runs many servers on one host off
one shared copy of the content. Two rules there are load-bearing rather than
stylistic, and both have tests against a real Docker daemon:
- **A pack cannot name a container image.** An image selects the code a node
  executes, so it is agent config, chosen by the node's operator.
- **The agent only touches containers carrying its own label**, never a name
  prefix. This machine already runs unrelated containers; so will any real node.

The agent's Docker tests skip themselves where there is no daemon, and they
build a tiny local image from busybox rather than pulling anything.

**Phase 4 is underway**: `crates/platform-auth` (challenge/response against a
Reticulum identity) and `crates/platform-index` (registry, HTTP front door,
quota engine, and the index served over a Reticulum destination). Hosted deploy
is built, and **multi-node is built**: the agent's control surface runs over
Reticulum as a `platform-agent.control` destination (`crates/platform-agent/src/uplink.rs`),
so an agent needs no inbound port; the index drives it over a Link with
challenge/response auth and the operator's `trusted_indexes` allowlist
(`crates/platform-index/src/agent_client.rs`). The loopback HTTP API stays for
local use. A Docker-gated two-node round-trip test pins it end to end.

Two rules in phase 4's index and uplink that a later change could quietly break:
- **An auth signature is bound to the verifier's identity.** Anyone can run an
  index, so a hostile one could otherwise relay a challenge to a second index
  and replay the user's signature there. Removing the audience from the signed
  material would not fail any obvious test — the one that catches it is
  `a_signature_for_one_index_is_worthless_at_another`.
- **The index is a cache in the code, not just the prose.** Its registry is fed
  by a Browse session and its queries run the launcher's own `browse` filter. Do
  not give it a privileged source or its own query semantics; that is how a
  cache becomes a second source of truth.
- **The agent's `trusted_indexes` allowlist is re-checked on every op, not just
  at verify.** A session token alone is never enough, so an index removed from
  the list is refused on its next request, not its next login. Caching the
  trust decision at verify time would not fail any obvious test — the one that
  catches it is `a_token_stops_authorizing_when_the_identity_leaves_the_allowlist`
  and the live-wire `an_untrusted_index_cannot_even_list_over_the_uplink`.
- **The owner stamped over the uplink is the end-user's identity, not the
  index's.** The create request carries the user's identity hash; the agent sets
  `spec.owner` from it and stamps `OWNER_LABEL` unchanged. The authenticated
  index is trusted to assert it; the agent does not overwrite it with the
  caller's identity. Pinned by
  `an_index_creates_lists_stops_and_removes_on_a_remote_agent`.

**`StreamRelay` is built** (2026-08-31, `crates/game-bridge/src/stream.rs`): a
pack declaring `transport = "tcp"` runs. It rides the link's **channel**, not
`SendToLink` — a link data packet is acked individually and carries no sequence
number (`prns-core/src/routing/links/data.rs:149`), so a stream on top of it
would need sequencing, retransmission and a half-close invented inside a game
bridge. Two rules here:
- **`framing.rs` is not used on the stream path.** Chunking and ordering are the
  channel's job; a second framing layer is a second place to lose boundaries.
- **The server opens its stream reader when the link comes up, not when the
  allowlist finishes deciding.** Stream data arriving before a sink is
  registered is forwarded past it and dropped, and a TCP client sends its
  handshake immediately. Pinned end to end by `tests/stream_relay.rs`, whose
  half-close test is the one an echo-only test would not catch.

**Multi-port is built** (2026-08-31, `GAMES.md` §3): a pack's `[[extra_ports]]`
puts several of a server's ports on one destination — UDP extras on framing
generation 2's channel ids, TCP extras on their own stream id pairs
(`stream::stream_ids`), because a stream never passes through `frame()`. Rules a
later change could quietly break:
- **Only a multi-port game announces generation 2**, derived from `extra_ports`
  by `GameProfile::protocol_version()`.
- **The gate reads the *peer's* announce, never the local pack.**
  `relay::may_use_channel` decides, and a legacy announce reads as generation 1.
  Removing it would not fail any obvious test and would silently corrupt every
  deployed peer; the ones that catch it are
  `a_channel_id_is_never_sent_to_a_peer_that_did_not_advertise_v2` and
  `a_client_with_extra_ports_sends_none_of_them_to_a_v1_server`.
- **A reply rides the channel its request came in on**, so this side never
  initiates a channel a peer did not ask for, and a chunk for an undeclared
  channel is dropped rather than guessed onto a port.
- **Extra local ports are `listen_port + channel`** (overridable per channel),
  never the game's own numbers: those belong to the server's host.

The agent hosts a multi-port game too (2026-08-31): `ports.rs::acquire` takes a
whole set or none, `PublishedPort` carries the container-side and host-side
numbers separately, and each port is published in its own transport. Rules:
- **A port set is all-or-nothing.** A half-allocated instance never starts and
  leaks the rest of its set on every retry (`a_set_that_does_not_fit_gives_back_everything_it_took`).
- **The container is the record for ports, as it is for ownership.**
  `PORTS_LABEL` carries `channel:host_port/proto`, so a restarted agent knows
  which published port is the game and which is RCON; Docker alone cannot say.
  Seeding and release both read the whole set, not just `port`.
- **The container-side number is the pack's, the host-side is the node's.** A
  Source server binds 27015 inside whichever node it lands on; what that is
  reachable as outside comes from the operator's range.
- **The index passes a port set through, never invents one.** Only the node
  knows what is free there.

**A second game landed as data** (2026-08-31): `packs/half-life.toml` and
`packs/counter-strike-16.toml`, GoldSrc siblings on steamcmd app 90, with **no
Rust change** — `GAMES.md` §7 step 1's whole purpose. `tests/second_game.rs`
reads a pack off disk, runs a server from it and makes a browse node list it,
and never names the game in code; keep it that way, because a test that hardcodes
the game stops testing the abstraction. The next rung (Source/TF2) is not free:
it forced multi-port and framing v2 — both now built.

**Pack distribution has started** (`PLAN.md` §11.2): a pack's `[content]` block
names a driver — `manual` (what always happened, and the default when the block
is absent), `archive`, or `steamcmd` — and `crates/platform-agent/src/content.rs`
fetches, verifies, and extracts. It does not weaken "a pack cannot name what runs": the
pack names an enum variant this build implements and hands it typed parameters.

Rules there a later change could quietly break:
- **The digest decides, not the URL.** The archive is hashed before anything is
  extracted, and mismatched bytes are discarded. Verifying after extraction, or
  making `sha256` optional, would pass casual testing and turn a hijacked mirror
  into node compromise.
- **Archive entry paths are rejected, never repaired** — a link's *target*
  included, since a symlink to `/` is a path escape with extra steps.
- **Extraction stages and renames.** `plan_and_check` treats "the directory
  exists" as "the content is installed", so extracting straight into place would
  let an interrupted download look like a complete install.
- **`steamcmd_image` is operator config and a pack supplies only an app id.**
  Letting a pack name the image, or adding a `login` field to the driver, breaks
  the same rule as `GameRuntime.image` — and a credentials field on a format
  built to be shared is a credentials leak with a schema.
- **A failed provisioning run installs nothing.** Non-zero exit discards the
  staging directory, like a failed digest does.
- **Existing content is never replaced.** It may be bind-mounted read-only into
  containers running right now; a new version is a new directory.


**Pack signing is wired into the node** (`PLAN.md` §11.3, §11.4):
`crates/game-bridge/src/signing.rs` verifies detached Ed25519 signatures with a
validity window, and `crates/platform-agent/src/packs.rs` is the node's half —
the agent loads only packs its operator's `[pack_trust]` policy will deploy.
Rules there:
- **A signature that does not verify is an error, never a downgrade to
  unsigned.** An expired, forged or truncated `.sig` must not read as "this pack
  is unsigned", or an operator who allowed unsigned local packs would silently
  accept the one case they most wanted to hear about. Caught by
  `an_expired_signature_is_an_error_not_an_unsigned_pack` and
  `a_forged_signature_is_not_retried_as_an_unsigned_pack`.
- **The gate must have a caller.** It shipped inert once: a trust module nothing
  calls tests green and enforces nothing. `a_strict_policy_refuses_the_same_pack_and_says_what_to_write`
  fails if `strict()` and `allowing_unsigned()` ever again behave identically on
  a node.
- **A missing `[pack_trust]` section deploys everything, and the agent says so.**
  Deliberate while there is no first-party key and every shipped pack is
  unsigned; a strict default would be deleted rather than satisfied. Inside a
  section that exists, `allow_unsigned` defaults to false.

**The repo is not `cargo fmt`-clean** and has no `rustfmt.toml`. Do not run
`cargo fmt --all` — it reformats every file, in a style the tree was not written
in. Format new files with `rustfmt --config use_small_heuristics=Max <file>`,
which is close to the surrounding style, and match neighbouring code by eye.

Two tests are load-bearing rather than routine, and a change that breaks either
is breaking `PLAN.md` §5, not just a test:
`profile::tests::destination_hash_matches_deployed_sven` freezes the destination
hashes a deployed v0.1.10 Sven peer derives, and
`framing::tests::reserved_header_bits_are_ignored_not_rejected` pins the fact
that a deployed peer *silently corrupts* rather than rejects a header carrying a
channel id.

## Read this first

**`PLAN.md` is the entry point.** It carries the decided design, the measured
wire budgets, and the build order. `DESIGN.md` is the architecture, `GAMES.md`
the per-game variation and viability tiers, `MODES.md` the four transport modes.
`ENGINE.md` records the pinned Prns fork — read it before touching the engine
dependency or quoting a payload size.

When those disagree with each other, `PLAN.md` wins — it is the newest and it
records why. When any of them disagrees with the source, **the source wins**; fix
the doc and say so in the commit.

## Non-negotiables

1. **Decentralized by default, centralized only as a convenience, never as a
   dependency.** The zero-infrastructure baseline — two launchers, any shared
   Reticulum interface, no account, no index, no internet — must always work. An
   index is a cache of the mesh, never the source of truth. Game traffic never
   transits a platform component. (`DESIGN.md` §0)
2. **This is a server browser, not matchmaking.** Matchmaking needs a shared
   queue, which is load-bearing centralization. Do not add one. (`PLAN.md` §0)
3. **Never break `svencoop-prns`.** It stays a standalone product with its own
   repo, releases, and users. Sven Co-op becomes one game option here; the
   platform is never a prerequisite for it. (`PLAN.md` §5)
4. **Extraction is one-directional.** The platform copies from `svencoop-prns`
   and parametrizes. `svencoop-prns` must never gain a dependency on a platform
   crate. Accept the cost: shared relay/framing fixes get applied twice, each
   time as a deliberate decision. (`PLAN.md` §5)
5. **Wire compatibility with deployed v0.1.10 peers is mandatory.** Keep the
   bare-UTF-8 fallback decode in the announce record, and keep framing channel ids
   gated behind announce-advertised version negotiation with channel 0 frozen —
   `Reassembler::push` masks only `FLAG_FINAL` and ignores the other header bits,
   so an ungated change silently corrupts every deployed peer. (`PLAN.md` §3.3, §5)

## Measure the engine, never quote its README

Prns facts in this repo are read out of the pinned fork, checked out at
`/home/pi/prns-fork` (branch `platform/0.3.7`, `ENGINE.md`). That tree is
byte-identical to the engine vendored in `svencoop-prns` at
`/home/pi/svencoop-prns-clone/vendor`, so either reads the same — but the fork is
the one with history, and it is what this repo compiles.

**The Prns and Sven READMEs are stale on payload sizes** — the "384 bytes" figure
in the Sven README refers to the broadcast packet class, not to links. Verify
against source and cite `file:line`.

The numbers that matter (`PLAN.md` §2):

- Link plaintext **1967 B on our fork, 431 B on upstream Prns** — the size comes
  from a patch we carry, not from the engine as published. The bridge uses
  `MAX_CHUNK = 1900` (`src/framing.rs:24`). Never quote 1967 as a Prns fact;
  see `ENGINE.md`.
- Group plaintext **383 B, fire-and-forget** — a GROUP cannot prove delivery.
  Never build reassembly or retransmit on top of it.
- Announce `app_data` **316 B** (284 ratcheted). That is the entire
  server-browser row per server. Anything larger is a Link query or an index
  lookup.

Two facts that decide the browser's design, both easy to get wrong:

- `Diagnostic::AnnounceHeard` exposes only
  `{destination, hops, source_interface, app_data}` — **no aspect and no
  identity**. The destination hash is one-way, so a server's game cannot be
  recovered from it. The game id must travel in `app_data`.
- The announce's own Ed25519 signature already covers `app_data`
  (`prns-core/src/routing/announce/mod.rs:384`), so server metadata is
  tamper-evident for free. Do not add a second signature.

## Decisions already made — do not relitigate without a reason

| Decision | Where |
| --- | --- |
| Server browser, not matchmaking | `PLAN.md` §0 |
| `svencoop-prns` stays standalone; one-directional extraction | `PLAN.md` §5 |
| Engine dependency is a **fork** of Prns, pinned by rev | `PLAN.md` §7 |
| Launcher stays **Tauri v2** | `PLAN.md` §9 |

Open questions are listed in `PLAN.md` §10 and `DESIGN.md` §7. Two are now
decided: which games are hostable (the operator's choice, no shipped list) and
whether community packs are allowed (yes, tiered — `PLAN.md` §11). If you resolve
one, move it out of the open list and record the reasoning.

## The reference implementation

`/home/pi/svencoop-prns-clone` (GitHub `idan2025/Svencoop-Prns`, v0.1.10) is the
working single-host bridge this platform generalizes. **Read its `AGENTS.md`
before touching anything resembling DS process supervision, settings persistence,
or the release flow** — it records gotchas that broke three releases in a row
(process-group kill via `libc::kill`, settings.json interface dedup,
WebView2Loader.dll).

Do **not** make platform changes there. The one exception is a defect that
affects installed v0.1.10 users on its own merits — currently the unconditional
relaying described in `PLAN.md` §4 — and that ships as its own release decision,
not as platform work.

## Build order

`PLAN.md` §8. Phase 0 (benchmark the Reticulum ceiling) needs no code and runs
against `svencoop-prns` as-is. Phase 1 is the first code here, and its first step
blocks everything else: fork Prns and depend on the fork (§7), because Prns is not
on crates.io and the engine is already locally patched.

## Conventions

- Conventional commit subjects (`feat:`, `fix:`, `docs:`, `chore:`).
- Cite `file:line` for any claim about behavior. A claim without one is a guess.
- When a doc's numbers come from source, say which file they came from.
