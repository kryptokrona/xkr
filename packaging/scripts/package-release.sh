#!/usr/bin/env bash
# Packs one release archive for one target: the node, the wallet tools, the miner and the
# Hyperswarm bridge.
#
#   packaging/scripts/package-release.sh --target <triple> --bridge <bridge dir> \
#       [--version <version>] [--bin-dir <dir>] [--bare-kit <dir>] [--out <dir>] [--max-glibc <v>] \
#       [--codesign-identity <id>] [--entitlements <plist>]
#
# --version defaults to the workspace version in Cargo.toml. --bin-dir defaults to
# target/<target>/release when xkrd is there, else target/release (both under $CARGO_TARGET_DIR
# when set). --bridge is a bundle from bundle-bridge.sh or a bridge/ checkout with its
# node_modules installed. --bare-kit is the directory with libbare-kit (default: $LIBBARE_DIR,
# else target/bare-kit/lib from scripts/build-bare-kit.sh). The archive is <out>/xkr-<version>-<target>.tar.gz (.zip for Windows
# targets; --out defaults to dist/) and holds one directory:
#
#   xkr-<version>-<target>/
#     xkrd                  node (Levin P2P, RPC, Hyperswarm through bridge/)
#     xkr-wallet-api        REST wallet service
#     xkr-walletd           JSON-RPC wallet service
#     xkrwallet             terminal wallet
#     xkr-miner             CPU miner
#     libbare-kit.so        the Bare runtime for xkrd (libbare-kit.dylib, bare-kit.dll)
#     bridge/               src/, package.json, node_modules/ (this target's prebuilds only)
#     docs/wallet-tools.md  moving from the C++ tools
#     README.md, LICENSE
#     SHA256SUMS            every other file in the directory
#
# On Windows every program ends in .exe. There are no copies under the C++ program names
# (docs/wallet-tools.md says how to keep those names). xkrd finds bridge/ next to itself and
# runs it on Bare worker threads through libbare-kit, which the archive carries next to xkrd. On
# Linux targets the run path of the packed xkrd is set to $ORIGIN:$ORIGIN/lib with patchelf when
# it is installed (a release build without XKR_DEV_RPATH=1 has exactly that already); without
# patchelf, a run path that names any other directory is reported. --max-glibc (Linux targets,
# 2.35 for the releases) stops when any program, library or addon in the archive needs a newer
# glibc (check-glibc.sh). On an Apple target, --codesign-identity signs every Mach-O in the
# archive (the programs, libbare-kit and the bridge's .bare addons) with that Developer ID
# Application identity and the hardened runtime before the checksums are taken, so the archive
# can be notarised (packaging/macos/sign-release.sh; --entitlements overrides its default
# plist). Prints the archive path.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
target="" version="" bridge="" bin_dir="" bare_kit="" out="$repo/dist" max_glibc=""
codesign_identity="" entitlements=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) target=${2:?}; shift 2 ;;
    --version) version=${2:?}; shift 2 ;;
    --bridge) bridge=${2:?}; shift 2 ;;
    --bin-dir) bin_dir=${2:?}; shift 2 ;;
    --bare-kit) bare_kit=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    --max-glibc) max_glibc=${2:?}; shift 2 ;;
    --codesign-identity) codesign_identity=${2:?}; shift 2 ;;
    --entitlements) entitlements=${2:?}; shift 2 ;;
    -h|--help) sed -n '2,39p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ -n "$target" && -n "$bridge" ]] || { echo "--target and --bridge are required" >&2; exit 2; }
[[ -f "$bridge/src/worker.js" && -f "$bridge/package.json" && -d "$bridge/node_modules" ]] || {
  echo "$bridge is not a bridge with node_modules (see bundle-bridge.sh)" >&2; exit 2; }

if [[ -z "$version" ]]; then
  version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' "$repo/Cargo.toml")
  [[ -n "$version" ]] || { echo "no workspace version in Cargo.toml; pass --version" >&2; exit 2; }
fi

exe=""
[[ "$target" == *windows* ]] && exe=.exe

if [[ -z "$bin_dir" ]]; then
  base=${CARGO_TARGET_DIR:-$repo/target}
  bin_dir="$base/release"
  [[ -f "$base/$target/release/xkrd$exe" ]] && bin_dir="$base/$target/release"
fi

programs=(xkrd xkr-wallet-api xkr-walletd xkrwallet xkr-miner)
missing=0
for bin in "${programs[@]}"; do
  [[ -f "$bin_dir/$bin$exe" ]] || { echo "missing $bin_dir/$bin$exe" >&2; missing=1; }
done
[[ $missing -eq 0 ]] || {
  echo "build them first: cargo build --release -p xkrd -p xkr-wallet-api -p xkr-walletd" \
       "-p xkr-wallet-cli -p xkr-miner" >&2
  exit 2
}

sha256=(sha256sum)
command -v sha256sum >/dev/null 2>&1 || sha256=(shasum -a 256)

name="xkr-$version-$target"
mkdir -p "$out"
out=$(cd "$out" && pwd)
stage="$out/$name"
rm -rf "$stage"
mkdir -p "$stage/docs" "$stage/bridge"

for bin in "${programs[@]}"; do
  cp "$bin_dir/$bin$exe" "$stage/$bin$exe"
  chmod 755 "$stage/$bin$exe"
done

# xkrd looks for its libraries only next to itself and in its lib/.
bare_kit=${bare_kit:-${LIBBARE_DIR:-$repo/target/bare-kit/lib}}
case "$target" in
  *windows*) libbare=bare-kit.dll ;;
  *apple*) libbare=libbare-kit.dylib ;;
  *) libbare=libbare-kit.so ;;
esac
[[ -f "$bare_kit/$libbare" ]] || {
  echo "$bare_kit/$libbare is missing (scripts/build-bare-kit.sh)" >&2; exit 2; }
cp "$bare_kit/$libbare" "$stage/$libbare"
if [[ "$target" == *linux* ]]; then
  if command -v patchelf >/dev/null 2>&1; then
    # shellcheck disable=SC2016 # $ORIGIN is for the loader, not the shell
    patchelf --set-rpath '$ORIGIN:$ORIGIN/lib' "$stage/xkrd"
  elif command -v readelf >/dev/null 2>&1; then
    runpath=$(readelf -d "$stage/xkrd" | sed -n 's/.*(\(RUNPATH\|RPATH\)).*\[\(.*\)\]/\2/p')
    # shellcheck disable=SC2016
    other=$(printf '%s' "$runpath" | tr ':' '\n' | grep -v -e '^\$ORIGIN$' -e '^\$ORIGIN/lib$' || true)
    if [[ -n "$other" ]]; then
      echo "warning: $stage/xkrd searches $runpath; install patchelf or build without" \
           "XKR_DEV_RPATH=1" >&2
    fi
  fi
fi

# Only what xkrd runs: no tests, no install-time links.
cp -R "$bridge/src" "$stage/bridge/src"
cp -R "$bridge/node_modules" "$stage/bridge/node_modules"
cp "$bridge/package.json" "$stage/bridge/"
for f in package-lock.json README.md; do
  [[ -f "$bridge/$f" ]] && cp "$bridge/$f" "$stage/bridge/"
done
cp "$repo/LICENSE" "$stage/bridge/LICENSE"
rm -rf "$stage/bridge/node_modules/.bin"
"$here/prune-prebuilds.sh" "$stage/bridge" "$target" >&2

cp "$repo/LICENSE" "$repo/README.md" "$stage/"
cp "$repo/docs/wallet-tools.md" "$stage/docs/"

# xkrd uses a bridge/ found next to itself only when nobody else can write to it.
chmod -R go-w "$stage"

if [[ -n "$max_glibc" && "$target" == *linux* ]]; then
  "$here/check-glibc.sh" --max "$max_glibc" "$stage" >&2
fi

# macOS: codesign every Mach-O (the programs, libbare-kit and the bridge's .bare addons) with
# the hardened runtime before the checksums below, so the archive can be notarised.
if [[ -n "$codesign_identity" && "$target" == *apple* ]]; then
  sign=("$repo/packaging/macos/sign-release.sh" --identity "$codesign_identity" --dir "$stage")
  [[ -n "$entitlements" ]] && sign+=(--entitlements "$entitlements")
  "${sign[@]}" >&2
elif [[ -n "$codesign_identity" ]]; then
  echo "warning: --codesign-identity ignored for non-Apple target $target" >&2
fi

(
  cd "$stage"
  find . -type f ! -name SHA256SUMS -print0 | LC_ALL=C sort -z | xargs -0 "${sha256[@]}" \
    | sed 's#^\([0-9a-f]*\) [ *]\./#\1  #' > SHA256SUMS
)

cd "$out"
if [[ -n "$exe" ]]; then
  archive="$name.zip"
  rm -f "$archive"
  if command -v 7z >/dev/null 2>&1; then
    7z a -tzip -mx=9 "$archive" "$name" >/dev/null
  else
    zip -qr9 "$archive" "$name"
  fi
else
  archive="$name.tar.gz"
  rm -f "$archive"
  tar -czf "$archive" "$name"
fi
rm -rf "$stage"
echo "$out/$archive"
