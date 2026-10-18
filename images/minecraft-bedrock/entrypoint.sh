#!/bin/sh
# Run a Minecraft: Bedrock Edition server against the copy the node mounted
# (`MINECRAFT.md` §16). Every value comes from the agent's validated spec; none
# is ever run as shell.
set -eu

CONTENT="${GPP_CONTENT_ROOT:-/game}"
SERVER="$CONTENT/server"

die() {
    echo "$*" >&2
    exit 1
}

digits() {
    case "$1" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac
}

[ -x "$CONTENT/bedrock_server" ] || die "No Bedrock server at $CONTENT. The node installs it from the pack."
[ -d "$SERVER" ] && [ -w "$SERVER" ] || die "No writable server directory at $SERVER. The node mounts one per server."

# **The EULA is the owner's to accept, and only the agent says they did.**
digits "${GPP_EULA_ACCEPTED_AT:-}" \
    || die "The Minecraft EULA has not been accepted for this server. Its owner accepts it when creating the server in Lanthorn."

cd "$SERVER"
# The server's own files are linked from the read-only copy; the settings,
# the permission and allow lists, and the worlds are this server's own.
for path in "$CONTENT"/*; do
    name="$(basename "$path")"
    case "$name" in
        server) ;;
        server.properties|permissions.json|allowlist.json)
            [ -e "$name" ] || cp "$path" "$name"
            ;;
        *)
            ln -sfn "$path" "$name"
            ;;
    esac
done

setprop() {
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

PORT="${GPP_PORT:-19132}"
digits "$PORT" || die "The port must be a number."
PLAYERS="${GPP_MAX_PLAYERS:-10}"
digits "$PLAYERS" || die "The player count must be a number."
case "${GPP_MC_GAMEMODE:-survival}" in survival|creative|adventure) GAMEMODE="${GPP_MC_GAMEMODE:-survival}" ;; *) GAMEMODE=survival ;; esac
case "${GPP_MC_DIFFICULTY:-normal}" in peaceful|easy|normal|hard) DIFFICULTY="${GPP_MC_DIFFICULTY:-normal}" ;; *) DIFFICULTY=normal ;; esac
# A `;` would end the name in the server's own status reply; the name rule
# already refuses it.
NAME="${GPP_SERVER_NAME:-A Bedrock server}"

setprop server-name "$NAME"
setprop server-port "$PORT"
setprop server-portv6 19133
setprop max-players "$PLAYERS"
setprop gamemode "$GAMEMODE"
setprop difficulty "$DIFFICULTY"
setprop allow-list "$(bool "${GPP_MC_WHITELIST:-false}" false)"
setprop online-mode "$(bool "${GPP_MC_ONLINE_MODE:-true}" true)"
setprop level-name world
# NetherNet, the only transport this release's players connect with
# (measured: with `raknet` the server logs that players cannot connect): an
# HTTP handshake on server-port over TCP, then WebRTC over UDP. The server
# tells each client where to send that UDP. It is told to name the client's
# own loopback and the four ports after the game's, which is where a Lanthorn
# client bridge listens for the pack's channels 1 to 4 (a client's extra port
# is its game port plus the channel).
setprop transport nethernet
setprop server-ip ""
setprop server-udp-ports "127.0.0.1:19133-19136:19133-19136"
# A container's broadcasts reach nobody; players join by address.
setprop enable-lan-visibility false
SEED="${GPP_MC_SEED:-}"
if [ -n "$SEED" ] && [ ! -d worlds/world ]; then
    case "$SEED" in *[!A-Za-z0-9-]*) die "The seed may only hold letters, digits and '-'." ;; esac
    setprop level-seed "$SEED"
fi

echo "Starting the Bedrock server on port $PORT"
# The server ignores SIGTERM (measured: `docker stop` ended it with SIGKILL
# and an unsaved world), and stops cleanly only when told `stop` on its
# console. So its console is a pipe this script holds: the container's stdin
# (where the node's console lines arrive) is copied into it, and a TERM or INT
# writes `stop` into it and waits for the server to save and quit.
PIPE="$(mktemp -u /tmp/bedrock-console.XXXXXX)"
mkfifo "$PIPE"
exec 3<>"$PIPE"
# A background job's stdin is /dev/null in a script, so the container's
# stdin is kept on fd 4 for the copy.
exec 4<&0
LD_LIBRARY_PATH=. ./bedrock_server <"$PIPE" &
SERVER_PID=$!
cat <&4 >&3 &
trap 'printf "stop\n" >&3' TERM INT
status=0
while kill -0 "$SERVER_PID" 2>/dev/null; do
    wait "$SERVER_PID" || status=$?
done
exit "$status"
