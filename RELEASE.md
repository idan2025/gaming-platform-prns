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
