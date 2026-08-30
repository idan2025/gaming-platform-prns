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
announces alone. Remaining in phase 2: the §3.4 detail probe over a Link, and
the launcher UI. `cargo test` is 74 tests and green, clippy clean.

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

Open questions are listed in `PLAN.md` §10 and `DESIGN.md` §7. If you resolve
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
