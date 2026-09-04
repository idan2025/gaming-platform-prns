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

Then:

```sh
docker compose up -d
sudo cat /var/lib/gaming-platform-prns/api.token   # paste this into the UI
xdg-open http://localhost:4750
```

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

### UDP mesh interfaces serve one instance per node

A TCP mesh interface *dials out*, so every instance on the node can have its
own. A **UDP one binds a local port**, and every instance is its own Reticulum
node — so the first instance to start gets the port and the rest cannot have it.
Those instances still run, still announce, and are still joinable over the
node's other interfaces; they are simply absent from that one link.

The agent reports it per instance in the Mesh panel rather than only in its log,
because nothing about the affected server looks wrong from the outside. If you
run several servers and want all of them on one link, use TCP.

There is a second half of this worth knowing, because it wastes an evening
otherwise: **a containerised peer must publish the UDP port**. An outbound
datagram leaves a container through NAT whether or not a port is published, so a
one-way flow looks like a healthy interface from the sending side while nothing
can arrive. If a UDP interface is configured correctly at both ends and carries
no announces, check `docker ps` for `<port>/udp` before touching either config.

One thing this limit is *not*: a cap on connections. A Reticulum interface is a
transport link, not a per-connection socket — one UDP interface multiplexes
every destination, every link and every player on it. The limit is instances per
node, not players per instance.

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
