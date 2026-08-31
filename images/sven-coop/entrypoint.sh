#!/bin/sh
# Run the Sven Co-op dedicated server against the content the node mounted.
#
# Everything this needs comes from the platform's own environment, set by the
# agent from a validated instance spec. A pack cannot reach these: a pack
# describes a game, and what a node executes is the operator's choice of image.
set -eu

CONTENT="${GPP_CONTENT_ROOT:-/game}"
PORT="${GPP_PORT:-27015}"
MAXPLAYERS="${GPP_MAX_PLAYERS:-16}"
MAP="${GPP_MAP:-svencoop1}"
NAME="${GPP_SERVER_NAME:-Sven Co-op}"

if [ ! -x "$CONTENT/svends_run" ]; then
    echo "No Sven Co-op install at $CONTENT." >&2
    echo "The node mounts its content there; install it first (the agent's" >&2
    echo "steamcmd driver fetches app 276060)." >&2
    exit 1
fi

# The DS resolves its own data relative to the working directory, so this is not
# cosmetic — started from anywhere else it finds no game.
cd "$CONTENT"

# `hostname` is how a GoldSrc server names itself. Written to a config the DS
# reads at start rather than passed as an argument, because a server name can
# contain spaces and quoting it through `+hostname` is a way to lose half of it.
#
# svencoop/ is inside the read-only content mount; the node makes the paths a
# pack declares writable into writable binds, and this is one of them.
if [ -w "$CONTENT/svencoop/logs" ]; then
    printf 'hostname "%s"\n' "$NAME" > "$CONTENT/svencoop/logs/gpp-hostname.cfg" 2>/dev/null || true
fi

echo "Starting Sven Co-op: port=$PORT maxplayers=$MAXPLAYERS map=$MAP name=$NAME"

# `-port`, not `-ip`: the DS ignores `-ip` and always binds 0.0.0.0 (verified in
# svencoop-prns's controller — `/proc/net/udp` shows 0.0.0.0 regardless). The
# node decides what is reachable from outside by which port it publishes.
#
# `exec` so the DS is PID 1 and receives `docker stop`'s SIGTERM directly.
# Without it the shell is PID 1, the signal never reaches the game, and every
# stop takes the full ten-second timeout before a SIGKILL.
exec ./svends_run \
    -port "$PORT" \
    +maxplayers "$MAXPLAYERS" \
    +map "$MAP" \
    +hostname "$NAME"
