#!/usr/bin/env bash
# Builds the Hyperswarm bridge bundle: bridge/ sources plus production node_modules.
#
#   packaging/scripts/bundle-bridge.sh --out <dir> [--target <triple | platform-arch>]
#
# xkrd runs <bridge>/src/worker.js on Bare worker threads (bare-kit, not bare-pack), so the
# bundle is the bridge directory itself: src/, package.json, package-lock.json and the dependencies
# installed with `npm ci --omit=dev --ignore-scripts` (no package in the lock file has an
# install script; the native addons ship prebuilds). Without --target the bundle keeps the
# prebuilds of every platform. Needs Node.js 22 or later and npm. Uses Socket Firewall
# (`sfw npm ci`) when `sfw` is on PATH.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
out=""
target=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --out) out=${2:?}; shift 2 ;;
    --target) target=${2:?}; shift 2 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ -n "$out" ]] || { echo "--out is required" >&2; exit 2; }

if [[ -e "$out" ]] && [[ -n "$(ls -A "$out" 2>/dev/null)" ]]; then
  echo "$out exists and is not empty" >&2
  exit 2
fi
mkdir -p "$out"
cp -R "$repo/bridge/src" "$out/src"
cp "$repo/bridge/package.json" "$repo/bridge/package-lock.json" "$repo/bridge/README.md" "$out/"
cp "$repo/LICENSE" "$out/LICENSE"

npm_cmd=(npm)
if command -v sfw >/dev/null 2>&1; then
  npm_cmd=(sfw npm)
fi
(cd "$out" && "${npm_cmd[@]}" ci --omit=dev --ignore-scripts --no-audit --no-fund)

# Symlinks to CLI entry points; not used at runtime and awkward in zip files and app bundles.
rm -rf "$out/node_modules/.bin"

if [[ -n "$target" ]]; then
  "$here/prune-prebuilds.sh" "$out" "$target"
fi
echo "bridge bundle in $out"
