# gaming-platform-prns

A **decentralized server browser** for game servers over
[Reticulum](https://reticulum.network/), built on
[Prns](https://github.com/KenAKAFrosty/Prns).

Find a game server on the mesh, join it, host one, or donate transit — with no
account, no port forwarding, no central service, and no internet required.

> **Status** (`PLAN.md` §8 has the detail). Phases 1-4 are complete; phase 5 has
> started with four shipped game packs and the transport work Source-engine
> games need.
>
> | Phase | | |
> | --- | --- | --- |
> | 1 | The bridge | done — engine pinned to a patched Prns fork ([`ENGINE.md`](ENGINE.md)); relay and framing parametrized by a game pack; the announce record; a link allowlist; a Relay role with a transit off switch |
> | 2 | Browse | done — list and filter from announces alone, no index and no internet; a detail probe over a Link; a Tauri launcher |
> | 3 | One node, many servers | done — `platform-agent` runs many servers off one shared copy of the content, loopback-only local API, no central service; a server starts on a chosen map and its map can be changed live without dropping players |
> | 4 | Index + hosting | done — identity challenge/response bound to the verifying index, an index served over both HTTP and Reticulum with quotas, hosted deploy, and multi-node over an agent uplink that needs no inbound port |
> | 5 | More games | started — TCP games over a link's channel; Half-Life, CS 1.6 and Team Fortress 2 added as data with no Rust change; multi-port games (game + RCON + SourceTV on one destination) and a port set per hosted instance; a GoldSrc node image, so Counter-Strike 1.6 and Half-Life actually host |
>
> Current release: **v0.2.19**. What changed, release by release, is in
> [`RELEASE.md`](RELEASE.md); building and tagging one is in there too.
>
> The working single-host implementation this generalizes is
> [`idan2025/Svencoop-Prns`](https://github.com/idan2025/Svencoop-Prns).

## Install and use

Every artifact is on the [releases page](https://github.com/idan2025/gaming-platform-prns/releases).
The launcher is the only one a player needs; the rest are for hosting.

| You want to | Take |
| --- | --- |
| Browse and join servers | `Mesh Game Servers` — `.deb`, `.rpm`, `.AppImage` (x86-64, arm64), `.dmg` (universal), `.exe` (Windows) |
| Run a node with a web UI | the Docker image, `ghcr.io/idan2025/gaming-platform-prns` |
| Host or relay without a desktop | the CLI tarball for your target: `game-bridge`, `platform-agent`, `platform-index` |

### Browse and join

Install the launcher and start it. It needs one thing to hear anything: a
Reticulum interface. Either tick **Wi-Fi / LAN auto-discovery** to find
neighbours on the same physical network, or give it a **TCP peer** — an address
someone already on the mesh gave you. With neither, the list stays empty and
says so.

Then: pick a row, read the detail pane, press **Join server**. That binds a
local port and tunnels it to the server over a Reticulum Link. **Play** then
starts your own copy of the game pointed at that port — the first time, the
button reads **Locate game** instead, because the launcher never guesses an
executable. It never downloads a game either: a pack cannot name a program, so
what runs is always what you installed (`PLAN.md` §13.1). Any game can also be
pointed at `127.0.0.1:<port>` by hand.

Two things worth knowing:

- **Legacy servers show as "Unknown"** for game, map and players. A deployed
  `svencoop-prns` v0.1.10 announce carries a name and nothing else, so the
  launcher refuses to guess; tell it which game the server runs and the join
  works.
- **Every shipped pack rides in the bundle** since v0.2.11, so the game filter
  lists Sven Co-op, Half-Life, Counter-Strike 1.6 and Team Fortress 2 out of the
  box. Before that, an installed launcher fell back to the one pack built into
  the binary.

### Run a node

The node runs game servers as **sibling containers** on your own Docker daemon,
so one copy of a game's files serves every instance of it, and manages them from
a browser. [`HOSTING.md`](HOSTING.md) is the full account, including the two
rules that will bite you (`data_root` must be the same path on both sides of the
bind; the Docker socket is root-equivalent).

The short version, as actually deployed:

```sh
mkdir -p ~/gpp/packs && cd ~/gpp
cp /path/to/checkout/crates/platform-agent/agent.example.toml ./agent.toml
cp /path/to/checkout/packs/*.toml ./packs/
```

In `agent.toml`: `data_root = "/home/you/gpp/data"` (an absolute path under the
directory bound below), `api_bind = "0.0.0.0:4750"` — inside a container,
loopback *is* the container — and an `api_token_file` under that same data root.
Add `allow_content_fetch = true` and a `steamcmd_image` if you want the node to
download games it has packs for.

```sh
docker run -d --name gpp --restart unless-stopped \
  --user "$(id -u):$(id -g)" --group-add "$(getent group docker | cut -d: -f3)" \
  -p 4750:4750 -p 4789:4789 \
  -v "$HOME/gpp:$HOME/gpp" -v /var/run/docker.sock:/var/run/docker.sock \
  ghcr.io/idan2025/gaming-platform-prns:v0.2.12 \
  platform-agent "$HOME/gpp/agent.toml" "$HOME/gpp/packs"
```

Three things in that command are load-bearing:

- **The bind is the same path on both sides.** The agent asks *your* daemon to
  mount each instance's directory, and the daemon resolves those paths on the
  host — it cannot see inside the agent's container. A named volume, or a
  different path either side of the colon, produces a game server that starts
  with no game files rather than an error.
- **`--user`.** The image's own user is `mesh` (uid 10001), while `data_root`
  and its 0600 API token belong to you. Without it the agent dies on
  `reading the API token: Permission denied`.
- **`--group-add`.** `/var/run/docker.sock` is `root:docker 660`, so a
  container process outside that group cannot reach the daemon at all:
  `client error (Connect): Permission denied`.

Then read the token the agent generated (`api_token_file`, mode 0600) and paste
it into `http://localhost:4750`. `-p 4789:4789` is only needed if your config
has a `[mesh] tcp` listener for other nodes to dial.

### Host Counter-Strike 1.6 or Half-Life

A pack describes a game and can never name a container image — an image selects
the code your node executes, so that choice stays in your config
(`crates/platform-agent/src/config.rs`). This repo ships the image to point at:

```sh
docker build -t gpp/goldsrc:1 images/goldsrc
```

```toml
[games.counter-strike-16]
image = "gpp/goldsrc:1"
content_root = "/game"
content_version = "app90"
env = { HLDS_MOD = "cstrike" }
```

With `allow_content_fetch = true` and a `steamcmd_image` set, the node fetches
Steam app 90 itself — the Half-Life Dedicated Server, ~930 MB, anonymous — and
that one install contains Counter-Strike, Half-Life, DoD and TFC. Which of them
a server runs is a start argument, so it is `HLDS_MOD` in your `env` and not a
pack field. `images/sven-coop` is the same shape for Sven Co-op.

These servers run **secure**, with Steam authentication and VAC on: a player
arriving over a Reticulum link validates normally. The image registers the
server as the app the player actually owns — Counter-Strike is 10, Half-Life 70
— derived from `HLDS_MOD`. A server that claims to be app 90, the Half-Life
Dedicated Server, boots and connects to Steam and then rejects every client with
`STEAM validation rejected`. For a mod with no Steam app of its own, or players
whose Steam cannot reach Valve, `env = { HLDS_SV_LAN = "1" }` turns client
authentication off, at the cost of VAC.

### Bots

Counter-Strike 1.6 has none: Valve's Z-Bot lives in its server library and runs
only as Condition Zero. So the pack that declares bots is
`packs/condition-zero.toml` — the same steamcmd app 90, the `czero` mod, and the
nav meshes and bot profiles the bots need come with it.

```toml
[games.condition-zero]
image = "gpp/goldsrc:1"
content_root = "/game"
content_version = "app90-czero"
env = { HLDS_MOD = "czero" }
```

Counter-Strike 1.6 itself can have bots too, by installing one:
`scripts/install-yapb.sh <content-dir>` adds YaPB (pinned, digest-checked) to a
copy of your content, and the node declares it with `bots = "yapb"` in that
game's `[games.<id>]` section. That declaration is the operator's because the
bot is a binary they installed — a pack may only name bots the game ships, and
the parser refuses `bots = "yapb"` in a pack.

The node's web UI then offers a bot count when you start a server and a **Bots**
button on a running one. Both are a quota — 0 empties the server, asking twice
for 4 leaves 4 — and neither disconnects anyone. Games with no bots show no
control at all.

### Host or relay from a terminal

```sh
game-bridge server sven-coop --auto --name "My Server"   # bridge a server you already run
game-bridge client sven-coop --tcp relay.example:4242    # bind a local port and join
game-bridge relay --tcp 0.0.0.0:4242                     # carry other people's traffic only
game-bridge browse --auto                                # list what the mesh is announcing
```

`game-bridge --help` has the rest, including `sign` and `verify` for pack
signatures (`PLAN.md` §11.3).

### Which version am I running

Both UIs show it in a corner — the node's web UI after the token is accepted,
the launcher as soon as it starts (v0.2.12). Every binary also answers
`--version`, and the node's `GET /health` carries a `version` field.

## What changed recently

Full notes per release are in [`RELEASE.md`](RELEASE.md); this is the shape of
the last three.

- **v0.2.19** — the Linux launcher works at all. The AppImage bundled the build
  host's `libwayland-client`, which the host's NVIDIA EGL driver then crashed
  on, so every AppImage ever published segfaulted before drawing a window; and
  pressing Start hung on "Starting…" forever once any server had been
  remembered, because the remembered-server sweep asked the mesh about each one
  in sequence, unbounded, holding the lock the status poll needed.
- **v0.2.18** — `docker stop` stops the agent. As PID 1 it never handled
  SIGTERM, so every redeploy waited out Docker's 10-second timeout and ended in
  a kill. Game servers are separate containers and keep running, as always.
- **v0.2.17** — a TCP game's greeting is no longer lost to a race with the
  player's own identify, and a server stops answering every game datagram with
  a signed proof nothing read.

## The idea

A bridge already exists that tunnels a GoldSrc game server's UDP traffic over
Reticulum Links — end-to-end encrypted, with no port forwarding on the game
server. That proves the hard part. What is missing is many games, many nodes, and
a way to *find* servers without anyone running a directory everyone depends on.

Discovery is Reticulum announces. A server announces itself; launchers hear it
and list it. An index is a cache of the mesh that anyone can run, never the
source of truth — a server no index has heard of still joins. Identity is a
Reticulum keypair, so there is no password database anywhere. Game traffic never
passes through any platform component.

If every index and every hosted node disappeared, people who know a destination
hash keep playing and neighbours on a mesh keep finding each other. Only
convenience degrades.

**This is a server browser, not matchmaking** — closer to the old GoldSrc and
Quake server lists than to a modern ranked queue. Matchmaking needs a shared
queue, and a queue is a single point of truth the whole design exists to avoid.

## Four roles

Pick one, or several:

- **Browse** — list and filter every server on the mesh, by game and other
  criteria.
- **Host** — run a server other people can find and join.
- **Play** — join a server; the launcher wires up the transport and starts the
  game.
- **Relay** — donate transit and carry other people's traffic. A relay
  **cannot read what it carries**: Links are end-to-end encrypted and a transport
  node forwards ciphertext.

## Documents

| Document | What is in it |
| --- | --- |
| **[PLAN.md](PLAN.md)** | **Start here.** The four roles, measured wire budgets, decisions, build order. |
| [DESIGN.md](DESIGN.md) | Architecture, components, hard problems and positions on them. |
| [GAMES.md](GAMES.md) | Nine axes of per-game variation, viability tiers, the game ladder. |
| [MODES.md](MODES.md) | Dedicated, listen-server, virtual-LAN, and the unsupported case. |
| [ENGINE.md](ENGINE.md) | The pinned Prns fork: what it patches, why, and how to move the pin. |
| [CLAUDE.md](CLAUDE.md) | Instructions for agents working in this repo. |

## Which games

Anything that can bind to a LAN or a direct IP. Dedicated servers are the dense
case — Minecraft, Terraria, Valheim, Factorio, Project Zomboid, Minetest, all of
GoldSrc and Source. Games that only find peers by LAN broadcast need a virtual
LAN adapter (`MODES.md` Mode 3). Games with no direct-IP or LAN path at all —
Steam Datagram Relay, Epic Online Services P2P, console networks — are
**explicitly unsupported**, because there is nothing to bind and nothing to
bridge.

Throughput is the real ceiling, and it varies by two orders of magnitude across
games. `GAMES.md` labels every game with a minimum link class; the numbers are an
ordering until phase 0 measures them.

## Relationship to Svencoop-Prns

[`idan2025/Svencoop-Prns`](https://github.com/idan2025/Svencoop-Prns) **stays a
standalone product** with its own repo, releases, and users. Sven Co-op becomes
one game option here; this platform is never a prerequisite for running it.

Extraction is one-directional — the platform copies from it and parametrizes,
never the reverse — and a platform launcher must remain able to join a deployed
standalone server.
