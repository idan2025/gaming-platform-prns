# Launcher

Tauri v2 shell (`PLAN.md` §9) over `crates/launcher-core`. The split is
deliberate: `launcher-core` holds every shape the UI consumes and is tested
without a webview; `src-tauri` only forwards.

**`launcher/src-tauri` is excluded from the workspace**, so `cargo test` at the
repo root does not build it. Build it from its own directory.

## Frontend contract

`launcher-core`'s serde field names *are* the frontend contract — JavaScript
reads a missing property as `undefined`, so a rename is a silent break.
`crates/launcher-core` has tests pinning the key sets.

Two rules the frontend has to keep, both from `launcher-core`'s own docs:

- **Unknown is not zero.** A legacy announce carries no player count; the row
  renders `—`, never `0/0`.
- **A probe that did not answer is a state, not an error.** Mesh routing is
  asymmetric and an allowlisted server refuses probes on purpose, so the detail
  pane says so — it does not raise an error banner or call the server offline.

## Looking at it

There is no screenshot tooling on this machine, but Xvfb and ffmpeg are enough:

```sh
Xvfb :99 -screen 0 1280x800x24 &
DISPLAY=:99 ./src-tauri/target/release/mesh-game-servers &
# WebKit takes ~30s to map a window on a cold start here
DISPLAY=:99 ffmpeg -f x11grab -video_size 1280x800 -i :99 -frames:v 1 -y shot.png
```

**WebKitGTK caches the embedded frontend per app identifier.** A CSS or JS
change that does not appear after a rebuild is almost always that cache, not the
build:

```sh
rm -rf ~/.local/share/org.idan2025.gamingplatformprns.launcher
```

This cost an hour once: two correct rebuilds in a row rendered the old
stylesheet, which looked exactly like `cargo build` failing to re-embed
`dist/`.

## Checks

```sh
# Rust side, from the repo root
cargo test -p launcher-core

# The Tauri shell (its own workspace)
cd launcher/src-tauri && cargo build

# Headless render pass over dist/ — real DOM, mocked Tauri bridge
cd launcher/uicheck && npm install && node render.mjs
```

`uicheck` is not a screenshot test. It renders `dist/` in jsdom against payloads
shaped exactly like `launcher-core` serializes them and asserts what the two
rules above require, plus that no `undefined` reaches the DOM. It needs `npm
install` (jsdom), which is why it is not wired into `cargo test`.
