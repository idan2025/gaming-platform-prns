# Hosted servers and monetization

Written 2026-10-15, from a conversation with the maintainer about `PLAN.md`
§10. The decisions in §1 are the maintainer's. Everything in §2 to §5 is a
**proposal**: the numbers are drafts to change, not commitments. §6 is the
assessment of whether the product is ready to charge, and the answer was no.

## 1. Decided

- **Node supply: two ways, both supported.**
  - **Self-hosting is free, forever.** Anyone runs a node on their own
    hardware, with whatever Reticulum hub or interface they choose, on their
    own responsibility. Nothing in this document may make that path worse:
    the zero-infrastructure baseline (`CLAUDE.md`, non-negotiable 1) stays
    whole.
  - **Hosted servers run by the project**, as a monetized platform with
    tiers, a free tier among them. These are the project's own nodes behind
    the project's own index.
- **Uplink: the project runs hubs for hosted servers**, free tier included.
  Self-hosters bring their own. The project's hubs are a convenience and are
  never required by anything a self-hoster or a player runs.
- **Monetization direction: paid hosted servers, plus add-ons.** The software
  stays free and open; no payment goes into the protocol.
- **The free tier is limited and cleans up after itself**: a server empty
  for a while stops, a server nobody starts again is eventually deleted, and
  it keeps at most 2 backups (§4).

## 2. Tiers (draft)

| | Free | Plus | Pro | Clan / Dedicated |
|---|---|---|---|---|
| Servers | 1 | 3 | 10 | a whole node |
| Players per server | small (e.g. 12) | 32 | 64 | any |
| Stops when empty | 15 min | 2 h | never | never |
| Deleted after stopped | 14 days | 60 days | never | never |
| Backups | 2 | 7 | 30, downloadable | any |
| Console, daily restart, mods | console only | yes | yes | yes |
| Region choice | no | yes | yes | yes |

Prices are set from the cost per server measured in the free beta (§6), not
before.

## 3. More ways to earn (proposed)

1. **Always-on add-on**: one server never stops, per month, on any tier.
2. **Slot and RAM packs**: more players or memory without changing tier.
3. **Dedicated node rental**: a clan gets its own machine; the project runs
   the agent on it.
4. **Managed self-hosting**: the person keeps their hardware and pays for
   remote management, automatic updates and offsite backups.
5. **Premium hub**: a paid, low-latency hub with an uptime promise that
   self-hosters can connect to.
6. **Supporter badge and donations**, which also fund the free tier.
7. **Backup retention add-on**: backups kept after a server is deleted.

### Never for sale

These would break rules the rest of the project depends on.

- **A place in the server list.** The index is a cache of the mesh and ranks
  with the launcher's own `browse` filter (`CLAUDE.md`, phase 4). Paid
  ranking turns it into a second source of truth.
- **Pack signing or trust.** Trust that can be bought is worthless (`PLAN.md`
  §11, §13.2).
- **Anything that makes free self-hosting worse.**

## 4. Free-tier lifecycle (proposed)

1. **Running and empty for 15 minutes: stopped.** The index already does this
   (`[quota] idle_timeout_secs`, `crates/platform-index/src/hosting.rs`).
2. **Stopped: the owner can start it again** from the launcher's "My
   servers". It is the same instance id, files and mesh address.
3. **Day 10 stopped: the owner is warned** in the launcher.
4. **Day 14 stopped: the server and its files are deleted.** Its backups
   stay 30 more days, then they go too.
5. **Backups: a rolling 2.** The node's backup schedule already deletes only
   the backups it made itself (`backups.rs`), so the cap reuses that.

**Open: a game that cannot be asked is never empty.** "Silence is not empty"
is a deliberate rule (`players_now: None` makes an instance exempt from
reaping, `CLAUDE.md`), so on the free tier such a server would never stop.
Two ways out, not yet chosen:
- the free tier offers only games that answer A2S; or
- every free session has a hard cap (e.g. 4 h) whatever the game reports.

Do not solve it by treating silence as zero; that rule protects busy servers
whose query was dropped.

## 5. What is missing in the code

- **Tiers per account in the index.** Quotas exist today but are one policy
  for every account (`quota.rs`).
- **Start a stopped server through the index**; today it creates, lists,
  stops and removes.
- **Delete after N days stopped**, with the warning before it.
- **A backup cap per tier.**
- **A "My servers" page in the launcher.**
- **Payments**: a provider (Stripe or similar) mapped to the account's
  Reticulum identity hash, which is already the index's `AccountId`.
- **Project hubs per region**, and monitoring and alerts for the fleet.

## 6. Ready to charge? Not yet (assessed 2026-10-15)

### Why not yet

- **No real players yet.** Nobody has joined Left 4 Dead 2; the wave 1 Source
  packs boot but none has been played; the last field run of an NFSU2 room
  join failed. Charging now would be charging people to find the bugs.
- **Real-device checks are open**: Steam Deck, the tray, the welcome screen,
  hosting and chat (`ROADMAP.md`, "Check needed").
- **Hosting has run on one machine.** Multi-node is covered by tests, but no
  fleet has run: no uptime history, no monitoring, no on-call, no restore
  drill.
- **No user base yet**, so a price would be a guess.
- **The billing pieces in §5 do not exist.**
- **Each game's terms for commercial hosting are unchecked.** The platform
  hosts only games that install with anonymous steamcmd (`GAMES.md` §5); a
  publisher's terms may still forbid selling hosting. That is a check for a
  person, game by game, before it is sold.

### What is ripe

The core: browsing, joining over the mesh, packs and signing, the node agent,
backups, updates, crash restarts, quotas and multi-node. Free self-hosting
can go out to people now.

### The path

1. **Real players first.** Two or three public game nights (Sven Co-op,
   Counter-Strike 1.6, TF2); fix what breaks; clear the "Check needed" list.
2. **Free hosted tier as a beta, no payment.** Build §5 except payments. The
   free tier is the same code the paid tiers will use, so nothing is thrown
   away. It measures cost per server, RAM, idle time and who comes back.
3. **Paid tiers once all of these hold:**
   - about a month of free beta with no data lost;
   - a backup restore drill done;
   - monitoring and alerts running;
   - a known cost per server;
   - a dozen or more returning users;
   - the terms of each sold game checked.

   Prices come from the measured cost.
4. **Add-ons (§3) after that.**
