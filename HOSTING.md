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
- **Start a server** — name, max players, optionally a fixed host port.
  Otherwise the node picks from your `port_range`.
- **Servers** — what is running, with players and uptime, and stop/remove.
  "Players: —" means the game could not be asked, which is not the same as zero.

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
