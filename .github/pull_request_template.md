<!-- For a game pack, keep the checklist; for anything else, delete it. -->

## What and why


## Game pack checklist

- [ ] `cargo run -p game-bridge --bin game-bridge -- review packs/<id>.toml --against packs` refuses nothing; every flag is explained below
- [ ] Ports, transport and query checked against the game (how: )
- [ ] `[content]` app id is the dedicated server and downloads anonymously, or the archive's sha256 was checked, or it is `manual` with a note
- [ ] `writable_paths` has no `maps_dir` and no folder of shipped configs
- [ ] Tested: ran a server / joined it / not played yet (say which)
