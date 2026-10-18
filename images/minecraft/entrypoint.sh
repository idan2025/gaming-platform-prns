#!/bin/sh
# Run a Minecraft: Java Edition server against the build the node mounted
# (`MINECRAFT.md` §11).
#
# Everything about the *instance* comes from the platform's own environment,
# set by the agent from a validated spec: the port, the player count, the
# heap, the EULA acceptance and the server settings. Every value is a number,
# a word from a closed set, or a name held to the server-name rule. None is
# ever run as shell: values are passed as arguments or written as data.
set -eu

CONTENT="${GPP_CONTENT_ROOT:-/game}"
SERVER="$CONTENT/server"
INFO="$CONTENT/lanthorn-build.txt"

die() {
    echo "$*" >&2
    exit 1
}

# One `key=value` per line, read as data, never sourced.
info() {
    sed -n "s/^$1=//p" "$INFO" | head -n 1
}

[ -f "$INFO" ] || die "No Minecraft build at $CONTENT. The node installs one with the minecraft content driver."
[ -d "$SERVER" ] && [ -w "$SERVER" ] || die "No writable server directory at $SERVER. The node mounts one per server."

BUILD="$(info build)"
TYPE="$(info server)"
VERSION="$(info version)"
LOADER="$(info loader)"
JAVA_MAJOR="$(info java)"
FILE="$(info file)"

case "$TYPE" in vanilla|paper|fabric|forge) ;; *) die "Unknown server type '$TYPE' in $INFO." ;; esac
case "$JAVA_MAJOR" in 17|21|25) ;; *) die "This image has no Java $JAVA_MAJOR." ;; esac
case "$FILE" in server.jar|installer.jar) ;; *) die "Unknown file '$FILE' in $INFO." ;; esac
JAVA="/opt/java/$JAVA_MAJOR/bin/java"

digits() {
    case "$1" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac
}

# **The EULA is the owner's to accept, and only the agent says they did.**
cd "$SERVER"
ACCEPTED="${GPP_EULA_ACCEPTED_AT:-}"
if ! digits "$ACCEPTED"; then
    printf 'eula=false\n' > eula.txt
    die "The Minecraft EULA has not been accepted for this server. Its owner accepts it when creating the server in Lanthorn."
fi
printf '# Accepted by the server owner in Lanthorn (unix time %s).\n# https://aka.ms/MinecraftEULA\neula=true\n' "$ACCEPTED" > eula.txt

MIN_MB="${GPP_MEMORY_MIN_MB:-1024}"
MAX_MB="${GPP_MEMORY_MAX_MB:-2048}"
digits "$MIN_MB" && digits "$MAX_MB" || die "Memory must be whole megabytes."
[ "$MIN_MB" -le "$MAX_MB" ] || die "Starting memory ($MIN_MB MB) is more than the most the server may use ($MAX_MB MB)."

# Prepare the server directory from the read-only build, once per build.
MARKER=.lanthorn-build
if [ "$(cat "$MARKER" 2>/dev/null || true)" != "$BUILD" ]; then
    echo "Preparing $BUILD in $SERVER"
    case "$TYPE" in
        vanilla|paper)
            # Nothing to copy: the jar runs from the read-only copy, and
            # unpacks its libraries into this directory on first start.
            ;;
        fabric)
            "$JAVA" -jar "$CONTENT/installer.jar" server \
                -mcversion "$VERSION" -loader "$LOADER" -downloadMinecraft -dir "$SERVER"
            ;;
        forge)
            "$JAVA" -jar "$CONTENT/installer.jar" --installServer "$SERVER"
            rm -f ./*installer*.log
            ;;
    esac
    printf '%s\n' "$BUILD" > "$MARKER"
fi

# The keys the node manages; every other key in the file is the owner's.
# Values reach awk through the environment, not `-v`, which would read
# backslashes as escapes.
setprop() {
    touch server.properties
    if grep -q "^$1=" server.properties; then
        K="$1" V="$2" awk 'BEGIN { k = ENVIRON["K"]; v = ENVIRON["V"] }
            index($0, k "=") == 1 { print k "=" v; next } { print }' server.properties > server.properties.tmp
        mv server.properties.tmp server.properties
    else
        printf '%s=%s\n' "$1" "$2" >> server.properties
    fi
}

bool() {
    case "$1" in true|false) printf '%s' "$1" ;; *) printf '%s' "$2" ;; esac
}

PORT="${GPP_PORT:-25565}"
digits "$PORT" || die "The port must be a number."
PLAYERS="${GPP_MAX_PLAYERS:-20}"
digits "$PLAYERS" || die "The player count must be a number."
# A properties file reads a backslash as an escape, so the name's own are
# doubled. The name holds no control character (`validate_server_name`).
MOTD="$(printf '%s' "${GPP_SERVER_NAME:-A Minecraft server}" | sed 's/\\/\\\\/g')"
case "${GPP_MC_GAMEMODE:-survival}" in survival|creative|adventure|spectator) GAMEMODE="${GPP_MC_GAMEMODE:-survival}" ;; *) GAMEMODE=survival ;; esac
case "${GPP_MC_DIFFICULTY:-normal}" in peaceful|easy|normal|hard) DIFFICULTY="${GPP_MC_DIFFICULTY:-normal}" ;; *) DIFFICULTY=normal ;; esac

setprop server-port "$PORT"
setprop server-ip ""
setprop max-players "$PLAYERS"
setprop motd "$MOTD"
setprop gamemode "$GAMEMODE"
setprop difficulty "$DIFFICULTY"
setprop pvp "$(bool "${GPP_MC_PVP:-true}" true)"
setprop white-list "$(bool "${GPP_MC_WHITELIST:-false}" false)"
setprop enforce-whitelist "$(bool "${GPP_MC_WHITELIST:-false}" false)"
setprop online-mode "$(bool "${GPP_MC_ONLINE_MODE:-true}" true)"
setprop level-name world
# RCON needs a password; the node talks to the console on stdin instead.
setprop enable-rcon false
# A seed only means something before the world exists.
SEED="${GPP_MC_SEED:-}"
if [ -n "$SEED" ] && [ ! -f world/level.dat ]; then
    case "$SEED" in *[!A-Za-z0-9-]*) die "The seed may only hold letters, digits and '-'." ;; esac
    setprop level-seed "$SEED"
fi

HEAP="-Xms${MIN_MB}M -Xmx${MAX_MB}M"
echo "Starting $BUILD on Java $JAVA_MAJOR with ${MIN_MB}-${MAX_MB} MB, port $PORT"

# `exec`, so the JVM is the container's first process and `docker stop`
# reaches it: Minecraft saves the world on SIGTERM.
case "$TYPE" in
    vanilla|paper)
        # shellcheck disable=SC2086 # HEAP is two flags built from numbers above
        exec "$JAVA" $HEAP -jar "$CONTENT/server.jar" nogui
        ;;
    fabric)
        # shellcheck disable=SC2086
        exec "$JAVA" $HEAP -jar fabric-server-launch.jar nogui
        ;;
    forge)
        ARGS="$(ls libraries/net/minecraftforge/forge/*/unix_args.txt 2>/dev/null | head -n 1 || true)"
        if [ -n "$ARGS" ]; then
            # shellcheck disable=SC2086
            exec "$JAVA" $HEAP "@$ARGS" nogui
        fi
        JAR="$(ls forge-*.jar 2>/dev/null | grep -v installer | head -n 1 || true)"
        [ -n "$JAR" ] || die "The Forge installer left no server to start in $SERVER."
        # shellcheck disable=SC2086
        exec "$JAVA" $HEAP -jar "$JAR" nogui
        ;;
esac
