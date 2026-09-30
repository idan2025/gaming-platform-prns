#!/bin/sh
# Run a Source dedicated server (srcds) against the content the node mounted.
#
# Sibling of images/goldsrc, and the same contract: everything about the
# *instance* comes from the platform's own `GPP_*` environment, set by the
# agent from a validated instance spec, and a pack cannot reach any of it.
# `SRCDS_GAME` is the one knob that is not GPP_-prefixed, because it is the
# operator's, not the spec's — the agent drops operator env named `GPP_*`
# (crates/platform-agent/src/docker.rs:373).
set -eu

CONTENT="${GPP_CONTENT_ROOT:-/game}"
PORT="${GPP_PORT:-27015}"
MAXPLAYERS="${GPP_MAX_PLAYERS:-24}"
GAME="${SRCDS_GAME:-tf}"
MAP="${GPP_MAP:-}"
NAME="${GPP_SERVER_NAME:-Source}"

if [ ! -x "$CONTENT/srcds_linux" ]; then
    echo "No Source dedicated server at $CONTENT." >&2
    echo "The node mounts its content there; install it first (the agent's" >&2
    echo "steamcmd driver fetches the game's dedicated-server app)." >&2
    exit 1
fi

if [ ! -d "$CONTENT/$GAME" ]; then
    echo "No game directory '$GAME' in $CONTENT." >&2
    echo "SRCDS_GAME names a directory of the install; this node's" >&2
    echo "[games.<id>].env sets it." >&2
    exit 1
fi

# **`SteamAppId` decides whether this is an Internet server at all.** Unset,
# srcds loads `steamclient.so`, then logs `Unable to load Steam support
# library` and `This server will operate in LAN mode only`, and answers no
# A2S query from outside — a server nobody can find. Set to the app players
# own, it logs `Connection to Steam servers successful.` and `VAC secure mode
# is activated.` Measured 2026-09-30 against TF2 build 10828683.
#
# Only games measured on a node are listed. The value is the *client's* app
# (440 for TF2, which is also `appID` in tf/steam.inf), not the dedicated
# server's (232250) — the GoldSrc image documents why that difference is a
# trap. Any other game names its own with `SRCDS_APP_ID`.
case "$GAME" in
    tf) GAME_APP_ID=440 ;;
    *)  GAME_APP_ID="" ;;
esac
SteamAppId="${SRCDS_APP_ID:-$GAME_APP_ID}"
if [ -z "$SteamAppId" ]; then
    echo "No Steam app id for game '$GAME'." >&2
    echo "Set SRCDS_APP_ID to the app a player owns (the appID line of" >&2
    echo "$GAME/steam.inf) in this node's [games.<id>].env." >&2
    exit 1
fi
export SteamAppId

if [ -z "$MAP" ]; then
    case "$GAME" in
        tf) MAP="cp_badlands" ;;
        *)
            echo "No default map for game '$GAME'; the instance must name one." >&2
            exit 1
            ;;
    esac
fi

# srcds dlopens the Steam client from `$HOME/.steam/sdk32`. The 32-bit one
# ships in the install's `bin/`; the content mount is read-only and shared,
# so the link is made in this container's own home.
#
# 32-bit, not `srcds_linux64`, although the install carries both: the 64-bit
# server wants a 64-bit `steamclient.so` in `sdk64`, and the dedicated-server
# app does not ship one — it fails with `wrong ELF class: ELFCLASS32` on the
# only copy there is.
mkdir -p "$HOME/.steam/sdk32"
ln -sf "$CONTENT/bin/steamclient.so" "$HOME/.steam/sdk32/steamclient.so"

cd "$CONTENT"
LD_LIBRARY_PATH="$CONTENT:$CONTENT/bin:${LD_LIBRARY_PATH:-}"
export LD_LIBRARY_PATH

echo "Starting Source: game=$GAME port=$PORT maxplayers=$MAXPLAYERS map=$MAP app=$SteamAppId name=$NAME"

# **srcds reads its console only from a terminal.** The agent sends a map
# change as a line on the container's stdin, which is a pipe — and srcds
# ignores a pipe completely, so a `changelevel` would be accepted and do
# nothing. Measured: the same line through a pseudo-terminal changes the map.
# `script` gives srcds that terminal inside the image, so the agent keeps one
# console mechanism for every engine and `docker logs` stays plain text.
#
# **The command string below is a constant, and must stay one.** `script -c`
# hands its argument to a shell, and `NAME` is whatever a user typed as their
# server's name. Each value is referenced as a quoted variable, which the
# shell expands as a single word and never parses again; building the string
# out of the values instead would turn `x; rm -rf /game` in a server name into
# a command.
#
# `-strictportbind`: fail if the port is taken rather than quietly binding the
# next one, which the node would then publish nothing for. `-e` makes `script`
# exit with srcds's status, and it passes the SIGTERM of `docker stop` through.
export PORT MAXPLAYERS GAME MAP NAME
exec script -qfec 'exec ./srcds_linux -game "$GAME" -port "$PORT" -strictportbind +maxplayers "$MAXPLAYERS" +map "$MAP" +hostname "$NAME"' /dev/null
