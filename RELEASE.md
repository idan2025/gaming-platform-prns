# Releasing

What ships, how it is built, and what has to be true before a tag. `PLAN.md` is
still the design authority; this file is only the mechanics.

## What ships

| Artifact | From | Who runs it |
| --- | --- | --- |
| `Mesh Game Servers` (launcher) | `launcher/src-tauri` | a player, or anyone hosting from a desktop |
| `platform-agent` | `crates/platform-agent` | a node operator, alongside Docker |
| `platform-index` | `crates/platform-index` | anyone who wants to run an index; **nobody has to** |
| `packs/*.toml` | this repo | shipped beside the binaries; a pack is data |

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
cargo build --release --bins        # target/release/platform-{agent,index}
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
- [ ] The launcher opens, lists a server, and joins it against a real server on
      a second machine. Loopback tests do not prove the mesh path.
- [ ] A deployed `svencoop-prns` v0.1.10 peer still appears in the launcher's
      list by name, and can still be joined — the wire-compatibility promise in
      `PLAN.md` §5 is a promise to people who already installed something.
- [ ] Version bumped in the three places above; `ENGINE.md`'s pin matches
      `Cargo.toml`.
- [ ] `README.md`'s status table matches what is actually built.

## What is deliberately not automated

There is no CI in this repo and no release workflow. Two of the checks above
need a Docker daemon and a second machine on a shared interface, so a green
pipeline that skipped them would be a worse signal than no pipeline: it would
report success for the parts that do not need proving.
