# Releasing

What ships, how it is built, and what has to be true before a tag. `PLAN.md` is
still the design authority; this file is only the mechanics.

## What ships

| Artifact | From | Who runs it |
| --- | --- | --- |
| `Lanthorn` (launcher) | `launcher/src-tauri` | a player, or anyone hosting from a desktop |
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

## v0.2.46

### Minecraft servers, and kick and ban for every game

- A node can host **Minecraft: Java Edition** (vanilla, Paper, Fabric or
  Forge, a version per server, 1.18.1 and newer) and **Bedrock Edition**.
  Each Java build comes from its publisher's own site and is checked against
  the publisher's digest; the owner accepts the EULA before a server is
  made; memory and CPU are checked against the machine first, and each
  server gets a hard memory cap. A world can be imported, and the old one is
  backed up first. Bedrock servers report their player count.
- Every game with a console has **kick and ban** on its node page.

### Invites as a QR code

- Next to Invite, the launcher shows the invite as a QR code, drawn on your
  own computer, so a phone joins by scanning it.

### Many machines, one place to start servers

For people running servers on more than one machine (`HOSTING.md`):

- **Workers.** `scripts/setup-worker.sh` turns a machine into a worker for an
  index in one command. The index places each new server on the worker with
  the most room, counting memory, disk and the operator's priority; a worker
  that does not answer is never treated as empty.
- **Your node's page can place servers across every worker**, through a
  `[controller]` section: create, stop, start, remove, logs and backups, with
  the create form's choices (bots, crash restart, a Minecraft build and its
  settings) carried along. Several open pages share one list from the index.
- **Moving a server to another worker** keeps it running while its files are
  copied, then switches over and keeps its address, so invite links keep
  working. A real Sven Co-op server with 300 MiB of files moved between two
  machines in 22 seconds, and players lost it for 1.4 seconds. A move that
  fails never loses the server.
- **Regions and classes.** A worker can carry a region (`eu`), and a create
  can ask for one; a worker can carry a class that only some creates use.
- **More than one hub.** An index and a worker can each reach the mesh through
  several hubs and keep working while any one is up.
- **Quotas per account**, and stopped servers removed after a number of days,
  with a countdown shown, if the operator turns it on.
- **The index keeps its address** across restarts (`--identity FILE`), no
  longer shows up as a server in the list, and its web address now shows a
  page saying what it is and what it hosts.

### Servers stop at once

- A game server now stops in a quarter of a second instead of ten. Sven
  Co-op and other servers that ignore the stop signal were killed after a
  ten-second wait on every stop, restart and move; they now run under a small
  init that passes the signal on. Sven Co-op also runs without Valve's
  restart wrapper, which hid crashes from the node.
- A removed server's address key is deleted with it (its files stay until you
  delete them).

## v0.2.45

### A Linux-hosted LAN race no longer ends as it starts

- A race hosted on Linux sometimes fell over the moment it started for a
  player on Windows, while the same race hosted on Windows always worked.
  Under Wine a game opens its race sockets when the race starts, and
  Lanthorn noticed them only at its next look, up to two seconds later.
  A race packet that arrived in between was refused by the host computer,
  and on Windows that refusal fails the game's next receive, which Need for
  Speed takes for the host leaving. That refusal is no longer passed on to
  the room, and Lanthorn looks again straight away, so the game's next
  packet already reaches it. Only a refusal for one of the game's own ports
  is held back; a firewall's refusal still reaches the other player.
  Contributed by danakoz.

## v0.2.44

### The room check: a VPN that takes the route, and a game not started yet

- The room check now says when a VPN, a proxy running as a tunnel (TUN
  mode) or a virtual network sends this computer's traffic to its local
  network, or to the room, out of another address. That is what made a
  race hang while the lobby worked: a zero-trust client routed the Wi-Fi's
  own subnet through its tunnel, so the game talked to the other players
  from an address they did not expect. The warning names the network, the
  address and (on Linux) the interface the traffic leaves from, and shows
  beside a room that otherwise works. A proxy set only in a program's or
  the system's settings does not touch game traffic and is not looked for.
- A TCP port that gets no answer is tried three times before the check
  calls it silent, as the UDP probes already were. One lost packet on the
  mesh looked exactly like a firewall, and showed a healthy room as "did
  not answer TCP 3282, 3284, 3285, 9900".
- When no game is hosted yet on another computer (its ports refuse the
  connection), the check says so as a next step, not a fault: the room
  reached that computer and it answered; start the game there and host a
  LAN game, then check again.

### Windows Firewall: looked after, not left to the player

- The Windows Firewall rule that lets a room in (`Lanthorn LAN rooms`) is
  now checked, not assumed. While a room is up, Lanthorn's administrator
  helper looks at it every 20 seconds and puts it back if it was removed,
  switched off or edited by a clean-up tool, an update or a security suite.
  A rule that is fine is never touched, so there is no gap.
- The rule names no game and no port, only the room's addresses, so a game
  added later needs nothing more.
- A problem Lanthorn cannot fix by itself is said in the room pane as soon
  as the room starts, before any game fails, in plain steps and with no
  command to paste: Windows set to "Block all incoming connections", or a
  policy that ignores rules made on the computer.

## v0.2.43

### LAN room traffic leaves through the room

- **Windows:** a game's broadcasts now go through the room even on a
  computer with Hamachi, Radmin VPN or ZeroTier installed. Before, the room
  tied with those adapters and lost, and the room check said "a broadcast to
  255.255.255.255 did not go through the room's adapter". Windows already
  has a broadcast route on every adapter, so the helper's own route was
  never added; it now takes over the existing one and keeps it while the
  room is up. Only that broadcast moves to the room: casting, printer and
  NAS discovery on the home network keep working.
- **Linux:** some broadcasts never look at routes at all: one sent from the
  Wi-Fi's address, one sent to the Wi-Fi's own network, and anything from a
  game Wine ties to the Wi-Fi. Lanthorn now copies those into the room with
  traffic-control rules that exist only while the room is up and are removed
  when it ends, even if starting it fails partway.
- **Both:** a game that works out a `/24` broadcast address inside the
  room's `/16`, or sends to its own network's broadcast address, now
  reaches the room, and replies to a search sent from another address find
  their way back. A member never sends a packet that is not from its own
  address, and system discovery (mDNS, SSDP, Steam) from the home network is
  never passed on to the room, whatever a game's pack allows.
- **Games with unpredictable ports** (both Need for Speed packs) no longer
  carry this machine's own name lookups and discovery broadcasts (NetBIOS,
  SSDP, mDNS) to the other players. A pack that names such a port itself
  still gets it.

## v0.2.42

### A disk limit per server

- `writable_limit_bytes` in a game's `[games.<id>]` caps what one server may
  keep in its own files. A server over it is stopped and will not start
  until it is back under; nothing is deleted.

### Game servers kept apart on a node

- Every game server now runs on its own Docker network, `gpp-games`, where it
  cannot reach any other server or container on the machine. Each also runs
  without Linux capabilities, can never gain one, and has a ceiling of 512
  processes. Servers created before this move onto it when they are next
  recreated, and keep working until then. Keeping game servers off your home
  network is still the host firewall's job (`HOSTING.md`).

### Placement by memory, and node priority

- A node now reports its memory and CPUs, and an index places a new server
  on the node with the most free memory, after the operator's `priority`
  (`[[nodes]] priority`, higher first): a rented machine can take servers
  first and a home machine only the overflow. A node with under 1 GiB free
  is treated as full.

### Index config without a restart

- An index now picks up edits to its hosting file within about ten seconds:
  add or remove a node, a game or a quota by saving the file. A save that
  does not parse is logged and changes nothing.

### Stop and start a hosted server

- Someone running a server through an index can now stop it and start it
  again (`POST /instances/:id/stop`, `POST /instances/:id/start`), with the
  same files, ports and address. Start never restarts a server that is
  already running. A node needs this version of the agent for start over
  the mesh; an older one is named as needing an update.

### Controllers on Linux

- Big screen mode now works with a gamepad on Linux even where the system's
  web view cannot see one (WebKitGTK without libmanette, as on some Steam
  Deck and distribution builds): Lanthorn reads the controller itself. It
  only reads it; games and Steam still get every press.

## v0.2.41

### Quiet over a fullscreen game on Linux

- Lanthorn's desktop notifications now wait while a fullscreen game has
  the screen on Linux too (X11, and games under XWayland such as Proton's),
  as they already did on Windows.

### Getting a game

- A game that is not on this computer has **Install with Steam** in its
  server's pane and on the Games page (the store page without Steam), and
  while Steam downloads it the launcher shows how far, in Steam's own
  numbers. A game Steam is still downloading is no longer counted as
  installed.
- Lanthorn Host says, on the game's card, exactly where a hand-installed
  game's files go and which folders the copy needs, with **Recheck**,
  before a Start fails. An installed copy missing a folder a server needs
  says so too, and a game this node may not download says why.

### The official pack repository

- `packs/` is published to <https://idan2025.github.io/gaming-platform-prns/>
  with a website, every pack and the list signed by the project key, after
  the same review a pull request gets; weekly, so signatures never lapse.
- Lanthorn and Lanthorn Host read it by default and take new and updated
  packs by themselves, every six hours and with **Check now**; either can
  turn it off. A node never changes a pack while a server of that game
  exists.
- `game-bridge repo mirror` makes an exact, one-way copy of a signed
  repository (the official one by default), and a folder can be added as a
  repository in Lanthorn and Lanthorn Host.
- `game-bridge repo seed` serves a mirror over Reticulum and keeps it
  current. Lanthorn lists repositories it hears on the mesh, and both the
  launcher and Lanthorn Host take the official packs from a seed by
  themselves when GitHub cannot be reached, held to the project key.
- Adding a game: `CONTRIBUTING.md` walks through a pack pull request, the
  review posts its result on the pull request, and there is a form to
  request a game.
- Lanthorn Host can share the official packs on the mesh itself
  (**Share on the mesh** beside the official packs): it keeps an exact copy
  in `mirrors/official` and serves it through the node's own Reticulum
  connection, so nodes and launchers without internet still get the packs.
  Off by default.
- Fixed: `game-bridge repo build` left its list in a hidden partial file.
- **The project key (`7a656c1da3347bfcfe6b3981f71099c8`) is a release
  secret like the updater key**: `PACK_SIGNING_IDENTITY` in the repository
  secrets, and an offline copy. Losing it means re-keying every launcher.

### Chat on servers

- Players who join a server through Lanthorn can talk to each other and to
  its host, before and while they play, in a chat under the server list.
  Only servers that offer chat are asked; older servers and launchers are
  unaffected.
- A server hosted from the launcher offers chat by default (not when it
  has a password); the host can turn it off and mute someone until it
  stops. On a node, chat is off until the operator turns it on for a server
  under **Chat…** in Lanthorn Host.

### Game pack review

- `game-bridge review <pack.toml> [--against DIR]` names what a person must
  read in a pack before it is shared, and refuses what no honest pack needs:
  launch arguments that run console commands or point the game at a fixed
  address, LAN ports that are remote-access services, invisible characters,
  and names or `app_name`s copying another game. CI runs it on every pull
  request that changes a pack.
- `repo build` will not list a refused pack, and the launcher and Lanthorn
  Host refuse to install one.

### Lanthorn Host: each game says whose word its pack is

- A game card on the Games page shows its pack's tier under this node's
  trust settings (signed, signed by an unknown key, unsigned local), with the
  signer on hover.

## v0.2.40

### Lanthorn: chat and names in LAN rooms

- A LAN room has a **chat**, and members are listed by **name** beside their
  address. Set your name in Settings → You.
- A room whose host runs an older Lanthorn still works; it says chat is
  missing until the host updates.

### Lanthorn: host a server from the launcher

- **Host a server** on the Games page puts a game server you already run on
  this computer on the mesh — a dedicated server or a game's own "host a
  game" — with a name, max players and port, and gives you an invite link.
  Closing the window keeps it up in the tray. Lanthorn never starts the game
  server itself.

### Lanthorn: a welcome screen

- A fresh install opens on one screen: your name, and whether to look on
  this network, use a hub address, or both. Start connects with that; Skip
  leaves it for the Connections page. Shown once.

### Lanthorn Host: add a game from a repository

- **Add a game…** on the Games page adds a pack from a pack repository (the
  same kind the launcher reads) or from a pack's own address. The node
  fetches it, refuses bytes the repository did not list or a signature that
  does not hold, and then applies its own `[pack_trust]` policy as before.
- A pack's id may no longer contain a path: it names the pack's file.
- Replacing a pack no longer leaves the old one's signature beside it, which
  made the new pack fail to load.

### Lanthorn: quiet over a fullscreen game

- On Windows, desktop notifications wait while a game or presentation is
  fullscreen. Linux desktops and macOS already hold them back themselves.

### Lanthorn: a Games page, and packs from a repository

- A new **Games** page lists the games Lanthorn knows, with whether each pack
  is signed.
- **Add a pack repository** by its address and install games from it with
  one click; an update shows when the author publishes one. A pack whose
  bytes are not what the repository lists, or whose signature does not hold,
  is refused. Lanthorn ships with no repository.
- For pack authors: `game-bridge repo build <dir>` writes the `index.json`
  that makes a folder of packs a repository; with `--identity` it signs the
  list too, and Lanthorn then holds that repository to the key it was added
  with.

## v0.2.39

### Lanthorn: favorites, installed games, a tray, slow-link warnings and big screen

- **Star a server** from its right-click menu or the detail pane. Starred
  servers that are up lead the list; one that is off has its own section with
  when it was last heard, and you are told when it comes up.
- Rows say **Installed** for a game on this computer. Steam is also found in
  Flatpak and Snap installs, and Locate opens the system's file picker.
- **A tray icon** while you are in a LAN room or joined to a server: closing
  the window keeps the launcher running there. Otherwise closing quits, as
  before. **Desktop notifications** (a favorite comes up, someone joins your
  room, a room check fails, an update is ready) can be turned off in
  Settings.
- **Slow link**: a row is marked when the game needs a faster link than the
  server was heard over, and the first press on Join says why and becomes
  **Join anyway**. A warning, never a block.
- **Big screen mode** for a TV or a Steam Deck, in Settings, and a controller
  drives the list: up and down move, A opens, X joins, Y opens the menu, B
  goes back, the shoulder buttons switch views, Start switches big screen.

### Lanthorn Host

- The node page works on a phone: the tabs get their own row, and the toolbar
  and server rows fit at 320 px.
- A server's **Put back** is now **Restore**.
- A stop returns only once Docker's own list says the server stopped, so a
  Remove right after a Stop is no longer refused as "stop the server first".

### Lanthorn Host: backups on a schedule, as files, and after a server is gone

- **Back up automatically** in a server's Backups: every 6 or 12 hours, daily
  or weekly, keeping the newest few. Only while the server runs, and only the
  backups the schedule made are ever deleted; ones made with Back up now stay.
- **Download** saves a backup as a file; **Import a file…** brings one in, from
  this node or another, to restore like any other. A file that is not a
  server backup is refused.
- The Servers page says when removed servers left backups behind, and lists
  them to download or delete.

## v0.2.38

### Lanthorn Host: new builds that install themselves, and servers that move onto them

- A game can be set to **download new builds automatically**. It uses the
  same queue and disk reserve as Update, and keeps the old copy: nothing
  automatic ever deletes. Remind me tomorrow and Skip build hold it back.
- **Moving servers onto the new copy** is per game: never (the default), when
  a server is empty, inside a daily window (optionally with players on, after
  a warning in game), or once at a date you set. A server keeps its address,
  name and port. One moves at a time; one that does not come up on the new
  copy is put back on the old one.
- Each game's update settings are on the Installed page: its own check
  interval or none, what to do with a new build, and when to move servers.

### Lanthorn Host: managing a node's games and servers

- **Verify files** checks a game's files against Valve's on a copy, never the
  one servers use, and lists every file it put back. Servers switch to the
  repaired copy only when you press **Use this copy**, because a check also
  undoes changes made on purpose (such as YaPB bots).
- **Remove a pack** from the Games page. Refused while any server of that game
  exists or it is downloading; the game's files stay.
- **Log and console**: type a command at a running server, one line at a time,
  from the node's own page only.
- **Daily restart…**: a window per server. It restarts once a day when nobody
  is on it or, if you allow it, after warning the players.
- **Backups…**: back up a server's own files (saves, settings, logs) and put
  them back while it is stopped. Backups are kept after a server is removed.
- The Installed table no longer overflows at medium window widths.

### Docs

- The Mac app is marked experimental (browse and join, no LAN rooms), and how
  to open an unsigned app on macOS 15 is written down.

## v0.2.37

### Lanthorn Host: knowing when a game has a new build, and never filling the disk

- The Installed page can check Steam for each game's newest build, on a
  schedule you pick (every 6 or 12 hours, daily or weekly) or with **Check
  now**. Checks are off until you choose. A game with a new build says so on
  its row and on the Installed tab; **Remind me tomorrow** and **Skip build**
  hide the notice, and a newer build shows again.
- Downloads keep space free for running servers: by default a tenth of the
  disk, at least 5 GB, and you can set your own. A download or update that
  would go below it does not start, and says which unused copies would make
  room; one that gets there while running is stopped and leaves nothing
  behind. Nothing is ever deleted for you.
- One download at a time: the rest wait in line as **Queued**, and can be
  cancelled while they wait.

## v0.2.36

### Lanthorn Host: an Installed page for game files

- **Installed** lists every copy of a game's files on the node: its version,
  size, when it was installed, and which servers use it, stopped ones
  included. A copy that nothing uses and the node's config does not name is
  marked **Unused** and listed first, with **Delete unused…** above the list.
- **Delete…** frees a copy's space after asking. A copy any server uses,
  running or stopped, is refused and the servers are named; so is any copy of
  a game that is downloading. The game stays on the Games page and can be
  downloaded again.
- A download shows here while it runs: bytes on the node's disk so far, the
  rate and how long it has taken, and **Cancel**, which discards what it
  fetched. There is no percentage: no download knows its total yet, and the
  page will not invent one.
- **Update…** on a Steam game's copy downloads the newest build into a new
  copy beside it; only what changed is downloaded, and what was added to the
  copy (such as YaPB bots) comes along. New servers use the new copy; servers
  that exist keep theirs. Tick **Remove the old version when the update is
  done** to delete the old copy too: at once if nothing uses it, otherwise as
  soon as the last server using it is removed. A game with nothing newer says
  it is up to date and leaves no extra copy.
- The Games page's download message shows the bytes so far too.
- Pictures on the Installed page stay put while it refreshes, rather than
  blinking every two seconds. (Rebuilt into this release's files on
  2026-10-13; the first v0.2.36 build blinked.)
- Several error messages carried runs of spaces from broken line
  continuations; they read as one sentence again.

## v0.2.35

### Lanthorn Host: who is playing, why a server stopped, and restarts after a crash

- **Players…** in a server's menu (or a click on its player count) lists who
  is on it, with score and time connected, as the game reports it. Bots are
  listed too.
- A server that crashed says so: its status reads **Crashed**, and the map
  column says how, such as `segfault (139)` or `out of memory`. A server you
  stopped still reads Stopped.
- **Restart if it crashes**, on the start form or in a server's menu: the node
  starts it again after a crash, up to 5 times an hour, and never after a Stop.
- Leftover folders from servers deleted earlier are counted above the list,
  with a **Clean up…** button that asks first. Folders touched in the last ten
  minutes are left alone.

### The launcher warns about slow links

A server heard over LoRa or another radio link says so in its details: a game
that needs a fast link is warned about, and a low-rate one is told joining will
be slow. It is a warning, never a block.

## v0.2.34

### Lanthorn Host shows a server's player cap

Players read `14/16` instead of `14`, both in the list and in the live usage
strip, with the cap the game itself reports. A server whose players are bots
this node asked for says so, and a full one says "full".

## v0.2.33

### Lanthorn Host shows what each server is using, live

Each running server on the node's page now has a strip with its players, CPU,
memory, network and disk, updated every 2 seconds with small graphs of the
last two minutes, and the top of the page shows the whole node's total. CPU
is counted like `docker stats`: 100% is one full core. Memory turns amber when
a server nears its limit, before Docker stops it for running out.

### A server's log, an invite link and free disk space, on the node's page

- **Show log** in a server's menu opens the last 200, 1000 or 2000 lines it
  wrote, following new lines while it is open. It is read-only.
- **Copy invite link** gives the same `lanthorn://join/…` link the launcher
  makes, so you can share a server straight from the node.
- The sidebar shows the free disk space where game files are kept, and the
  page asks before a download when less than 10 GB is left.

### The launcher

- The game picture in a server's details no longer blinks every two seconds.
- Under the numbers, the details now say what the round trip means for
  playing: close by, a little delay, expect lag, or very far.

## v0.2.32

The first release a v0.2.31 launcher can update to by itself: on the Windows
installer, the macOS app and the AppImage, Update now in the banner is all it
takes.

### Connections no longer say "down" when they are working

The Connections page drew itself once, while a relay was still connecting, and
never looked again, so a working relay kept showing as down. It now updates as
connections come and go, and a relay that is connected but slow counts as up.

LAN auto-discovery with nobody else on your network now reads **searching**
instead of down, and is not counted against you in "x of y connections up".
That made a perfectly good setup look broken. A neighbour it finds shows as
"LAN auto-discovery · peer", connections are named by their address instead
of `TcpClient` / `AutoWifi`, and rows no longer jump around.

### Settings that stick

- A relay you save now replaces the one you had. Before, a relay added later
  was used until the next restart and then the old one came back.
- LAN auto-discovery can be turned off, and stays off after a restart.
- The saved-server count on Connections is up to date when you open it.

### Buttons that do not miss clicks

Parts of the window redrew every two seconds, so a click that happened to
span a redraw was lost: Leave room most noticeably. They now redraw only when
something changed, and never while you are pressing a button in them.

## v0.2.31

### Mesh Game Servers is now Lanthorn

New name, new icon: a lantern whose light reaches out across a mesh, inside
the ring of Reticulum, the network everything here runs on. The node's web page
is now Lanthorn Host. Nothing about how it works changes: your servers, rooms,
saved connections and settings carry over, and servers running older versions
are found exactly as before.

Because the app's name changed, its downloads are now named `Lanthorn_…`, and
installing it does not replace the old app:
- **Windows:** after installing Lanthorn, uninstall "Mesh Game Servers" from
  Settings → Apps. Your settings are kept. The old firewall rule for LAN rooms
  is replaced by Lanthorn's the first time you open or join a room.
- **Linux `.deb` / `.rpm`:** remove the `mesh-game-servers` package.
  AppImage and portable: delete the old file or folder.


### Lanthorn updates itself

Lanthorn now looks for a newer version when it starts, and Settings has a
"Check now" button. When one is out a banner says so: Update now downloads it,
checks it is really from us (every update is signed), installs it and
restarts. The Windows installer, the macOS app and the AppImage update this
way; the portable builds, `.deb`, `.rpm` and the Arch package say a new version
is out and open the download page instead. Without internet nothing is checked
and nothing changes. You can turn the check off in Settings.

This is the first version that can update itself, so getting here is one last
manual download.

### Invite links

Right-click a server or room, or press Invite in its panel, to copy a link such
as `lanthorn://join/…`. Anyone with Lanthorn who clicks it in Discord,
WhatsApp or anywhere else gets that server or room opened in their launcher,
ready to join — even before it has shown up in their list. A link never joins
anything by itself. You can also paste a link into Join by invite, or anywhere
in the window. (Portable copies do not register the link type with the system;
paste the link instead.)

### A new look for the launcher and the node's web page

Both were redesigned. The launcher has a sidebar (Servers, LAN rooms,
Connections, Indexes), game artwork on every row, and a detail panel with the
game's picture, a big Join button and the technical details folded away under
Advanced. The node's web page has the same look, game cards with artwork, and
a dialog for starting a server.

Game pictures come from Steam's image server, or for games that are not on
Steam (both Need for Speed games) their box art from Wikipedia's image server.
Offline, a lettered tile shows instead. Nothing else about you or your servers
is sent there. A pack can name its own picture with `art = "https://…"`, but
only on those two image servers.

### The list shows only what is online

A server is listed while it is announcing or answers the mesh, and drops off
three minutes after it goes quiet. Servers you heard before are no longer
listed from memory: the launcher asks the mesh about them once a minute, and
any that answer come back by themselves. The server or room you are in stays
listed while you are in it.

### Right-click menus, Remove that works, and Announce

Right-click a server for Join, View details, Trace path, Copy address and
Remove from list. The browser's own menu, whose Reload broke the launcher until
it was restarted, is gone, and F5 and Ctrl+R do nothing. Remove now takes the
server off the list at once and keeps it off until it announces again; before,
it could wait for a slow server check and then come straight back.

The new Announce button announces your LAN room if you host one and asks the
mesh where your saved servers are. Its menu also traces the path to the
selected server. On the node's web page, Announce sends every server's
announce at once, and each server's menu can announce just that one.

## v0.2.30

### A game that listens on the wrong address can be joined

Some games listen on the computer's Wi-Fi or Ethernet address instead of the
room's. Under Wine or Proton this is common, and a native game with a fixed
address set does the same, on Linux or Windows. Need for Speed: Underground 2
under Proton did it: the race showed up in the other player's list, and every
join was refused, firewalls on or off. The room now finds where the game is
listening and passes its traffic through. There is nothing to click and
nothing to set up, on either computer, whichever one hosts. Only the game's
own ports are passed, and only to programs on this computer; nothing else on
your network becomes reachable from the room. The room check says when it is
doing this.

On Windows this needs one network setting ("weak host send") on the adapters
that reach the internet while a room is open, so the game's answers can leave
through the room. The room turns it on when it starts and off when it ends,
and only where it was off. It is never saved: if the launcher is killed
instead of closed, it stays on until the next restart.

Steam's own LAN discovery (UDP 27036), which Proton runs beside every game,
is no longer listed as a port the game needs. If you allowed it for a game
before, it is dropped.

## v0.2.29

### When a LAN game shows up but will not let you join

Each game's LAN pack lists the network ports the game uses, and the room lets
in only those. Some of those lists come from other people's packet captures
and can be incomplete; Need for Speed: Underground 2's is one of them. When a
game used a port its pack did not list, the room blocked it without a word:
the game showed up, joining hung, and turning the firewall off did not help.

Now the room pane deals with it:

- **It names the port.** "This game used ports its pack does not list", with
  the ports it probably needs (for example *UDP 3660*) and what was blocked.
  The room check shows the same.
- **Allow on this computer** lets those ports through for that game, from
  then on, and opens this computer's firewall to the room too if it is not
  open yet: Windows Firewall, ufw or firewalld, whichever this computer runs.
  **Undo** takes it back.
- **Still can't join?** If nothing is named and joining still hangs, one
  switch lets the game use every port above 1024 on that computer. Do it on
  both computers. **Turn off** takes it back.
- **Copy report** puts what was blocked on the clipboard. Please send it in,
  so the game's pack gets fixed for everyone.

Only room members can reach a port opened this way, and system ports (below
1024: file sharing, remote login and the like) are never offered — nor are
the remote-access and database services above it (Remote Desktop, VNC,
PowerShell remoting, MySQL, PostgreSQL and the like). Your
system's own network chatter is left out of the list, so what is listed is
the game's.

### The room check says when nothing is listening

If a game is hosted but the room check finds nothing listening on its port
on the host's room address, the check now says so instead of passing quietly.
Some games, for example under Wine or with a fixed address set, listen only
on the computer's other addresses.

### One room adapter on Windows, not "gbl0 2", "gbl0 3"…

Every room created a brand-new network adapter as far as Windows was
concerned, numbered one higher each time, each with a new "Network" profile.
The room's adapter now has the same identity every time, so Windows sees one
adapter, room after room. Leftover ones from earlier versions can be removed
in Device Manager (View → Show hidden devices → Network adapters), but they
do no harm.

## v0.2.28

### LAN rooms open the firewall themselves

The first real Need for Speed: Underground 2 race in a room hung on joining.
The other player saw the race in the game's LAN list, picked it, and waited
until it timed out, because the host's firewall dropped the connection.
CachyOS turns ufw on with "deny incoming" by default, and Windows treats a
new network adapter as a Public network. Seeing a game needs only your own
side open; joining it needs the host's. So a room could look fine and still
not work.

Now the room opens the firewall itself, at the moment it already asks for
permission:

- **Windows**: the administrator prompt every room already shows also adds
  one firewall rule, "Mesh Game Servers LAN rooms". It lets in room addresses
  only, and it covers every room and every game from then on.
- **Linux**: the password prompt a room asks for also opens ufw or firewalld
  to the room adapter, `gbl0`. Granting the helper permission once now does
  the same, under the same single prompt. If you granted the permission with
  an older version, the room pane offers **Fix it**, which asks for your
  password once.
- **Portable** downloads still leave nothing behind: their rule is removed
  when the room ends. A ufw rule that was already there is never removed,
  since you may have added it yourself.

Behind the firewall the launcher still lets in only the game's own ports,
so a room member can reach nothing else on your computer.

### What the room cannot open, it names

- **The room checks itself** a few seconds after someone joins. The pane says
  *Ready to play*, or what is wrong in plain words.
- **"Someone tried to join a game on this computer, but this computer's
  firewall blocked it"**, the moment it happens, with a button to fix it.
- **Windows blocking the game itself**, which happens after someone once
  clicked "Cancel" on that game's firewall question. A block beats any allow,
  so the pane names the program and offers **Unblock**, which turns that
  block off with one administrator prompt.
- **Another firewall program** (Norton and the like) ignores Windows' rules.
  The pane names it and says what to allow in it.
- The room check also connects to each of the game's TCP ports on every
  member. A broadcast check could not see this fault.

The by-hand commands are still in the pane, under "Or do it yourself".

Tested in CI on a real Windows network adapter with Windows Firewall on and
blocking incoming connections: without the rule, a join is dropped and the
host's launcher notices; the rule lets it in; a block rule for the program
beats it, is named, and Unblock lifts it; a portable room's rule goes with
the room and an installed room's rule stays. The Linux ufw and firewalld
steps are tested against stand-ins, not yet against a real ufw.

## v0.2.27

### Linux downloads say x86_64 or aarch64

Every Linux download now names its architecture the way `uname -m` prints it:
`Mesh.Game.Servers_0.2.27_x86_64.AppImage`, `…_x86_64.deb`,
`…_x86_64-portable.tar.gz`, and the same with `aarch64`. They used to say
`amd64` and `arm64`, which differ by one letter. A CachyOS player unpacked the
arm64 portable build on an x86_64 PC and got only *the file could not be run by
the operating system*. The rpm already used these names and is unchanged.

Only the file names change. The `.deb` still declares `Architecture: amd64`
inside, as dpkg requires, so installing and upgrading work as before. A link
straight to an older release's Linux file keeps working; a link that builds the
name from a version number needs the new spelling from 0.2.27 on. Windows
downloads keep `x64`.

## v0.2.26

The engine underneath moved again: Prns `v0.3.7-hotfix.5` to upstream `main`
as of 2026-10-06. Upstream has not tagged a release since hotfix.5, so this pin
is a commit, not a tag; `ENGINE.md` says which. Nothing to reconfigure, and
nothing a player or operator sees changes.

### Still compatible with what is already deployed

Tested the same way as v0.2.16: the v0.1.10 `sc-rns-bridge` and this
`game-bridge` found each other by announce and passed 32, 600 and 1400-byte
datagrams both ways, each side taking a turn as the server.

### Still one patch, and remote control is still off

The fork still carries one commit, the one that lets a link packet hold a whole
game datagram (1967 B instead of 431); it applied to the new base unchanged.
Prns changed how a node's remote control is set up, and every role here sets it
up switched off, exactly as before.

## v0.2.25

### Six more Source games

Insurgency (2014), Day of Infamy, No More Room in Hell, Fistful of Frags,
Black Mesa and Pirates, Vikings, and Knights II can now be hosted, in the same
`images/source` image — **rebuild it**, since this release teaches it their
settings. Each was booted on a node before its pack was written: secure, VAC
on, answering as its own Steam app, and changing maps from the UI. All six are
joined by address, like every other Source game here.

Two need care, and `HOSTING.md` says how:

- **No More Room in Hell** will not start if the node mounts its content only
  one folder deep (`/game`). Mount it deeper, such as `/opt/game`.
- **Fistful of Frags** maps have their own player limits, and a server with
  more slots than a map allows skips it. 16 slots works.

Several of these name themselves in game from their own config (Fistful of
Frags, Black Mesa, PVKII); the name players browse by is still yours.

### Not yet

Zombie Panic! Source, Synergy and Nuclear Dawn have no working Linux server
today. All three have a Windows one, so they are waiting on running Windows
servers under Wine; `GAMES.md` §9 records what was found.

Nobody has played any of the six through the mesh yet.

## v0.2.24

### Four more Source games

Counter-Strike: Source, Garry's Mod, Half-Life 2: Deathmatch and Day of
Defeat: Source join Team Fortress 2 as games a node can host, all in the same
`images/source` image — **rebuild it**, since this release teaches it their
settings. Each was booted on a node before its pack was written: secure, VAC
on, answering as its own Steam app, and changing maps from the UI.
`HOSTING.md` has the config for each.

Two things you may notice. A Garry's Mod server reports its gamemode,
"Sandbox", as its game in a server query. An HL2: Deathmatch server calls
itself "Half-Life 2 Deathmatch" in game whatever you named it — the game sets
that itself. In both cases the name players browse by is still the one you
gave it.

### A server's full name reaches the game

A Source server named "verify css" showed up in game as "verify": the engine
cut the name at the first space. The image now passes it whole — "Bob's CS -
24/7 [EU]" arrives as written.

### Left 4 Dead 2: not yet

Its server will not install on Linux the way the node installs games, and it
is built to refuse players who connect by address, which is how the mesh
joins. What it would take is written down in `GAMES.md` §9.

Nobody has played any of the four new games through the mesh yet.

## v0.2.23

### A server's name can no longer type at its console — rebuild and upgrade

**Node operators: upgrade the agent.** Until this release a server's name went
to the game unchecked, and both engines read parts of it as commands. A name
like `fun; sv_password x` set the server's password, `fun +rcon_password x`
set RCON's, and `fun -insecure` turned VAC off — on GoldSrc and Source alike,
measured on a node. Anyone who could create a server could do it, including a
user hosting through an index on your node.

A node now refuses a name containing `;`, `"`, `+`, a control character, or a
word starting with `-` (a lone `-`, or one inside a word, is fine), and says
which. An index refuses the same names before it asks a node. Servers created
before the upgrade keep their names until they are recreated.

### Team Fortress 2 hosts

`images/source` is a Source dedicated-server image, and TF2 runs in it: secure,
VAC on, findable, and its map changes from the UI. Build it and point the game
at it — `HOSTING.md` has the config. The TF2 pack itself had a bug that only a
real server shows: it mounted an empty folder over the game's config
directory, so a server started fine and then never left its first map. Fixed.

steamcmd fails a fresh TF2 install now and then with `Missing configuration`
before downloading anything. The node retries that once by itself.

### Day of Defeat, Team Fortress Classic, Opposing Force

Three more GoldSrc games as packs. Each was booted on a node secure and
answering as its own Steam app. They need their own download — the standard
Half-Life server install has only Half-Life and Counter-Strike — which the
packs now ask for. **Rebuild `images/goldsrc`** before hosting Opposing Force:
an older image turns every player away.

Nobody has played any of the four new games through the mesh yet.

## v0.2.22

### Need for Speed: Most Wanted (2005)

`packs/nfs-most-wanted-2005.toml` is the third game with LAN rooms — the 2005
game, not the 2012 one, which has no LAN mode. Host a LAN race in the game;
the others pick it from the game's own LAN list. Allow `speed.exe` through
the firewall, and press **Check room** before starting the game.

Its LAN search and its host port were seen for Most Wanted itself; its other
ports are Underground 2's, on the word of the person who carried both games'
LAN across networks in 2005 that the two speak the same protocol. CI carries
all of them through real adapters, but **nobody has raced it in a room yet**,
so it is marked untested. If a race is found but will not start, that is
worth reporting.

## v0.2.21

### Check a LAN room before you play

A room that does not work on your machine looks exactly like a game with
nobody hosting: an empty LAN list. The room panel now has **Check room**. It
sends a real LAN broadcast on the game's own port from this machine, every
member's launcher answers, and it tells you what failed:

- **your broadcast left by another network** — on Windows, a VPN, Hamachi or
  a virtual machine's adapter is preferred over the room's;
- **a firewall on this machine** is stopping the room's answers;
- **a member answered nothing** — their room adapter is not up, or their
  launcher is older than 0.2.21 (everyone should update).

Run it before starting the game. It proves the room, not the game: if the
check passes and the game still lists nothing, allow the game through the
firewall on every machine. From a terminal, `game-bridge lan-host` /
`lan-join` take `--check`, which reruns it whenever the members change.

### Need for Speed: Underground 2

`packs/nfs-underground-2.toml` is the second game with LAN rooms, and the
first commercial one (Windows). Host a LAN race in the game; the others pick
it from the game's own LAN list. Its ports come from people who captured the
game's LAN traffic in 2004 and are carried end to end by CI in network
namespaces — but **nobody has raced in a room yet**, so it is marked
untested. If the game warns that it cannot test connection quality, or the
host appears with the wrong address, that is worth reporting.

## v0.2.20

### LAN rooms (Mode 3)

For games that only find each other by LAN broadcast — the Hamachi and
Tunngle back catalogue — the launcher can now host or join a **LAN room**
(`PLAN.md` §14). Everyone in a room looks like one local network: the game's
own LAN browser finds the others, over the mesh, with no central server. The
room's host is just a player.

- **In the launcher**: a "LAN rooms" panel to host one, a "LAN room" badge and a
  **Join room** button on a room in the list, and a banner while in one. The
  game's pack says which ports other members may reach, and the launcher shows
  that — and "untested", for a game nobody has played in a room yet — before
  anyone joins.
- **Linux**: the launcher never runs as root. `lan-helper`, installed beside it,
  makes the network adapter, and needs one permission once: the launcher offers
  **Grant permission** (a `pkexec` prompt), or run
  `sudo setcap cap_net_admin+ep /usr/bin/lan-helper`. **The AppImage cannot
  hold that permission** (its mount ignores it); install the `.deb` or `.rpm`
  for rooms.
- **Windows**: `lan-helper.exe` and WireGuard's signed `wintun.dll` ship beside
  the launcher. Windows asks for administrator rights when a room starts, for
  the helper only. `lan-helper.exe` is not code-signed, so SmartScreen may warn.
- **macOS**: no rooms yet.
- **Games**: OpenTTD (`packs/openttd.toml`) is the first, played end to end in
  CI with a real server and client. It is marked untested until a person has
  played it in a room. Old Need for Speed titles are next.
- **The CLI** has `game-bridge lan-host <game>` and `lan-join <game>`, and the
  Windows zip now carries `lan-helper.exe` and `wintun.dll`.

Other members reach your machine through a room **only on the game's declared
ports** and on replies to connections you opened; your own system's background
broadcasts (mDNS, NetBIOS, SSDP) are not sent into it.

### Portable, and nothing left behind

Rebuilt in place on 2026-09-24, the day it was first published, to add this.

- **Portable downloads**: `Mesh.Game.Servers_0.2.20_x64-portable.zip`
  (Windows) and `…_amd64-portable.tar.gz` / `…_arm64-portable.tar.gz` (Linux;
  needs the system's WebKitGTK 4.1). Unpack and run. Everything the launcher
  keeps — settings, identities, remembered servers, the web view's storage and
  caches — stays in its `portable-data` folder. The AppImage becomes portable
  the same way when a `<AppImage>.home` folder sits beside it.
- **LAN rooms install nothing in portable mode, and on the AppImage**: on Linux
  your password is asked when a room starts, and the helper runs from a copy
  that is deleted as soon as it runs; on Windows the Wintun driver is removed
  again when the room ends. The AppImage can now host and join rooms.
- **A room leaves nothing behind, however the launcher ends** — a quit, a
  crash or `kill -9`. On Linux the room's network adapter is no longer
  persistent (in the first v0.2.20 build a killed launcher left `gbl0` until
  reboot); it now disappears with the helper, which exits when the launcher is
  gone.
- **Installed Linux launchers** can still grant the helper its permission
  once, to stop being asked per room, and can now **Revoke** it.

## v0.2.19

Two fixes, either of which on its own stopped a Linux player from getting as
far as a server list.

### The AppImage starts on a machine with the NVIDIA driver

Every AppImage the project has published segfaulted before drawing a window —
v0.2.18, v0.2.17, all of them — with nothing on stderr but two unrelated GTK
module warnings, which made it look like the launcher silently did nothing.

`linuxdeploy` bundles the build host's `libwayland-client.so.0`, ubuntu-22.04's
1.20. `libEGL` cannot be bundled in turn, because glvnd is host-only, so EGL
init loads the *host's* `libnvidia-egl-wayland2.so.1` — built against 1.26 —
and that calls into the bundled 1.20 client. It dies on a mutex in a struct
whose layout the two versions do not agree on.

The release workflow now unpacks each AppImage, drops that one library and
repacks. `libwayland-client` is host-by-nature: it speaks a protocol to the
host's compositor, and the host's copy is always the right one.

Worth recording for anyone who hits something like it, because all three are
the obvious first guesses and none of them helps: `WEBKIT_DISABLE_DMABUF_RENDERER`,
`WEBKIT_DISABLE_COMPOSITING_MODE` and `GDK_BACKEND=x11` all still crash. The
X11 one is the informative failure — EGL probes every external platform at init
whatever the GDK backend is, so the NVIDIA wayland platform loads even with no
compositor in sight. The crash is underneath WebKit, in EGL.

### Start no longer hangs on "Starting…"

Pressing Start left the button reading "Starting…" and the chip reading
"Browse node stopped" indefinitely, while the log said the opposite a second
after the click. The node was up; what never returned was the command.

`start_browse` awaited the remembered-server sweep, which asked the mesh for a
path to each known server one at a time — and a path request settles when a
path is found *or* when the engine's timeout expires, so a server that is
merely switched off costs the full timeout. Four remembered servers meant four
timeouts, in sequence, before the button could change.

It asked them all while holding the session lock, so `browse_status`,
`list_servers` and `stop_browse` queued behind it too. That is why the chip
never caught up on the next poll: nothing could answer. A launcher that has
heard of a server is the normal case, so this was every launcher after its
first successful browse.

The sweep now takes the node handle and drops the session lock before touching
the network, asks concurrently rather than in sequence, and bounds each request
at 8 seconds — "no path" has no other way to end. `start_browse` spawns it
instead of awaiting it: the node is listening the moment it is attached and the
UI has to be told so then.

## v0.2.18

### `docker stop` stops the agent

Inside a container the agent runs as PID 1, and the kernel ignores a signal PID
1 has not asked to handle. The agent never asked, so every `docker stop` — and
so every redeploy — waited out Docker's 10-second timeout and ended in a kill
(exit 137).

It now handles SIGTERM and Ctrl-C: it stops taking API requests, gives any
request already running up to 5 seconds, and exits cleanly. On the same image,
a stop went from 10.2 seconds to 0.2.

**Game servers keep running.** They are separate containers and always
outlived the agent on purpose; the next start puts them back on the mesh. A
game download cut short by the stop leaves only a staging directory, never a
half-installed version.

## v0.2.17

Two fixes on top of the engine move in v0.2.16.

### A TCP game's greeting could be lost

A game that speaks first — most TCP games greet the moment someone connects —
could have that greeting dropped on its way to the player, leaving a connection
that sat open saying nothing. The player's side only started listening on the
link after it had also identified itself, and anything arriving before it
listened was thrown away rather than resent. v0.2.16's engine made identifying
slower, which turned a rare race into roughly one connection in eight, and
failed every stream test on macOS CI.

Now the player's side listens before it does anything else, and a server with
no allowlist waits for the player to identify before connecting a TCP game —
which is the moment it knows the far side can hear. A client that never
identifies still gets its game once the identify timeout passes.

No shipped pack was affected: TF2's RCON port is the only TCP one, and it
connects on the player's first byte, not the server's. It matters for any pack
whose game itself runs over TCP.

### A server no longer answers every datagram with a proof

Each game packet a server received cost a ~118-byte signed receipt sent back to
the player: a copy of the player's upstream traffic, travelling against the
game's own updates, for receipts nothing ever read. A game server's destination
now asks for none. Joining, the detail probe and TCP games are unaffected, and
players on Sven Co-op over Reticulum v0.1.10 still join — tested for 90 seconds
of traffic with every packet answered.

## v0.2.16

The engine underneath moved: Prns `v0.3.7` to `v0.3.7-hotfix.5`. Nothing to
reconfigure, and a node or launcher on this version still talks to one on
v0.2.15 — and to Sven Co-op over Reticulum v0.1.10.

### Why move at all

Upstream now does one of the two things this project had to patch into Prns
itself: an announce's `app_data`, the whole row a server browser shows, is on
the event an app hears. That patch is gone. The other — letting one link packet
carry a whole game datagram (1967 B instead of 431) — is still ours, and is now
the only commit the fork carries. `ENGINE.md` has both, and how to move again.

It also brings upstream's own work since August: announces verified before they
are acted on, and reworked link maintenance and resource transfer.

### Still compatible with what is already deployed

Until now the platform and the standalone Sven Co-op app ran the same engine
tree, so compatibility came for free. It no longer does, so it was tested: the
v0.1.10 `sc-rns-bridge` and this `game-bridge` found each other by announce and
passed 32, 600 and 1400-byte datagrams both ways, each side taking a turn as the
server.

### Prns's new remote control is off

Prns now ships a service for administering a node over the mesh. Every role here
turns it off: a node is administered by whoever runs it, and the agent's control
surface is its own uplink with the operator's `trusted_indexes` allowlist — not a
second door nobody asked for.

## v0.2.15

Counter-Strike 1.6 can have bots after all — by installing one.

### A node may declare a bot it installed itself

1.6 ships none, and no setting changes that: Valve's Z-Bot sits in the same
server library and is gated on a Condition Zero flag. That was worth testing
past the obvious explanation, so it was: with `BotProfile.db`, `BotChatter.db`
and a nav mesh copied in from a Condition Zero install, `bot_quota 4` on a
`cstrike` server still produces nothing. The missing thing is the mod, not the
data.

[YaPB](https://github.com/yapb/yapb) does work there, and it is a binary the
node executes — so it arrives the way a container image does, from the operator:

```sh
cp -a <data_root>/content/counter-strike-16/app90 \
      <data_root>/content/counter-strike-16/app90-yapb
scripts/install-yapb.sh <data_root>/content/counter-strike-16/app90-yapb
```

```toml
[games.counter-strike-16]
content_version = "app90-yapb"
bots = "yapb"
writable_paths = ["cstrike/addons/yapb/data/train", "cstrike/addons/yapb/data/logs"]
```

The installer pins a version, checks its SHA-256 before extracting anything,
stages and then moves, and refuses a directory that already has YaPB in it.

**`bots` belongs to the node here, and a pack cannot say it.** A pack may name
bots the game *ships* — Condition Zero's Z-Bot — because every node with that
game has them. An installed bot is a fact about one machine, so a pack carrying
`bots = "yapb"` is now a parse error rather than a Bots button that does
nothing on every node that never installed it. Same rule as `image`, for the
same reason.

`[games.<id>].writable_paths` is new alongside it: a pack cannot know this
node's copy has a bot that caches pathfinding, and without those entries YaPB
reports `Unable to open Pathmatrix file for writing` on every map. Each
instance now gets its own cache.

## v0.2.14

Bots, and a fifth game to have them.

### Counter-Strike 1.6 has no bots; Condition Zero does

Valve's Z-Bot is compiled into the Counter-Strike server library — the bot
manager, the profiles, `bot_add`, `bot_quota` — and gated on an internal
Condition Zero flag. On a `cstrike` server the command adds nothing and says
nothing, which is the worst shape a missing feature can take. Sven Co-op has no
bots to add either: it exposes an AngelScript `CreateBot` for plugins and
nothing reachable from a console.

So `packs/condition-zero.toml` joins the shipped packs. It is the same steamcmd
app 90 as Half-Life and Counter-Strike, plus the parameter that selects its
depots:

```toml
[content]
driver = "steamcmd"
app_id = 90
mod = "czero"
```

`mod` is new on the steamcmd driver and validated as an identifier, because it
becomes an argument in a command the node runs — refused rather than repaired,
the same rule as a map name.

```toml
[games.condition-zero]
image = "gpp/goldsrc:1"
content_root = "/game"
content_version = "app90-czero"
env = { HLDS_MOD = "czero" }
```

### Asking for bots

The start form takes a bot count, and a running server gets a **Bots** button
beside Change map — both only for a game whose pack declares them. What travels
is a **quota**: 4 means "hold four", asking twice changes nothing, 0 empties the
server, and nobody is disconnected. `POST /instances/:id/bots` is the API.

Two behaviours worth knowing, because both were measured on a real server after
looking wrong first:

- **`+bot_quota` on the command line does nothing.** The bot manager is not
  there when the command line is parsed, so a server started that way comes up
  empty. The node sends the quota over the console a few seconds after the map
  loads instead, which is the same path the button uses.
- **`bot_join_after_player` defaults to 1**, holding every bot out of the game
  until a human joins. A quota on its own produces a server that reports bots
  and looks deserted, so the two lines always go together.

`InstanceStatus.bots` reports what the node asked for, never a queried number:
A2S counts bots among its players, and presenting that under the same name would
be a different fact.

## v0.2.13

One fix, and it is the difference between a Counter-Strike server nobody can
join and one people are playing on.

### `STEAM validation rejected`, and why it was not the transport

A GoldSrc server registers with Steam as whatever `SteamAppId` names, and
v0.2.11 named 90 — the Half-Life *Dedicated Server* app. That server boots,
logs `Connection to Steam servers successful`, activates VAC, and then turns
away every player with `STEAM validation rejected` on their screen and nothing
whatsoever in its own log. A client's session ticket is for Counter-Strike (10)
or Half-Life (70); the server is not that app, so there is nothing to validate
against.

`images/goldsrc` now derives the id from `HLDS_MOD` — cstrike 10, valve 70,
tfc 20, dod 30, and 90 only for a mod Steam has no app for. Rebuild the image
and recreate the instance; no config change is needed.

The wrong lesson was available and briefly taken: `sv_lan 1` also lets clients
in, by switching Steam authentication and VAC off. That would have been a
platform-wide downgrade in response to a one-line configuration error, so it is
back to being the escape hatch — `env = { HLDS_SV_LAN = "1" }` — for a mod
Steam does not know, or players whose Steam cannot reach Valve.

What settled it was the reference implementation. A deployed Sven Co-op server
on this same Reticulum transport logs `STEAM USERID validated` for a player
arriving from a private address, which means a bridged client is perfectly
validatable and the fault had to be ours. `PLAN.md` §5 keeps extraction
one-directional, and this is a case of it paying rent in the other currency:
`svencoop-prns` had no code to copy, but it had the evidence.

Docs corrected with it, including the v0.2.11 note that told operators to set
90.

## v0.2.12

The build number is on screen now, in both UIs.

### Which version am I running?

A quiet chip in the bottom-right corner of the node's web UI and of the
launcher names the build. It exists because two questions had no answer short
of a shell: *did my redeploy actually land*, and *which version is the person
reporting this bug on*.

The node's `/health` carries a `version` field and the web UI renders it once
the token is accepted. Deliberately not before: every API route here is
token-gated, and a version string served to anyone who can reach the port would
be the only thing that is not.

The launcher asks its core rather than its Tauri shell. Both hold the same
number, but the shell's `Cargo.toml` is one of the two places a release has to
bump **by hand** (see *Version* above), so reporting the shell's would show the
wrong build in exactly the case where somebody forgot to bump it.

Neither chip lies to an older backend: a node with no `version` field reads
"version unknown", and a launcher shell without the `app_version` command
leaves the chip empty rather than rendering "vundefined" or raising an error
banner.

## v0.2.11

A third game that actually runs on a node, and a launcher that can see it.

### Counter-Strike 1.6 hosts

The pack has shipped since 0.2.9 and no node could start it, because a pack
cannot name a container image and no GoldSrc image existed. `images/goldsrc` is
that image, in the same shape as `images/sven-coop`: it carries no game files
and runs whatever the node mounted at `content_root`.

```
docker build -t gpp/goldsrc:1 images/goldsrc
```

```toml
[games.counter-strike-16]
image = "gpp/goldsrc:1"
content_root = "/game"
content_version = "app90"
env = { HLDS_MOD = "cstrike" }
```

One steamcmd app 90 install — about 930 MB, fetched anonymously — is the
Half-Life Dedicated Server, and it contains Counter-Strike, Half-Life, DoD and
TFC. Which one a server runs is a start argument, so `HLDS_MOD` is the
operator's `env` and not a pack field: one download, several games. It is
deliberately not named `GPP_MOD`, because the agent drops operator env with that
prefix so config can never overwrite what an instance spec set.

Two things the image does that a hand-written one usually misses, both measured
against HLDS build 10211 on a real daemon:

- **`SteamAppId` in the environment.** Without it the server initialises, loads
  the map, prints a log that reads exactly like a healthy server, and then dies
  on the last line with `FATAL ERROR (shutting down): Unable to initialize
  Steam`. The mod's own `steam_appid.txt` is not a substitute — it is what the
  failing case reads.

  **Corrected after this release shipped:** 0.2.11 set it to 90, the Half-Life
  Dedicated Server app. That boots, connects to Steam and activates VAC, and
  then rejects every player with `STEAM validation rejected`, because a client's
  session ticket is for Counter-Strike (10) or Half-Life (70) and the server is
  not that app. The image now derives the id from `HLDS_MOD`.
- **`~/.steam/sdk32/steamclient.so`.** HLDS dlopens it from its home directory,
  and the copy it needs is in a content mount that is read-only and shared, so
  the image links it at start.

The name a player browses by is the announce, which the agent sets from the
instance spec. The server's *own* `hostname` stays whatever the mod's
`server.cfg` says, because that file runs after the command line and lives in
the read-only content copy.

### Every pack hid its own maps

A `writable_path` becomes an empty per-instance directory bind-mounted **over**
the shared content, and `half-life`, `counter-strike-16` and `team-fortress-2`
all declared their own map directory writable. So `cstrike/maps` hid all 53
maps the install ships and the server had no `de_dust2` to start — the same bug
`svencoop/maps` was before 0.1.x. Fixed in all three packs;
`no_shipped_pack_declares_its_own_maps_dir_writable` finds its subjects by
property rather than by id, so a pack added later is checked without anyone
remembering to.

**If you host these games, take the new packs.** The old ones are why a
GoldSrc server could not start.

### The installed launcher shipped no packs

`pack_dir` looks for `packs/` beside the executable and otherwise falls back to
a path that only exists in a source checkout — and nothing put `packs/` in the
bundle. An installed launcher therefore fell back to the single built-in Sven
Co-op pack: one entry in the game filter, and no way to name any other game a
legacy server might be running. The bundle now carries every shipped pack.

### "Any game" showed no games

The filter's first option was built with no `value` attribute, and an `<option>`
without one reports its own text as its value. Clearing the filter asked for
servers running a game called `Any game`, which matched nothing, so the list
went empty instead of showing everything. `launcher/uicheck/render.mjs` now
pins the whole dropdown: every loaded pack is offered, the first option's value
is empty, and picking a game and then clearing it sends the id and then sends
none.

## v0.2.10

Three things: a join that stops lying, a transport that carries a game, and the
index a launcher can finally ask.

### A join that cannot reach the server says so

Binding a local port always succeeds and says nothing about the server. Point a
game at a port whose bridge has no route and the game sits on "establishing
connection" until it gives up, while the launcher reports success — no error
anywhere, nothing pointing at the address.

The commonest cause is an address that is simply gone: **a mesh identity is per
instance id, so recreating a server mints a new destination**, and a row from
an earlier session, or from the launcher's memory, names one nobody can route
to. `JoinResult.reachable` now carries the answer, from the same probe the
detail pane uses. A warning and never a refusal — mesh routing is asymmetric
and an allowlisted server declines probes on purpose.

### Backbone interfaces

A node can reach a hub over Reticulum's `BackboneInterface` instead of a
generic TCP client: a 1 MB frame size and a gigabit rate, against
`UDPInterface`'s 1064 bytes and 10 Mbps guess. A hub offering one was already
being dialled — with the wrong protocol at our end.

**UDP does not carry a game to a stock RNS peer, and HOSTING.md now records
why that stays unexplained.** With the destination held constant and only the
transport changed, UDP carried announces and A2S queries perfectly while a game
connect stalled every time. Two mechanisms were proposed and both were wrong:
`HW_MTU = 1064` is not a receive buffer (RNS reads through
`socketserver.UDPServer` at 8192), and raising it to 4096 with a 1 Gbps
bitrate, confirmed live, changed nothing. The measurement is solid; the
explanation is not, and the note says so rather than inventing a third story —
a plausible-but-false mechanism is exactly what makes someone retry it.

### The launcher can ask an index

Index addresses can be added in the launcher, which queries each and merges the
rows. **This is the only way to find a server that was already running before
you started**: the mesh floods an announce once and suppresses the repeats, and
remembering covers only servers you have seen yourself.

An index **loses every tie**. A row heard directly is the launcher's own
evidence and wins; an index row fills a gap and is badged `via index`, because
it is second-hand — the numbers are real, but somebody else heard them and
`hops` is the distance from the index. Empty by default and complete when
empty: adding one is a player trusting somebody's list, not the platform
acquiring a dependency (`DESIGN.md` §0).

`index-client` is a new crate carrying the query call and the codec both ends
speak, because `platform-index` depends on `platform-agent` and therefore
bollard — a launcher must not ship a Docker client to read a list.

## v0.2.9

**The launcher remembers servers, and asks the mesh where they are.** If you
have ever opened the launcher and seen an empty list while a server was plainly
running, this is that.

### Why an empty list was correct behaviour

A transport node floods an announce when a destination is **new** and suppresses
the repeats once it holds a path. It has to — otherwise announces would
re-flood the mesh forever. But a browser is passive: it learns of a server only
when that server next announces. So a launcher started *after* a server was
already running never hears about it, no matter how often the server announces.

Measured on a live mesh, one browse session, two servers on the same node
sharing the same link and the same 15-second timer:

| destination | state during the browse | seen |
| --- | --- | --- |
| new one | created 12 s in | yes, at hops=2 |
| existing one | already running | no, in 100 s |

Novelty was the only difference between them. Lowering the announce interval
does not help, because the repeats are precisely what is being suppressed.

### What the launcher does now

- **Remembers.** Every server heard is written to `launcher.json` by
  destination hash, with its last name and game. Only a real change earns a
  write; a list polled every two seconds must not rewrite the file every two
  seconds.
- **Asks.** On browse start, and from the **Find remembered** button, it issues
  a path request per remembered server. **The answer to a path request is the
  cached announce**, so rows fill in through the ordinary `AnnounceHeard` path
  with real names, maps and counts — nothing is synthesised from memory.
- **Forgets**, one or all, from the detail pane.

No index, no account, no infrastructure — `DESIGN.md` §0 is intact. A
destination hash is all a join needs, so a server seen once stays joinable from
the player's own machine forever after.

A path request rather than a probe on purpose: re-probing every remembered
server over a Link would open a connection to each just to draw a list, which
is exactly the traffic `PLAN.md` §3.4 says a decentralized browser must not
generate.

**The rule the UI must not break:** a remembered row is never dressed as a live
one. Dimmed, badged, and every live field reads Unknown rather than a stale
value — a stale player count shown as current is the one thing a server browser
must not do. It is still joinable and says so. A remembered row is also not
evidence: feeding one back into the recorder cannot refresh its own timestamp,
or memory would keep itself alive and a dead server would never age out.

## v0.2.8

**One shared Reticulum node per host, instead of one per game server.** If you
run more than one server on a node, take this release: on 0.2.7 and earlier the
second and later servers silently lost every interface that binds.

A node used to run a Reticulum node *per server*, each with its own copy of the
node's interfaces. That is backwards. **Interfaces are scarce per host in a way
destinations are not**: a point-to-point UDP interface binds one local port, a
TCP server binds one, an RNode owns one serial device — while a machine can
announce as many destinations as it likes. Six servers were six nodes each
wanting its own copy of the same link, and only the first could have it.

The failure was quiet, which is why it took a live node to find. The first
server to start got the port; the rest got `AddrInUse`, kept running, kept
announcing over whatever else they had, and were simply absent from that link.
Healthy from every angle except the one that mattered. Note that
`tcp = "0.0.0.0:PORT"` in `[mesh]` binds too, so this was never only about UDP.

Now the agent runs one **hub** and each bridge joins it as a shared instance and
binds nothing. This is the engine's own mechanism — the same one `rnsd` uses to
let several programs on a machine share one mesh connection — and it needed the
`shared-instance` feature, which no crate had enabled.

Two decisions inside it that are load-bearing:

- **The hub is a `Relay` session, not a `Browse` one.** It has to forward: a
  bridge joined to it holds no interfaces, so every packet in either direction
  crosses the hub, and forwarding needs a transport identity. `Browse`
  deliberately holds none — browsing a list is not consent to carry strangers'
  traffic (`PLAN.md` §4) — and what the hub carries is this node's own servers'.
- **`shared_instance_port` defaults to 37429, not RNS's 37428.** A node sharing
  a machine with `rnsd` would otherwise bind that machine's shared-instance bus
  and quietly take over its mesh instead of forming its own.

Runtime interfaces now attach to the hub alone, so a running server picks up a
newly added relay with no restart and nothing to re-attach.

Verified on a live node: two servers over one UDP leg, both announced with their
own live map, 8/8 A2S round trips to each concurrently at a median of 12 ms, and
the whole thing surviving an agent restart. Before this the second server had no
UDP at all.

## v0.2.7

### The map in a server list was always "Unknown"

`server_announce_bytes` ran **once**, before the announce loop, and the same
bytes went out every interval forever. So a browser's row showed whatever the
server started with and never anything else. For a node-hosted server that
meant a permanently empty map — nothing sets `args.map` there — which the
launcher renders as "Unknown", and a player count frozen at its initial value.

The live A2S poller sitting beside it already read both from the game. This is
the half that was missing: the record is rebuilt each tick, taking map and
counts from the last live read. A `changelevel` now reaches the list.

**The announce is the entire server-browser row** (`PLAN.md` §2 — 316 bytes of
`app_data`), so a stale one is not cosmetic. It is the listing.

A legacy announce is untouched: a bare name carries no map or counts, and §5
freezes what a deployed v0.1.10 peer sees. A zero-slot reading is ignored
rather than published — a game reporting no capacity is answering nonsense.

### UDP interfaces

TCP makes Reticulum ride a reliable ordered stream underneath a protocol that
does its own retransmission and ordering; on a local network that duplicated
machinery is latency for nothing. Both the mesh and the uplink now take a UDP
interface. It needed the engine's `udp` feature, which no crate had enabled.

**Point-to-point and not discovered**, so the far side needs the mirror of the
entry with `local` and `peer` swapped. The web UI says so where it is
configured, because an entry with no counterpart silently sends into nothing.

**A containerised peer must publish the UDP port**, and this is the trap: an
outbound datagram escapes a container through NAT whether or not a port is
published, so a one-way flow looks like a working interface from the sending
side. The receiving side sees nothing and says nothing. If a UDP interface is
configured correctly at both ends and still carries no announces, check
`docker ps` for `4790/udp` before touching either config.

### Stats

`InstanceStatus` gains `map_now`, read from the same A2S reply that already
supplied `players_now` rather than a second query, and the agent's UI grows a
Map column. `None` keeps its `players_now` meaning — "could not ask", not "no
map" — and renders as a dash with the reason in its tooltip, never as a blank
cell. Cells update in place; no row node is replaced, so a poll cannot steal
focus or scroll.

## v0.2.6

Two things a live node made obvious, once joining actually worked.

### The Sven Co-op sound cache could never be written

A server writes `svencoop/maps/soundcache/<map>.txt` on every map load, and
that path sits inside the read-only content mount, so every attempt failed:

```
[Sound Engine] - Failed to write sound cache "crystal.txt" - Error #30
```

Errno 30 is `EROFS`. The cache is what lets a map skip re-scanning its sounds,
so a server that can never write it re-precaches from scratch at every
`changelevel` — which an operator sees as the server looping on precache.

`svencoop/maps/soundcache` is now in `writable_paths`, and it is safe there for
**two reasons that must both keep holding**:

1. a steamcmd install **ships the directory**, so the nested writable bind has
   a mountpoint. One cannot create its own under a read-only mount, and
   `plan_and_check` refuses rather than letting runc fail with `mkdirat ...
   read-only file system`. The `uplink_roundtrip` fixture proved this the
   instant its fake content tree lacked the directory;
2. it ships that directory **empty**, so an empty per-instance directory
   mounted over it hides nothing.

Listing the parent, `svencoop/maps`, once hid all 108 shipped maps. This sits
just inside that rule, and a test pins both halves. **If a future content
version starts shipping files under `soundcache`, this entry becomes the same
bug and must go.**

### A map is chosen from a list, not typed from memory

Packs gain `maps_dir` — a relative path to where a game keeps its maps
(`svencoop/maps`, `valve/maps`, `cstrike/maps`, `tf/maps`) — and the agent
answers `GET /games/:game/maps` by listing the `.bsp` files in the content copy
it actually has. 108 for a full Sven Co-op install.

The pack names a **directory** and never a map. The path is validated with the
same rule as `writable_paths` before anything is read, and the node only ever
lists it: a pack cannot reach outside the content copy, and listing a directory
runs nothing. What is installed is a fact about the machine, so the node
answers rather than the manifest — a node with partial content, or with maps an
operator added by hand, reports what is there.

The web UI offers them on the start form and replaces the change-map
`prompt()` with a picker. Deliberately a `datalist` and a scrolled list of every
map, not a `select` and not a truncated one: the node lists what it has
installed, and an operator may know about a map it does not.

## v0.2.5

**A join could not reach anything, and this is the release that fixes it.** If
you are on 0.2.1–0.2.4, the launcher can browse and probe but cannot actually
carry a game to a server. Upgrade.

`start_browse` has always passed the player's interfaces to its Reticulum node.
`join_server` did not: it built `ClientArgs::new`'s defaults — `tcp: None,
auto: false` — so the client bridge attached no interface at all and had no way
off the machine.

What made it survive two releases is that nothing about it looks broken:

- the local port **binds**, because binding a UDP port is purely local;
- the server list **fills**, and the detail probe answers with **live stats** —
  both of those run on the *browse* node (`server_details` reads `inner.browse`);
- only the game's own packets went into the isolated node, so a join reported
  success and the game sat at "Connecting…" until it gave up — on the pack's
  default port and on a custom one alike, which is what ruled the port out as
  the cause.

The engine had been saying so the whole time:

```
attach_interfaces called tcp=None auto=false
WARN no interfaces attached; this node cannot talk to anything
no route to server; requesting path then retrying
  error=Failed(Rejected(NoRouteToDestination))
```

That warning goes to stdout, and a windowed app never shows stdout. **When a
launcher symptom does not match what the library does under test, run the
`game-bridge` CLI against the same server — it prints what the GUI swallows.**
That is what found this.

A join now attaches whatever the running browse node was started with, or the
player's saved interfaces when nothing is browsing; a join with neither is
refused with a sentence rather than binding a port that leads nowhere.
`join_interfaces` is its own function so the rule — *a join is never given
fewer interfaces than a browse* — is testable without starting a node.

### A test that failed on what else the machine was running

`uplink_roundtrip` set `max_instances = 2`. An agent counts every container
carrying `MANAGED_LABEL` on the daemon it drives, by design — the containers
are the record — so a development box already hosting two game servers failed
that test with "this node is at its limit of 2 instances". It reads exactly
like a flake and is not one. The limit is now far above what the test needs.

## v0.2.4

**No change to the shipped software.** The binaries and the launcher are
byte-for-byte what v0.2.3 does; the diff between the two tags is
`release.yml` and this file. It is a release because the *pipeline* changed,
and because a version number is the honest way to say which artifacts came out
of the fixed one.

- **`macos-13` is gone from the matrix.** GitHub retired the Intel macOS
  runners, and a job naming a label with no runner behind it is not failed and
  not reported — it queues, indefinitely, and the run never completes. Every
  release from v0.1.0 to v0.2.3 hung that way, which is why none of them ever
  carried an Intel-mac asset. `ci.yml` was never affected because it only ever
  named `macos-14`; that difference between the two files was the whole answer.
- **Intel Macs are served from Apple Silicon runners now.** The CLI target
  cross-builds (same OS, same Apple SDK, nothing third-party in the link) and
  the launcher ships one `universal-apple-darwin` dmg covering both
  architectures.
- Both matrices carry `timeout-minutes`, so a build that hangs for some other
  reason fails rather than waiting out the six-hour default.

v0.2.1 and v0.2.2 were re-dispatched against the fixed workflow and now carry
their full asset sets, Intel-mac included. **v0.2.4 is the first release built
from a tag push end to end** rather than a hand-dispatched re-run, and the
first with a single mac dmg rather than two.

## v0.2.3

One fix, for the field 0.2.2 added.

- **The detail pane keeps its scroll position and the caret across a
  re-render.** It redraws on every poll because it shows "Last seen: 3s ago",
  and it rebuilt itself wholesale — including `.detail-body`, the element that
  actually scrolls — so the offset went back to zero several times a minute.
  0.2.2's local-port field sits near the bottom of that pane and so was the
  first control anyone had to scroll to: scrolling down bounced back up.

  The offset and the focused control are now carried across by hand, scroll
  restored before focus (with `preventScroll`, or focusing an element the
  browser thinks is off-screen undoes the restore). Every control in the pane
  has a stable id, because that is what the restore looks itself up by.
  `launcher/uicheck` has a scenario for it, checked to fail without the fix.

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

## Launcher updates and their signing key

From v0.2.31 the launcher updates itself (`launcher-core/src/update.rs`).
The release workflow signs every installer the updater can take — Windows
`-setup.exe` (and `.msi`), the AppImages after their libwayland repack, and the
macOS `.app.tar.gz` — and a last job, `updater-manifest`, writes `latest.json`
from those signatures. A launcher reads
`releases/latest/download/latest.json`, so **the newest published release is
what every launcher is offered**: never publish a broken one as latest; mark a
test build as a pre-release.

- The private key is the repository secret `TAURI_SIGNING_PRIVATE_KEY`, with
  its password in `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. The maintainer's copy is
  `~/.tauri/lanthorn-updater.key` (+ `.password`) on the build machine; **keep a
  backup offline.** Lose it and no installed launcher will accept another
  update — they would all have to be reinstalled by hand with a new key.
- The public key is in `tauri.conf.json` (`plugins.updater.pubkey`). Changing
  it strands every launcher that has the old one.
- `tauri.release.conf.json` turns the updater artifacts on for the release
  only; a build without the key (CI, a contributor) makes no `.sig` and works.

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
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `aarch64-apple-darwin`, `x86_64-apple-darwin` and `x86_64-pc-windows-msvc`,
  packaged with `packs/`. Every entry builds with an explicit `--target`, even
  the native ones, so one packaging step serves all of them.
- **launcher** — `cargo tauri build` on ubuntu-22.04, ubuntu-24.04-arm,
  macos-14 (as `universal-apple-darwin`) and windows-latest.

### `macos-13` is retired, and a retired label queues forever

This cost every release from v0.1.0 to v0.2.3 the same way, and it is worth
recognising quickly because the symptom does not look like a failure. GitHub
removed the Intel macOS runners. A job that asks for a label with no runner
behind it is **not** failed and not reported: it sits in `queued`, and the run
never completes. v0.2.1's release run was still queued more than two hours
after every other job had succeeded.

So no release has ever carried an Intel-mac asset, and the runs that "had not
finished yet" were never going to. `ci.yml` was unaffected throughout because
it only ever named `macos-14` — which is why CI stayed green while releases
hung, and why the difference between the two files was the thing to look at.

Intel Macs are now served from Apple Silicon runners instead: the CLI target is
cross-compiled (same OS, same SDK, nothing third-party in the link), and the
launcher gets a `universal-apple-darwin` bundle that runs on both. Both matrices
also carry `timeout-minutes`, so a build that hangs for another reason fails
rather than waiting out the six-hour default.

The universal launcher entry started out additive, beside the native
Apple-Silicon one, because that native bundle was the only mac artifact any
release had ever produced and an unproven replacement was not worth a
regression. It has since proven itself and the native entry is gone: v0.2.1 and
v0.2.2 each published both, and the universal dmg came out at **10.4 MB against
the native 5.3 MB** — roughly double, which is what a fat binary should be and
the reason to believe the label. A green job is not by itself evidence that an
artifact contains what it claims; the size was.

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
nor notarized and Gatekeeper will refuse it on first open. macOS 15 removed
the old right-click → Open bypass: open the app once, let it be refused, then
System Settings → Privacy & Security → **Open Anyway** (macOS 14 and older:
right-click → Open). From a terminal, `xattr -d com.apple.quarantine
/Applications/Lanthorn.app` does the same. This follows Apple's documented
flow and has not been checked on a real Mac. Signing is a paid account and a
decision about identity, not a build flag.

The Mac app is **experimental**: browse and join only, no LAN rooms (there is
no macOS adapter), and nobody on this project has run it.
