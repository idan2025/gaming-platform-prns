# Engine pin

The decision to fork is `PLAN.md` §7. This file records *what is pinned*, *what
the fork changes*, and *how to move the pin* — the three things §7 says must live
next to the dependency.

## The pin

| | |
| --- | --- |
| Fork | `https://github.com/idan2025/Prns` (fork of `KenAKAFrosty/Prns`) |
| Branch | `platform/0.3.7-hotfix.5` |
| Rev | `71e02528ff112eda3eea1948fed53d3c7b6c8554` |
| Base | upstream tag **`v0.3.7-hotfix.5`** (= upstream `main` for the engine) |
| Declared in | `Cargo.toml` `[workspace.dependencies]` |

**The platform and `svencoop-prns` no longer run the same engine tree.** The
standalone app's vendored copy (`/home/pi/svencoop-prns-clone/vendor`) is still
upstream `v0.3.7` plus the two patches below; the platform moved to
`hotfix.5` on 2026-09-23. The old pin (`platform/0.3.7`, `33f0a839`) is kept on
the fork so that tree stays reproducible.

That makes the wire-compatibility requirement in `PLAN.md` §5 something to
*measure*, not something inherited. It was measured before the move, with the
v0.1.10 `sc-rns-bridge` binary built from `svencoop-prns-clone` (its `src/` is
unchanged since the `v0.1.10` tag) against a `game-bridge` built on this pin,
over a TCP interface, with a UDP echo server standing in for the game:

| Server | Client | Discovered by announce | 32 / 600 / 1400 B round-trip |
| --- | --- | --- | --- |
| v0.1.10 (`v0.3.7`) | platform (`hotfix.5`) | yes | all three |
| platform (`hotfix.5`) | v0.1.10 (`v0.3.7`) | yes | all three |

No test in `cargo test` crosses engine versions, so **repeat that run before
every pin move** — a green suite on one engine proves nothing about the other.

## What the fork adds

One commit on top of `v0.3.7-hotfix.5`. It began as an unrecorded edit inside
the vendored copy in `svencoop-prns` (`c9ec90b` and earlier); as a real commit it
rebases onto a future Prns release with conflicts shown instead of silently
lost.

### Retired: `c393bae7` — expose announce `app_data` on `Diagnostic::AnnounceHeard`

**Upstream carries this now**, as `app_data: &'a [u8]`
(`prns-runtime/core/src/runtime/event.rs:137` at `hotfix.5`), so the patch was
dropped rather than rebased. The only caller change was a borrow: the field is
a slice, not an owned buffer. What follows is kept because it is why the field
matters.

`prns-runtime/core/src/runtime/event.rs`, plus a `..` in the tokio impl's
`tracing_events.rs`.

Upstream's `AnnounceHeard` carries `{destination, hops, source_interface}` only.
The destination hash is one-way, so an announce listener cannot recover the
announcer's aspect, identity, or name from it. **Without this patch there is no
server browser** — see `PLAN.md` §3.1. The field is `AnnounceAppDataBytes`
(owned, capped at 316 B) and is already covered by the announce's own Ed25519
signature, so the metadata is tamper-evident for free (`PLAN.md` §3.2).

Plausibly useful upstream; worth offering as a PR.

### `71e02528` — size the link plaintext cap for game-sized datagrams

(`33f0a839` on the old pin; cherry-picked onto `hotfix.5` without conflict.)
Still needed: upstream `main` and `trunk` both still size it off
`BROADCAST_MTU` (`prns-core/src/engine/commands/link.rs:23`).

`prns-core/src/engine/commands/link.rs`.

```
upstream: MAX_SEND_TO_LINK_PLAINTEXT_LEN = link_mdu(BROADCAST_MTU = 500) =  431
fork:     MAX_SEND_TO_LINK_PLAINTEXT_LEN = link_mdu(2048)                = 1967
```

with `link_mdu(mtu) = ((mtu - IFAC_MIN_LEN(1) - HEADER_MIN_LEN(19) -
TOKEN_OVERHEAD(48)) / 16) * 16 - 1` (`prns-core/src/routing/links/data.rs:35`,
constants in `prns-core/src/wire/limits.rs:22,26` and `crypto/token.rs:18`).

`BROADCAST_MTU` is the floor for broadcast-class packets, not a ceiling for
links — a link negotiates its own MTU and TCP interfaces go far higher
(`MAX_LINK_MTU = 524288`, `routing/links/mod.rs:27`). At 431 B every ~1400 B
GoldSrc datagram needs application-layer fragmentation.

**Consequence for the docs: `1967` is a fact about this fork, not about Prns.**
Anyone reading upstream and expecting 1967 gets 431. It is only a buffer size on
the caller-facing `SendToLinkPayload`, so an unpatched peer still interoperates —
it simply cannot send more than 431 B per call.

`crates/game-bridge/src/lib.rs` holds a `const _: () = assert!(...)` on this
constant, so a pin that loses the patch fails the build rather than quietly
turning `MAX_CHUNK = 1900` into a fragmentation storm. The `app_data` patch is
guarded by a test in the same file.

## Prns *is* on crates.io — and it changes nothing

`PLAN.md` §7 originally justified the fork partly with "Prns is not published on
crates.io". That premise is false: `personal-rns`, `prns-core` and `prns-runtime`
are all published, `0.3.7` included (crates.io, first published 2026-08-08).

The decision stands anyway, on the reason §7 already calls load-bearing: **we
patch the engine.** A registry cannot host our patches. What crates.io does buy
is an alternative *mechanism* — depend on the published versions and redirect
them with

```toml
[patch.crates-io]
personal-rns = { git = "https://github.com/idan2025/Prns.git", rev = "..." }
```

That is worth adopting if a third-party crate ever pulls `personal-rns` from the
registry into our tree (a `[patch]` unifies it; a plain git dep would give us two
copies of the engine and a type mismatch). Until then the direct git dep is
simpler and states the truth: we do not run a published version.

## Moving the pin

1. `git -C /home/pi/prns-fork fetch upstream --tags`
2. Branch `platform/<new tag>` from the tag and cherry-pick the link-cap commit
   — expect a conflict only in `link.rs`; anywhere else means upstream
   restructured and the patch needs rewriting, not merging. Keep the old branch:
   the rev in an old `Cargo.lock` must stay fetchable.
3. Rebuild here; the compile-time assertion and the `app_data` test are the
   gate, then the cross-engine run above.
4. **Then decide separately whether `svencoop-prns` follows.** It may keep its
   `vendor/` copy indefinitely (`PLAN.md` §7). Moving the platform's engine while
   the standalone stays on `v0.3.7` puts the two on different engine trees, which
   is exactly when the §5 wire-compatibility rules stop being free.
5. Update the rev, the base tag, and the diff evidence in this file.

### What moving to `hotfix.5` cost

- **`RemoteControl`.** Every `PrnsNodeRecipe` now takes a `remote_control`
  field, and `RequestEndpoint::handle` receives the node. Every role sets
  `RemoteControlService::Unavailable`: a node's control surface is the agent's
  uplink, not a second one the operator did not ask for.
- **Stack.** The node future no longer fits the 2 MiB `std::thread` default in a
  debug build — `browse_discovery`, `reticulum_query` and `uplink_roundtrip`
  died with `fatal runtime error: stack overflow`. `game_bridge::NODE_THREAD_STACK`
  (16 MiB) is applied to all three node threads.
- **Crypto crates.** The engine moved to `ed25519-dalek`/`x25519-dalek` 3.x.
  None of our crates names either directly, so `Cargo.lock` holds one copy.

Upstream `trunk` is further ahead again (unreleased), and building against it
adds a required `RemoteControlHostControls` bound on each role's app state. Wait
for it to reach a tag.

## Offline builds

A pinned git rev needs the network on a first build, which is awkward for a
project selling offline mesh operation (`PLAN.md` §7). Not solved yet. Release
builds will need `cargo vendor` output or a warm registry+git cache; the
Cargo.lock in this repo pins every transitive crate, so the remaining variable is
only the git checkout.
