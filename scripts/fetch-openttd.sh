#!/bin/sh
# Fetch the OpenTTD build that `tests/lan_openttd.rs` plays a LAN room with
# (`PLAN.md` §14.3, step 3).
#
# OpenTTD is free and open source (GPL), and so is its OpenGFX base set, which
# even a headless dedicated server refuses to start without. Both come from
# OpenTTD's own CDN.
#
# Usage:
#     scripts/fetch-openttd.sh <dir>
#     OPENTTD_DIR=<dir>/openttd-15.3-linux-generic-amd64 \
#         cargo test -p game-bridge --test lan_openttd -- --nocapture
#
# Linux x86_64 only, like the test.
set -eu

# Pinned, and verified before anything is extracted: a digest is what decides,
# never the URL. Recorded from the CDN on 2026-09-24, which publishes no
# checksum beside these files. Bump the version and its digest together.
OPENTTD_VERSION="15.3"
OPENTTD_URL="https://cdn.openttd.org/openttd-releases/${OPENTTD_VERSION}/openttd-${OPENTTD_VERSION}-linux-generic-amd64.tar.xz"
OPENTTD_SHA256="f49eb25d61b00f8f4d332fee02b530ad75552d1efb8f2bb01e7ca5e6540fe059"
OPENGFX_VERSION="8.0"
OPENGFX_URL="https://cdn.openttd.org/opengfx-releases/${OPENGFX_VERSION}/opengfx-${OPENGFX_VERSION}-all.zip"
OPENGFX_SHA256="43a0c1dabf39cb865394f3a6cc36d4da5c10ecfaaf55652043104806810903be"

[ $# -eq 1 ] || { echo "usage: $0 <dir>" >&2; exit 2; }
DEST="$1"
GAME="$DEST/openttd-${OPENTTD_VERSION}-linux-generic-amd64"

if [ -x "$GAME/openttd" ]; then
    echo "OpenTTD $OPENTTD_VERSION is already at $GAME"
    exit 0
fi

for tool in curl sha256sum tar unzip; do
    command -v "$tool" >/dev/null 2>&1 || { echo "$tool is required" >&2; exit 1; }
done

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

fetch() { # url sha256 file
    curl -fsSL -o "$TMP/$3" "$1"
    actual="$(sha256sum "$TMP/$3" | cut -d' ' -f1)"
    if [ "$actual" != "$2" ]; then
        echo "Digest mismatch for $3. Expected $2, got $actual. Nothing was installed." >&2
        exit 1
    fi
}

echo "Downloading OpenTTD $OPENTTD_VERSION and OpenGFX $OPENGFX_VERSION…"
fetch "$OPENTTD_URL" "$OPENTTD_SHA256" openttd.tar.xz
fetch "$OPENGFX_URL" "$OPENGFX_SHA256" opengfx.zip

# Staged, then moved into place, so an interrupted run never looks installed.
mkdir -p "$TMP/stage"
tar xf "$TMP/openttd.tar.xz" -C "$TMP/stage"
mkdir -p "$TMP/stage/openttd-${OPENTTD_VERSION}-linux-generic-amd64/baseset"
unzip -oq "$TMP/opengfx.zip" -d "$TMP/stage/openttd-${OPENTTD_VERSION}-linux-generic-amd64/baseset"
mkdir -p "$DEST"
mv "$TMP/stage/openttd-${OPENTTD_VERSION}-linux-generic-amd64" "$GAME"

echo "OpenTTD $OPENTTD_VERSION is at $GAME"
echo "Run the LAN room test with: OPENTTD_DIR=$GAME cargo test -p game-bridge --test lan_openttd -- --nocapture"
