#!/usr/bin/env bash
# Puts the sidecar and resources where tauri.conf.json expects them:
#
#   src-tauri/binaries/xkrd-<triple>[.exe]   xkrd (externalBin)
#   src-tauri/resources/lib/                 libbare-kit, which xkrd loads (resource)
#   src-tauri/resources/bridge/              bridge bundle (resource)
#
#   scripts/prepare-sidecars.sh [--target <triple>] [--xkrd-dir <dir>] [--bare-kit-dir <dir>]
#                               [--bridge-dir <bundle dir>]
#   scripts/prepare-sidecars.sh --placeholders [--target <triple>]
#
# --xkrd-dir     directory with xkrd (default: <repo>/target/release; or an unpacked release
#                archive)
# --bare-kit-dir directory with libbare-kit.so / libbare-kit.dylib / bare-kit.dll (default: the
#                --xkrd-dir if it holds the library, else <repo>/target/bare-kit/lib, which
#                scripts/build-bare-kit.sh fills)
# --bridge-dir   an existing bridge bundle (e.g. bridge/ of a release archive); default: build
#                one with packaging/scripts/bundle-bridge.sh (runs npm ci).
# --placeholders  empty stand-ins, enough for `cargo check` / `cargo clippy` of the app without
#              building xkrd (tauri-build refuses missing externalBin files and resources).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
app=$(cd "$here/../src-tauri" && pwd)
repo=$(cd "$here/../../.." && pwd)
target="" xkrd_dir="$repo/target/release" bare_kit_dir="" bridge_src="" placeholders=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) target=${2:?}; shift 2 ;;
    --xkrd-dir) xkrd_dir=${2:?}; shift 2 ;;
    --bare-kit-dir) bare_kit_dir=${2:?}; shift 2 ;;
    --bridge-dir) bridge_src=${2:?}; shift 2 ;;
    --placeholders) placeholders=1; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [[ -z "$target" ]]; then
  target=$(rustc -vV | sed -n 's/^host: //p')
fi
exe=""
[[ "$target" == *windows* ]] && exe=.exe
case "$target" in
  *windows*) libname=bare-kit.dll ;;
  *apple*) libname=libbare-kit.dylib ;;
  *) libname=libbare-kit.so ;;
esac

bins="$app/binaries"
res="$app/resources/bridge"
libs="$app/resources/lib"
mkdir -p "$bins"
rm -rf "$res" "$libs"
mkdir -p "$(dirname "$res")" "$libs"

if [[ $placeholders -eq 1 ]]; then
  : > "$bins/xkrd-$target$exe"
  : > "$libs/$libname"
  mkdir -p "$res"
  echo "placeholder: run scripts/prepare-sidecars.sh without --placeholders" > "$res/PLACEHOLDER"
  echo "placeholders for $target in $app/binaries and $app/resources (check only; the app cannot run)"
  exit 0
fi

[[ -f "$xkrd_dir/xkrd$exe" ]] || {
  echo "no xkrd$exe in $xkrd_dir (cargo build --release -p xkrd, or --xkrd-dir)" >&2; exit 2; }
cp "$xkrd_dir/xkrd$exe" "$bins/xkrd-$target$exe"

chmod +x "$bins/xkrd-$target$exe"

if [[ -z "$bare_kit_dir" ]]; then
  bare_kit_dir="$repo/target/bare-kit/lib"
  [[ -f "$xkrd_dir/$libname" ]] && bare_kit_dir=$xkrd_dir
fi
[[ -f "$bare_kit_dir/$libname" ]] || {
  echo "no $libname in $bare_kit_dir (scripts/build-bare-kit.sh, or --bare-kit-dir)" >&2; exit 2; }
cp "$bare_kit_dir/$libname" "$libs/$libname"

if [[ -n "$bridge_src" ]]; then
  [[ -f "$bridge_src/src/worker.js" ]] || { echo "$bridge_src is not a bridge bundle" >&2; exit 2; }
  cp -R "$bridge_src" "$res"
  "$repo/packaging/scripts/prune-prebuilds.sh" "$res" "$target"
else
  "$repo/packaging/scripts/bundle-bridge.sh" --out "$res" --target "$target"
fi
echo "xkrd for $target ready in $bins, $libs and $res"
