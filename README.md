# gaming-platform-prns

A **decentralized server browser** for game servers over
[Reticulum](https://reticulum.network/), built on
[Prns](https://github.com/KenAKAFrosty/Prns).

Find a game server on the mesh, join it, host one, or donate transit — with no
account, no port forwarding, no central service, and no internet required.

> **Status: phase 1 complete, phase 2's browse core with it** (`PLAN.md` §8).
> The engine is pinned to a patched fork of Prns ([`ENGINE.md`](ENGINE.md)), and
> `crates/game-bridge/` carries the relay and framing parametrized by a game
> pack, the §3.3 announce record, a link allowlist, the Relay role with an off
> switch, and a Browse role that lists and filters from announces alone — no
> index, no account, no internet, proven end to end in
> `tests/browse_discovery.rs`. Still to come in phase 2: the detail probe over a
> Link, and the launcher UI. Everything past that is still plan. The working single-host implementation this generalizes is
> [`idan2025/Svencoop-Prns`](https://github.com/idan2025/Svencoop-Prns).

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
