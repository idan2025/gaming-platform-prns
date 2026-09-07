#!/bin/sh
# Install YaPB into a node's copy of Counter-Strike, so that copy has bots.
#
# Counter-Strike 1.6 has no bots. Valve's Z-Bot is compiled into the same
# server library and gated on an internal Condition Zero flag, so `bot_add` on
# a `cstrike` server adds nothing and says nothing — verified with the bot
# databases and a nav mesh copied in, which changes nothing. YaPB is a
# third-party bot that does work there.
#
# **This is operator work on purpose.** YaPB is a binary the node executes, so
# no game pack can ask for it (`crates/game-bridge/src/pack.rs` — `PackBots`
# has no variant for it). Running this script is a person deciding to run that
# code on their own hardware, and the node's config is where they say so:
#
#     [games.counter-strike-16]
#     image = "gpp/goldsrc:1"
#     content_root = "/game"
#     content_version = "app90-yapb"
#     env = { HLDS_MOD = "cstrike" }
#     bots = "yapb"
#     writable_paths = ["cstrike/addons/yapb/data/train", "cstrike/addons/yapb/data/logs"]
#
# Usage:
#     scripts/install-yapb.sh <content-dir>
#
# where <content-dir> is one version directory of an app 90 install —
# `<data_root>/content/counter-strike-16/<version>/`, the one holding
# `hlds_linux`.
#
# **Give it a new version directory, not one that is in use.** Content is
# shared: every instance of a game bind-mounts the same copy read-only, and
# some of them are running right now. Copy first:
#
#     cp -a .../content/counter-strike-16/app90 .../content/counter-strike-16/app90-yapb
#
# That is the same rule the agent's own content driver follows — existing
# content is never replaced, a new version is a new directory.
set -eu

# Pinned, and verified before anything is extracted. The digest is the safety
# property here exactly as it is for a pack's `archive` driver: a hijacked
# mirror or a rewritten release produces bytes that do not match, and this
# stops. Bump both together, never one.
YAPB_VERSION="4.4.957"
YAPB_URL="https://github.com/yapb/yapb/releases/download/${YAPB_VERSION}/yapb-${YAPB_VERSION}-linux.tar.xz"
YAPB_SHA256="8c095ac89b9b2ccc70a66a71d608e1a570b5268c57c6083ced8c06161533a4b1"

usage() {
    echo "usage: $0 <content-dir>" >&2
    echo "  <content-dir>  a version directory of an app 90 install (the one with hlds_linux)" >&2
    exit 2
}

[ $# -eq 1 ] || usage
CONTENT="$1"

[ -x "$CONTENT/hlds_linux" ] || {
    echo "No Half-Life Dedicated Server at $CONTENT (no hlds_linux)." >&2
    exit 1
}
[ -d "$CONTENT/cstrike" ] || {
    echo "No cstrike/ in $CONTENT. YaPB is a Counter-Strike bot; this install has no Counter-Strike." >&2
    exit 1
}
if [ -e "$CONTENT/cstrike/addons/yapb/bin/yapb.so" ]; then
    echo "YaPB is already installed in $CONTENT." >&2
    echo "Remove cstrike/addons/yapb first, or install into a fresh copy." >&2
    exit 1
fi

for tool in curl sha256sum tar; do
    command -v "$tool" >/dev/null 2>&1 || { echo "$tool is required" >&2; exit 1; }
done

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

echo "Downloading YaPB $YAPB_VERSION…"
curl -fsSL -o "$TMP/yapb.tar.xz" "$YAPB_URL"

echo "Verifying…"
actual="$(sha256sum "$TMP/yapb.tar.xz" | cut -d' ' -f1)"
if [ "$actual" != "$YAPB_SHA256" ]; then
    echo "Digest mismatch. Expected $YAPB_SHA256, got $actual." >&2
    echo "Nothing was installed." >&2
    exit 1
fi

# Staged, then moved into place: an interrupted extraction must not leave half
# a bot in a content copy that other instances are already mounting.
echo "Extracting…"
mkdir -p "$TMP/stage"
tar xf "$TMP/yapb.tar.xz" -C "$TMP/stage"
[ -f "$TMP/stage/addons/yapb/bin/yapb.so" ] || {
    echo "The archive did not contain addons/yapb/bin/yapb.so; refusing to install it." >&2
    exit 1
}
mkdir -p "$CONTENT/cstrike/addons"
mv "$TMP/stage/addons/yapb" "$CONTENT/cstrike/addons/yapb"

# The one edit to Valve's own files: point the game library at YaPB, which
# loads the real one itself. Metamod is the other way to do this and is only
# worth it if something else on the node already needs Metamod.
LIBLIST="$CONTENT/cstrike/liblist.gam"
cp "$LIBLIST" "$LIBLIST.gpp-backup"
sed -i 's|^gamedll_linux "dlls/cs.so"$|gamedll_linux "addons/yapb/bin/yapb.so"|' "$LIBLIST"
grep -q '^gamedll_linux "addons/yapb/bin/yapb.so"$' "$LIBLIST" || {
    echo "Could not point liblist.gam at YaPB; the original is at $LIBLIST.gpp-backup." >&2
    exit 1
}

echo
echo "YaPB $YAPB_VERSION installed in $CONTENT."
echo "Point a [games.<id>] section at this directory with bots = \"yapb\", then"
echo "start a server: the Bots button and the start form's bot count appear once"
echo "the node says the game has them."
