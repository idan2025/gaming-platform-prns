#!/bin/sh
# Build lan-helper and put it where the launcher's bundle expects it
# (`launcher/src-tauri/binaries/`, `PLAN.md` §14 step 5).
#
# Tauri ships it as a sidecar on Linux, Windows and macOS
# (`tauri.linux.conf.json`, `tauri.windows.conf.json`, `tauri.macos.conf.json`),
# and a sidecar is named for the target triple it was built for. `cargo build`
# of the launcher refuses to start without it, so CI and the release run this
# first, and so does anyone building the launcher by hand.
#
# `--target universal-apple-darwin` builds the helper for both Macs and joins
# them with `lipo`: a universal bundle wants `lan-helper-universal-apple-darwin`
# and, while it builds each half, the two single-architecture names too.
#
# Usage: scripts/stage-lan-helper.sh [--release] [--target <triple>]
set -eu

profile=debug
target=""
while [ $# -gt 0 ]; do
    case "$1" in
        --release) profile=release ;;
        --target) target="$2"; shift ;;
        *) echo "usage: $0 [--release] [--target <triple>]" >&2; exit 2 ;;
    esac
    shift
done

root="$(cd "$(dirname "$0")/.." && pwd)"
# Where cargo puts what it builds: `./target`, unless CARGO_TARGET_DIR says
# otherwise (a CI runner with a warm cache outside the checkout).
out="${CARGO_TARGET_DIR:-$root/target}"
triple="${target:-$(rustc -vV | sed -n 's/^host: //p')}"
case "$triple" in
    *windows*) exe=".exe" ;;
    *) exe="" ;;
esac

if [ "$triple" = universal-apple-darwin ]; then
    mkdir -p "$root/launcher/src-tauri/binaries"
    for arch in x86_64-apple-darwin aarch64-apple-darwin; do
        set -- build -p game-bridge --bin lan-helper --target "$arch"
        [ "$profile" = release ] && set -- "$@" --release
        (cd "$root" && cargo "$@")
        cp "$out/$arch/$profile/lan-helper" "$root/launcher/src-tauri/binaries/lan-helper-$arch"
        echo "staged lan-helper-$arch"
    done
    lipo -create \
        "$out/x86_64-apple-darwin/$profile/lan-helper" \
        "$out/aarch64-apple-darwin/$profile/lan-helper" \
        -output "$root/launcher/src-tauri/binaries/lan-helper-universal-apple-darwin"
    echo "staged lan-helper-universal-apple-darwin"
    exit 0
fi

set -- build -p game-bridge --bin lan-helper
[ "$profile" = release ] && set -- "$@" --release
[ -n "$target" ] && set -- "$@" --target "$target"
(cd "$root" && cargo "$@")

if [ -n "$target" ]; then
    built="$out/$target/$profile/lan-helper$exe"
else
    built="$out/$profile/lan-helper$exe"
fi
mkdir -p "$root/launcher/src-tauri/binaries"
cp "$built" "$root/launcher/src-tauri/binaries/lan-helper-$triple$exe"
echo "staged lan-helper-$triple$exe"
