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
# Steam authentication stays **on** by default, because a bridged client is not
# inherently unvalidatable: a deployed Sven Co-op server on this same transport
# logs `STEAM USERID validated` for a player arriving from a private address.
# What rejects a player is a server claiming the wrong app id, which is the
# block above.
#
# `HLDS_SV_LAN=1` turns client Steam authentication off — the log then says
# `VAC secure mode disabled` — and is the escape hatch for a mod with no Steam
# app of its own, or for players whose Steam cannot reach Valve. It also costs
# VAC and the master listing, so it is not the default.
#
# Nothing in a shipped `server.cfg` sets `sv_lan`, so unlike `hostname` this one
# survives from the command line.
SV_LAN="${HLDS_SV_LAN:-0}"
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

# **`SteamAppId` in the environment is what makes this work at all**, and the
# value decides who the server is to Steam.
#
# Unset, the engine reaches the Steam *client* interfaces, asks for the
# connected universe, gets nothing, and dies with `FATAL ERROR (shutting down):
# Unable to initialize Steam` — after the map has loaded, so the log reads like
# a working server right up to the last line. The mod's shipped
# `steam_appid.txt` is not a substitute: the failing case is exactly a run that
# has one and no environment variable.
#
# Set, the server registers as that app. It must be **the app a player owns**,
# not app 90: a client presents a session ticket for Counter-Strike (10) or
# Half-Life (70), and a server authenticating as the Half-Life Dedicated Server
# is not the app that ticket is for, so every connection ends in
# `STEAM validation rejected` on the client and nothing at all in the server
# log. 90 boots perfectly well, which is what makes it such a good trap.
#
# Both were verified against build 10211 on this node's daemon: 90 and 10 each
# reach `Connection to Steam servers successful` and `VAC secure mode is
# activated`.
case "$MOD" in
    cstrike) MOD_APP_ID=10 ;;
    valve)   MOD_APP_ID=70 ;;
    tfc)     MOD_APP_ID=20 ;;
    dod)     MOD_APP_ID=30 ;;
    # An unknown mod is somebody's own: it has no Steam app of its own to
    # authenticate as, so fall back to the dedicated server's. Such a server
    # can still be joined with HLDS_SV_LAN=1.
    *)       MOD_APP_ID=90 ;;
esac
SteamAppId="${HLDS_APP_ID:-$MOD_APP_ID}"
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
