#!/bin/sh
# Build lan-helper and put it where the launcher's bundle expects it
# (`launcher/src-tauri/binaries/`, `PLAN.md` §14 step 5).
#
# Tauri ships it as a sidecar on Linux and Windows (`tauri.linux.conf.json`,
# `tauri.windows.conf.json`), and a sidecar is named for the target triple it
# was built for. `cargo build` of the launcher refuses to start without it, so
# CI and the release run this first, and so does anyone building the launcher
# by hand. macOS has no room adapter yet and bundles no helper.
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
triple="${target:-$(rustc -vV | sed -n 's/^host: //p')}"
case "$triple" in
    *windows*) exe=".exe" ;;
    *) exe="" ;;
esac

set -- build -p game-bridge --bin lan-helper
[ "$profile" = release ] && set -- "$@" --release
[ -n "$target" ] && set -- "$@" --target "$target"
(cd "$root" && cargo "$@")

if [ -n "$target" ]; then
    built="$root/target/$target/$profile/lan-helper$exe"
else
    built="$root/target/$profile/lan-helper$exe"
fi
mkdir -p "$root/launcher/src-tauri/binaries"
cp "$built" "$root/launcher/src-tauri/binaries/lan-helper-$triple$exe"
echo "staged lan-helper-$triple$exe"
