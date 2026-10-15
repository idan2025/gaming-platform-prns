# images/source-wine — work in progress, NOT shipped

Measured 2026-10-15 with Zombie Panic! Source's Windows server (app 17505,
7.8 GB, `[content] platform = "windows"`), Wine 8 from Debian bookworm.

Works:
- `srcds.exe -console` under Wine with **Xvfb** started by hand (xvfb-run
  hangs for a non-root user; make `/tmp/.X11-unix` first).
- The install must be **writable** (srcds.exe exits 255 on a read-only
  copy). A symlink copy, `cp -rs /game/. $HOME/game`, gives writable
  directories over the read-only files in under a second, as uid 1000.
- `steamclient.dll`, `tier0_s.dll`, `vstdlib_s.dll` from the Steamworks
  redistributable (app 1007, Windows) linked beside `srcds.exe`: then
  Steam connects, VAC on, A2S answers (app 17500, env `w`).

Does not work (why it is not shipped):
- **Live map change.** A line on the container's stdin (the agent's only
  console mechanism) is ignored: the Windows server reads a Windows console
  only. Under a pseudo-terminal (`script`, `docker -t`) Wine exits 255.
  Untried: `wineconsole`; a stdin-to-RCON relay inside the image with a
  per-start random password.
- `+hostname` loses to the game's `server.cfg` (announce name is right).

Synergy (app 17525): needs paid Half-Life 2 vpks (`../../Half-Life 2`),
which anonymous steamcmd cannot fetch. Licensing, not Wine.
Nuclear Dawn (app 111710): not tried.
