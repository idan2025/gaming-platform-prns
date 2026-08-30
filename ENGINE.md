# Engine pin

The decision to fork is `PLAN.md` §7. This file records *what is pinned*, *what
the fork changes*, and *how to move the pin* — the three things §7 says must live
next to the dependency.

## The pin

| | |
| --- | --- |
| Fork | `https://github.com/idan2025/Prns` (fork of `KenAKAFrosty/Prns`) |
| Branch | `platform/0.3.7` |
| Rev | `33f0a8391bd0a07888e44207ec87f07fbe2b132b` |
| Base | upstream tag **`v0.3.7`** |
| Declared in | `Cargo.toml` `[workspace.dependencies]` |

The base is not arbitrary. The engine tree vendored in `svencoop-prns` v0.1.10
(`/home/pi/svencoop-prns-clone/vendor`) is byte-identical to upstream `v0.3.7`
except for the two patches below and three files the vendoring dropped
(`benchmarks/reference/requirements.lock`, `validation/oracles/requirements.lock`,
`docs/website/public/browser-node-playground-console/pkg`). Verified by
directory diff against every candidate tag: `v0.3.7` differs in 6 entries,
`v0.3.7-hotfix.1` in 50, `hotfix.4` in 143, `upstream/main` in 697.

So the platform and the shipped standalone app run **the same engine tree**.
That is what makes the wire-compatibility requirement in `PLAN.md` §5 testable
rather than aspirational.

## What the fork adds

Two commits on top of `v0.3.7`. Both were previously unrecorded edits inside the
vendored copy in `svencoop-prns` (`c9ec90b` and earlier); they are now real
commits that can be rebased onto a future Prns release with conflicts shown
instead of silently lost.

### `c393bae7` — expose announce `app_data` on `Diagnostic::AnnounceHeard`

`prns-runtime/core/src/runtime/event.rs`, plus a `..` in the tokio impl's
`tracing_events.rs`.

Upstream's `AnnounceHeard` carries `{destination, hops, source_interface}` only.
The destination hash is one-way, so an announce listener cannot recover the
announcer's aspect, identity, or name from it. **Without this patch there is no
server browser** — see `PLAN.md` §3.1. The field is `AnnounceAppDataBytes`
(owned, capped at 316 B) and is already covered by the announce's own Ed25519
signature, so the metadata is tamper-evident for free (`PLAN.md` §3.2).

Plausibly useful upstream; worth offering as a PR.

### `33f0a839` — size the link plaintext cap for game-sized datagrams

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
2. `git rebase <new tag> platform/0.3.7` — expect conflicts only in the three
   patched files; if a conflict lands anywhere else, upstream restructured and
   the patch needs rewriting, not merging.
3. Rebuild here; the compile-time assertion and the `app_data` test are the gate.
4. **Then decide separately whether `svencoop-prns` follows.** It may keep its
   `vendor/` copy indefinitely (`PLAN.md` §7). Moving the platform's engine while
   the standalone stays on `v0.3.7` puts the two on different engine trees, which
   is exactly when the §5 wire-compatibility rules stop being free.
5. Update the rev, the base tag, and the diff evidence in this file.

Upstream has already moved past the pin: `v0.3.7-hotfix.1` through `hotfix.4`
(a Heltec LoRa fix) and `main` at `a2f1fabf`. Nothing in them is known to matter
to the platform, and none has been validated against the bridge. Staying on
`v0.3.7` keeps parity with the shipped app; that is the trade being made.

## Offline builds

A pinned git rev needs the network on a first build, which is awkward for a
project selling offline mesh operation (`PLAN.md` §7). Not solved yet. Release
builds will need `cargo vendor` output or a warm registry+git cache; the
Cargo.lock in this repo pins every transitive crate, so the remaining variable is
only the git checkout.
