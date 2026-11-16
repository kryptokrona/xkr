#!/usr/bin/env bash
# Packs one release archive for one target: the node, the wallet tools, the miner, copies of
# them under the C++ program names, and the Hyperswarm bridge.
#
#   packaging/scripts/package-release.sh --target <triple> --bridge <bridge dir> \
#       [--version <version>] [--bin-dir <dir>] [--out <dir>]
#
# --version defaults to the workspace version in Cargo.toml. --bin-dir defaults to
# target/<target>/release when xkrd is there, else target/release (both under $CARGO_TARGET_DIR
# when set). --bridge is a bundle from bundle-bridge.sh or a bridge/ checkout with its
# node_modules installed. The archive is <out>/xkr-<version>-<target>.tar.gz (.zip for Windows
# targets; --out defaults to dist/) and holds one directory:
#
#   xkr-<version>-<target>/
#     xkrd                  node (Levin P2P, RPC, Hyperswarm through bridge/)
#     xkr-wallet-api        REST wallet service       wallet-api           (same program)
#     xkr-walletd           JSON-RPC wallet service   kryptokrona-service  (same program)
#     xkrwallet             terminal wallet
#     xkr-miner             CPU miner                 miner                (same program)
#     bridge/               src/, package.json, node_modules/ (this target's prebuilds only)
#     docs/wallet-tools.md  moving from the C++ tools
#     README.md, LICENSE
#     SHA256SUMS            every other file in the directory
#
# On Windows every program ends in .exe. The old-name copies are plain copies (no links), so
# the zip works everywhere. xkrd finds bridge/ next to itself and runs it on Bare worker threads
# through libbare-kit, which it loads from its own directory or its lib/ (this script does not
# copy the library yet; see packaging/README.md). On Linux targets the run path of the packed
# xkrd is set to $ORIGIN:$ORIGIN/lib with patchelf when it is installed (a release build without
# XKR_DEV_RPATH=1 has exactly that already); without patchelf, a run path that names any other
# directory is reported. Prints the archive path.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
target="" version="" bridge="" bin_dir="" out="$repo/dist"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) target=${2:?}; shift 2 ;;
    --version) version=${2:?}; shift 2 ;;
    --bridge) bridge=${2:?}; shift 2 ;;
    --bin-dir) bin_dir=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
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

# Program name, then the C++ name it also ships under (empty: no copy).
programs=(
  "xkrd:"
  "xkr-wallet-api:wallet-api"
  "xkr-walletd:kryptokrona-service"
  "xkrwallet:"
  "xkr-miner:miner"
)
missing=0
for entry in "${programs[@]}"; do
  bin=${entry%%:*}
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

for entry in "${programs[@]}"; do
  bin=${entry%%:*}
  old=${entry#*:}
  cp "$bin_dir/$bin$exe" "$stage/$bin$exe"
  chmod 755 "$stage/$bin$exe"
  if [[ -n "$old" ]]; then
    cp "$stage/$bin$exe" "$stage/$old$exe"
  fi
done

# xkrd looks for its libraries only next to itself and in its lib/.
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
