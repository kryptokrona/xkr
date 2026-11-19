#!/usr/bin/env bash
# Notarises a signed macOS xkr release archive with Apple's notary service. The archive's
# programs and libraries must already be signed with a Developer ID and the hardened runtime
# (package-release.sh --codesign-identity, or sign-release.sh). notarytool takes a .zip, so this
# repacks the archive's directory into a zip and submits it with --wait.
#
#   packaging/macos/notarize.sh --archive <xkr-*.tar.gz> \
#       --apple-id <id> --team-id <team> --password <app-specific-password>
#
# Loose command-line programs cannot be stapled (only .app/.dmg/.pkg carry a stapled ticket),
# so the ticket stays on Apple's servers: Gatekeeper fetches it online the first time a
# quarantined copy runs. Notarisation here confirms the archive is accepted; it does not change
# the files, so the .tar.gz uploaded to the release is the signed one as built.
set -euo pipefail

archive="" apple_id="" team_id="" password=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --archive) archive=${2:?}; shift 2 ;;
    --apple-id) apple_id=${2:?}; shift 2 ;;
    --team-id) team_id=${2:?}; shift 2 ;;
    --password) password=${2:?}; shift 2 ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ -n "$archive" && -n "$apple_id" && -n "$team_id" && -n "$password" ]] || {
  echo "--archive, --apple-id, --team-id and --password are required" >&2; exit 2; }
[[ -f "$archive" ]] || { echo "$archive is missing" >&2; exit 2; }
command -v xcrun >/dev/null 2>&1 || {
  echo "xcrun not found (install the Xcode command line tools)" >&2; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
tar -C "$work" -xzf "$archive"
dir=$(find "$work" -maxdepth 1 -type d -name 'xkr-*' | head -n1)
[[ -n "$dir" ]] || { echo "no xkr-* directory in $archive" >&2; exit 2; }

zip="$work/notarize.zip"
# ditto keeps the extended attributes the notary service expects on the zip entries.
(cd "$(dirname "$dir")" && /usr/bin/ditto -c -k --keepParent "$(basename "$dir")" "$zip")

echo "submitting $(basename "$archive") to the notary service…"
xcrun notarytool submit "$zip" \
  --apple-id "$apple_id" --team-id "$team_id" --password "$password" --wait
echo "notarisation accepted (loose binaries are not stapled; the ticket is served online)"
