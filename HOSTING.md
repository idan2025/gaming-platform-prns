# Running the host side

One container, managed from a browser. It runs game servers as **sibling**
containers on the same Docker daemon, so one copy of a game's files serves every
instance of it.

This is a convenience, never a dependency. Two launchers on a shared Reticulum
interface still work with none of this (`DESIGN.md` §0).

## Quick start

```sh
cp crates/platform-agent/agent.example.toml ./agent.toml
mkdir -p ./packs
sudo mkdir -p /var/lib/gaming-platform-prns
```

Edit `agent.toml`:

```toml
data_root = "/var/lib/gaming-platform-prns"   # the same path the compose file binds
api_bind = "0.0.0.0:4750"                     # loopback inside a container is the container
api_token_file = "/var/lib/gaming-platform-prns/api.token"
```

**Signing in to the page.** The first time you open the page it asks for the API
token once, with a username and password, and makes your account. From then on
you sign in with the username and password. Under **Account** you can change the
password, turn on codes from an authenticator app (scan the QR code, type back
a code), and add or remove accounts. Accounts are kept in
`<data_root>/web-accounts.json` (owner-only, passwords as Argon2id hashes). The
API token keeps working for scripts and a controller, and it is the way back in
if the phone with the codes is lost: "Use the API token instead" on the sign-in
page.

The container has to run as you, and in the `docker` group. Put your own ids in
`docker-compose.yml` (`user:` and `group_add:`), and make the data root yours:

```sh
id -u; id -g                              # -> user: "<uid>:<gid>"
getent group docker | cut -d: -f3         # -> group_add: ["<gid>"]
sudo chown "$(id -u):$(id -g)" /var/lib/gaming-platform-prns
```

Neither is optional and neither fails gently. The image's own user is `mesh`
(uid 10001) while `data_root` and its 0600 API token are yours, so without
`user:` the agent dies on `reading the API token: Permission denied`; and
`/var/run/docker.sock` is `root:docker 0660`, so without `group_add:` it cannot
reach the daemon at all — `client error (Connect): Permission denied`.

Then:

```sh
docker compose up -d
sudo cat /var/lib/gaming-platform-prns/api.token   # paste this into the UI
xdg-open http://localhost:4750
```

A node run with plain `docker run` needs the same two flags — `--user` and
`--group-add` — plus a bind whose path is identical on both sides. `README.md`
has that form of the command.

## What the UI does

- **Games** — every pack this node has, and whether it can actually run each
  one. A game with no `[games.<id>]` runtime in your config is shown greyed out
  **with the reason**, not hidden: a pack describes a game, and you decide which
  image runs it.
- **Add a game** — paste a pack or give a URL. It is installed and live without
  a restart. If your `[pack_trust]` policy refuses it, it lands on disk, does
  not run, and the UI says which and why.
- **Start a server** — name, max players, a starting map, and optionally a
  fixed host port. Otherwise the node picks from your `port_range`. Leaving the
  map blank keeps the image's own default; a name given here reaches the game as
  `GPP_MAP`.
- **Servers** — what is running, with players and uptime, and
  change-map/restart/stop/remove. "Players: —" means the game could not be
  asked, which is not the same as zero.
- **Change map** — talks to the running server's own console, so the players
  stay connected: it is `changelevel`, not a restart. It needs two things, and
  the button says which is missing when it is greyed out: a server that is
  actually running, and a pack that declares which console its game speaks
  (`console = "goldsrc"` or `"source"`). A pack names the protocol and never the
  command — the words live in `crates/game-bridge/src/console.rs`, for the same
  reason a pack cannot name an image.

  A server started by an agent older than this one has no console to talk to:
  Docker decides at *create* whether a container has stdin, and it cannot be
  added afterwards. Recreate the server once and it will accept map changes.

### One node owns the interfaces; every server shares it

A node runs **one** Reticulum node — a hub — that holds every interface, and
each game server joins it as a *shared instance*. This is the engine's own
model, the same one `rnsd` uses to let several programs on a machine share one
mesh connection.

It is not tidiness. **Interfaces are scarce per host in a way destinations are
not**: a point-to-point UDP interface binds one local port, a TCP server binds
one, an RNode owns one serial device. Destinations cost nothing — a machine can
announce as many as it likes. So a node running six servers must not be six
Reticulum nodes each wanting its own copy of the same link.

Before this, it was exactly that, and the failure was quiet: the first server to
start got the UDP port and the rest got `AddrInUse`, kept running, kept
announcing over whatever else they had, and simply were not on that link. The
server looked healthy from every angle except the one that mattered.

The hub is a **relay** session — interfaces and a transport identity, no game
and no destination of its own. It has to forward, because a bridge joined to it
holds no interfaces and every packet in either direction crosses it.
`shared_instance_port` in `[mesh]` is the loopback port it serves on; it
defaults to 37429 rather than RNS's own 37428, so a node sharing a machine with
`rnsd` does not quietly take over that machine's mesh.

Interfaces added at runtime go straight onto the hub, so running servers pick
them up with no restart.

### UDP does not carry a game to a stock RNS peer

Measured, not assumed, and recorded so nobody spends an evening on it twice.
With the destination held constant and **only the transport changed**, a link
to a stock `rnsd` peer over `UDPInterface` carried announces (316 B) and A2S
queries (~100 B) perfectly while a game connect stalled partway through, every
time. TCP and Backbone over the same route carried it.

Two explanations were tried and **both were wrong**, which is the useful part:

* RNS's `UDPInterface` sets `HW_MTU = 1064`, well under this platform's
  1967-byte link plaintext (`ENGINE.md`) — but that is not a receive buffer.
  RNS reads through `socketserver.UDPServer`, whose `max_packet_size` is 8192.
* Raising `HW_MTU` to 4096 and the bitrate to 1 Gbps, via a custom interface
  module and confirmed live in `rnstatus`, did not help.

So the mechanism is still unknown. What is established is the measurement.
**Use Backbone for a hub link** — it is what RNS built for the job, declares a
1 MB frame size and a gigabit rate, and is what a hub's listener already is.

**A containerised peer must publish the UDP port** — worth knowing because it
wastes an evening otherwise. An outbound datagram leaves a container through NAT
whether or not a port is published, so a one-way flow looks like a healthy
interface from the sending side while nothing can arrive. If a UDP interface is
configured correctly at both ends and carries no announces, check `docker ps`
for `<port>/udp` before touching either config.

None of this caps connections. A Reticulum interface is a transport link, not a
per-connection socket: one interface multiplexes every destination, every link
and every player on it.

## Three things worth understanding before you run it

### `data_root` must be the same path on both sides

The agent asks the **host's** Docker daemon to bind-mount each instance
directory, and the daemon resolves those paths on the host — it cannot see
inside the agent's container. So bind an identical path on each side:

```yaml
- /var/lib/gaming-platform-prns:/var/lib/gaming-platform-prns
```

**A named volume does not work**, however tidy it looks: Docker would place it
at `/var/lib/docker/volumes/<name>/_data` while the agent calls it `/data`, and
game containers would be handed `/data` from the host root. Docker creates a
missing bind source as an empty directory rather than failing, so the symptom is
not an error — it is a game server starting with no game files. The agent warns
at startup when it detects this shape.

### The Docker socket is root on the host

Anything that can reach `/var/run/docker.sock` can start a container with the
host filesystem mounted. A process holding it is not meaningfully isolated; it
is root wearing a container. That is what running containers requires, but it
means the API in front of it deserves the care you would give a root shell.

The API token is the only thing between the network and that socket. The agent
generates it on first run and **refuses to bind off loopback without one**. The
compose file publishes to `127.0.0.1` deliberately; if you widen that, put TLS
and a real proxy in front, because the token travels in a header and plain HTTP
hands it to anyone on the path.

### Game servers are kept apart from each other

Every game server joins one Docker network the agent creates, `gpp-games`,
with container-to-container traffic turned off. A server cannot reach another
server, the agent, or any other container on the machine directly; the agent
reaches each game through the network's gateway (this machine's own address
on it) and the game's published port, which works whether the agent runs on
the host or in a container. Each game container also drops every Linux
capability, can never gain one (`no-new-privileges`), and may hold at most
512 processes and threads.

- A network called `gpp-games` that **does** let containers talk is refused,
  with the command to remove it; the agent then makes its own.
- Servers created by an older agent stay on Docker's default bridge until
  they are recreated (a move onto a new copy does this). They keep working:
  the agent reaches them through their published port too.
- **One server cannot fill the disk for the others** if you set
  `writable_limit_bytes` in a game's `[games.<id>]`: the agent measures each
  server's own files every minute, stops one that is over, and refuses to
  start it until it is back under. Nothing is deleted.
- **What this does not do: keep a game off your home network.** A game
  server still reaches the internet (Steam needs it) and, like any
  container, your LAN. If strangers run servers on this machine, block
  container traffic to your LAN in the host firewall (Docker's
  `DOCKER-USER` chain).

### Game files are not included

A pack describes a game; it does not ship one. `[content]` drivers can fetch
what is fetchable — `steamcmd` for anonymous Steam apps, `archive` for a URL
with a digest — but only if you set `allow_content_fetch = true`, and anything
needing credentials or a licence click stays manual. Sven Co-op is manual.

## Adding a game an image does not exist for

A pack can never name a container image, because an image selects the code your
node executes — that choice is yours, in `agent.toml`:

```toml
[games.half-life]
image = "your/goldsrc-server:tag"
content_root = "/game"
content_version = "1.0"
```

Until that section exists, the UI lists the game and says exactly this.

## Counter-Strike 1.6 and Half-Life: one image, one download, two games

`images/goldsrc` is a bare Half-Life dedicated server (HLDS) the same shape as
`images/sven-coop`: no game files, run whatever the node mounted at
`content_root`. Build it, then point one or both packs at it:

```
docker build -t gpp/goldsrc:1 images/goldsrc
```

```toml
[games.counter-strike-16]
image = "gpp/goldsrc:1"
content_root = "/game"
content_version = "app90"
env = { HLDS_MOD = "cstrike" }

[games.half-life]
image = "gpp/goldsrc:1"
content_root = "/game"
content_version = "app90"
env = { HLDS_MOD = "valve" }
```

Both packs fetch steamcmd app 90 — the Half-Life Dedicated Server, about 930 MB
— and the mod directory is the only difference between them, which is why it is
`env` here and not a pack field: *which* mod a server runs is a start argument,
and start arguments are the node's. Note that the two games are separate
`content_version` trees under `data_root/content/<game id>/`, so running both
downloads app 90 twice; a node short on disk can host one and leave the other
unconfigured.

Two things the image does for you, both of which are the difference between a
server and a puzzling log:

- **`SteamAppId`, set to the app your players own.** Without it in the
  environment the engine loads the map and dies on the last line with
  `FATAL ERROR (shutting down): Unable to initialize Steam`; the mod's own
  `steam_appid.txt` is not a substitute. With it set to 90 — the *dedicated
  server* app — everything looks healthy and every client is turned away with
  `STEAM validation rejected`, because their session ticket is for
  Counter-Strike (10) or Half-Life (70). The image derives it from `HLDS_MOD`.
- **`~/.steam/sdk32/steamclient.so`.** HLDS dlopens it from its home directory;
  the image links it to the copy in the content mount at start, because the
  content mount is read-only and shared.

Servers here run **secure** — Steam authentication and VAC on — because a
player arriving over a Reticulum link validates like any other. If you see
`STEAM validation rejected` on the client with nothing in the server log, the
server is claiming the wrong app id: that is the bullet above, and the image
derives the right one from `HLDS_MOD`.

`env = { HLDS_SV_LAN = "1" }` turns client Steam authentication off, for a mod
with no Steam app of its own or players whose Steam cannot reach Valve. The log
then reads `VAC secure mode disabled` rather than `activated`, and you lose VAC
and the master listing with it.

The server's own name is best-effort: every mod's `server.cfg` sets `hostname`
and runs after the command line, so a Counter-Strike server calls itself
`Counter-Strike 1.6 Server` in an A2S reply no matter what you named it. The
name people browse by — the one in the announce — is the name you gave it.

## Day of Defeat, Team Fortress Classic, Opposing Force

Same image, same shape, one difference: each is its own download. A default
app 90 install carries only `valve` and `cstrike`, so these packs name their mod
in `[content]` and the agent fetches it (about 1.1–1.3 GB each, whole install).
The `env` is still yours:

```toml
[games.day-of-defeat]
image = "gpp/goldsrc:1"
content_root = "/game"
content_version = "app90-dod"
env = { HLDS_MOD = "dod" }

[games.team-fortress-classic]
image = "gpp/goldsrc:1"
content_root = "/game"
content_version = "app90-tfc"
env = { HLDS_MOD = "tfc" }

[games.opposing-force]
image = "gpp/goldsrc:1"
content_root = "/game"
content_version = "app90-gearbox"
env = { HLDS_MOD = "gearbox" }
```

Opposing Force's directory is **`gearbox`**, after the studio, not `op4`. Rebuild
the image if yours predates 2026-09-30: an older entrypoint has no app id for
`gearbox`, falls back to 90, and turns every player away with
`STEAM validation rejected` from a server whose log looks healthy.

## Source games

`images/source` is a bare Source dedicated server (srcds), the same shape as
`images/goldsrc`. The pack fetches steamcmd app 232250 — about 14 GB — and the
game directory is your `env`:

```
docker build -t gpp/source:1 images/source
```

```toml
[games.team-fortress-2]
image = "gpp/source:1"
content_root = "/game"
content_version = "app232250"
env = { SRCDS_GAME = "tf" }
```

What the image does for you:

- **`SteamAppId=440`**, the app your players own. Without it srcds runs in
  LAN mode only and nobody outside can find it.
- **A terminal for the console.** srcds ignores console input from a pipe, so
  the image gives it one; map changes from the UI need it.
- **32-bit srcds.** The install ships a 64-bit server too, but not the 64-bit
  Steam library it needs.

steamcmd sometimes fails a fresh TF2 install with `Missing configuration`
before downloading anything; the agent retries that once by itself.

Counter-Strike: Source (app 232330, about 2.3 GB) and Garry's Mod (app 4020,
about 6.5 GB) run in the same image; only the game directory differs:

```toml
[games.counter-strike-source]
image = "gpp/source:1"
content_root = "/game"
content_version = "app232330"
env = { SRCDS_GAME = "cstrike" }

[games.garrys-mod]
image = "gpp/source:1"
content_root = "/game"
content_version = "app4020"
env = { SRCDS_GAME = "garrysmod" }
```

Half-Life 2: Deathmatch (app 232370, `SRCDS_GAME = "hl2mp"`) and Day of
Defeat: Source (app 232290, `SRCDS_GAME = "dod"`) follow the same pattern.

A Garry's Mod server reports its gamemode, "Sandbox", as its game in a server
query, and an HL2: Deathmatch server calls itself "Half-Life 2 Deathmatch" in
game whatever you named it — the game sets that itself. The name players
browse by is still the one you gave it.

Six non-Valve Source games run in it too, each with its own `SRCDS_GAME`:
Insurgency (2014) `insurgency`, Day of Infamy `doi`, No More Room in Hell
`nmrih`, Fistful of Frags `fof`, Black Mesa `bms` (29 GB), and Pirates,
Vikings, and Knights II `pvkii`. Two need care:

- **No More Room in Hell** exits at start if `content_root` is only one
  directory deep (`/game`), logging `tried to ".." past the root`. Use
  something like `/opt/game` for it.
- **Fistful of Frags** maps have their own slot limits; a server with more
  slots than a map allows skips that map. 16 slots was measured to work.

**Left 4 Dead 2** runs in it as `SRCDS_GAME = "left4dead2"`. Its pack
fetches the Linux server through the Windows one (steamcmd refuses a direct
Linux install), 9.3 GB:

```toml
[games.left-4-dead-2]
image = "gpp/source:1"
content_root = "/game"
content_version = "app222860"
env = { SRCDS_GAME = "left4dead2" }
```

The image turns off its lobby-only joins so players can connect by address,
and the game fixes its own slots (4 for a co-op campaign).

Other Source games can run in this image too, but only these twelve are measured.
Set `SRCDS_GAME` to the game directory, `SRCDS_APP_ID` to the `appID` line of
its `steam.inf`, and pick a starting map — the image refuses to guess.

## Minecraft: Java Edition

`images/minecraft` runs vanilla, Paper, Fabric and Forge servers on Java 17,
21 or 25, whichever the version needs (`MINECRAFT.md`). Unlike every other
game, the server type and version are picked per server in the create form;
the node downloads each build once, from the publisher's own site, and checks
it against the digest the publisher lists.

```
docker build -t gpp/minecraft:1 images/minecraft
```

```toml
allow_content_fetch = true

[games.minecraft-java]
image = "gpp/minecraft:1"
content_root = "/game"
content_version = "paper-26.3"   # the build a server gets when none is picked
memory_limit_bytes = 8589934592  # optional: the most any one server may ask for
```

What to know:

- **The owner accepts Mojang's EULA in the create form.** The node refuses a
  server without that acceptance, and the image will not start one.
- **Releases from 1.18.1 on.** Older ones let any player run code on the
  server through its logging (Log4Shell), so a node does not host them.
- **No mods or plugins yet.** Fabric and Forge run as they come.
- **Memory is checked against the host.** A size that would leave less than
  1 GB (or a tenth of the machine) free is refused; more than half of what is
  free is a warning. Each server gets a hard memory limit of its heap plus a
  quarter plus 512 MB.
- **The first start needs the internet once**: Paper, Fabric and Forge fetch
  their libraries into the server's own folder.
- **A backup holds the whole server folder**, libraries included (about
  200 MB for Paper), so the daily backup keeps seven of those.
- **Players connect with their own Minecraft launcher** to the address
  Lanthorn shows after Join.

## Minecraft: Bedrock Edition

For players on the Minecraft app (Android, iOS, consoles, Windows). Mojang's
own server, Linux x86-64 only. The pack pins the download by its SHA-256.

```
docker build -t gpp/minecraft-bedrock:1 images/minecraft-bedrock
```

```toml
[games.minecraft-bedrock]
image = "gpp/minecraft-bedrock:1"
content_root = "/game"
content_version = "1.26.52.3"
```

Each server takes five ports from the range (TCP 19132 and UDP 19133 to
19136 inside). Players join from Lanthorn and then, in the app, Servers → Add
Server with the address Lanthorn shows, on port 19132. When Mojang releases a
new version, the pack has to be updated before players with the new app can
join. Its server has no ban: use the allowlist. See `MINECRAFT.md` §16.

## Server names

A server's name reaches the game's own console at start, so a node refuses
names containing `;`, `"`, `+`, a control character, or a word that begins with
`-` (a lone `-` is fine). Each of those was measured turning part of a name
into a console command or a start option — setting a password, or turning VAC
off. Refused, never rewritten.

## Bots: Condition Zero, and only Condition Zero

Valve's Z-Bot is compiled into the Counter-Strike server library and gated on
an internal Condition Zero flag, so on a `cstrike` server `bot_add` adds nothing
and reports nothing. Condition Zero is the same steamcmd app (90) with a
different set of depots, and it brings the bot profiles and the 55 nav meshes
the bots need — so `packs/condition-zero.toml` is the one shipped pack that
declares bots.

```
docker build -t gpp/goldsrc:1 images/goldsrc
```

```toml
[games.condition-zero]
image = "gpp/goldsrc:1"
content_root = "/game"
content_version = "app90-czero"
env = { HLDS_MOD = "czero" }
```

The start form gains a **Bots** field for a game whose pack declares them, and a
running server gains a **Bots** button beside Change map. Both send a *quota*,
not an addition: 4 means "hold four", asking twice changes nothing, and 0 empties
the server. Nobody is disconnected either way.

Two facts behind that, both measured rather than assumed:

- **`bot_quota` on the command line does nothing.** A server started with
  `+bot_quota 4` comes up empty — the bot manager is not there yet when the
  command line is parsed. So the node sets bots over the console a few seconds
  after the map loads, and the create returns before that has happened.
- **`bot_join_after_player` defaults to 1**, which keeps every bot out of the
  game until a human joins. A node that sent only the quota would leave an
  operator staring at a server that says it has bots and looks empty, so the
  two lines always go together.

A game whose pack declares no bots has no field and no button, rather than a
control that explains itself.

### Bots on Counter-Strike 1.6, with YaPB

Counter-Strike 1.6 has no bots of its own, and no amount of configuration
changes that. What can change it is installing one — YaPB is a third-party bot
that works there, and `scripts/install-yapb.sh` puts it into a copy of your
content:

```sh
# Never into a copy that servers are already running from: content is shared.
cp -a <data_root>/content/counter-strike-16/app90 \
      <data_root>/content/counter-strike-16/app90-yapb
scripts/install-yapb.sh <data_root>/content/counter-strike-16/app90-yapb
```

It downloads a pinned release, checks its SHA-256 before extracting anything,
and points `liblist.gam` at YaPB, which loads the real game library itself.

Then tell the node what its copy now has:

```toml
[games.counter-strike-16]
image = "gpp/goldsrc:1"
content_root = "/game"
content_version = "app90-yapb"
env = { HLDS_MOD = "cstrike" }
bots = "yapb"
writable_paths = ["cstrike/addons/yapb/data/train", "cstrike/addons/yapb/data/logs"]
```

**`bots` is in your config and not in the pack, deliberately.** A pack may name
only bots the game ships — Condition Zero's Z-Bot — because any node with that
game has them. YaPB is a binary *you* installed on *your* hardware, so only you
can assert it is there; a pack that could claim it would put a Bots button on
every node that had never heard of it. Same rule as the image, and
`crates/game-bridge/src/pack.rs` enforces it: `bots = "yapb"` in a pack is a
parse error.

`writable_paths` here is the same kind of escape hatch: a pack cannot know that
this node's copy has a bot that wants to cache its pathfinding. Without those two
entries YaPB logs `Unable to open Pathmatrix file for writing` on every map and
recomputes it each time; with them each instance gets its own cache. Remember
that a writable path mounts an **empty** directory over the content, so name
only directories that are empty in the install.

## Adding a worker to an index

A worker is an ordinary agent on another machine that an index places servers
on. It needs no inbound port: it dials out to a hub, and the index reaches it
over the mesh.

On the worker, with Docker running and a built `platform-agent`:

```
scripts/setup-worker.sh --index <index identity hash> --hub <hub host:port> \
    --name <node name> [--class <label>] [--region <label>] [--priority <n>] [--service]
```

**More than one hub.** Give `--hub` more than once and the worker dials each;
give the index `--tcp` more than once and it does the same. Reticulum carries
the traffic through whichever hub is up, so one hub going down does not take
the cluster with it. Measured on two LXC workers with one hub on the index's
machine and one on a worker: with the first hub stopped, logs, stop, start and
a new create all went through the second without one failed request, and
every client was back on the first hub within 4 s of it returning. A worker's
hubs live in `data/agent-interfaces.json`, the same file its page edits.

It writes `~/lanthorn-worker/agent.toml` that trusts the index, makes the
worker's identity (`agent.identity`, owner-only), points the worker at the hub,
and prints a `[[nodes]]` block. Paste that block into the index's hosting file;
the index picks it up within 10 s. It starts nothing unless you pass `--service`,
which writes a systemd user unit and leaves enabling it to you.

Add a `[games.<id>]` section to `agent.toml` for each game the worker runs. A
game with no entry cannot be placed there.

`--region` (`region = "eu"` in the node's block) says where the worker is. A
create may ask for a region (`"region": "eu"`, or the Region field on a node
page with a controller), and a worker there that answered is picked before any
other; with none there that has room, the server goes wherever it would have
gone. A move prefers the region it is leaving. The index lists its regions on
`/hosting` and its page, never which worker is in which.

The index's identity hash is in its log at start (`index identity ... identity=`),
and on the page its HTTP address serves (`/`), which also says whether it hosts
and which games, and names no worker.
Nothing the script prints is secret: the two hashes are public, and the identity
file never leaves the worker.

## Placing servers through a controller

A worker's page shows one machine. Add a `[controller]` section and the same
page places servers through an index instead, across every worker it drives:

```toml
[uplink]
identity_secret_path = "/var/lib/gaming-platform-prns/agent.identity"
trusted_indexes = ["<index identity hash>"]

[controller]
url = "https://index.example.org"        # or http://127.0.0.1:4760 on this machine
identity = "<index identity hash>"       # the same hash, pinned
```

The node logs in to the index with its uplink identity, so **the servers the
page places belong to this node's identity**, and the quotas that index keeps
apply to it. The login's signature is bound to the pinned identity: an index
the node is not pinned to cannot use it. A plain `http://` address off this
machine is refused at start, because it would send the session in the clear.

The page lists every server on every worker the index drives. Create, stop,
start, remove, logs and backups go through the index, which sends each to the
node that holds the server. **What the controller does not carry**, and what
stays on the node's own page: console, map change, changing bots on a running
server, chat, players, restart, world upload and backup download. The node
refuses those on this route, so nothing that types at a server or moves its
files is reachable from the controller. With a controller the Games page offers the games the index hosts (this node's
own setup does not count; the workers fetch the files), a server's row names its
worker and shows its address, a stopped one has **Start** where a node has
Restart, and what the controller does not carry is greyed out with the reason.
What the create form asks for travels
with the create: bots to start with, restart on crash, and a game's own
choices (a Minecraft build, its heap, the EULA, world settings).

Each action on the controller list is one request to the index. The page polls
the list every 5 s; the node shares one list from the index among every page
open on it for 4 s, and drops it whenever a change goes through, so several
open pages cost the index what one does.

## Moving a server to another worker

"Move to another worker…" in a server's menu (on a page with a controller), or
`POST /instances/<id>/relocate` on the index (`{"node": "<worker>"}` to pick
one), moves one of your servers to the worker you pick, or the one with the most
room, **among the workers of its own region** and class
(`GET /instances/<id>/move-targets` lists them). **The server keeps running
while its files are copied**, and comes up on the new worker at the **same mesh
address**.

**Another region is asked for, not taken.** A server's region is the region of
the worker it runs on, so the one it was created in is its home. To move it to
another, its owner asks (`POST /instances/<id>/region-request`, or "Ask an
admin" in the move dialog), and an admin of the index decides on the page's
**Requests** tab (`/admin/region-requests/<id>/approve` or `/deny`). On
approval the index moves it to the worker with the most room in that region,
which becomes its home; going back is another request. An approval that finds
no room leaves the request waiting and says why. Admins are identity hashes in
the hosting file:

```toml
admins = ["<identity hash>"]   # who may decide region requests; nobody by default
```

Requests are kept in `region-requests.json` beside the hosting file. One
request may wait per server, and its owner can take it back.

The move itself:

1. reads how the server was made and creates the same server on the new worker
   (the new worker picks the ports), held stopped;
2. while the old server still runs, copies all its files across (in 256 KiB
   pieces over the uplink, through a temporary file on the index's machine),
   then copies only what changed since, up to three times, until a copy is
   under 1 MiB;
3. stops the old server, copies the last changes, and gives the new server the
   old one's identity;
4. starts the new server, then removes the old one, last.

Players are down from step 3 to the game's own boot, not for the copy: the time
is the last small delta plus the boot (the index logs it as `downtime_ms`, which
is the part it can see; the game's boot comes after). A server that is not
running moves in one full copy.

Measured between two LXC workers on one Proxmox host, over the LAN through a
real index and hub, with 300 MiB of server files and a log being written to: the
whole move took 18 s and `downtime_ms` was 478. The first run logged 10275: the
game was PID 1 and ignored SIGTERM, so Docker waited out its 10 s stop timeout.
Every game container now runs under Docker's init, which hands the signal on,
so a game with no handler of its own stops in a quarter of a second. With the
real Sven Co-op server (same 300 MiB, same workers) the move took 22 s and the
server stopped answering A2S queries from the LAN for **1.4 s**, its own boot
included; the index logged 303. Two machines in different
buildings will add each piece's round trip to the copy, not to the downtime.

Until step 3 the old server is not touched. If any step before the last fails,
what was made on the new worker is removed and the old server is started again
if it had been stopped. A worker whose agent predates moves is named as too old.
The server gets a new instance id, but players' invite links name its address,
which does not change.

The index reads and writes a server's files on its workers to do this, and
**carries the server's identity**, the secret that is its address, in memory
(never written on the index). A worker that trusts an index already lets it
create, stop and remove servers there; this also lets it stand in for that one
server's address. If the old worker cannot remove the old server after a
successful move, it stays there stopped and is logged: start it and two servers
would answer to one address.

Carrying a copy over the uplink ran at about 47 MiB/s read and 108 MiB/s write
on a local hub in a release build (`a_backup_carry_is_timed`, 32 MiB, Ryzen 9
8945HS VM). A debug build ran the same code at 2.5 MiB/s: always time a release
build. A real link adds its own round trip to each 256 KiB piece, and the
pieces go one at a time, so a distant worker will be slower than the hub. The
copy is time the server is up, not down.

## Reaching the mesh: how anyone finds your server

Reticulum has no global directory. A node reaches the mesh through an
**interface**, and there are exactly two ways to get one:

1. **LAN auto-discovery** (`auto = true`). Finds neighbours on the same physical
   network with no address typed anywhere. Zero configuration, and limited to
   one LAN.
2. **A TCP address somebody told you.** Either you bind one and share it
   (`tcp = "0.0.0.0:4789"`, making your node a relay others dial), or you dial
   someone else's (`tcp = "hub.example.org:4789"`).

That is the whole bootstrap. Somebody has to already know something — an
address, or a shared LAN. It is the same problem every peer-to-peer network has,
and Reticulum does not pretend otherwise.

So a player joining your server either runs on your LAN with `--auto`, or is
given your TCP address:

```sh
game-bridge browse --tcp 192.168.1.50:4789
```

### The containerised catch

**A node in a container cannot use LAN auto-discovery.** Its "local network" is
Docker's bridge, not yours, and the multicast discovery uses never leaves it.
Tested: a browse node on the same host with `--auto` and nothing else does *not*
find a server this agent is hosting, while `--tcp <host>:4789` finds it
immediately.

So a containerised node must **bind a TCP interface and publish it**:

```toml
[mesh]
tcp = "0.0.0.0:4789"
auto = true      # harmless, and it works if you later run outside a container
```

```yaml
ports:
  - "4789:4789"   # alongside the API port
```

and then share `<your address>:4789` with anyone who should find your servers.
If you want auto-discovery to work for LAN players, run the agent with
`network_mode: host` instead of publishing ports — at which cost the port
publishing above stops applying and the game ports become the host's directly.
