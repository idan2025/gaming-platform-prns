# Contributing

Lanthorn is a decentralized server browser over Reticulum. `PLAN.md` is the
design authority and `CLAUDE.md` lists the rules a change must not quietly
break; read both before changing Rust code. Commit subjects are conventional
(`feat:`, `fix:`, `docs:`, `chore:`).

## Adding a game

A game reaches every Lanthorn by its **pack**: one TOML file in `packs/`.
Once a pull request adding it is merged, the `packs` workflow signs it with
the project key and publishes it to the
[official repository](https://idan2025.github.io/gaming-platform-prns/), and
every launcher and node picks it up within hours. You do not need a key.

### What a pack can and cannot do

A pack describes a game: its ports, its query protocol, where a node gets its
dedicated server's files, and how the player's own copy is pointed at a
server. **It never names a program, an image or a command line.** That is
the format, not a policy: the fields do not exist. Launch arguments are a
typed template given to the player's own game as an argument vector, never
through a shell. Read `PLAN.md` §13.1 before adding a `[launch]` block.

### Steps

1. Start from a shipped pack of the same engine. `packs/sven-coop.toml`
   (GoldSrc) and `packs/team-fortress-2.toml` (Source) explain every field
   in their comments. `GAMES.md` §9 has notes per game, including the ones
   known not to work yet and why.
2. Get the facts from the game, not from memory: its ports from the
   dedicated server's documentation or `ss -lunp` on a running server, the
   Steam app ids from SteamDB (the *dedicated server* app for `[content]`,
   the app players own for `[launch]`), and whether the server app downloads
   anonymously (`steamcmd +login anonymous +app_update <id> +quit`).
3. Run the review the pull request will run, from the repository root:

   ```
   cargo run -p game-bridge --bin game-bridge -- review packs/my-game.toml --against packs
   ```

   It **refuses** what no honest pack needs (a launch argument that runs a
   console command or names a fixed address, a `[lan]` port that opens a
   remote-access or system service, invisible characters, another game's
   name or `app_name`), and **flags** what a maintainer will ask about
   (an archive download, an unfamiliar Steam app or argument, links in the
   notes, a writable path over maps or configs). Fix every refusal; explain
   every flag in the pull request.
4. Run the tests that hold every shipped pack to the format:
   `cargo test -p game-bridge` (they find packs by property, so yours is
   included without naming it).
5. Open a pull request that adds only the pack (and its `GAMES.md` notes, if
   you have them). The template asks how you tested it.

The review runs again in CI and posts its result on the pull request.

### What a maintainer checks

The review is an aid for this, never a replacement for it: intent is not in
the syntax, and passing does not make a pack safe (`PLAN.md` §13.2).

- `id` and `app_name` are new, lowercase and final. **`app_name` is a wire
  contract**: once servers announce under it, changing it hides them all.
- The ports, transport and query protocol match the game.
- `[content]`: a `steamcmd` app is the dedicated server, downloads
  anonymously, and `mod` (if any) is a mod that app publishes; an `archive`
  URL is the publisher's own and its `sha256` was checked against a fresh
  download; otherwise `manual`, with a `note` saying where the files come
  from. There is never a login.
- `writable_paths` lists only what a server writes, never `maps_dir` or a
  folder of shipped configs (it is mounted empty over the game's files).
- `[launch]` arguments are ones the game documents, with `{host}`, `{port}`
  and `{password}` where they belong.
- `art`, if set, is on Steam's CDN or `upload.wikimedia.org`.
- Ideally someone ran a server and joined it. A pack nobody has played says
  so in its `notes`.

### Updating or removing a pack

An update is a pull request like any other; it is re-reviewed against the
packs already published. Launchers and nodes take it by themselves, and a
node never replaces a pack while a server of that game exists. A pack is
retired by removing it from `packs/`: it stops being published, and
installed copies stop loading when their signature lapses (400 days at
most). An urgent withdrawal is a change of key, which strands every
installed launcher, so it is the maintainer's call.

## Your own repository

You do not need this repository to share packs. `game-bridge repo build`
turns a folder of packs into a repository any web host can serve, and
`game-bridge sign` signs them with your own key; see README's "Sharing game
packs".
