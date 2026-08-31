# Multi-Game — what generalizing actually costs

Companion to `DESIGN.md`. The platform's premise is many games, not one. This
file says exactly what varies across game servers, which of it the current
Sven-only bridge cannot express, and the cheapest order to prove the abstraction.

## 1. Nine axes of variation

A game pack must describe all of these. Anything not in the manifest becomes
per-game Rust code, which is the failure mode to avoid.

| Axis | Range across real games | Sven Co-op today |
| --- | --- | --- |
| Content source | steamcmd (anonymous), steamcmd (account required), plain HTTP download, jar, container image | steamcmd anonymous, app 276060 |
| Runtime | native i686, native x86_64, JVM, .NET, node | native i686 (needs `lib32*`) |
| Transport | UDP only, TCP only, both | UDP only |
| Ports | one, or game + query + rcon on separate ports | one (27015/udp) |
| Status probe | A2S, Minecraft SLP, GameSpy, none | A2S |
| Admin channel | stdin pipe, RCON over TCP, none | stdin pipe |
| Config | cvar `.cfg`, `server.properties`, JSON/YAML/TOML | `server.cfg` + `mapcycle.txt` |
| Content size | ~50 MB to ~30 GB | 2.74 GB |
| Client join | console `connect host:port`, URI handler, manual paste | console connect |

Two of these — **transport** and **ports** — the current bridge structurally
cannot express. The rest are manifest fields.

## 2. Blocker A — the relay is datagram-only

`src/relay.rs` maps one client UDP source address to one Reticulum Link and
pumps datagrams. TCP games (Minecraft Java, Terraria) need a second session
model:

- Link per **TCP connection**, not per source address.
- Stream framing, not datagram framing: no message boundaries to preserve, so
  the `FLAG_FINAL` reassembly logic is wrong for it — a stream wants
  backpressure and ordered bytes, nothing else.
- Connection lifecycle maps to link lifecycle in both directions: peer closes
  TCP → close link; link drops → close TCP (the UDP path can just re-establish
  on the next packet, a stream cannot).

Treat this as a distinct `StreamRelay` alongside the existing `DatagramRelay`,
selected by the pack's `transport`. Do not try to make one code path serve both.
**Built 2026-08-31** exactly that way: `crates/game-bridge/src/stream.rs`, with
`relay.rs` branching on `profile.transport` at the two points where a socket is
created.

## 3. Blocker B — one destination carries one port

Source-engine and Minecraft servers want game + query + rcon reachable. Today a
destination fronts exactly one UDP port.

The framing header has room: `frame()` writes bit 0 (`FLAG_FINAL`) and leaves
bits 1–7 zero. Bits 1–3 can become a channel id, so one destination multiplexes
up to 8 ports.

**This is not silently backward compatible.** `Reassembler::push` masks only
`FLAG_FINAL` and ignores every other bit, so a deployed v0.1.8 peer receiving
channel-tagged chunks would happily merge two channels into one corrupt stream.
Gate it: put a protocol version in the announce `app_data` and only send a
non-zero channel to peers that advertised support. Channel 0 stays exactly the
current wire format forever.

**Built 2026-08-31**, exactly that shape. A pack declares `[[extra_ports]]` —
`channel`, `name`, `port`, `transport` — and `GameProfile::ports()` puts the
game's own port on channel 0 in front of them, so the frozen port is written
down once (`crates/game-bridge/src/profile.rs`). Four rules carry the weight:

- **A UDP extra port rides framing's channel bits; a TCP extra port rides its
  own stream id pair** — `stream_ids(channel)` in `stream.rs`, channel 0 keeping
  ids 1 and 2. A stream never passes through `frame()`, so the channel bits are
  a datagram concern and putting a stream on them would invent a second framing
  layer (`PLAN.md` §8's `StreamRelay` note).
- **Only a multi-port game announces generation 2.**
  `GameProfile::protocol_version()` derives it from `extra_ports` being
  non-empty. A single-port game announcing a capability it never exercises would
  put a number on the wire that means less than it says.
- **The client checks the peer's announce before it sends a channel id, not its
  own pack.** `relay::may_use_channel` is the gate; a legacy announce, which
  carries no version at all, reads as generation 1. Pinned by
  `a_channel_id_is_never_sent_to_a_peer_that_did_not_advertise_v2` and, on a
  real mesh, `a_client_with_extra_ports_sends_none_of_them_to_a_v1_server`.
- **A reply rides the channel its request arrived on.** The server never
  *initiates* a channel, so a v1 peer — which only ever sends channel 0 — only
  ever receives channel 0, and a chunk for a channel the pack does not declare is
  dropped rather than guessed onto a port.

Local ports are the player's, not the server's: channel 0 lands on
`listen_port` and an extra channel on `listen_port + channel` unless the caller
names one in `extra_listen_ports`. A player whose machine already runs something
on 27015 should not have a bridge fight it for the port.
`crates/game-bridge/tests/multi_port.rs` runs a game port, an RCON port and a
second UDP port across one destination at the same time.

## 4. Viability tiers — label every pack

The link budget is real and differs per game. `MAX_CHUNK` is 1900 bytes
(`src/framing.rs:24`), so a typical GoldSrc datagram rides in a single chunk —
but the *sustained rate* is what decides playability, and that varies by two
orders of magnitude across games.

- **Tier 1 — low-rate UDP tick.** GoldSrc (Sven, HLDM, CS 1.6), Quake-family,
  Minetest. Tens of kbit/s per player. Works, including over slow links.
- **Tier 2 — TCP or bursty.** Minecraft Java, Terraria. Playable, but the
  initial world/chunk transfer is a multi-megabyte burst; joins will be slow on
  anything but a TCP/Wi-Fi interface.
- **Tier 3 — modern high-bitrate.** Rust, Ark, Valheim-scale state sync.
  Hundreds of kbit/s per player. Only viable over fast interfaces, never over a
  LoRa-class link.

Each pack declares a **minimum link class**. The platform refuses to deploy — or
loudly warns — when a node's interfaces can't meet it. Shipping a tier-3 game
onto a radio link and letting the player discover it is how the platform gets a
reputation for not working.

Numbers above are ordering, not measurements. Phase 0 of `DESIGN.md` measures
tier 1 for real; each new tier needs its own measurement before it ships.

## 5. Content licensing — the non-technical blocker

Sven Co-op's app 276060 allows anonymous steamcmd. Many dedicated servers do
not: they require a Steam account that owns the game, and their binaries can't
be redistributed by a third party.

**Settled differently in the code, 2026-08-31, and the code wins.** This section
proposed a pack field `content.auth = "anonymous" | "user_supplied"`. The
`[content]` block that shipped has no such field and no `login` field at all
(`crates/game-bridge/src/content.rs`): `driver = "steamcmd"` is anonymous by
construction, and a game whose files need credentials is a `manual` pack, which
is the same answer expressed as a driver rather than a flag. A pack is a file
that gets shared, and a field for credentials is a field people put credentials
in — so a node operator who owns the game installs it themselves, on their own
node, and nothing about that reaches a pack or the platform. Central-hosted instances can only offer anonymous-pull
games; everything else is bring-your-own-node. Some games also need an explicit
EULA acceptance step (Minecraft) — that's a pack-declared pre-start gate, not
something the platform can click through on the user's behalf.

This constraint shapes the business model, so decide it before phase 3.

## 6. Pack registry is an RCE surface

A pack specifies a binary and its argv. A community-contributed pack is
therefore arbitrary code execution on whichever node runs it. Two mitigations,
both needed:

- Packs are signed; agents only run packs from trusted signers.
- Instances run containerized with no host mounts beyond their own content and
  data volumes — the same isolation the current Docker image already has.

## 7. Cheapest order to prove the abstraction

Each step is chosen to exercise exactly one new axis:

1. ~~**Half-Life DM / CS 1.6 / DoD** (steamcmd app 90).~~ **Done 2026-08-31**,
   and the claim held: `packs/half-life.toml` and `packs/counter-strike-16.toml`
   are the entire change, with no Rust touched.
   `crates/game-bridge/tests/second_game.rs` runs a bridge server from a pack
   read off disk and makes a browse node list it; `every_shipped_pack_plans_an_instance`
   does the node-side half. Which mod runs (valve, cstrike, dod, tfc) is a
   runtime argument, so DoD and TFC are two more files whenever somebody wants
   them.
2. **Team Fortress 2 / CS:S / Garry's Mod** (Source). New: x86_64 runtime,
   RCON admin channel, multi-port. Forced blocker B, which is **built as of
   2026-08-31** (§3) — what is left for TF2 itself is the runtime and the pack,
   not the transport.
3. **Minetest.** New: non-Steam content source (plain download), no A2S probe.
   Forces the probe and content-source abstractions apart.
4. **Minecraft Java.** New: TCP transport, JVM runtime, SLP probe,
   `server.properties`, EULA gate. Forces blocker A. This is the expensive one —
   do it after the cheap three have shaken out the manifest schema.

Ordering rule: never let the second game be the hard game. The manifest schema
will be wrong in ways only a second implementation reveals, and it's much
cheaper to discover that against a GoldSrc sibling than against a JVM TCP game.

## 8. Consequences for `DESIGN.md`

- `game-bridge` grows a `StreamRelay` and a channel-multiplexed framing v2 with
  announce-advertised version negotiation. **Both built 2026-08-31** (§2, §3).
- `game-pack` covers all nine axes in §1, plus `min_link_class`, `content.auth`,
  and pre-start gates (EULA).
- `platform-agent` allocates a **port set** per instance, not a single port, and
  dedupes content **per game**, not per instance.
- `platform-api` filters the deploy catalog by what a node can actually run:
  link class, runtime arch, and whether the operator supplied content credentials.
