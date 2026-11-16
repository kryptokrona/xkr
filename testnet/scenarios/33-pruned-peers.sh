#!/usr/bin/env bash
# 33-pruned-peers: a stripe-pruned xkrd serves only the stripe it holds; a fresh node syncs that
# stripe from it, reports the missing one in /status and finishes once a full node is added.
#
#   C++ seed  (shared, see 10) <- xkrd A dials it once to sync the seed chain
#   xkrd A    P2P 23898, RPC 23899, full; after the seed sync it runs without peers
#             (--add-exclusive-node 127.0.0.1:9) and extends its chain to PRUNED_TOP with
#             `xkr-testwallet mine` (getblocktemplate/submitblock; it sets the merge-mining tag
#             like a miner does and keeps nonce 0, which testnet difficulty 1 accepts), so the
#             shared seed chain is not changed
#   xkrd P    P2P 23888, RPC 23889: a copy of A's chain.redb after `xkrd db prune --stripes 4
#             --stripe 1 --below PRUNE_BELOW --force-below` (testnet never prunes by itself: only
#             genesis is a checkpoint, so its safety bound is 0 and `--below` above it needs the
#             override); seed 4 << 8 | 1 = 1025, keeps span 0 (blocks 0..4095) and everything
#             from PRUNE_BELOW (8192) up, drops span 1 (4096..8191); no outbound peers
#   xkrd F    no P2P listener, RPC 23879, empty data dir; first only P (--add-exclusive-node),
#             then P and A
#   C++ C     container xkr-suite-pruned-cpp (old image), P2P 23868, RPC 23869, fresh, only P
#
# PRUNED_TOP must keep span 1 out of P's recent blocks as F sees them
# (downloader::PRUNED_TIP_BLOCKS = 7 680 below P's height), so it is at least 11 777.
#
# Checks: (1) A syncs the seed chain; (2) A extends to PRUNED_TOP; (3) `db prune` on the copy
# reports seed 0x0401 below PRUNE_BELOW and `db verify` finds 0 problems; (4) P's /getinfo
# pruning_seed is 1025 (A's 0) and /status.pruning is seed 1025, full false, stripes 4, stripe
# 1 at A's top; (5) F with only P reaches top 4095 (span 0), stays there, /status.pruning
# missing_stripe 1026 (stripe 2 of 4) with one pruned peer, logs the missing range and keeps P
# (no ban); (6) the C++ node syncs what P holds (top 3 895..4 095: it drops P after a partial
# answer without using it) and keeps reconnecting (3+ inbound connections on P, no ban on P);
# (7) F with P and A reaches A's top, its headers at the span edges equal A's, missing_stripe is
# null again, and `db verify` on F finds 0 problems.
#
# Env: PRUNED_TOP (12300), PRUNE_BELOW (8192), TIMEOUT (900), LOG_LEVEL, KEEP=1.
# suite: expected=150 timeout=1200
set -euo pipefail
: "${TIMEOUT:=900}"
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

PRUNED_TOP=${PRUNED_TOP:-12300}
PRUNE_BELOW=${PRUNE_BELOW:-8192}
SPAN=4096
C_PRUNED_CPP=xkr-suite-pruned-cpp
A_RPC=127.0.0.1:$PORT_PR_A_RPC
P_RPC=127.0.0.1:$PORT_PR_P_RPC
F_RPC=127.0.0.1:$PORT_PR_F_RPC
C_RPC=127.0.0.1:$PORT_PR_CPP_RPC
P_PEER=127.0.0.1:$PORT_PR_P_P2P
A_PEER=127.0.0.1:$PORT_PR_A_P2P
SEED_PEER=127.0.0.1:$PORT_SEED_P2P
# The seed of stripe 1 of 4 (what P keeps) and of the stripe holding span 1 (what F misses).
P_SEED=$(((4 << 8) | 1))
MISSING_SEED=$(((4 << 8) | 2))

require_xkrd
require_testwallet
require_cmd curl
require_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE"
[ "$PRUNE_BELOW" -ge $((2 * SPAN)) ] && [ $((PRUNE_BELOW % SPAN)) = 0 ] \
    || die 0 "PRUNE_BELOW ($PRUNE_BELOW) must be a multiple of $SPAN and at least $((2 * SPAN))"
[ "$PRUNED_TOP" -ge $((SPAN + 7680 + 1)) ] && [ "$PRUNED_TOP" -ge "$PRUNE_BELOW" ] \
    || die 0 "PRUNED_TOP ($PRUNED_TOP) must be at least $((SPAN + 7680 + 1)) and PRUNE_BELOW"
reclaim pr-a pr-p pr-f
rm_container "$C_PRUNED_CPP"
require_ports_free "$PORT_PR_A_P2P" "$PORT_PR_A_RPC" "$PORT_PR_P_P2P" "$PORT_PR_P_RPC" \
    "$PORT_PR_F_RPC" "$PORT_PR_CPP_P2P" "$PORT_PR_CPP_RPC"

# start_a <log> <exclusive peer>: xkrd A with its listener
start_a() {
    start_xkrd pr-a "$PORT_PR_A_RPC" "$SD/a" "$1" \
        --p2p-bind-ip 127.0.0.1 --p2p-bind-port "$PORT_PR_A_P2P" --add-exclusive-node "$2" --no-hyper
}
# start_p <log>: xkrd P (pruned copy), accepts connections, dials nobody
start_p() {
    start_xkrd pr-p "$PORT_PR_P_RPC" "$SD/p" "$1" \
        --p2p-bind-ip 127.0.0.1 --p2p-bind-port "$PORT_PR_P_P2P" --add-exclusive-node "$DEAD_PEER" --no-hyper
}
# start_f <log> <peer...>: xkrd F without a listener, dialing only the given peers
start_f() {
    local log=$1 peers=() x
    shift
    for x in "$@"; do peers+=(--add-exclusive-node "$x"); done
    start_xkrd pr-f "$PORT_PR_F_RPC" "$SD/f" "$log" --p2p-bind-port 0 --no-hyper "${peers[@]}"
}
pruning_of() { obj "$(get "$1" /status)" pruning; }
# lines of a log with both fixed texts
log_count2() { { grep -aF -- "$2" "$1" 2>/dev/null || true; } | { grep -acF -- "$3" || true; } | head -n 1; }

# ------------------------------------------------------------------ 1. A syncs the seed chain

ensure_wallets
ensure_seed
start_a "$SD/a-seed.log" "$SEED_PEER"
watch pr-a
if wait_same_top "$A_RPC" "$SEED_RPC"; then
    pass 1 "xkrd A synced the seed chain: top $(top_of "$A_RPC")"
else
    die 1 "A [$(top_of "$A_RPC")] did not reach the seed's top [$(top_of "$SEED_RPC")] within ${TIMEOUT}s"
fi
unwatch
stop_proc pr-a

# ------------------------------------------------------------------ 2. A extends its own chain

start_a "$SD/a-extend.log" "$DEAD_PEER"
watch pr-a
H0=$(height_of "$A_RPC")
t0=$(date +%s)
# getblocktemplate + merge-mining tag + submitblock until A's top index is PRUNED_TOP; the
# helper stops with an error after five refused blocks in a row (a progress line per 1 000).
if wallet "$W1_DIR" "$A_RPC" mine --address "$W1" --to "$PRUNED_TOP" > "$SD/mine.out" 2>&1; then
    secs=$(($(date +%s) - t0))
    pass 2 "A extended its chain from $H0 to $(height_of "$A_RPC") without peers in ${secs}s ($(((PRUNED_TOP - H0) / (secs > 0 ? secs : 1))) blocks/s)"
else
    check_alive
    die 2 "A stopped accepting submitted blocks at $(height_of "$A_RPC") (wanted $PRUNED_TOP): $(tail -n 2 "$SD/mine.out" | tr '\n' ' ')(log $SD/a-extend.log, $SD/mine.out)"
fi
read -r TOP TOP_HASH _ <<< "$(top_of "$A_RPC")"
unwatch
stop_proc pr-a

# ------------------------------------------------------------------ 3. prune a copy

mkdir -p "$SD/p"
cp "$SD/a/chain.redb" "$SD/p/chain.redb"
if xkrd_db "$SD/db-prune.out" prune --network testnet --data-dir "$SD/p" \
    --stripes 4 --stripe 1 --below "$PRUNE_BELOW" --force-below \
    && grep -q "seed stripe 1/4 (0x0401), pruned below $PRUNE_BELOW" "$SD/db-prune.out"; then
    pass 3 "db prune on the copy: $(grep '^seed ' "$SD/db-prune.out" | tail -n 1)"
else
    die 3 "db prune failed: $(tail -n 3 "$SD/db-prune.out" | tr '\n' ' ')"
fi
if xkrd_db "$SD/db-verify-p.out" verify --network testnet --data-dir "$SD/p" \
    && grep -q ': 0 problems$' "$SD/db-verify-p.out"; then
    pass 3 "db verify on the pruned copy: $(grep '^verified blocks' "$SD/db-verify-p.out" | tail -n 1)"
else
    fail 3 "db verify on the pruned copy failed: $(tail -n 3 "$SD/db-verify-p.out" | tr '\n' ' ')"
fi

# ------------------------------------------------------------------ 4. P announces its seed

start_a "$SD/a.log" "$DEAD_PEER"
start_p "$SD/p.log"
watch pr-a pr-p
ps=$(num "$(get "$P_RPC" /getinfo)" pruning_seed)
as=$(num "$(get "$A_RPC" /getinfo)" pruning_seed)
if [ "$ps" = "$P_SEED" ] && [ "$as" = 0 ]; then
    pass 4 "/getinfo pruning_seed: P $ps (stripe 1 of 4), A $as (full)"
else
    fail 4 "/getinfo pruning_seed: P [$ps] (want $P_SEED), A [$as] (want 0)"
fi
pp=$(pruning_of "$P_RPC")
if [ "$(num "$pp" seed)" = "$P_SEED" ] && [ "$(bool "$pp" full)" = false ] \
    && [ "$(num "$pp" stripes)" = 4 ] && [ "$(num "$pp" stripe)" = 1 ] \
    && [ "$(top_hash_of "$P_RPC")" = "$TOP_HASH" ]; then
    pass 4 "P /status.pruning $pp at A's top $TOP"
else
    fail 4 "P /status.pruning $pp, top [$(top_of "$P_RPC")] (A: $TOP $TOP_HASH)"
fi

# ------------------------------------------------------------------ 5. F with only P

LAST_HELD=$((SPAN - 1))
start_f "$SD/f-p.log" "$P_PEER"
watch pr-a pr-p pr-f
missing_reported() { [ "$(num "$(pruning_of "$F_RPC")" missing_stripe)" = "$MISSING_SEED" ]; }
if wait_height "$F_RPC" "$LAST_HELD" && wait_for "$TIMEOUT" missing_reported; then
    pass 5 "F synced span 0 from P alone: top $(height_of "$F_RPC")"
else
    fail 5 "F at [$(top_of "$F_RPC")], /status.pruning $(pruning_of "$F_RPC"); wanted top $LAST_HELD and missing_stripe $MISSING_SEED"
fi
sleep 10 # F must stay at the end of span 0: nobody connected holds span 1
fh=$(height_of "$F_RPC")
fp=$(pruning_of "$F_RPC")
if [ "$fh" = "$LAST_HELD" ] && [ "$(num "$fp" missing_stripe)" = "$MISSING_SEED" ] \
    && [ "$(num "$fp" pruned_peers)" = 1 ] \
    && [ "$(header_hash_at "$F_RPC" "$LAST_HELD")" = "$(header_hash_at "$A_RPC" "$LAST_HELD")" ]; then
    pass 5 "F stays at $fh (block $LAST_HELD equal to A's), /status.pruning $fp"
else
    fail 5 "F at [$fh] (want $LAST_HELD), /status.pruning $fp (want missing_stripe $MISSING_SEED, pruned_peers 1)"
fi
warned=$(log_count "$SD/f-p.log" "no connected peer holds these blocks in full")
fout=$(num "$(get "$F_RPC" /status)" legacy_outgoing)
fbans=$(log_count2 "$SD/f-p.log" "dropping peer" "ban=true")
pbans=$(log_count2 "$SD/p.log" "dropping peer" "ban=true")
if [ "${warned:-0}" -ge 1 ] && [ "${fout:-0}" -ge 1 ] && [ "${fbans:-0}" = 0 ] && [ "${pbans:-0}" = 0 ]; then
    pass 5 "F logged the missing range ($warned warning(s)) and keeps P (legacy_outgoing $fout, no bans on F or P)"
else
    fail 5 "F: missing-range warnings $warned, legacy_outgoing $fout, bans on F $fbans, on P $pbans"
fi
unwatch
stop_proc pr-f

# ------------------------------------------------------------------ 6. a C++ node and P

# P with a fresh log, so its inbound connections are the C++ node's.
stop_proc pr-p
start_p "$SD/p-cpp.log"
watch pr-a pr-p
start_cpp "$C_PRUNED_CPP" "$CPP_OLD_IMAGE" "$PORT_PR_CPP_P2P" "$PORT_PR_CPP_RPC" - \
    --add-exclusive-node="$P_PEER"
CPP_MIN=$((SPAN - 1 - 200))
if wait_height "$C_RPC" "$CPP_MIN"; then
    ch=$(height_of "$C_RPC")
    if [ "$ch" -le "$LAST_HELD" ] \
        && [ "$(header_hash_at "$C_RPC" "$ch")" = "$(header_hash_at "$A_RPC" "$ch")" ]; then
        pass 6 "C++ node synced the stripe P holds from P: top $ch (A's block $ch)"
    else
        fail 6 "C++ node at [$(top_of "$C_RPC")]: beyond $LAST_HELD or not A's chain"
    fi
else
    fail 6 "C++ node at [$(top_of "$C_RPC")] after ${TIMEOUT}s, wanted at least $CPP_MIN from P"
fi
reconnected() { [ "$(log_count2 "$SD/p-cpp.log" "peer connected" "inbound=true")" -ge 3 ]; }
if wait_for 180 reconnected; then
    cbans=$(log_count2 "$SD/p-cpp.log" "dropping peer" "ban=true")
    partial=$(docker logs "$C_PRUNED_CPP" 2>&1 | grep -c "returned not all requested objects" || true)
    if [ "${cbans:-0}" = 0 ]; then
        pass 6 "the C++ node keeps reconnecting to P ($(log_count2 "$SD/p-cpp.log" "peer connected" "inbound=true") inbound connections, $partial partial answers in its log, no ban on P)"
    else
        fail 6 "P banned a peer while serving the C++ node ($cbans lines, see $SD/p-cpp.log)"
    fi
else
    fail 6 "P saw $(log_count2 "$SD/p-cpp.log" "peer connected" "inbound=true") inbound connections from the C++ node in 180 s (want 3: it must reconnect after a partial answer)"
fi

# ------------------------------------------------------------------ 7. F with P and A

start_f "$SD/f-pa.log" "$P_PEER" "$A_PEER"
watch pr-a pr-p pr-f
if wait_same_top "$F_RPC" "$A_RPC"; then
    pass 7 "F with P and A reached A's top $(top_of "$F_RPC")"
else
    fail 7 "F [$(top_of "$F_RPC")] did not reach A's top [$(top_of "$A_RPC")] within ${TIMEOUT}s"
fi
diff=""
for h in $((SPAN - 1)) "$SPAN" $((PRUNE_BELOW - 1)) "$PRUNE_BELOW" "$TOP"; do
    if [ "$(header_hash_at "$F_RPC" "$h")" != "$(header_hash_at "$A_RPC" "$h")" ]; then
        diff="$diff $h"
    fi
done
fp=$(pruning_of "$F_RPC")
if [ -z "$diff" ] && [ -z "$(num "$fp" missing_stripe)" ]; then
    pass 7 "F's blocks $((SPAN - 1)), $SPAN, $((PRUNE_BELOW - 1)), $PRUNE_BELOW and $TOP equal A's; /status.pruning $fp"
else
    fail 7 "F differs from A at [${diff# }], /status.pruning $fp"
fi
unwatch
stop_proc pr-f
if xkrd_db "$SD/db-verify-f.out" verify --network testnet --data-dir "$SD/f" \
    && grep -q ': 0 problems$' "$SD/db-verify-f.out"; then
    pass 7 "db verify on F: $(grep '^verified blocks' "$SD/db-verify-f.out" | tail -n 1)"
else
    fail 7 "db verify on F failed: $(tail -n 3 "$SD/db-verify-f.out" | tr '\n' ' ')"
fi
finish
