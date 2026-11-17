#!/usr/bin/env bash
# Removes the native-addon prebuilds of other platforms from a bridge bundle.
#
#   packaging/scripts/prune-prebuilds.sh <bridge dir> <target triple | platform-arch>
#
# sodium-native and udx-native (and the bare-* packages) ship prebuilds for every platform
# inside their npm packages, so an unpruned bundle runs everywhere (about 40 MB). For one
# target only its own directory (e.g. linux-x64) is needed, and in it only the `.bare` addons:
# xkrd runs the bridge on Bare, so the `.node` builds for Node.js are dropped too.
set -euo pipefail

dir=${1:?usage: prune-prebuilds.sh <bridge dir> <target triple | platform-arch>}
target=${2:?usage: prune-prebuilds.sh <bridge dir> <target triple | platform-arch>}

case "$target" in
  x86_64-unknown-linux-gnu)   keep=linux-x64 ;;
  aarch64-unknown-linux-gnu)  keep=linux-arm64 ;;
  x86_64-apple-darwin)        keep=darwin-x64 ;;
  aarch64-apple-darwin)       keep=darwin-arm64 ;;
  x86_64-pc-windows-msvc)     keep=win32-x64 ;;
  aarch64-pc-windows-msvc)    keep=win32-arm64 ;;
  *-*)                        keep=$target ;;
  *) echo "unknown target: $target" >&2; exit 2 ;;
esac

[[ -d "$dir/node_modules" ]] || { echo "no node_modules in $dir" >&2; exit 2; }

shopt -s nullglob
found=0
while IFS= read -r -d '' prebuilds; do
  [[ -d "$prebuilds/$keep" ]] && found=1
  for p in "$prebuilds"/*/; do
    p=${p%/}
    [[ "$(basename "$p")" == "$keep" ]] || rm -rf "$p"
  done
done < <(find "$dir/node_modules" -type d -name prebuilds -prune -print0)

if [[ $found -eq 0 ]]; then
  echo "no prebuilds for $keep in $dir/node_modules" >&2
  exit 1
fi
find "$dir/node_modules" -path "*/prebuilds/$keep/*.node" -type f -delete
if [[ -z "$(find "$dir/node_modules" -path "*/prebuilds/$keep/*.bare" -type f -print -quit)" ]]; then
  echo "no .bare addons for $keep in $dir/node_modules" >&2
  exit 1
fi
echo "kept the .bare prebuilds for $keep"
