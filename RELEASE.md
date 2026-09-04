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
nor notarized and Gatekeeper will refuse it on first open. Right-click → Open,
or `xattr -d com.apple.quarantine`. Signing is a paid account and a decision
about identity, not a build flag.
