#!/usr/bin/env bash
# Join this machine to a Lanthorn index as a worker, with a join code an admin
# made on the node page's Admin tab ("Add a worker").
#
#   curl -fsSL http://<index>/join.sh | bash -s -- --url http://<index> --token <code>
#
#   --url URL      the index's HTTP address, as this machine reaches it
#   --token CODE   the join code (works once, then it is spent)
#   --name NAME    a name for this worker if the code did not fix one
#   --dir DIR      where the worker keeps its identity, config and data
#                  (default: ~/lanthorn-worker, or /var/lib/lanthorn-worker as root)
#   --image IMAGE  the platform image (default: ghcr.io/idan2025/gaming-platform-prns:latest)
#   --container N  the container's name (default: lanthorn-worker)
#
# It needs Docker. It pulls the image, starts one container that joins and then
# runs as the worker (restarted by Docker, so it survives a reboot), and waits
# to hear that it joined. The script holds no secret: the code is an argument.
# Read it first if you like: `curl -fsSL http://<index>/join.sh | less`.
#
# What it does not do is pick the games this worker runs. That is this machine's
# operator's choice (a game's image is operator config), so a new worker takes
# no game until one is added to its agent.toml.

set -euo pipefail

die() { echo "join-worker: $*" >&2; exit 1; }

URL=""; TOKEN=""; NAME=""; DIR=""; CONTAINER="lanthorn-worker"
IMAGE="ghcr.io/idan2025/gaming-platform-prns:latest"

while [ $# -gt 0 ]; do
  case "$1" in
    --url) URL="${2:-}"; shift 2 ;;
    --token) TOKEN="${2:-}"; shift 2 ;;
    --name) NAME="${2:-}"; shift 2 ;;
    --dir) DIR="${2:-}"; shift 2 ;;
    --image) IMAGE="${2:-}"; shift 2 ;;
    --container) CONTAINER="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,24p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option $1" ;;
  esac
done

# Everything is checked as data before anything is done. Refused, never repaired.
[[ "$URL" =~ ^https?://[A-Za-z0-9.:-]+/?$ ]] || die "--url must be the index's http:// or https:// address"
URL="${URL%/}"
[[ "$TOKEN" =~ ^[A-Za-z0-9]{1,128}$ ]] || die "--token is not a join code"
if [ -n "$NAME" ]; then
  [[ "$NAME" =~ ^[A-Za-z0-9._-]{1,64}$ ]] || die "--name must be 1-64 characters: letters, digits, . _ -"
fi
[[ "$CONTAINER" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,62}$ ]] || die "--container is not a container name"
[[ "$IMAGE" =~ ^[A-Za-z0-9][A-Za-z0-9._/:@-]{0,200}$ ]] || die "--image is not an image reference"

command -v docker >/dev/null 2>&1 || die "Docker is not installed. Install it, then run this again."
docker info >/dev/null 2>&1 || die "Docker does not answer. Start it, and add this user to the docker group (or run as root)."

if docker inspect "$CONTAINER" >/dev/null 2>&1; then
  die "a container named $CONTAINER already exists. If it is this worker, it is already set up; to start over: docker rm -f $CONTAINER"
fi

# The worker runs as whoever runs this. Its data folder is mounted at the same
# path on both sides, because the agent starts game servers as siblings and the
# daemon resolves their paths on the host.
if [ "$(id -u)" -eq 0 ]; then
  USER_FLAGS=(--user 0:0)
  [ -n "$DIR" ] || DIR="/var/lib/lanthorn-worker"
else
  DOCKER_GID="$(getent group docker | cut -d: -f3 || true)"
  [ -n "$DOCKER_GID" ] || die "there is no docker group on this machine; run as root, or set one up"
  USER_FLAGS=(--user "$(id -u):$(id -g)" --group-add "$DOCKER_GID")
  [ -n "$DIR" ] || DIR="$HOME/lanthorn-worker"
fi
[[ "$DIR" = /* ]] || DIR="$(pwd)/$DIR"
[[ "$DIR" =~ ^/[A-Za-z0-9._/-]+$ ]] || die "--dir may hold letters, digits and . _ / - only"
mkdir -p "$DIR"
chmod 700 "$DIR"

echo "Pulling $IMAGE ..."
if ! docker pull -q "$IMAGE" >/dev/null 2>&1; then
  docker image inspect "$IMAGE" >/dev/null 2>&1 || die "could not pull $IMAGE"
  echo "Could not pull; using the copy already here."
fi

JOIN=(platform-agent join --url "$URL" --token "$TOKEN" --dir "$DIR" --run)
[ -z "$NAME" ] || JOIN+=(--name "$NAME")

docker run -d --name "$CONTAINER" --restart unless-stopped \
  "${USER_FLAGS[@]}" \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v "$DIR:$DIR" \
  "$IMAGE" "${JOIN[@]}" >/dev/null

echo "Started $CONTAINER. Waiting for the index to take it in ..."
for _ in $(seq 1 60); do
  LOGS="$(docker logs "$CONTAINER" 2>&1 || true)"
  if grep -q "^joined as " <<<"$LOGS"; then
    grep "^joined as " <<<"$LOGS" | tail -1
    echo "It shows in the index's Workers table within about 10 seconds."
    echo "Its files are in $DIR. Add a [games.<id>] section to $DIR/agent.toml for each game it should run."
    exit 0
  fi
  if grep -q "the index refused\|Error:" <<<"$LOGS"; then
    echo "$LOGS" | tail -5 >&2
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    die "the worker could not join. Make a new code and run this again."
  fi
  sleep 1
done
docker logs --tail 10 "$CONTAINER" >&2 || true
die "no word from the index after a minute. Check that this machine reaches $URL."
