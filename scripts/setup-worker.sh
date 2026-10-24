#!/usr/bin/env bash
# Set up one worker: a platform-agent that an index can place servers on.
#
#   scripts/setup-worker.sh --index <32 hex> [--hub HOST:PORT] [--name NAME]
#       [--class LABEL] [--priority N] [--dir DIR] [--agent PATH]
#       [--packs DIR] [--service]
#
# What it does, once:
#   1. checks Docker answers and the agent binary runs,
#   2. writes DIR/agent.toml trusting the index (an index's identity hash),
#   3. makes the worker's identity (DIR/agent.identity, owner-only),
#   4. with --hub, tells the worker to dial that hub over TCP,
#   5. prints the block to paste into the index's hosting file.
#
# It never prints a secret: the identity file stays where it is, and only the
# two public hashes are shown. It refuses to overwrite an existing agent.toml.
# It starts nothing; with --service it writes a systemd user unit and leaves
# enabling it to you.

set -euo pipefail

usage() { sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }
die() { echo "setup-worker: $*" >&2; exit 1; }

INDEX=""; HUB=""; NAME=""; CLASS=""; PRIORITY="0"
DIR="${HOME}/lanthorn-worker"; AGENT=""; PACKS=""; SERVICE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --index) INDEX="${2:-}"; shift 2 ;;
    --hub) HUB="${2:-}"; shift 2 ;;
    --name) NAME="${2:-}"; shift 2 ;;
    --class) CLASS="${2:-}"; shift 2 ;;
    --priority) PRIORITY="${2:-}"; shift 2 ;;
    --dir) DIR="${2:-}"; shift 2 ;;
    --agent) AGENT="${2:-}"; shift 2 ;;
    --packs) PACKS="${2:-}"; shift 2 ;;
    --service) SERVICE=1; shift ;;
    -h|--help) usage 0 ;;
    *) usage 2 ;;
  esac
done

# Inputs are checked as data before anything is written. Refused, never repaired.
[[ "$INDEX" =~ ^[0-9a-f]{32}$ ]] || die "--index must be the index's identity hash: 32 lowercase hex characters"
if [ -n "$HUB" ]; then
  [[ "$HUB" =~ ^[A-Za-z0-9.-]+:[0-9]{1,5}$ ]] || die "--hub must be host:port (IPv4 or a host name)"
fi
if [ -n "$CLASS" ]; then
  [[ "$CLASS" =~ ^[a-z0-9_-]{1,32}$ ]] || die "--class must be 1-32 characters: lower case, digits, - and _"
fi
[[ "$PRIORITY" =~ ^-?[0-9]+$ ]] || die "--priority must be a whole number"
[[ "$DIR" = /* ]] || DIR="$(pwd)/$DIR"
[ -n "$NAME" ] || NAME="$(hostname -s)"
[[ "$NAME" =~ ^[A-Za-z0-9._-]{1,64}$ ]] || die "--name must be 1-64 characters: letters, digits, . _ -"

# 1. Docker and the agent.
if ! docker info >/dev/null 2>&1; then
  die "Docker does not answer. Install it and add this user to the docker group, then run again."
fi
if [ -z "$AGENT" ]; then
  AGENT="$(command -v platform-agent || true)"
fi
[ -n "$AGENT" ] && [ -x "$AGENT" ] || die "no platform-agent binary; pass --agent PATH"
AGENT="$(cd "$(dirname "$AGENT")" && pwd)/$(basename "$AGENT")"
"$AGENT" --version >/dev/null || die "$AGENT does not run"

# The packs the worker runs: the checkout's own by default, which is what
# --service and the printed start line both need to name exactly.
if [ -z "$PACKS" ]; then
  PACKS="$(cd "$(dirname "$0")/.." && pwd)/packs"
fi
[ -d "$PACKS" ] || die "no packs directory at $PACKS; pass --packs DIR"
PACKS="$(cd "$PACKS" && pwd)"

# 2. The config. Refuse to clobber one an operator wrote.
CONFIG="$DIR/agent.toml"
[ ! -e "$CONFIG" ] || die "$CONFIG already exists; edit it, or remove it to start over"
mkdir -p "$DIR/data"
chmod 700 "$DIR"

cat > "$CONFIG" <<TOML
# Written by scripts/setup-worker.sh. Add [games.<id>] sections for what this
# worker runs; a game with no entry cannot be started here.
data_root = "$DIR/data"
max_instances = 8
api_bind = "127.0.0.1:4750"

[port_range]
start = 27100
end = 27199

[uplink]
identity_secret_path = "$DIR/agent.identity"
trusted_indexes = ["$INDEX"]
TOML
chmod 600 "$CONFIG"

# 4. The way out to the mesh: a TCP client to a hub, in the agent's own
#    sidecar file (the same one the web UI writes).
if [ -n "$HUB" ]; then
  printf '{\n  "interfaces": [\n    {"kind": "tcp", "addr": "%s"}\n  ]\n}\n' "$HUB" > "$DIR/data/agent-interfaces.json"
fi

# 3 and the addresses. The agent makes its identity on first use, exactly as it
# does at start; only the public hashes come back.
ADDRESSES="$("$AGENT" --control-address "$CONFIG")"
CONTROL="$(printf '%s\n' "$ADDRESSES" | awk '$1 == "control" {print $2}')"
[[ "$CONTROL" =~ ^[0-9a-f]{32}$ ]] || die "could not read this worker's control address"

# 5. Optional service.
if [ "$SERVICE" -eq 1 ]; then
  UNIT_DIR="${HOME}/.config/systemd/user"
  mkdir -p "$UNIT_DIR"
  cat > "$UNIT_DIR/lanthorn-worker.service" <<UNIT
[Unit]
Description=Lanthorn worker ($NAME)
After=network-online.target docker.service

[Service]
ExecStart=$AGENT $CONFIG $PACKS
Restart=on-failure

[Install]
WantedBy=default.target
UNIT
  echo "wrote $UNIT_DIR/lanthorn-worker.service; enable it with: systemctl --user enable --now lanthorn-worker"
fi

# The hosting-file block for the index.
echo
echo "Worker set up at $DIR. Identity and config stay on this machine."
echo "Start it by hand with: $AGENT $CONFIG $PACKS"
echo
echo "Paste this into the index's hosting file, then the index picks it up within 10 s:"
echo
echo "[[nodes]]"
echo "name = \"$NAME\""
echo "api = \"http://127.0.0.1:4750\"   # unused while agent is set"
echo "agent = \"$CONTROL\""
echo "priority = $PRIORITY"
[ -z "$CLASS" ] || echo "class = \"$CLASS\""
if [ -z "$HUB" ]; then
  echo
  echo "No --hub given: this worker reaches the mesh only on a LAN it can auto-discover."
fi
