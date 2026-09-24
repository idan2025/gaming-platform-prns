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
- **Tier 2 — TCP or bursty.** Minecraft Java, Terraria, and Source (TF2, CS:S,
  Garry's Mod). Playable, but something about the join is a burst: for Minecraft
  the initial world/chunk transfer, for Source the map and asset download. Joins
  will be slow on anything but a TCP/Wi-Fi interface. Source is UDP and so sits
  oddly beside two TCP games — it is here because the tier is about what a link
  must sustain, not about which transport carries it, and Source's per-player
  rate is several times GoldSrc's.
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
2. ~~**Team Fortress 2 / CS:S / Garry's Mod** (Source).~~ **The pack landed
   2026-08-31**: `packs/team-fortress-2.toml`, and again no Rust changed —
   blocker B (multi-port, §3) had already been paid for, which is exactly what
   made this rung data. It is the first shipped pack with `[[extra_ports]]` and
   so the first to announce framing generation 2; RCON rides channel 1 as TCP
   beside a UDP game port, on the same port *number*, because a channel is what
   separates them on the wire. `second_game.rs::a_multi_port_game_is_data_too`
   and `ports::tests::every_shipped_pack_gets_its_whole_port_set_or_nothing`
   pin the two halves, and neither names the game.

   **What is left is the runtime**, which was always the operator's: a Source
   dedicated server image, in `[games.team-fortress-2]`. A pack cannot name one
   (§1), so the ladder's step-2 claim is honestly "the pack is data", not "TF2
   runs on any node today". CS:S and Garry's Mod are two more files whenever
   somebody wants them, the same way DoD and TFC are.
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
  dedupes content **per game**, not per instance. **Port sets built 2026-08-31**
  (`ports.rs::acquire`, all-or-nothing): one host port per port the pack
  declares, each published in its own transport, recorded on the container in
  `PORTS_LABEL` so a restarted agent gives the same answer as the one that
  created it.
- `platform-api` filters the deploy catalog by what a node can actually run:
  link class, runtime arch, and whether the operator supplied content credentials.

## 9. Roadmap — which games, in which order

Recorded 2026-09-24, after a live run over an Internet-exposed TCP interface
held 40–50 ms. At that latency the link is not the constraint; what a game
needs from the platform is. Waves are ordered by **how much new Rust each game
forces**, the same rule as §7: data first, new protocol words later.

A game can be embedded at all only if **all** of these hold:

- A **dedicated server** that runs headless in a container.
- The client can **join by `host:port`** — the player's bridge is `127.0.0.1`.
- It speaks **UDP or TCP** (both relays and multi-port are built, §2, §3).
- Joining does **not** need a central service to broker the connection. Steam
  ticket validation is fine (a bridged GoldSrc player validates, `CLAUDE.md`);
  Steam Datagram Relay, P2P lobbies, or a vendor token that must approve the
  server are not — that is Non-negotiable #1.

Nothing in this section is measured yet. Store app ids and default ports are
deliberately not written here: look each one up and put it in the pack, with a
`file:line`-grade source, when the pack is written.

### Wave 0 — shipped

Sven Co-op, Half-Life DM, Counter-Strike 1.6, Condition Zero (zbot), and the
Team Fortress 2 pack (runs once an operator supplies a Source image). OpenTTD
since v0.2.20, both as a server and as the first LAN-room game.

### Wave 1 — pure data, no Rust

Every one of these is a `.toml` and, for Source, an operator image. `query =
"a2s"`, `console` and `LaunchKind` already have the words.

- **GoldSrc on app 90:** Day of Defeat, Team Fortress Classic, Opposing Force.
  The mod is `HLDS_MOD`; mind the `SteamAppId` rule (must be the app the player
  owns).
- **Valve Source:** Counter-Strike: Source, Garry's Mod, HL2 Deathmatch, Day of
  Defeat: Source, Left 4 Dead 2.
- **Non-Valve games on Source** — the cheapest non-Valve wins in the whole
  list, because they inherit A2S, the `source` console and the Source launch
  kind: Insurgency (2014), Day of Infamy, No More Room in Hell, Fistful of
  Frags, Black Mesa, Zombie Panic! Source, Pirates, Vikings & Knights II,
  NEOTOKYO, Nuclear Dawn, Synergy. Check per game that the dedicated server is
  an anonymous steamcmd pull; one that is not is a `manual` pack (§5).

One shared Source image, parametrized by operator env the way `images/goldsrc`
takes `HLDS_MOD`, would serve most of this wave.

### Wave 2 — non-Steam, `archive` driver, no query

Free or open-source servers, fetched by digest (`PLAN.md` §11.2). `query` is
omitted (`pack.rs:82`), so `players_now` is `None` and the reaper exempts them.
All tier 1 UDP, the best fit after GoldSrc.

- **Arena shooters:** Xonotic, OpenArena, Urban Terror, Warsow/Warfork, Red
  Eclipse, Cube 2: Sauerbraten, AssaultCube, Unvanquished.
- **Classic id-tech:** QuakeWorld (mvdsv, shareware data), Doom via Zandronum
  or Odamex with Freedoom. Commercial Quake/Doom/UT data is the player's own and
  so a `manual` pack on the node.
- **Other:** Teeworlds / DDNet, Soldat, Luanti (Minetest) — which is §7 step 3.

What this wave adds: a player-count probe per protocol (Quake `getstatus`
covers most of the arena list) is optional polish, and each launch needs a
`LaunchKind` before one-click join; until then the player types the address.

### Wave 3 — new protocol words (Rust per family)

- **Minecraft Java** — TCP, JVM. Needs an SLP `QueryProtocol`, the EULA
  pre-start gate, and a `LaunchKind`. §7 step 4.
- **Minecraft Bedrock** — UDP (RakNet), free server download; PC clients can
  add `127.0.0.1`, consoles cannot.
- **Terraria / tModLoader**, **Starbound** — TCP.
- **Factorio** — UDP, free headless download.
- **OpenTTD — done 2026-09-24**, as a Mode 1 pack and the first Mode 3 LAN
  room game (`packs/openttd.toml`, `PLAN.md` §14.3 step 3).
- **Mindustry**, **OpenRA**, **Battle for Wesnoth**, **Hedgewars**,
  **Freeciv**, **Veloren** — open source, TCP or UDP, low rate; strategy games
  are the kindest traffic in this file.

### Wave 4 — heavier Steam survival/sandbox

Anonymous-steamcmd dedicated servers with direct-IP join, but tier 2–3 rate
and multi-gigabyte content: Project Zomboid, 7 Days to Die, Barotrauma,
Unturned, Space Engineers, V Rising, Satisfactory (TCP API beside UDP game —
multi-port), Sons of the Forest, Valheim. Each must declare
`min_link_class = 3` where it earns it; fine over this Internet interface,
never over radio (§4). Verify direct-IP join per game before writing the pack —
several of these default to Steam networking.

### The LAN back catalogue — Mode 3 rooms

Games that only find each other by LAN broadcast, the Hamachi and Tunngle
catalogue. **The platform half is shipped (v0.2.20, `PLAN.md` §14)**: rooms in
the launcher, on Linux and Windows, portable or installed, leaving nothing
behind. Each game now needs only a pack with a `[lan]` block, and what makes
one hard is knowing its ports and testing it with people.

- **OpenTTD — shipped**, played end to end by CI with a real server and client.
  `tested = false` until a person has played it in a room.
- **Need for Speed: Most Wanted (2005) — next** (`PLAN.md` §14.3 step 6): two
  Windows players over the Internet interface. Needs its LAN ports, captured
  while hosting, or a first pack with `inbound = "any"` narrowed afterwards.
- **Underground 1/2, Carbon, Hot Pursuit 2** — same family, after Most Wanted.
- **Saints Row 2 (PC)** — only if a LAN or direct-IP path survived GameSpy's
  shutdown; check before writing a pack.
- **Saints Row The Third / IV** — co-op is Steam networking (Mode 4) as
  shipped. A player who replaces the game's Steam layer with a LAN emulator on
  their own copy produces ordinary LAN traffic a room carries; the platform
  never ships, names or links to such an emulator.
- **Warcraft III, StarCraft, Age of Empires II** and the rest of the RTS era —
  packs, once the NFS test has shown a commercial game through a room.
- **IPX-era games** (NFS III, early Command & Conquer) need a layer-2 room,
  which is deliberately not built (`MODES.md`).

### Not embeddable, and why

- **A vendor token must approve the server:** Counter-Strike 2 public servers
  (GSLT), Don't Starve Together (Klei cluster token), games on EOS/PlayFab-only
  session auth.
- **No dedicated server, or P2P/Steam-networking-only join:** Stardew Valley,
  Core Keeper, most co-op titles without an IP field. **Jump Space** probably
  belongs here: its developer describes peer-to-peer lobbies with one player
  hosting and no dedicated servers, with no mention of direct-IP or LAN play.
  A packet capture on the host while a friend joins would settle it.
- **Server files require a login:** not refused — they are `manual` packs on a
  node whose operator owns the game (§5). Central hosting cannot offer them.

A TCP interface carries every datagram in order, so one lost segment stalls
everything behind it. Tier 1 games absorb it; Source and wave 4 show it as
spikes under loss even when mean latency reads well.
