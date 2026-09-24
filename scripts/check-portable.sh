#!/bin/sh
# Prove a portable launcher writes nothing outside its folder (`PLAN.md` §14,
# step 5b): run it with an empty HOME under a virtual display, then fail if
# anything appeared in that HOME, or if nothing appeared in portable-data
# (which would mean portable mode never switched on and the check proved
# nothing).
#
# Usage: scripts/check-portable.sh <launcher-binary>
# Needs Xvfb.
set -eu

bin="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
data="$(dirname "$bin")/portable-data"
tmp="$(mktemp -d)"
trap 'kill "$xpid" 2>/dev/null || true; rm -rf "$tmp" "$data"' EXIT

rm -rf "$data"
mkdir -p "$tmp/home" "$data"
Xvfb :98 -screen 0 1280x800x24 >/dev/null 2>&1 &
xpid=$!
sleep 1
env -i PATH=/usr/bin:/bin HOME="$tmp/home" DISPLAY=:98 "$bin" >"$tmp/log" 2>&1 &
lpid=$!
sleep 25
kill "$lpid" 2>/dev/null || true
sleep 1
kill -9 "$lpid" 2>/dev/null || true

leaked="$(find "$tmp/home" -mindepth 1 | head -20)"
if [ -n "$leaked" ]; then
    echo "A portable run wrote outside its folder:" >&2
    echo "$leaked" >&2
    exit 1
fi
if [ ! -d "$data/webview" ]; then
    echo "portable-data/webview was never created: portable mode did not switch on, so this proves nothing." >&2
    cat "$tmp/log" >&2
    exit 1
fi
echo "portable run: nothing outside portable-data; it holds:"
find "$data" -maxdepth 1 -mindepth 1
