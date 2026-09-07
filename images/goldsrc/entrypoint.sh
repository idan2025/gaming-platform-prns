#!/bin/sh
# Run a GoldSrc dedicated server against the content the node mounted.
#
# Everything about the *instance* comes from the platform's own environment,
# set by the agent from a validated instance spec. A pack cannot reach these: a
# pack describes a game, and what a node executes is the operator's choice of
# image. `HLDS_MOD` is the one knob that is not GPP_-prefixed, because it is
# the operator's, not the spec's — one steamcmd app 90 install runs Half-Life,
# Counter-Strike 1.6, DoD or TFC depending on it.
set -eu

CONTENT="${GPP_CONTENT_ROOT:-/game}"
PORT="${GPP_PORT:-27015}"
MAXPLAYERS="${GPP_MAX_PLAYERS:-16}"
MOD="${HLDS_MOD:-valve}"
# LAN mode, which on a bridged server is not a nicety. Every player reaches a
# server here through a Reticulum link and connects to 127.0.0.1 on their *own*
# machine, so a secure server asks Steam to validate a session ticket against an
# address Steam has no server at — and the client is dropped with
# `STEAM validation rejected` before it ever spawns. `sv_lan 1` skips client
# Steam authentication, and the log says `VAC secure mode disabled` instead of
# `activated`.
#
# The cost is real and is the operator's to weigh: no VAC, and no Steam master
# listing, on a server nobody was going to reach through the master list anyway.
# A node publishing a port straight to the internet can set HLDS_SV_LAN=0 and
# get authentication back. Nothing in the shipped `server.cfg` sets `sv_lan`, so
# unlike `hostname` this one survives from the command line.
SV_LAN="${HLDS_SV_LAN:-1}"
MAP="${GPP_MAP:-}"
NAME="${GPP_SERVER_NAME:-Half-Life}"

if [ ! -x "$CONTENT/hlds_linux" ]; then
    echo "No Half-Life Dedicated Server at $CONTENT." >&2
    echo "The node mounts its content there; install it first (the agent's" >&2
    echo "steamcmd driver fetches app 90)." >&2
    exit 1
fi

if [ ! -d "$CONTENT/$MOD" ]; then
    echo "No mod directory '$MOD' in $CONTENT." >&2
    echo "HLDS_MOD names a directory of the app 90 install; this node's" >&2
    echo "[games.<id>].env sets it." >&2
    exit 1
fi

# Each mod has its own first map, and no single default is right for all of
# them: 'crossfire' does not exist in cstrike and 'de_dust2' does not exist in
# valve. An instance spec that names no map gets the mod's own.
if [ -z "$MAP" ]; then
    case "$MOD" in
        cstrike) MAP="de_dust2" ;;
        dod)     MAP="dod_avalanche" ;;
        tfc)     MAP="2fort" ;;
        *)       MAP="crossfire" ;;
    esac
fi

# **`SteamAppId` is what makes this work at all.** Without it the engine reaches
# the Steam client interfaces, asks for the connected universe, gets nothing,
# and dies with `FATAL ERROR (shutting down): Unable to initialize Steam` —
# after the map has loaded, so the log reads like a working server right up to
# the last line. With it the server logs `Connection to Steam servers
# successful` and VAC comes up. Verified against build 10211 (Oct 2024) on this
# node's daemon.
#
# 90 is the Half-Life *Dedicated Server* app, the one steamcmd installed. It is
# not the mod's client app id (10 for Counter-Strike, 70 for Half-Life), and
# `cstrike/steam_appid.txt` — which ships as 10 — is not a substitute: a run
# with that value and no `SteamAppId` is exactly the failing case above.
SteamAppId="${HLDS_APP_ID:-90}"
export SteamAppId

# HLDS refuses to run without a Steam client library it can dlopen from its own
# home directory: with `$HOME/.steam/sdk32/steamclient.so` missing it prints
# `FATAL ERROR (shutting down): Unable to initialize Steam` and exits 0, which
# looks like a clean shutdown rather than a failure. The library is in the
# install the node mounted, and the content mount is read-only, so the link is
# made here, in the container's own writable home, and points into it.
STEAM_SDK="${HOME:-/home/hlds}/.steam/sdk32"
if [ -f "$CONTENT/steamclient.so" ]; then
    mkdir -p "$STEAM_SDK"
    ln -sf "$CONTENT/steamclient.so" "$STEAM_SDK/steamclient.so"
fi

# The DS resolves its own data relative to the working directory, so this is not
# cosmetic — started from anywhere else it finds no game.
cd "$CONTENT"

# `hostname` is how a GoldSrc server names itself, and this is best-effort on
# purpose. Every shipped mod's `server.cfg` sets `hostname` itself and is
# executed *after* the command line, so a Counter-Strike server started with a
# name still calls itself `Counter-Strike 1.6 Server` in the A2S reply. The
# ways around that all need to write inside the mod directory, which is the
# read-only content mount shared by every instance on this node — measured:
# `-servercfgfile logs/gpp-server.cfg` loads nothing at all (and then the mod's
# own config does not run either), and `+exec` from the command line runs
# before `server.cfg` rather than after.
#
# It costs little: the name a player browses by is the one in the server's
# **announce**, which the agent sets from the instance spec and never reads back
# from the game. Setting the running server's name properly is a console word
# for the agent to send (crates/game-bridge/src/console.rs), not something an
# image can do without a config file it is allowed to write.

echo "Starting GoldSrc: mod=$MOD port=$PORT maxplayers=$MAXPLAYERS map=$MAP sv_lan=$SV_LAN name=$NAME"

# `hlds_linux`, not the `hlds_run` wrapper. The wrapper is a restart loop that
# runs the DS as a child, so `docker stop`'s SIGTERM reaches the shell and not
# the game, and every stop waits out the ten-second timeout before a SIGKILL.
# The wrapper's only other job is the library path, which is set here.
#
# `-port`, not `-ip`: the DS ignores `-ip` and always binds 0.0.0.0. The node
# decides what is reachable from outside by which port it publishes.
LD_LIBRARY_PATH="$CONTENT:$CONTENT/$MOD:${LD_LIBRARY_PATH:-}"
export LD_LIBRARY_PATH

exec ./hlds_linux \
    -game "$MOD" \
    -port "$PORT" \
    +sv_lan "$SV_LAN" \
    +maxplayers "$MAXPLAYERS" \
    +map "$MAP" \
    +hostname "$NAME"
