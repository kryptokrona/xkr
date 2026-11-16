#!/usr/bin/env bash
# Differential harness (see ../README.md):
#
#   1. known answers   every backend against compat/kat.txt and the monero tests.txt subset
#   2. Rust vs C++     the Rust core against both hugin-native copies, AES and portable builds,
#                      over the same seeded commands (keys, derivations, key images,
#                      hash_to_ec, hash_to_scalar, sc_reduce32, cn_fast_hash, the slow hash,
#                      underive, subwallet keys, processBlockOutputs on generated blocks)
#   3. iOS vs Android  the two C++ copies against each other
#   4. signatures      every backend's signatures (random) verified by every other backend
#
# usage: compat/run.sh [seed] [count]      (defaults: seed 1, count 2000)
# env:   CAP   optional wrapper for heavy steps (default: none; e.g. CAP="nice -n 19")
#        RUN   prefix for running the drivers (e.g. qemu-aarch64 for a cross build)
#        HUGIN path of the hugin-native checkout (default ../hugin-native next to this repo)
# exit:  0 all identical, 1 a difference or failure
set -euo pipefail
cd "$(dirname "$0")"

SEED=${1:-1}
COUNT=${2:-2000}
CAP=${CAP:-}
RUN=${RUN:-}
MAKE_VARS=()
[[ -n ${HUGIN:-} ]] && MAKE_VARS+=("HUGIN=$HUGIN")
[[ -n ${CROSS:-} ]] && MAKE_VARS+=("CROSS=$CROSS")

# one heavy step at a time: cargo first, then the C++ build with 3 jobs
make rust "CAP=$CAP" "${MAKE_VARS[@]}"
$CAP nice -n 19 make -j3 drivers "${MAKE_VARS[@]}"

B=build
OUT=$B/out
mkdir -p "$OUT"
CPP="ios-aes ios-portable android-aes android-portable"
ALL="$CPP rust"
MONERO=../../xkr-wallet-core/tests/data/crypto_vectors.txt
fail=0

note() { printf '%s\n' "$*"; }

# shellcheck disable=SC2086 # RUN is a command prefix
driver() { local name=$1; shift; $RUN "$B/driver-$name" "$@"; }

note "== 1. known answers"
for b in $ALL; do
    driver "$b" vectors kat.txt "$MONERO" || fail=1
done

note "== commands: seed $SEED, count $COUNT"
driver rust gen "$SEED" "$COUNT" > "$OUT/commands.txt"
note "$(wc -l < "$OUT/commands.txt") commands"
for b in $ALL; do
    # shellcheck disable=SC2086 # CAP and RUN are command prefixes
    $CAP nice -n 19 $RUN "$B/driver-$b" run < "$OUT/commands.txt" > "$OUT/$b.txt"
    grep -v '^VERIFY ' "$OUT/$b.txt" > "$OUT/$b.results" || true
    grep '^VERIFY ' "$OUT/$b.txt" | cut -c8- > "$OUT/$b.verify" || true
    if grep -q -e 'INCONSISTENT' -e 'own=false' -e ' => UNKNOWN' "$OUT/$b.results"; then
        note "$b: internal inconsistency:"
        grep -m 5 -e 'INCONSISTENT' -e 'own=false' -e ' => UNKNOWN' "$OUT/$b.results"
        fail=1
    fi
done

compare() {
    local a=$1 b=$2
    if cmp -s "$OUT/$a.results" "$OUT/$b.results"; then
        note "identical: $a = $b ($(wc -l < "$OUT/$a.results") results)"
    else
        # the whole diff goes to a file first: no pipe into a reader that stops early
        local d="$OUT/$a-vs-$b.diff"
        diff "$OUT/$a.results" "$OUT/$b.results" > "$d" || true
        note "DIFFERENT: $a vs $b ($(grep -c '^<' "$d" || true) lines, full diff in $d), first ones:"
        sed -n '1,12p' "$d"
        fail=1
    fi
}

note "== 2. Rust vs C++"
for b in $CPP; do
    compare rust "$b"
done

note "== 3. iOS copy vs Android copy (and AES vs portable builds)"
compare ios-aes android-aes
compare ios-portable android-portable
compare ios-aes ios-portable
compare android-aes android-portable

note "== 4. cross verification of signatures"
for signer in $ALL; do
    for verifier in $ALL; do
        bad=$(driver "$verifier" run < "$OUT/$signer.verify" | grep -v -c ' => ok$' || true)
        total=$(wc -l < "$OUT/$signer.verify")
        if [[ $bad != 0 ]]; then
            note "DIFFERENT: $verifier rejects $bad of $total checks made by $signer"
            fail=1
        fi
    done
    note "signatures of $signer: $(wc -l < "$OUT/$signer.verify") checks run by every backend"
done

if [[ $fail == 0 ]]; then
    note "ALL IDENTICAL"
else
    note "DIFFERENCES FOUND (outputs in $OUT)"
fi
exit "$fail"
