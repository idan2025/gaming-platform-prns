# Connection modes — how "any game" actually reaches Reticulum

Companion to `DESIGN.md` and `GAMES.md`.

The goal is not GoldSrc. It is **any game that can be bound to a LAN or a direct
IP**. That splits into four modes with very different costs. Mode 1 is built.
Mode 2 is nearly free. Mode 3 is the big one and unlocks the LAN-party back
catalogue. Mode 4 is impossible and must be said out loud.

## The two Prns primitives this rests on

Measured from the vendored engine, not from the README (which is stale here):

| Primitive | Plaintext cap | Delivery | Use for |
| --- | --- | --- | --- |
| **Link** (`MAX_SEND_TO_LINK_PLAINTEXT_LEN`, `link_mdu(2048)` — our patch; upstream `link_mdu(500)` = 431, `ENGINE.md`) | 1967 B; bridge uses `MAX_CHUNK = 1900` | reliable, ordered, retransmitted | all unicast game traffic |
| **Group** (`MAX_SEND_GROUP_PLAINTEXT_LEN`) | **383 B** | **fire-and-forget, cannot prove** | LAN broadcast/multicast emulation |

383 B derives from `BROADCAST_MTU = 500` → `BROADCAST_MDU = 464` → minus the
ephemeral pubkey (32) and token overhead (48), rounded to AES blocks, minus one.
The `send_group.rs` comment is explicit: *"a GROUP cannot prove, so the send is
fire-and-forget."*

A shared-key GROUP destination is exactly the right shape for a virtual LAN's
broadcast domain — one send reaches every member. It is also small and lossy,
which bounds what Mode 3 can do.

## Mode 1 — Dedicated server (built)

Game ships a headless server. Platform runs it on a node; the bridge fronts its
port with a Reticulum destination; players run a bridge client and connect to
`127.0.0.1:<port>`.

Covers most of what people actually want to host: Minecraft (Java + Bedrock),
Terraria, Valheim, Factorio, Project Zomboid, 7 Days to Die, Satisfactory,
Minetest, all of GoldSrc/Source. This is where the platform's value is densest —
finish it before touching Mode 3.

Needs from `GAMES.md`: the stream relay (TCP games) and channel multiplexing
(multi-port games).

## Mode 2 — Listen server / direct-IP join (cheap)

No dedicated binary; one *player* hosts, others join by typing an IP. Same relay
as Mode 1, but the "node" is somebody's desktop and the instance lifecycle is
"while the host is playing", not "until stopped".

Cost is mostly product, not protocol: the launcher must be able to announce a
destination from a player's machine, and the directory must show ephemeral,
player-hosted servers distinctly from persistent ones. Do this right after Mode 1.

## Mode 3 — Virtual LAN (the LAN-party back catalogue)

For games with **no direct-IP entry** — they only find peers by UDP broadcast on
the local subnet. Age of Empires II, StarCraft, older RTS and shooters, a large
slice of Diablo/Warcraft-era LAN play. This is the Hamachi/ZeroTier use case,
done over Reticulum.

### Design

- **Virtual adapter per member.** L3 TUN, not L2 TAP. TAP doubles overhead and
  only buys non-IP protocols (IPX, NetBIOS); defer that until a specific target
  game demands it.
- **Deterministic addressing.** Derive each member's virtual IPv4 from its
  Reticulum identity hash inside a CGNAT-ish range. No DHCP, no central
  allocator, works offline; the group coordinator only arbitrates the rare
  collision.
- **Unicast → Link.** Ordinary IP traffic between two members rides a Reticulum
  Link, reusing the existing framing. Reliable and 1900-byte chunked.
- **Broadcast/multicast → GROUP.** Packets to `255.255.255.255`, the subnet
  broadcast, or local multicast go out on the LAN group's shared destination.
  One send, every member, and E2E encrypted by default — which is a genuine
  advantage over Hamachi, not a parity feature.
- **TUN MTU ~1400.** Keeps one IP datagram inside one link chunk (1900) with
  headroom for framing and IP headers. Fragmenting at both the IP layer and the
  bridge layer would be miserable to debug.
- **No ARP.** L3 TUN sidesteps it entirely.

### The thing that will kill it if ignored

**Broadcast storms.** LAN games beacon constantly, some once per second per
scanned port. Group sends are 383 B, unacknowledged, and hit every member — so
cost scales with member count and is pure overhead on a constrained mesh.
Mandatory from the first commit, not bolted on later:

- per-source rate limit on group sends,
- dedupe identical consecutive beacons,
- a hard ceiling on group traffic as a fraction of the link budget,
- **answer discovery locally where possible** — synthesize beacon replies on each
  member from directory data instead of forwarding the real broadcasts. This is
  the single biggest lever; a game that can be told about peers out-of-band
  doesn't need its broadcasts relayed at all.

Anything over 383 B cannot be reliably split across group sends (fire-and-forget,
no retransmit). Drop it and log, or move that flow onto a Link. Do not build a
reassembly layer on an unreliable primitive.

### The distribution cost

A TUN adapter is not a user-space binary any more:

- Linux: `CAP_NET_ADMIN` or root.
- Windows: Wintun driver, admin install, code signing.
- macOS: `utun`, entitlement or root.
- Android/iOS: `VpnService` / `NetworkExtension` — ironically the *cleanest*
  path, and a real reason to consider mobile early.

The current app is a plain unprivileged binary. Mode 3 changes the installer,
the signing story, and the support burden. Which is why Mode 3 comes after Modes
1 and 2 have users, and why **port mapping stays the default** — only games that
genuinely require broadcast discovery should pull in a virtual adapter.

## Mode 4 — Publisher relay only (impossible, say so)

Games with no direct-IP or LAN path at all: Steam Datagram Relay, Epic Online
Services P2P, console platform networks. Traffic is addressed to the publisher's
relay, not to a peer. There is nothing to bind to a LAN and nothing to bridge.

The catalogue must mark these **explicitly unsupported**. A platform whose pitch
is "any game" and whose reality is "not that one, or that one" burns trust fast.
Naming the boundary is what makes the rest credible.

## Anti-cheat, honestly

EAC/BattlEye/VAC-class anti-cheat may flag virtual adapters, block LAN play, or
refuse to run against a non-official server. This is a per-title fact, not
something the platform can engineer around. Each pack carries a tested-status
field; untested titles say untested rather than implying they work.

## Ordering

1. Finish Mode 1 (dedicated) across the `GAMES.md` ladder.
2. Add Mode 2 (listen server) — small protocol delta, large catalogue gain.
3. Prototype Mode 3 (virtual LAN) on **one** broadcast-discovery game, with
   storm control in from the start, before generalizing.
4. Never promise Mode 4.
