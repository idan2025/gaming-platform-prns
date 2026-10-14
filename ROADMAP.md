# Roadmap

What comes after the 2026-10-10 redesign, in the order it is worth doing.
`PLAN.md` is still the design authority and its non-negotiables bind every item
here: decentralized by default, a server browser and never matchmaking, wire
compatibility with deployed peers, and a pack that can never name what runs.
Each item says which of those it has to respect, and what is still a decision.

Sizes are rough: **S** is a day or two, **M** about a week, **L** more.

| # | Feature | Who it is for | Size | Status |
| --- | --- | --- | --- | --- |
| 1 | Auto-update | every player | M | **done** (v0.2.31) |
| 2 | Invite links | players with friends | S–M | **done** (v0.2.31) |
| 3 | Favorites and "it's up" alerts | regulars | S | needs a decision |
| 4 | Installed-game detection | every player | M | proposed |
| 5 | Tray icon and notifications | hosts, regulars | M | proposed |
| 6 | Chat and player names in rooms | LAN-room players | L | proposed |
| 7 | Node: console, resources, schedules, backups | node operators | M–L | **started**: live usage, log, players, crash restarts |
| 8 | In-launcher pack browser | everyone, pack authors | L | planned (`PLAN.md` §13) |
| 9 | Playability check before joining | players on slow links | S | **started**: the round trip in words |
| 10 | Controller and big-screen mode | Steam Deck, couch | M | proposed |
| 11 | Node: installed games and downloads | node operators | M | **built**: Installed page, delete, cancel, update (v0.2.36) |
| 12 | Node: update checks, storage-safe downloads, moving servers | node operators | M–L | **started**: checks, snooze, reserve, queue (v0.2.37); automatic download, moving servers |
| — | Rebrand | the project | S–M | **done: Lanthorn** (v0.2.31) |

Recommended first: **1 and 2.** They remove the most friction for a new player:
nobody stays stuck on an old build, and nobody has to be talked through finding
a server.

---

## 1. Auto-update

**Done in v0.2.31** (`launcher-core/src/update.rs`, `check_update` /
`install_update` in the shell, `updater-manifest` in `release.yml`; key
handling in `RELEASE.md`). Decided: ask, with the notes shown, and a setting to
turn the check off.

**What.** The launcher checks for a newer release, downloads it, verifies its
signature, and restarts into it — on a click, or on its own at the next start.

**Why.** Every field bug so far was fixed in a release, and a player who never
updates keeps the bug. Today updating means finding the GitHub page and the
right one of 22 files.

**How.** Tauri v2's updater plugin, fed a `latest.json` the release workflow
already has everything to produce, signed with a key held in CI secrets. AppImage,
NSIS and the macOS bundle are supported by the plugin; the Arch package and the
portable builds are not, and should say "a new version is out" with a link
instead of updating themselves.

**Rules.**
- **An update check is the internet, and the baseline is no internet.** The
  check must fail quietly and change nothing when offline; nothing may wait on
  it at start. It must be possible to turn off.
- **The signature decides, not the URL** — the same rule as content archives
  (`content.rs`). An unsigned or mis-signed download is discarded.
- A portable launcher writes nothing outside its folder (`portable.rs`), so it
  never replaces itself in place without asking.

**Decide.** Update silently on start, or always ask? Recommendation: ask, with
the release notes shown, and a "do this automatically" checkbox.

## 2. Invite links

**Done in v0.2.31** (`launcher-core/src/invite.rs`; the scheme is
`lanthorn`, with `room=1` for rooms; the https fallback page is not built yet).

**What.** "Copy invite" on a server or room gives a link such as
`lanthorn://join/<destination>?game=<id>`. Clicking it in Discord, WhatsApp or
a browser opens the launcher on that server and offers Join.

**Why.** "Find Idan's room in the list" fails exactly when it matters: before a
server's announce reaches the friend. A destination hash is all a join needs, so
a link carries everything.

**How.** Tauri's deep-link plugin registers the scheme on all three platforms.
The launcher parses the link, validates the hash and game id as data, and opens
the detail pane — it **never joins on its own**; a person presses Join. An
`https://` fallback page that shows "open in the launcher, or download it" can
be a static page on GitHub Pages.

**Rules.**
- A link is untrusted input from a chat message: validate, never act on it
  without a click, and never let it set anything but which server is shown.
- No server-side component is required; the https page is a convenience.

## 3. Favorites and "it's up" alerts

**What.** Star a server or room. Starred ones are always looked for, and the
launcher tells you when one comes up.

**Why.** The list is live-only by design now, which is right for browsing and
wrong for "my friend's server, when is it on?".

**How.** A `favorites` set in settings; the keep-alive (`KEEPALIVE_EVERY`)
asks for favorites first; an alert when one moves from unheard to heard.

**Decide (needs the user).** The list shows only what is live. Should a
favorite that is offline show at all — in its own "Favorites" section, greyed,
"last online 2 days ago" — or only appear when it is up? Recommendation: its own
section, clearly offline, because a favorite is something the player asked to
remember; the main list stays live-only.

## 4. Installed-game detection

**What.** The launcher reads the player's Steam libraries (and, on Linux, the
Proton prefixes), shows an **Installed** badge, and makes Play work without
"Locate game".

**Why.** "Locate game" is the last manual step between Join and playing.

**How.** Parse `libraryfolders.vdf` and each `appmanifest_<id>.acf` for the
pack's `[launch] steam_app_id`; `steam.rs` already finds Steam. Games that are
not on Steam (both Need for Speed games) keep Locate, with a file picker in
place of today's text prompt.

**Rules.** The executable still comes from the player's own installation and is
never guessed from a pack (`PLAN.md` §13.1). Detection reads files only.

## 5. Tray icon and notifications

**What.** Closing the window keeps the launcher in the system tray while a room
or join is active. Native notifications for: someone joined your room, a
favorite came online, a room check failed, an update is ready.

**Why.** A host has to keep the launcher open for the room to exist, and today
that means a window in the way the whole evening.

**Rules.** Quitting from the tray must still end the room the way a quit does
now — nothing a room creates may outlive the launcher (`CLAUDE.md`, Mode 3).
Notifications are opt-out, and none fires while a game is fullscreen if the OS
can tell.

## 6. Chat and player names in rooms

**What.** A small text chat per room (and per server, for its bridged players),
and members shown by player name instead of `198.19.x.x`.

**Why.** A LAN room is a party; today it is a list of addresses.

**How.** Room members already hold Links to the host, so messages ride those
links' channel. Alternatively LXMF, which would make a room chat readable from
Sideband or NomadNet; worth measuring before choosing. The player name already
exists in settings (`player_name`).

**Rules.**
- **Chat is not matchmaking** (`PLAN.md` §0): no global lobby, no queue — only
  the people already in a room or on a server.
- Messages are plain text, length-capped, rate-limited per member at the host
  like broadcasts (`lan::BroadcastGate`).
- A name is data from another member: never trusted for identity, which stays
  the Reticulum identity.

## 7. Node: console, resources, schedules, backups

**What,** in the node's web page:
- **Live console and log** for each server (`docker logs` streamed), and
  sending a console line for games whose pack declares a console.
- **CPU, memory and network** per server, from Docker's stats API.
- **Scheduled restarts** and a daily restart window.
- **Backups** of a server's writable paths (saves, logs), with restore.

**Rules.**
- **A pack names a console protocol, never a command** (`console.rs`). A free
  console box is the operator typing at their own server — allowed — but it
  goes through the same newline-refusing validation as a map name, and it is
  never something a pack or an index can send.
- Backups cover only `writable_paths`; the shared content is read-only and
  never copied per instance.
- Everything rides behind the API token, like every other mutating route.

**Done (after v0.2.32).** Per running server: CPU, memory, network, disk and
players, every 2 s with two minutes of history, and a node total
(`GET /usage`, `Agent::usage`). It sends Docker's counters and the page works
out rates, so the agent keeps no per-viewer history. Memory turns amber above
90% of the limit. A read-only log tail (`GET /instances/:id/logs`, through
`assert_managed`), free disk space where content lives (`GET /storage`) with a
warning before a download on a nearly full disk, and "Copy invite link" in a
server's menu, byte for byte the launcher's link (pinned on both sides).
Then: a players list per server, why a stopped server ended (crash, signal,
out of memory), restart-on-crash done by the agent (`check_crashes`, at most
5 an hour, never after a Stop), and cleanup of leftover instance folders on
request. **Not yet:** sending a console line, schedules, backups.

## 8. In-launcher pack browser

Already planned as `PLAN.md` §13 (the marketplace). Adds: install a signed
community pack from inside the launcher, with its art (the `art` field now
exists), its trust tier shown, and an update when the author signs a new
version. The rule that keeps it safe is the format, not a scanner (`PLAN.md`
§13.2).

## 9. Playability check before joining

**What.** Before Join, compare the game's `min_link_class` with the path to the
server (hops, the interface it was heard on, the measured round trip from the
detail probe) and say it plainly: "This game needs a fast link; this server is
reached over LoRa and will not be playable."

**Why.** `GAMES.md` already has the tiers and the launcher already has the
numbers; today the player finds out after the game times out.

**Rules.** A warning, never a block — the player decides. No probe is sent
that the detail pane does not already send (`relay.rs`: one server, because a
person opened it).

**Started (after v0.2.32).** The detail pane puts the probe's round trip in
words under the numbers: close, a little delay, expect lag, very far. No
number, no hint. Then the pack's `min_link_class` against the interface the
server was heard on: a tier 2+ game over LoRa or another radio link is warned
about, a tier 1 game is told joins will be slow. Only the first hop is known.

## 10. Controller and big-screen mode

**What.** A full-screen layout with large tiles, driven by a gamepad: browse,
join, play, leave. Good on a Steam Deck in Game Mode and on a TV.

**How.** The Gamepad API in the webview, a second stylesheet, and focus that is
always visible. The AppImage already runs on SteamOS.

## 11. Node: installed games and downloads

**Status (2026-10-13): build plan steps 1–6 are built** (the Installed page,
`GET /content`, delete, progress, cancel, unused copies first). Where it
departs from the plan below, the code is right and says why: cancel is
`DELETE /content/:game` (the install's own resource) rather than
`/content/:game/install`, which would shadow a version named `install`; and
`GET /content` answers `{copies, downloads}` so the page polls one route, not
one per game. Both open decisions took the safe option: Delete only lists the
servers using a copy (409 with their ids), and no total is shown for any
download yet. From step 7, **Update a game** is built too, with a
"remove the old version when done" option (`CLAUDE.md` has its rules);
verify, room-before-download, remove a pack and a queue are still open.

**What.** An **Installed** page in Lanthorn Host listing every game whose files
are on this node, and every download in progress, with the controls to manage
them:

- One row per installed copy: the game, its version directory, its size on
  disk, when it was installed, and which servers use it (running or stopped).
- **Delete** a copy you no longer want, after a confirmation that names its
  size and that nothing uses it.
- Pressing **Install** on the Games page, or starting a server whose game is
  not on the node yet, adds a row here at once, marked **Downloading**, with
  live progress: bytes on disk so far, the rate, the time it has taken, and the
  total when one is known. When it finishes the row becomes an ordinary
  installed copy; when it fails, the row keeps the reason, as `install.rs`
  already does.
- **Cancel** on a download stops it and discards what it fetched.

Packs and game files are different things and the page keeps them apart: a
pack is the small `.toml` that says what a game is (the Games page), and the
game files are what a download puts in `data_root/content/<game>/<version>/`.
Deleting the files leaves the pack, so the game is still offered and can be
downloaded again; removing the pack (an addition below) leaves the files.

**Why.** A Source game is 2–29 GB and a node is somebody's disk. Today the
only way to see what is installed, or get the space back, is a shell on the
node, and a download shows nothing but how long it has been running. On this
node on 2026-10-11, Counter-Strike 1.6 had two copies, `app90` (928 MB) and
`app90-yapb` (931 MB), and only `app90-yapb` is the one `agent.toml` names: a
gigabyte that nothing uses and nothing shows.

**How.**
- `GET /content`: every `content/<game>/<version>` directory with its size,
  modification time, whether `agent.toml` names it as that game's
  `content_version` (in use by new servers, or an older version), and the ids
  of the containers that mount it. Read from the disk and from Docker, like
  everything the agent reports; no database.
- `DELETE /content/:game/:version`: deletes one copy. The path is built by
  `StoreLayout::content_dir` from validated ids, never taken from the request.
- Download progress: the install task already owns its staging directory
  (`data_root/staging/<task>`, `content.rs`). Its size, read every few seconds,
  is bytes landed so far; two readings give the rate. `InstallState::Running`
  gains `bytes` and, when known, `total_bytes`.
- `DELETE /content/:game/install` (cancel): stops the task's container,
  discards the staging directory, records `Failed { error: "cancelled" }`.

**Rules.**
- **A copy a container mounts is never deleted, running or stopped.** Servers
  bind-mount their game's files read-only, which is why existing content is
  never replaced (`content.rs:888`), and a stopped server restarts from the
  same mount. Delete
  is refused with the list of servers using it; remove those first.
- **No delete while that game is downloading.** The install would rename its
  staging directory into a tree that was just removed.
- **Progress is measured on disk, never parsed from steamcmd's output.**
  `install.rs` already refuses that: a progress bar driven by a tool's console
  is a promise that breaks the next time the tool is updated.
- **A total is shown only when it is known**: the archive download's
  `Content-Length` (not read today), or the size the same version had the last time this node
  installed it. Otherwise the page shows bytes and rate and no percentage. An
  invented total is a progress bar that lies.
- **A cancelled or failed download installs nothing**, as a failed digest or a
  non-zero steamcmd exit already does.
- Behind the API token like every other route that deletes.

**Decisions.**
- Whether **Delete** offers to stop and remove the servers using a copy in the
  same step, or only lists them. Listing is safer; one step is quicker.
- Where a known total comes from for steamcmd games. A pack field such as
  `approx_bytes` would be data, not code, and harmless if wrong, but it is a
  number someone has to keep current.

**More for this page.**
- **Unused copies first.** A copy that `agent.toml` no longer names and no
  container mounts is marked *unused*, with a one-click delete. That is the
  928 MB above.
- **Update a game.** Fetch the newest build into a new version directory,
  point new servers at it, and leave the old one until nothing mounts it,
  because existing content is never replaced in place. Then the old copy shows
  up as unused.
- **Verify files.** Run steamcmd's `validate` into a new copy and report
  whether anything differed, for a server that crashes on a corrupt file.
- **Room before downloading.** With a known total, refuse or warn before
  starting a download that will not fit, instead of the 10 GB rule of thumb the
  page uses now.
- **Remove a pack.** Delete an imported pack's `.toml` from the Games page.
  The built-in Sven Co-op pack cannot be removed (`PackTrust::BuiltIn`), and a
  pack with servers still running is refused, like a copy in use.
- **One download at a time, queued.** Two Source downloads at once each run
  slower than one after the other, and a queue shows what is next.

**Size.** M. The routes are small; the care is in the rules above, and each
needs a test against a real daemon: delete refused while a stopped container
mounts the copy, cancel leaves nothing in `content/`, and a progress total that
is never invented.

**Build plan** (for whoever builds it; line numbers as of v0.2.35, `3a8abdd`).
Build the base first, then each addition as its own commit:

1. **List installed copies.** `crates/platform-agent/src/agent.rs`: an
   `Agent::installed()` walking `data_root/content/<game>/<version>` (sizes
   with the `dir_size` helper already there for orphans), marking each copy
   `configured` when `self.config().games[game].content_version` names it,
   and listing the containers that mount it. `list_managed` has no mounts, so
   inspect each managed container's `Mounts` once per call, or carry the
   content version in a label at create (`docker.rs` `create_and_start`, near
   `PORTS_LABEL`) and fall back to inspect for older containers. Route
   `GET /content` in `api.rs` (routes start near `api.rs:107`; today there is
   only `/content/:game`, `api.rs:123`).
2. **Delete a copy.** `DELETE /content/:game/:version` →
   `Agent::delete_content`. Build the path with `StoreLayout::content_dir`
   (`store.rs:193`) from validated ids. Refuse when any managed container
   mounts it (running **or** stopped) and when `Installs::is_running(game)`
   (`install.rs:86`). Return 409 with the server ids, so the page can list
   them.
3. **Progress.** The staging directory is created per run with a random name
   (`content.rs:386`, `staging_root`), so the install task does not know it
   today. Have `Provisioner::ensure` (`content.rs:199`) report its staging path
   to `Installs` when it creates one (a callback or a shared slot), and have
   `install_status` (`api.rs:746`) add `bytes` = `dir_size(staging)` to
   `InstallState::Running` (`install.rs:46`). Keep `since_secs`; the page
   works out the rate from two readings, as it does for `/usage`. A
   `total_bytes` only when known (rules above).
4. **Cancel.** `DELETE /content/:game/install`. The task's container is named
   `gpp-content-{app_id}-{task}-{attempt}` (`content.rs:333`) and carries
   `TASK_LABEL` (`docker.rs:49`). Record the name in `Installs` when the run
   starts, stop and remove it, discard staging, and finish the task as
   `Failed { error: "cancelled" }`. `ensure` must then not rename a
   half-finished tree: check it returns the error rather than the rename
   (`content.rs:250` is the archive path's rename).
5. **The page.** `crates/platform-agent/webui`: a fourth tab, `installed`
   (`app.js:1817` `TABS`, `TITLES`; `index.html:71` for the nav button,
   `index.html:129` for a panel). Rows from `GET /content`, plus one row per
   game whose `/content/:game` state is `running`. Poll every 2 s while the
   tab is open, as `/usage` does. Delete asks first, with the size and,
   on 409, the servers that use the copy. Downloads show bytes, rate and
   elapsed time, a percentage only with a total, and Cancel.
6. **Unused copies.** `configured == false` and no container mounts it: mark
   it *unused* and put those rows first.
7. Then, each separately: update into a new version directory, verify,
   room-before-download, remove a pack, a queue.

**Tests to write** (Docker-gated ones skip without a daemon, like
`tests/docker_guard.rs`; build tiny images from busybox, never pull):
- delete refused while a **stopped** managed container mounts the copy, and
  allowed once it is removed;
- delete refused while that game's install is running;
- a hostile game or version id is refused (`../`, empty, too long), never
  turned into a path;
- cancel leaves nothing under `content/<game>/` and removes the task
  container; a later install still works;
- progress `bytes` grows during a fake install (a stand-in steamcmd image that
  writes a file slowly, like `gpp-test-steamcmd-ok` in `tests/content_provision.rs`),
  and `total_bytes` is absent when nothing declares one;
- every new route answers 401 without the token (add them to the list in
  `tests/api.rs`, `without_the_token_every_route_is_refused`);
- the page: `launcher/uicheck` is the launcher's harness, not the node's; the
  node page has none, so check it in headless Chromium against a mock API (the
  way PR #6 and #8 did) and say so in the PR.

**Done when:** `cargo test --workspace` and `cargo clippy --workspace
--all-targets` are clean, the Windows CI job passes (the agent builds there;
gate anything Unix-only with `#[cfg(unix)]`, as `disk_space` does), CLAUDE.md
records the rules a later change could break, `RELEASE.md` has an Unreleased
entry, and on the node the 928 MB `counter-strike-16/app90` copy shows as
unused and can be deleted from the page.

## 12. Node: update checks, storage-safe downloads, moving servers

**Status (2026-10-14): steps 1–4 are built** — checks, the notice,
snooze, the reserve, the queue (`updates.rs`, `CLAUDE.md` has the rules), per
game "download it" (`OnNewBuild`), and moving servers (`move_instance`: when
empty, a daily window with an opt-in forced move after an in-game warning, one
at a time, rollback, and **Move now** on the page). Not built from 12.4: the
one-off date. A stopped server is not moved until it runs again. The choices
are set from links and a dialog on the Installed row; the settings section is
step 5.
Where it departs from the text below, the code is right: the routes are their
own resource, `/updates` (a static segment under `/content/:game` would shadow
a version of that name for `DELETE`); and steamcmd nests `branches` inside
`depots`, measured. Step 5 is open.

Builds on item 11's **Update** button
(`Provisioner::update`, `Agent::update_content`, `active.rs`), which already
downloads a new build into a new copy beside the old one without touching
anything a server uses.

**What.** The node notices a new build by itself, downloads it only when the
disk can take it, and moves servers onto it with as little downtime as a
restart — never with a full disk, and never in the middle of a game unless the
operator said so.

**Why.** When Valve ships a game update, players' clients usually refuse a
server on the old build: a node nobody updates is a node nobody can join. And
an update is a full copy of a game's files, so on a small disk it is the one
thing most likely to fill it and take every running server down with it.

### 12.1 Checking for a new build

- Per game, on an interval the operator sets (a node-wide default and a
  per-game override). **Off until the operator sets one** (decided
  2026-10-13): nothing new runs on a node by surprise.
- Steam games only. The check asks steamcmd for the public branch's build
  (`+app_info_update 1 +app_info_print <app>`) in a short-lived task container,
  anonymously, like an install, and compares it with the copy's own manifest
  (`content::steam_build_id`, `steamapps/appmanifest_<app>.acf`). Up to date is
  decided from Steam's data, never from scraping steamcmd's progress output.
- An `archive` pack has no "newer": its digest pins the bytes, so a new version
  is a new pack. A `manual` game has no source to ask. Neither is checked.
- A newer build shows on the Installed page ("New build 12345 — you have
  12001") and as a mark on the Installed tab, and is carried in `GET /content`
  so an index or a launcher could show it later.
- **Snooze**: hide the notice for a time, or until a build newer than this one.

### 12.2 Storage: never fill the node

An update first copies the old copy (item 11: never in place, never hard
links), so it needs that copy's size plus whatever the new build changes. The
copy's size is already known (`InstalledCopy.bytes`).

- **A reserve that nothing downloads into.** Configurable; default **the
  larger of 10% of the disk and 5 GB** (decided 2026-10-13). `disk_space` already reads free
  and total for the data root.
- **Before a download starts:** it starts only if, after it, free space would
  still be above the reserve, counting the space already promised to downloads
  running now. Needed is estimated as the old copy's size plus an allowance for
  the delta (10% of the copy, say). If it does not fit, it does not start, and
  the notice says how much it needs, how much is free above the reserve, and
  which **Unused** copies would free enough — with their Delete buttons. **An
  automatic update never deletes anything to make room.**
- **While it runs:** the download is already measured every 2 s
  (`InstallProgress::bytes`). Free space is checked on the same tick; a
  download that would cross the reserve is cancelled through the existing
  cancel path (nothing left behind, item 11's rules) and says "stopped: the disk
  reached the reserve". A running server never sees a full disk because of an
  update.
- **A fresh install** has no known size, so it gets the reserve check only
  (item 11's open "room before download"). A pack could later declare an
  approximate size; it would be a hint, never trusted — the check while running
  is what actually protects the disk.
- **One download at a time per node**, queued: a waiting update takes no space
  until it runs, and the reserve arithmetic stays honest. This is item 11's
  queue.
- **Not doing: updating an unused copy in place to save space.** An interrupted
  in-place update leaves a copy that is neither build, which is exactly what
  "existing content is never replaced" (`CLAUDE.md`) exists to prevent.

### 12.3 What happens when a build is found

Per game:
- **Notify** (default): the operator presses Update.
- **Download automatically**: the new copy is downloaded in the background,
  through 12.2's gate. No downtime — servers keep running on the old copy.

### 12.4 Moving servers onto the new copy

This is the only step with downtime. A server keeps the copy it was created on,
and the copy is a bind mount fixed at container create, so moving it means
recreating its container on the new copy. Per game:
- **When empty**: recreate as soon as the server has 0 players. A server whose
  player count is `None` (no query protocol, or it did not answer) is **never**
  treated as empty — the same rule that keeps reaping from stopping a busy
  server (`hosting::record_for`, `exempt_from_reaping`).
- **At a set time**: a daily window (04:00–06:00) or a one-off date. Inside the
  window it still waits for empty, unless the operator allowed a forced move:
  **off by default, opt-in per game**, and it first warns the players in game
  over the console (`console.rs` words, never a command from a pack) (decided
  2026-10-13).
- **Never**: servers stay on the old copy until the operator restarts them;
  "remove the old version" then deletes it after the last one leaves (item 11's
  `sweep_old_copies`).

How it keeps downtime to the restart itself:
- The download has finished before anything is stopped.
- Servers move **one at a time**, not all at once.
- **Same instance id, so the same name, ports and mesh address.** A server's
  identity is `instances/<id>/mesh.identity` (`mesh::identity_path`), so a
  recreate that keeps the instance id and its folder keeps its destination: it
  stays the same row in every player's list and their bookmarks still work. The
  move must not go through anything that deletes the instance folder.
- **Rollback**: if the new copy's server does not come up (it exits, or the
  crash watch would call it a crash), it is recreated on the old copy and the
  page says why. The old copy is not swept while a move of its servers is in
  flight.

### 12.5 Settings

A section on the Installed page: check interval, per-game Notify / Download,
move policy (when empty / window / never), the reserve. Stored node-side beside
`content-active.json`, **never in `agent.toml`**, which stays the operator's
file and is never rewritten (item 11). If `agent.toml` later gains keys for
these, the file the operator wrote wins, the same way `active.rs`'s `base` does.

### Rules this has to respect

- **A pack cannot name what runs.** The check and the download run the
  operator's `steamcmd_image` with an app id from the pack; no new pack field
  names an image, a command or a login.
- **Silence is not zero.** `players_now: None` never counts as empty.
- **No Docker restart policy, and every move re-bridges** — a recreated
  container goes through the agent's own create and mesh path, like
  `Agent::restart`, never left to Docker.
- **Nothing automatic deletes** — not to make room, not after a failed move.
  Deleting stays item 11's operator action and its old-version sweep.

### Build order

1. Check, notice, snooze (12.1).
2. Storage gate, reserve and the queue (12.2), applied to the existing Update
   button too, so manual updates get it first.
3. Automatic download (12.3).
4. Moving servers: when empty, window, one at a time, rollback (12.4).
5. Settings section (12.5).

Each step ships on its own.

**Decided 2026-10-13:** checks are off until the operator sets an interval; a
window may force a move with players on only as a per-game opt-in, after an
in-game warning; the reserve defaults to the larger of 10% of the disk and
5 GB.

**Tests to write** (Docker-gated, busybox stand-ins, like
`tests/installed_content.rs`):
- a check against a stand-in steamcmd reporting a newer build shows it; the
  same build shows nothing; an `archive` or `manual` game is never checked;
- an update that does not fit above the reserve never starts and leaves
  nothing; one that crosses it while running is cancelled and leaves nothing;
  nothing is deleted to make room;
- two queued updates never run at once;
- a server with `players_now: None` is never moved by "when empty";
- a moved server keeps its instance id, ports and mesh destination;
- a move whose new server fails is rolled back onto the old copy, and the old
  copy is not swept meanwhile.

---

## Rebrand

**Decided 2026-10-10: Lanthorn.** The icon (`launcher/src-tauri/icons/lanthorn.svg`)
is a green-lit lantern inside a double ring, its light reaching out as links
between mesh nodes, one breaking out of the ring — a nod to Reticulum's mark,
without its lettering. Kept original on purpose: the user asked for a "Green
Lantern" feel, and DC's emblem and lantern shapes are trademarks, so the
resemblance is the colour and the glow only. Done as listed below: product
name, binary `lanthorn`, asset names, both UIs, the Windows firewall rule
(old one deleted once, `LEGACY_RULE_NAMES`); kept: the Tauri identifier, the
settings directory and every `app_name`. The Arch `PKGBUILD` switches at its
next hand bump (see its header).

The original analysis follows.


"Mesh Game Servers" says what it does, which is why it is hard to remember and
impossible to search for. A name should be short, searchable, say something
about *playing together without a central server*, and not collide with an
existing game product.

### Candidates

None of these turned up as an existing game launcher or server browser in a
quick search on 2026-10-10. **A proper trademark and domain check is still
needed before choosing.**

| Name | Why it fits | Watch out for |
| --- | --- | --- |
| **Lanthorn** | Old spelling of *lantern*, with "LAN" inside it. Servers announce themselves like lights in the dark, and it works with no internet. | People may misspell it "Lantern". |
| **Reticle** | A crosshair — gaming — and a nod to Reticulum, which carries everything. | A common word, so harder to own in search; check existing apps. |
| **Meshcade** | Mesh plus arcade: unmistakable, easy to search. | Sounds more retro or arcade than shooter or racer. |
| **Hopscotch** | Distance in this app is measured in hops; playful. | Many unrelated products use the name. |
| **Campfire** | Friends gathering with no host in charge. | Heavily used (Basecamp, Niantic, others). |

My pick: **Lanthorn** — distinctive, searchable, and it carries both halves of
the product (LAN, and a light that announces itself).

### What a rename changes, and what it must not

**Must not change — these are wire contracts, and renaming them breaks every
deployed peer:**
- Pack `app_name` values and Reticulum aspects (`PLAN.md` §5;
  `profile::tests::destination_hash_matches_deployed_sven`).
- `svencoop-prns`, which stays its own product (`PLAN.md` §5).

**Changes, with a migration:**
- **Product name and window title** — `tauri.conf.json` `productName`, the two
  UIs, README, release notes, the portable README, the Arch `PKGBUILD` and its
  `.desktop` entry, and `release.yml` asset names (the download file names
  change; old links in chats stop resolving to the newest file).
- **Windows firewall rule names** — `lan_firewall::RULE_NAME` and
  `PORTABLE_RULE_NAME` are `Mesh Game Servers LAN rooms`. An installed launcher
  keeps its rule on purpose, so after a rename the old rule stays forever unless
  the new build deletes rules with the old name once. Do that, or keep the old
  rule name.
- **Settings location** — settings live under `gaming-platform-prns/`
  (`settings::default_settings_path`), which is not the product name and can
  stay. If it moves, read the old file once and copy it, or every player loses
  saved connections, ports and indexes.
- **Tauri `identifier`** (`org.idan2025.gamingplatformprns.launcher`) — decides
  where the webview keeps its data and is what an auto-updater keys on.
  **Changing it is the expensive part**; leaving it as is costs nothing a player
  sees. Recommendation: keep it.
- **Binary name** `mesh-game-servers` and the Windows install folder: a new
  installer installs beside the old one unless it uninstalls the old one first.
- **The GitHub repository** can be renamed; GitHub redirects the old URL.

**Do it once, before auto-update ships** — so the updater's first release
already has the final name, and nobody has to update across a rename.
