# Minecraft servers, and moderation for every game

What it takes to host Minecraft: Java Edition on a Lanthorn node (vanilla,
Paper, Fabric, Forge) with a version picked per server, a world brought in
from elsewhere, memory a person can understand, a EULA the player accepts
themselves, and limits that keep one server from taking the host down. Also
kick, ban, the ban list and operators, which most of it applies to every game
with a console, not only Minecraft.

This is the plan before code. Where it states a fact about an upstream API, it
was read live on 2026-10-18 and the endpoint is named.

## 0. What already exists and is reused

Most of what was asked is already a node feature; Minecraft has to fit it.

| Asked for | Already there | What Minecraft needs from it |
| --- | --- | --- |
| Daily restart | `schedule.rs`, `Agent::scheduled_restarts` | "No humans on" needs a player count, so a query protocol (§5). `say` before a forced restart needs console words (§4). |
| Backups and schedules | `backups.rs` | A world written while it is copied can be torn: `save-off` / `save-all flush` before, `save-on` after (§7). |
| Session terminal | `Agent::send_console`, the log dialog | Works once the image keeps the server on stdin. The dialog follows the log live (§9). |
| Crash restart, disk limit, logs, usage | `check_crashes`, `check_disk_limits` | Nothing. A Java `SIGTERM` ends in 143, which is already a stop. |
| TCP game over Reticulum | `StreamRelay` (`stream.rs`) | Nothing: Java Edition is one TCP port. |

## 1. The rules this has to keep (`CLAUDE.md`)

- **A pack cannot name what runs.** The pack says "this game's content comes
  from the `minecraft` driver" and "its console is `minecraft`". The node's
  operator picks the image. The server type is an enum this build implements.
  No URL, command, JVM flag or image comes from a pack or a request.
- **The digest decides, not the URL.** Every jar and installer is hashed
  before it is written into the content tree, against the digest its publisher
  lists (§3). Mismatched bytes are discarded.
- **A value from a request is data.** Memory is two numbers in MiB. Version
  and loader strings pass an allowlist before they reach a URL, a directory
  name or an argument vector. A free-text field reaches a console line only
  through the existing server-name rule.
- **Refused, never repaired**, for every one of those checks.
- **Mods and plugins are out of v1.** A mod is arbitrary code on someone
  else's node. Forge and Fabric servers run with no mods until a separate
  design gives the operator a per-file approval. The world import (§6) carries
  world data only.

## 2. The pack

`packs/minecraft-java.toml`, one pack for all four server types:

- `transport = "tcp"`, `default_port = 25565`.
- `console = "minecraft"`, `query = "minecraft"` (§4, §5).
- `writable_paths = ["server"]`: the whole server directory is the
  instance's own. The content tree holds only the verified jar or installer.
- `[content] driver = "minecraft"`. No fields: the type and version are the
  player's choice per server, not the pack's.
- `[eula]` with the licence's name and its `https` URL. A pack with an
  `[eula]` cannot be created without a recorded acceptance (§8). Generic: any
  later pack whose game needs one gets the same gate.
- No `[launch]`: players use their own Minecraft launcher and connect to the
  address Lanthorn shows.

A launcher or node older than this build fails to parse the pack (unknown
`driver`) and skips it with a warning. That is the safe failure.

## 3. Versions and content

A Minecraft **build** is a type and a version: `vanilla-1.21.11`,
`paper-26.3`, `fabric-26.3-0.20.0`, `forge-1.21.1-52.1.0`. It is the content
version, so each build is its own read-only copy under
`content/minecraft-java/<build>/`, shared by every server on it. A server picks
its build at create (`InstanceSpec.content_version`). Moving servers onto new
copies (`ROADMAP.md` §12.4) does not apply: a world opened by a newer version
cannot go back, so a version changes only when the owner asks.

Where each comes from, read live:

| Type | Version list | Download and digest |
| --- | --- | --- |
| Vanilla | `piston-meta.mojang.com/mc/game/version_manifest_v2.json` (each entry carries the SHA-1 of its version file) | Version file `downloads.server.{url,sha1}`, on `piston-data.mojang.com`. Its `javaVersion.majorVersion` picks the Java runtime. |
| Paper | `fill.papermc.io/v3/projects/paper` | `/versions/<v>/builds/latest`: `downloads["server:default"].{url,checksums.sha256}`, on `fill-data.papermc.io`. |
| Fabric | `meta.fabricmc.net/v2/versions/loader/<mc>`, `/v2/versions/installer` | Installer jar on `maven.fabricmc.net` with its `.sha256` beside it. |
| Forge | `files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json` | `forge-<mc>-<forge>-installer.jar` on `maven.minecraftforge.net` with its `.sha256` beside it. |

Rules:

- **Hosts are an allowlist in code** (`MINECRAFT_HOSTS`), `https` only. A
  redirect off the list is refused.
- **Digest first**: SHA-1 where Mojang only publishes SHA-1, SHA-256
  everywhere else. The listed digest arrives over TLS from the same
  publisher; that is the trust, and the docs say so.
- **Stage and rename**, as every driver does, so an interrupted download never
  looks installed. The copy carries `lanthorn-build.json` (type, version,
  loader, Java major, file name) and an empty `server/` mountpoint.
- **What the jar fetches itself is the jar's business.** Paper patches the
  Mojang jar on first start, and the Fabric and Forge installers fetch their
  libraries and check them themselves. That happens in the server's own
  directory at first start, so the container needs outbound HTTPS once.
- **A node offers only versions its upstream lists**, from
  `GET /minecraft/versions?server=<type>`, cached for an hour.

## 4. Console words and moderation, for every game with a console

`ConsoleProtocol::Minecraft` joins `Goldsrc` and `Source`. `change_map` refuses
on it (Minecraft has no map rotation). `say` is the same rule as today.

Moderation is new for all three, as console words this file owns:

| Action | Minecraft | GoldSrc / Source |
| --- | --- | --- |
| Kick | `kick <name> [reason]` | `kick "<name>"` |
| Ban | `ban <name> [reason]` | `banid 0 <steamid> kick` |
| Unban | `pardon <name>` | `removeid <steamid>` |
| Op / deop | `op <name>` / `deop <name>` | not offered |
| Ban list | the game's own `banned-players.json` | the node's record (below) |
| Operator list | the game's own `ops.json` | not offered |

- **Targets are validated per protocol and refused, never rewritten.** A
  Minecraft name is `[A-Za-z0-9_]{1,16}`. A Steam ID is `STEAM_X:Y:Z` with
  digits. A GoldSrc/Source kick target is the player's name under the
  server-name rule plus no `"`; a name that fails cannot be kicked by name.
  The ban needs the Steam ID: a name can be anything, and a ban is the one
  action a wrong match makes permanent.
- **A reason is free text under the server-name rule** (no `;`, `"`, `+`,
  control characters).
- **The Steam engines' ban list is the node's.** `writeid` writes `banned.cfg`
  into the game's directory, which is the read-only shared copy, so the node
  keeps `bans.json` beside the instance and sends `banid` for each entry after
  every start, the same moment bots get their quota. Minecraft keeps its own,
  and the node reads it.
- **Loopback API only**, like the console route: an index can never kick or
  ban on someone's node.

## 5. Player count

`QueryProtocol::Minecraft` is Server List Ping: a TCP handshake to the game
port, a status request, a JSON reply with the online and maximum count and a
sample of names. It feeds the same `players_now` A2S does, so "when empty"
(scheduled restarts, reaping, moves) works, and "silence is not zero" still
holds: no answer is `None`.

## 6. World import

`POST /instances/:id/world`, streamed like the backup import.

- **Only while the server is stopped.**
- The upload is held to a `Content-Length`, the disk reserve and 4 GB, and
  extracted under `content::extract`'s path and link checks into a scratch
  directory beside the instance.
- **A world is a directory with `level.dat` at its top, or under exactly one
  wrapping folder.** Anything else is refused and nothing is kept.
- **The world it replaces is backed up first**, with an agent-made name, then
  swapped out. No world is ever deleted by an import.
- A new server can start from an imported world: the page creates it without
  starting (`InstanceSpec.start = false`), uploads, then starts it.

## 7. Backups of a running world

When the game's console protocol has them, a backup of a running server sends
`save-off` and `save-all flush`, waits for the flush, copies, and sends
`save-on` whether the copy succeeded or not. GoldSrc and Source have no world
to flush and keep today's path.

## 8. EULA

- The create form shows the licence's name, a link to it, and an unticked box.
  Create and Start stay disabled until it is ticked.
- The spec carries `eula_accepted_at` (seconds). For a pack with `[eula]`,
  `InstanceSpec::validate` refuses a spec without it, at the node and again at
  the index.
- The image writes `eula=true` only when `GPP_EULA_ACCEPTED_AT` is set, and
  otherwise writes `eula=false` and exits with a sentence saying why. Nothing
  accepts on a player's behalf.

## 9. Memory, CPU and the host

`InstanceSpec.memory = { min_mb, max_mb }`, Minecraft only (a pack whose
content driver is not `minecraft` refuses it). The page offers presets with a
line each:

| Preset | Xms / Xmx | Says |
| --- | --- | --- |
| Small | 1 GB / 2 GB | A few friends, vanilla or Paper. |
| Medium | 2 GB / 4 GB | Up to about 20 players. The default. |
| Large | 4 GB / 6 GB | Bigger groups, or a Forge or Fabric server. |
| Custom | yours | "Xmx is the most memory the server may use. Xms is what it takes at start; keep it at or below Xmx." |

The container gets a hard memory limit of `max_mb × 1.25 + 512 MiB`, so a JVM
past its heap is stopped by the kernel before it can take the host.

The node judges every request against its own `MemAvailable` (`/proc/meminfo`)
and CPU count, through one function the page asks first
(`GET /resources/check`) and `create` asks again:

- **Refuse** when the container's limit would leave less than the reserve
  free: 1 GiB, or 10 % of the host's memory if that is larger. Refuse
  `min_mb > max_mb`, and anything under 512 MiB.
- **Warn** when the limit is more than half of what is available now.
- **CPU**: `InstanceSpec.cpus` is optional. Refuse more than the host has,
  warn above half.
- A running server is never resized. A change takes effect on the next start,
  and the page says so.

## 10. Player slots and server settings

Slot presets 10, 20, 50 and 100, plus custom up to `max_players`' 255. Server
settings are a typed set the image writes into `server.properties` at every
start: `gamemode`, `difficulty`, `pvp`, `white-list`, `online-mode` (on unless
turned off, with a warning that off lets anyone use any name), MOTD (the
server name, under the name rule and escaped for the properties format), and
the port and player count from the spec. Everything else in the file is left
as the owner set it.

## 11. The image

`images/minecraft`: Debian slim with Eclipse Temurin JREs 8, 17, 21 and 25,
chosen by the build's Java major. The entrypoint:

1. refuses without `GPP_EULA_ACCEPTED_AT`;
2. on first start, or when the build changed, prepares the server directory
   from the read-only copy: copies the jar (vanilla, Paper), or runs the
   installer (`--installServer` for Forge, `server` for Fabric) into it;
3. writes the managed `server.properties` keys;
4. `exec`s Java with `-Xms`/`-Xmx` from the spec and `nogui`, so `docker stop`
   reaches the JVM, which saves the world.

## 12. Build order

Each step is one commit with its tests.

1. `game-bridge`: the `minecraft` content driver, `[eula]`, the build id and
   its validation, `ConsoleProtocol::Minecraft`, the moderation words and their
   target rules, `QueryProtocol::Minecraft` (Server List Ping).
2. `platform-agent`: version listing and provisioning (allowlist, digest,
   stage); the new spec fields and their validation; memory, CPU and EULA
   gates and the container limit; create without start; world import;
   moderation routes and the Steam engines' ban record; save hooks around
   backups; the player count.
3. `images/minecraft`.
4. The node's web UI: the create form (type, version, slots, memory, EULA,
   world), Players (kick, ban, banned list, operators), a live terminal.
5. The pack, its art, the pack review.
6. A real run on this machine: Paper and vanilla start, a Minecraft client
   joins through Lanthorn, a world imports, a backup restores, a ban sticks.

## 13. Tests that must exist

- A digest mismatch on a jar or installer installs nothing.
- A host off the allowlist, or a redirect to one, is refused.
- A build id with a path, a space, or an unknown type is refused before any
  URL is built.
- A spec for an `[eula]` pack without acceptance is refused; the image's
  `eula.txt` is `true` only with one.
- Memory: the reserve refusal, the half-of-available warning, `min > max`,
  and the container limit actually set.
- World import: a path escape, a link, no `level.dat`, or two worlds is
  refused, and nothing changes; a good one replaces the world and the old one
  is a backup.
- Moderation: a newline or `;` in a target or reason never reaches a console
  line; op is refused on a Steam engine; a Steam ban survives a restart.
- Server List Ping against a stand-in server: a count, and silence as `None`.

## 14. Not in v1

Mods and plugins. Bedrock (UDP 19132). Modpacks. Velocity or BungeeCord
proxies. RCON. A version change on an existing world.
