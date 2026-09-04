# Releasing

What ships, how it is built, and what has to be true before a tag. `PLAN.md` is
still the design authority; this file is only the mechanics.

## What ships

| Artifact | From | Who runs it |
| --- | --- | --- |
| `Mesh Game Servers` (launcher) | `launcher/src-tauri` | a player, or anyone hosting from a desktop |
| `platform-agent` | `crates/platform-agent` | a node operator, alongside Docker |
| `platform-index` | `crates/platform-index` | anyone who wants to run an index; **nobody has to** |
| `game-bridge` | `crates/game-bridge` | anyone hosting a game server, or donating transit, without a desktop |
| `packs/*.toml` | this repo | shipped beside the binaries; a pack is data |

`game-bridge` exists because two of the four roles (`PLAN.md` §1) otherwise had
no way to run: the launcher joins and browses, and the agent orchestrates
containers, so a person with a game server and no desktop could not host and
someone donating transit needed a GUI to do it. It is the same library the
launcher links, exposed as `server`, `client`, `relay` and `browse`.

The launcher is the only artifact a player needs. An index is a convenience and
never a dependency (`DESIGN.md` §0), so a release that shipped only the launcher
would still be a working product.

## Version

One version for the whole platform, in `[workspace.package]` at the repo root;
every crate inherits it with `version.workspace = true`. Two places do **not**
inherit and must be bumped by hand, because the Tauri shell is its own
workspace:

- `launcher/src-tauri/Cargo.toml`
- `launcher/src-tauri/tauri.conf.json` (`version`, which names the bundle)

`svencoop-prns` versions independently and always has — `PLAN.md` §5. Do not
sync them.

## Build

```sh
# Everything that is not the launcher shell
cargo build --release --bins        # target/release/{game-bridge,platform-agent,platform-index}
cargo test --workspace              # Docker-gated tests skip themselves without a daemon
cargo clippy --workspace --all-targets

# The launcher shell (its own workspace, excluded from `cargo test` at the root)
cd launcher/src-tauri && cargo build --release
cargo tauri build                   # bundles; needs `cargo install tauri-cli`
cargo tauri build --bundles deb     # one target, when the others are not wanted

# The frontend, headless
cd launcher/uicheck && npm install && node render.mjs
```

`CARGO_INCREMENTAL=0` is worth setting on a build machine: incremental artifacts
regrow tens of gigabytes over a few days here.

### Linux bundles, as actually observed

`cargo tauri build` on Debian produces `.deb` and `.rpm` from
`launcher/src-tauri/target/release/bundle/`. Both were built and inspected at
0.1.0: the deb installs `usr/bin/mesh-game-servers`, a desktop entry and icons,
and declares `libwebkit2gtk-4.1-0, libgtk-3-0`.

`mainBinaryName` in `tauri.conf.json` is what makes the installed binary
`mesh-game-servers` rather than `launcher`. It is worth keeping: `/usr/bin/launcher`
is a name any project could claim, and this repo already refuses to manage
containers by name prefix for the same reason.

**A tag alone used to build nothing.** The workflow only *uploaded* to a
release, so a tag with no hand-made GitHub Release failed every job with
`release not found` — after building every artifact. v0.1.0 hid this because its
release had been created by hand. The upload steps now create the release if it
is missing, which makes pushing a tag sufficient on its own.

## v0.2.2

The local port a join binds is now the player's to choose.

- **A join can bind a port other than the pack's default.** The default is the
  port the game's *own* dedicated server listens on, so any machine already
  running one owns it — the node this was developed on publishes
  `0.0.0.0:27015/udp` for an unrelated container — and the join failed with
  `Address already in use` on a number nobody picked. The detail pane now
  offers the port and remembers it per game; leaving it alone keeps the old
  behaviour exactly. Extra ports still follow it, because `relay.rs` derives
  them as `listen_port + channel`.
- **Tauri commands report an error's whole cause chain.** `e.to_string()`
  prints only the outermost context, which is how 0.2.0's join failure read
  `loading identity at ./game-bridge-client.identity` and dropped the reason
  the file could not be written. The helper is in `launcher-core`, so the
  shell stays a pure forwarder.

Verified against a live node rather than only in tests: a recreated instance
came up on `GPP_MAP=crystal`, `changelevel crystal2` moved it without a
restart, and an A2S query over a real Reticulum path reported the new map.

## v0.2.1

A join that could not work on an installed launcher, and map control on a node.

- **The launcher's client identity is no longer written to the working
  directory.** `ClientArgs::new` defaults it to
  `./game-bridge-client.identity`, which suits the CLI and not a desktop app:
  a `.desktop` entry starts the launcher in `/`, and macOS starts it in the
  bundle. Every Join ended in `loading identity at
  ./game-bridge-client.identity`. It now sits beside `launcher.json`. This
  affects **installed** 0.2.0 users, not developers running from a checkout,
  which is why it went unnoticed.
- **A server whose announce names no game can be joined.** Deployed
  `svencoop-prns` v0.1.10 peers announce a bare name, so `browse.rs` shows them
  only under "Any game" and no pack could be matched to them. The detail pane
  now offers a game picker and Join stays disabled until one is chosen; the
  launcher still never guesses a wire protocol.
- **A starting map, and a live map change.** `InstanceSpec.map` reaches the
  container as `GPP_MAP`; `POST /instances/:id/map` runs `changelevel` on the
  running server, so players stay connected. A pack declares which console its
  game speaks (`console = "goldsrc" | "source"`) and never the command itself.

**Nodes upgrading from 0.2.0 must recreate their servers to use map changes.**
Docker decides at *create* whether a container has stdin, and it answers an
attach to one that does not with `200 OK` while discarding every byte. The
agent inspects `Config.OpenStdin` and refuses rather than reporting a map
change that never happened — but `docker restart` does not fix such a
container. Remove the instance and start it again.

## v0.2.0

The first release with a Windows launcher, and the first where a hosted server
is reachable over the mesh rather than only on the node's LAN.

- **Game servers are announced on Reticulum.** One bridge per instance, its own
  identity, stable across restarts. Verified with an independent browse node.
- **A host-side web UI**, served by the agent: install a pack, start a server,
  stop, restart, and watch players. Token-authenticated, so it can be reached
  from another machine.
- **A Play button in the launcher**, with the game located from a saved path or
  a Steam library. A pack says how to point a game at a server and still cannot
  name a program to run.
- **Mesh interfaces configurable from both UIs**, IFAC included, persisted.
- Pack signing reachable from the command line (`game-bridge sign` / `verify`),
  the trust gate wired into the node, idle reaping actually running, and
  capacity-aware placement.
- A bare Sven Co-op dedicated-server image (`images/sven-coop`), because the
  standalone product's image runs its own controller and was never going to be
  driven by an agent.

**The Windows launcher did not ship in v0.1.0, and the release did not say so.**
`cargo tauri build` failed on the runner with

    path too long: '.../benchmark-wire-driver--rns-1.4.0-compiled--benchmark-wire-driver.jsonl'; class=Filesystem (30)

The engine fork carries benchmark result files whose paths run to 198
characters, and the default Windows `CARGO_HOME` puts the checkout at 266 — over
the 260-character limit. `core.longpaths` does not fix it: cargo *fetches* with
the git CLI when told to, but *checks out* with libgit2, which fails first. The
fix is `CARGO_HOME=C:\cg`, which brings the same path to 247. That step must run
**before** `rust-cache`, or the cache restores into the CARGO_HOME it is about
to abandon.

The release still published, because the launcher matrix has `fail-fast: false`
and every other target succeeded. **Check the assets, not the run's colour:** a
release is short a platform whenever a bundle is missing, and nothing else says
so. The checklist below now asks for it explicitly.

**AppImage needs two things this host did not have**, and it is the only target
that fails without them:

- `librsvg2-dev` on the build machine. Without it the run ends in
  `there is no 'libdir' variable for 'librsvg-2.0' library` from
  `linuxdeploy-plugin-gtk`, after the deb and rpm have already succeeded.
- The network *at bundle time*: AppImage bundling downloads `linuxdeploy`, its
  GTK and GStreamer plugins and `AppRun` into `~/.cache/tauri`. So an AppImage
  cannot be produced on the same air-gapped machine that `cargo vendor` was
  meant to serve. Build it somewhere with a network and ship the artifact.

**A stale frontend is a cache, not a build failure.** WebKitGTK keeps the
embedded assets under `~/.local/share/<bundle identifier>`; delete that
directory when a CSS or JS change does not appear in a rebuilt launcher
(`launcher/README.md`).

Windows and macOS bundles have not been produced or tested from this repo.
`webviewInstallMode: offlineInstaller` is set for the Windows case
(`PLAN.md` §9): a genuinely offline machine that lacks WebView2 could otherwise
neither start the launcher nor download the runtime.

## Building without the internet

The engine is a **pinned git rev of a fork**, not a crates.io release
(`ENGINE.md`, `PLAN.md` §7), so a first build needs the network — awkward for a
project that advertises offline mesh operation. `cargo vendor` covers it, git
dependency included:

```sh
cargo vendor vendor/    # ~500 MB, writes the [source] stanzas to paste
```

Paste the printed `[source]` stanzas into `.cargo/config.toml` and the tree
builds with no network at all. Do this for a release tarball rather than asking
an operator on a mesh-only machine to fetch a git dependency.

`vendor/` is deliberately not committed: it is half a gigabyte and it would
double every engine-pin bump in review.

## Before tagging

- [ ] `cargo test --workspace` green **with a Docker daemon present** — the
      agent and uplink tests skip themselves silently otherwise, and they are the
      ones that cover multi-node.
- [ ] `cargo clippy --workspace --all-targets` clean.
- [ ] `launcher/uicheck` passes.
- [ ] **Every platform's launcher bundle is actually attached to the release.**
      Linux `.deb`/`.rpm`/`.AppImage`, macOS `.dmg`, Windows `.msi` or `.exe`.
      A missing one is a platform with no GUI at all, and the workflow goes
      green without it.
- [ ] `cargo tauri build` produces a bundle, and the installed binary is
      `mesh-game-servers`.
- [ ] `python3 scripts/live_roundtrip.py` passes: the shipped `game-bridge`
      binaries announce, discover each other by announce alone, and carry a UDP
      round trip. This is the check that the *artifacts* work, not the library.
- [ ] The launcher opens, lists a server, and joins it against a real server on
      a second machine. Loopback tests do not prove the mesh path, and
      `live_roundtrip.py` is still one machine.
- [ ] A deployed `svencoop-prns` v0.1.10 peer still appears in the launcher's
      list by name, and can still be joined — the wire-compatibility promise in
      `PLAN.md` §5 is a promise to people who already installed something.
- [ ] Version bumped in the three places above; `ENGINE.md`'s pin matches
      `Cargo.toml`.
- [ ] `README.md`'s status table matches what is actually built.

## Cross-platform artifacts come from CI

`.github/workflows/release.yml` builds what this machine cannot: macOS and
Windows. A Tauri app cannot honestly be cross-compiled from Linux — macOS needs
the Apple SDK, Windows needs MSVC and WebView2 — and there are no cross linkers
here either.

Two jobs, both uploading to the tag's release with `gh release upload --clobber`:

- **cli** — `game-bridge`, `platform-agent`, `platform-index` for
  `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin` and
  `x86_64-pc-windows-msvc`, packaged with `packs/`.
- **launcher** — `cargo tauri build` on ubuntu-22.04, macos-14, macos-13 and
  windows-latest.

It fires on a `v*` tag push, and takes a `tag` input for `workflow_dispatch`
when the tag already exists:

```sh
gh workflow run release.yml -f tag=v0.1.0
```

Two things it has to do that are not obvious, both learned by watching it fail:

- **Windows cannot check out the engine without `core.longpaths`.** The Prns
  fork carries benchmark result files whose paths exceed 260 characters, and
  cargo fails before compiling anything with `path too long: ...`. Setting
  `core.longpaths` is not enough on its own — `CARGO_NET_GIT_FETCH_WITH_CLI=true`
  is what makes cargo honour it, because libgit2 does not.
- **`gh release upload` must not be handed an unmatched glob.** Only one archive
  shape exists per OS, so `dist/*.zip` on Linux failed the job with
  `no matches found` after a successful build. `nullglob` and an array.

## What is deliberately not automated

The workflow builds and uploads; it gates nothing. Two of the checks above need
a Docker daemon and a second machine on a shared interface, so a green pipeline
that skipped them would be a worse signal than no pipeline: it would report
success for the parts that do not need proving.

## macOS bundles are unsigned

There is no Apple Developer certificate here, so the `.dmg` is neither signed
nor notarized and Gatekeeper will refuse it on first open. Right-click → Open,
or `xattr -d com.apple.quarantine`. Signing is a paid account and a decision
about identity, not a build flag.
