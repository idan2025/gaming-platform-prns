# Roadmap

What comes after the 2026-10-10 redesign, in the order it is worth doing.
`PLAN.md` is still the design authority and its non-negotiables bind every item
here: decentralized by default, a server browser and never matchmaking, wire
compatibility with deployed peers, and a pack that can never name what runs.
Each item says which of those it has to respect, and what is still a decision.

Sizes are rough: **S** is a day or two, **M** about a week, **L** more.

| # | Feature | Who it is for | Size | Status |
| --- | --- | --- | --- | --- |
| 1 | Auto-update | every player | M | proposed |
| 2 | Invite links | players with friends | S–M | proposed |
| 3 | Favorites and "it's up" alerts | regulars | S | needs a decision |
| 4 | Installed-game detection | every player | M | proposed |
| 5 | Tray icon and notifications | hosts, regulars | M | proposed |
| 6 | Chat and player names in rooms | LAN-room players | L | proposed |
| 7 | Node: console, resources, schedules, backups | node operators | M–L | proposed |
| 8 | In-launcher pack browser | everyone, pack authors | L | planned (`PLAN.md` §13) |
| 9 | Playability check before joining | players on slow links | S | proposed |
| 10 | Controller and big-screen mode | Steam Deck, couch | M | proposed |
| — | Rebrand | the project | S–M | open |

Recommended first: **1 and 2.** They remove the most friction for a new player:
nobody stays stuck on an old build, and nobody has to be talked through finding
a server.

---

## 1. Auto-update

**What.** The launcher checks for a newer release, downloads it, verifies its
signature, and restarts into it — on a click, or on its own at the next start.

**Why.** Every field bug so far was fixed in a release, and a player who never
updates keeps the bug. Today updating means finding the GitHub page and the
right one of 22 files.

**How.** Tauri v2's updater plugin, fed a `latest.json` the release workflow
already has everything to produce, signed with a key held in CI secrets. AppImage,
NSIS and the macOS bundle are supported by the plugin; the Arch package and the
portable builds are not, and should say "a new version is out" with a link
instead of updating themselves.

**Rules.**
- **An update check is the internet, and the baseline is no internet.** The
  check must fail quietly and change nothing when offline; nothing may wait on
  it at start. It must be possible to turn off.
- **The signature decides, not the URL** — the same rule as content archives
  (`content.rs`). An unsigned or mis-signed download is discarded.
- A portable launcher writes nothing outside its folder (`portable.rs`), so it
  never replaces itself in place without asking.

**Decide.** Update silently on start, or always ask? Recommendation: ask, with
the release notes shown, and a "do this automatically" checkbox.

## 2. Invite links

**What.** "Copy invite" on a server or room gives a link such as
`meshgames://join/<destination>?game=<id>`. Clicking it in Discord, WhatsApp or
a browser opens the launcher on that server and offers Join.

**Why.** "Find Idan's room in the list" fails exactly when it matters: before a
server's announce reaches the friend. A destination hash is all a join needs, so
a link carries everything.

**How.** Tauri's deep-link plugin registers the scheme on all three platforms.
The launcher parses the link, validates the hash and game id as data, and opens
the detail pane — it **never joins on its own**; a person presses Join. An
`https://` fallback page that shows "open in the launcher, or download it" can
be a static page on GitHub Pages.

**Rules.**
- A link is untrusted input from a chat message: validate, never act on it
  without a click, and never let it set anything but which server is shown.
- No server-side component is required; the https page is a convenience.

## 3. Favorites and "it's up" alerts

**What.** Star a server or room. Starred ones are always looked for, and the
launcher tells you when one comes up.

**Why.** The list is live-only by design now, which is right for browsing and
wrong for "my friend's server, when is it on?".

**How.** A `favorites` set in settings; the keep-alive (`KEEPALIVE_EVERY`)
asks for favorites first; an alert when one moves from unheard to heard.

**Decide (needs the user).** The list shows only what is live. Should a
favorite that is offline show at all — in its own "Favorites" section, greyed,
"last online 2 days ago" — or only appear when it is up? Recommendation: its own
section, clearly offline, because a favorite is something the player asked to
remember; the main list stays live-only.

## 4. Installed-game detection

**What.** The launcher reads the player's Steam libraries (and, on Linux, the
Proton prefixes), shows an **Installed** badge, and makes Play work without
"Locate game".

**Why.** "Locate game" is the last manual step between Join and playing.

**How.** Parse `libraryfolders.vdf` and each `appmanifest_<id>.acf` for the
pack's `[launch] steam_app_id`; `steam.rs` already finds Steam. Games that are
not on Steam (both Need for Speed games) keep Locate, with a file picker in
place of today's text prompt.

**Rules.** The executable still comes from the player's own installation and is
never guessed from a pack (`PLAN.md` §13.1). Detection reads files only.

## 5. Tray icon and notifications

**What.** Closing the window keeps the launcher in the system tray while a room
or join is active. Native notifications for: someone joined your room, a
favorite came online, a room check failed, an update is ready.

**Why.** A host has to keep the launcher open for the room to exist, and today
that means a window in the way the whole evening.

**Rules.** Quitting from the tray must still end the room the way a quit does
now — nothing a room creates may outlive the launcher (`CLAUDE.md`, Mode 3).
Notifications are opt-out, and none fires while a game is fullscreen if the OS
can tell.

## 6. Chat and player names in rooms

**What.** A small text chat per room (and per server, for its bridged players),
and members shown by player name instead of `198.19.x.x`.

**Why.** A LAN room is a party; today it is a list of addresses.

**How.** Room members already hold Links to the host, so messages ride those
links' channel. Alternatively LXMF, which would make a room chat readable from
Sideband or NomadNet; worth measuring before choosing. The player name already
exists in settings (`player_name`).

**Rules.**
- **Chat is not matchmaking** (`PLAN.md` §0): no global lobby, no queue — only
  the people already in a room or on a server.
- Messages are plain text, length-capped, rate-limited per member at the host
  like broadcasts (`lan::BroadcastGate`).
- A name is data from another member: never trusted for identity, which stays
  the Reticulum identity.

## 7. Node: console, resources, schedules, backups

**What,** in the node's web page:
- **Live console and log** for each server (`docker logs` streamed), and
  sending a console line for games whose pack declares a console.
- **CPU, memory and network** per server, from Docker's stats API.
- **Scheduled restarts** and a daily restart window.
- **Backups** of a server's writable paths (saves, logs), with restore.

**Rules.**
- **A pack names a console protocol, never a command** (`console.rs`). A free
  console box is the operator typing at their own server — allowed — but it
  goes through the same newline-refusing validation as a map name, and it is
  never something a pack or an index can send.
- Backups cover only `writable_paths`; the shared content is read-only and
  never copied per instance.
- Everything rides behind the API token, like every other mutating route.

## 8. In-launcher pack browser

Already planned as `PLAN.md` §13 (the marketplace). Adds: install a signed
community pack from inside the launcher, with its art (the `art` field now
exists), its trust tier shown, and an update when the author signs a new
version. The rule that keeps it safe is the format, not a scanner (`PLAN.md`
§13.2).

## 9. Playability check before joining

**What.** Before Join, compare the game's `min_link_class` with the path to the
server (hops, the interface it was heard on, the measured round trip from the
detail probe) and say it plainly: "This game needs a fast link; this server is
reached over LoRa and will not be playable."

**Why.** `GAMES.md` already has the tiers and the launcher already has the
numbers; today the player finds out after the game times out.

**Rules.** A warning, never a block — the player decides. No probe is sent
that the detail pane does not already send (`relay.rs`: one server, because a
person opened it).

## 10. Controller and big-screen mode

**What.** A full-screen layout with large tiles, driven by a gamepad: browse,
join, play, leave. Good on a Steam Deck in Game Mode and on a TV.

**How.** The Gamepad API in the webview, a second stylesheet, and focus that is
always visible. The AppImage already runs on SteamOS.

---

## Rebrand

"Mesh Game Servers" says what it does, which is why it is hard to remember and
impossible to search for. A name should be short, searchable, say something
about *playing together without a central server*, and not collide with an
existing game product.

### Candidates

None of these turned up as an existing game launcher or server browser in a
quick search on 2026-10-10. **A proper trademark and domain check is still
needed before choosing.**

| Name | Why it fits | Watch out for |
| --- | --- | --- |
| **Lanthorn** | Old spelling of *lantern*, with "LAN" inside it. Servers announce themselves like lights in the dark, and it works with no internet. | People may misspell it "Lantern". |
| **Reticle** | A crosshair — gaming — and a nod to Reticulum, which carries everything. | A common word, so harder to own in search; check existing apps. |
| **Meshcade** | Mesh plus arcade: unmistakable, easy to search. | Sounds more retro or arcade than shooter or racer. |
| **Hopscotch** | Distance in this app is measured in hops; playful. | Many unrelated products use the name. |
| **Campfire** | Friends gathering with no host in charge. | Heavily used (Basecamp, Niantic, others). |

My pick: **Lanthorn** — distinctive, searchable, and it carries both halves of
the product (LAN, and a light that announces itself).

### What a rename changes, and what it must not

**Must not change — these are wire contracts, and renaming them breaks every
deployed peer:**
- Pack `app_name` values and Reticulum aspects (`PLAN.md` §5;
  `profile::tests::destination_hash_matches_deployed_sven`).
- `svencoop-prns`, which stays its own product (`PLAN.md` §5).

**Changes, with a migration:**
- **Product name and window title** — `tauri.conf.json` `productName`, the two
  UIs, README, release notes, the portable README, the Arch `PKGBUILD` and its
  `.desktop` entry, and `release.yml` asset names (the download file names
  change; old links in chats stop resolving to the newest file).
- **Windows firewall rule names** — `lan_firewall::RULE_NAME` and
  `PORTABLE_RULE_NAME` are `Mesh Game Servers LAN rooms`. An installed launcher
  keeps its rule on purpose, so after a rename the old rule stays forever unless
  the new build deletes rules with the old name once. Do that, or keep the old
  rule name.
- **Settings location** — settings live under `gaming-platform-prns/`
  (`settings::default_settings_path`), which is not the product name and can
  stay. If it moves, read the old file once and copy it, or every player loses
  saved connections, ports and indexes.
- **Tauri `identifier`** (`org.idan2025.gamingplatformprns.launcher`) — decides
  where the webview keeps its data and is what an auto-updater keys on.
  **Changing it is the expensive part**; leaving it as is costs nothing a player
  sees. Recommendation: keep it.
- **Binary name** `mesh-game-servers` and the Windows install folder: a new
  installer installs beside the old one unless it uninstalls the old one first.
- **The GitHub repository** can be renamed; GitHub redirects the old URL.

**Do it once, before auto-update ships** — so the updater's first release
already has the final name, and nobody has to update across a rename.
