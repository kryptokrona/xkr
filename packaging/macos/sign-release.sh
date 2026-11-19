#!/usr/bin/env bash
# Codesigns every Mach-O in a staged macOS xkr release directory with a Developer ID
# Application identity and the hardened runtime, so the archive passes notarisation and
# Gatekeeper. package-release.sh calls it through --codesign-identity; run it by hand on an
# unpacked archive directory to sign a local build.
#
#   packaging/macos/sign-release.sh --identity <id> --dir <dir> [--entitlements <plist>]
#
# <id> is a codesigning identity on the keychain search list: the certificate's common name
# ("Developer ID Application: Example (TEAMID)") or its SHA-1 hash (security find-identity -v
# -p codesigning). The five programs (xkrd, xkr-wallet-api, xkr-walletd, xkrwallet, xkr-miner)
# are signed with the hardened-runtime entitlements xkrd needs: JIT for the V8 in libbare-kit,
# library validation off so it loads libbare-kit.dylib and the bridge's native .bare addons
# (which carry no team ID), and DYLD_LIBRARY_PATH allowed so it finds libbare-kit next to
# itself. libbare-kit.dylib and the .bare addons are signed with the hardened runtime only.
# --entitlements defaults to entitlements.plist next to this script.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
identity="" dir="" entitlements="$here/entitlements.plist"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --identity) identity=${2:?}; shift 2 ;;
    --dir) dir=${2:?}; shift 2 ;;
    --entitlements) entitlements=${2:?}; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ -n "$identity" && -n "$dir" ]] || { echo "--identity and --dir are required" >&2; exit 2; }
[[ -d "$dir" ]] || { echo "$dir is not a directory" >&2; exit 2; }
[[ -f "$entitlements" ]] || { echo "$entitlements is missing" >&2; exit 2; }
command -v codesign >/dev/null 2>&1 || {
  echo "codesign not found (install the Xcode command line tools)" >&2; exit 2; }

programs=(xkrd xkr-wallet-api xkr-walletd xkrwallet xkr-miner)
is_program() {
  local b; b=$(basename "$1")
  for p in "${programs[@]}"; do [[ "$b" == "$p" ]] && return 0; done
  return 1
}
is_macho() { [[ "$(file -b "$1" 2>/dev/null)" == *Mach-O* ]]; }

# Sign the libraries and addons first (the inner code the programs load), then the programs
# with the entitlements. --force so a re-run re-signs.
libs=() exes=()
while IFS= read -r -d '' f; do
  is_macho "$f" || continue
  if is_program "$f"; then exes+=("$f"); else libs+=("$f"); fi
done < <(find "$dir" -type f -print0)

for f in "${libs[@]}"; do
  echo "signing (library) ${f#"$dir"/}"
  codesign --force --timestamp --options runtime -s "$identity" "$f"
done
for f in "${exes[@]}"; do
  echo "signing (program) ${f#"$dir"/}"
  codesign --force --timestamp --options runtime --entitlements "$entitlements" -s "$identity" "$f"
done

# Strict verification, the same check the notary service makes.
for f in "${libs[@]}" "${exes[@]}"; do
  codesign --verify --strict --verbose=2 "$f" >/dev/null
done
echo "signed ${#exes[@]} program(s) and ${#libs[@]} librar(ies) in $dir"
