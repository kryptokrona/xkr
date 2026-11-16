#!/usr/bin/env bash
# capture-all: runs every wallet fixture capture in order, one at a time, then cleans up.
#
#   testnet/fixtures/capture-all.sh <work dir> [script...]
#
# Scripts (default all, in this order): wallet-api walletd wallet-files cli miner, i.e.
# testnet/fixtures/capture-<name>.sh <work dir>. Each runs under nice -n 19 with the time limit of
# its "# suite:" header; its output goes to <work dir>/logs/fx-<name>.log, its state to
# <work dir>/fx-<name>/. Afterwards every container labelled xkr-suite and every process in
# <work dir>/pids is stopped (lib.sh suite_cleanup). Fixtures land under $FX_OUT_ROOT (default:
# this checkout): crates/xkr-wallet-api/tests/fixtures/cpp, crates/xkr-walletd/tests/fixtures/cpp,
# crates/xkr-wallet-files/tests/data/{backend,green,addressbook}, crates/xkr-wallet-core/tests/data/
# mnemonic.json, crates/xkr-wallet-cli/tests/fixtures/cpp, crates/xkr-miner/tests/fixtures/cpp.
# Exit status: 0 when every script passed, 1 otherwise, 2 for usage errors. Never builds anything.
set -euo pipefail
FXD=$(cd "$(dirname "$0")" && pwd)
. "$FXD/lib-wallet.sh"

[ -n "${1:-}" ] || { sed -n '3,15p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
mkdir -p "$1"
WD=$(cd "$1" && pwd)
shift
NAMES=("$@")
[ "${#NAMES[@]}" -gt 0 ] || NAMES=(wallet-api walletd wallet-files cli miner)
mkdir -p "$WD/logs" "$WD/pids"
CHECKS_TSV=$WD/checks.tsv
: > "$CHECKS_TSV"
SCEN=fx-all

meta() { sed -n 's/^# suite:.*'"$2"'=\([0-9]*\).*/\1/p' "$1" | head -n 1; }
echo "wallet fixtures: ${NAMES[*]}; work dir $WD; output below $FX_OUT_ROOT/crates"
echo "images: FX_IMAGE=$FX_IMAGE WALLETD_IMAGE=$WALLETD_IMAGE CLI_IMAGE=$CLI_IMAGE MINER_IMAGES=$MINER_IMAGES"
echo "walletd/xkrwallet daemon: $FX_WALLETD_DAEMON; follower image: $FX_FOLLOWER_IMAGE"
suite_cleanup
rm -f "$WD/fx-follow.json"

declare -A ST=()
overall=0
INTERRUPTED=0
trap 'INTERRUPTED=1' INT TERM
for n in "${NAMES[@]}"; do
    script=$FXD/capture-$n.sh
    [ -f "$script" ] || { echo "no such script: $script" >&2; overall=2; continue; }
    limit=$(meta "$script" timeout)
    echo
    echo "##### capture-$n (expected ~$(meta "$script" expected)s, limit ${limit:-1800}s)"
    t0=$(date +%s)
    set +e
    nice -n 19 timeout --foreground --signal=TERM --kill-after=60 "${limit:-1800}" bash "$script" "$WD" 2>&1 \
        | (trap '' INT TERM; exec tee "$WD/logs/fx-$n.log")
    rc=${PIPESTATUS[0]}
    set -e
    ST[$n]="exit $rc, $(($(date +%s) - t0)) s"
    [ "$rc" = 0 ] || overall=1
    suite_cleanup
    [ "$INTERRUPTED" = 0 ] || { echo "interrupted"; overall=1; break; }
done

echo
echo "================================================================ wallet fixtures"
for n in "${NAMES[@]}"; do printf '%-14s %s\n' "$n" "${ST[$n]:-not run}"; done
awk -F'\t' '$1 ~ /^fx-/ && ($3 == "FAIL" || $3 == "MISSING") {printf "  %-8s %s/%s: %s\n", $3, $1, $2, $5}' "$CHECKS_TSV" | tail -n 40
[ "$overall" = 0 ] && echo "ALL PASS" || echo "SOME FAILED (logs in $WD/logs)"
exit "$overall"
