#!/usr/bin/env bash
# 13-transactions: version-1 transfers W1 -> W2 sent through xkrd, relayed to the C++ node, mined
# once by the C++ node's template and once by xkrd's.
#
#   C++ seed  (shared, see 10) <- xkrd S (P2P 17898, RPC 17899); wallets W1 (seed-chain coinbase
#   outputs) and W2 in <wd>/wallets (xkr-testwallet)
#   TX_A  1.5 XKR, --tx-version 1 --mixin 0, mined by the C++ miner against the C++ seed
#   TX_B  2.5 XKR, --tx-version 1 --mixin 1 (one decoy from /getrandom_outs), mined by the C++
#         miner against S (xkrd's getblocktemplate fills it from xkrd's pool)
#
# Checks per transaction: S accepts it (status OK, version 1, the mixin asked for); it reaches the
# C++ seed's pool (xkrd -> C++ relay passes C++ validation); it is mined; S and the C++ seed have it
# in the same block (equal block hash); its inputs have mixin + 1 ring members (f_transaction_json).
# Then W2's unlocked balance rose by exactly 4.0 XKR, scanned through S and through the C++ seed.
# suite: expected=120 timeout=1200
set -euo pipefail
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

require_xkrd
require_testwallet
require_cmd curl
require_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE"

ensure_suite_chain "$SD/xkrd-S.log"
wait_funds "$W1_DIR" "$S_RPC" 1000000 || die 0 "W1 has no 10 XKR unlocked through S"
B0_S=$(unlocked_of "$W2_DIR" "$S_RPC")
B0_C=$(unlocked_of "$W2_DIR" "$SEED_RPC")
[ -n "$B0_S" ] && [ -n "$B0_C" ] || die 0 "W2's balance scan failed (S [$B0_S], C++ [$B0_C])"
note "W1 $W1"
note "W2 $W2 (unlocked before: $B0_S through S, $B0_C through the C++ seed)"

# one_tx <id> <amount> <mixin> <miner daemon> <miner daemon name>
one_tx() {
    local id=$1 amount=$2 mixin=$3 daemon=$4 dname=$5 tx h hs hc j rings bad n
    if tx=$(send_tx "$W1_DIR" "$S_RPC" "$W2" "$id" --tx-version 1 --mixin "$mixin" "$amount") && [ -n "$tx" ]; then
        pass "$id" "W1 -> W2 $amount XKR through S: $tx ($(grep -o '^version.*' "$SD/send-$id.out"))"
    else
        fail "$id" "send through S refused: $(tail -n 3 "$SD/send-$id.out" | tr '\n' ' ')"
        return 0
    fi
    if ! grep -q "^version 1 .* mixin $mixin " "$SD/send-$id.out"; then
        fail "$id" "the wallet built [$(grep -o '^version.*' "$SD/send-$id.out")], wanted version 1 mixin $mixin"
    fi
    if wait_for 120 tx_in_pool "$SEED_RPC" "$tx"; then
        pass "$id" "the C++ seed has $tx in its pool (relayed by xkrd)"
    else
        fail "$id" "$tx did not reach the C++ seed's pool within 120 s: $(flat "$(tx_status "$SEED_RPC" "$tx")" | cut -c1-200)"
    fi
    mine_cpp "$daemon" "$W1" 2 || fail "$id" "the C++ miner against $dname did not mine 2 blocks"
    if h=$(wait_mined "$S_RPC" "$tx" 120) && [ -n "$h" ] && wait_for 120 tx_in_block "$SEED_RPC" "$tx"; then
        wait_height "$SEED_RPC" "$h" 60 || true
        hs=$(header_hash_at "$S_RPC" "$h")
        hc=$(header_hash_at "$SEED_RPC" "$h")
        if [ -n "$hs" ] && [ "$hs" = "$hc" ]; then
            pass "$id" "mined via $dname at height $h; S and the C++ seed agree on block $hs"
        else
            fail "$id" "block $h: S $hs, C++ seed $hc"
        fi
    else
        fail "$id" "$tx not in a block on both nodes (S: $(flat "$(tx_status "$S_RPC" "$tx")" | cut -c1-120); C++: $(flat "$(tx_status "$SEED_RPC" "$tx")" | cut -c1-120))"
        return 0
    fi
    j=$(tx_json "$S_RPC" "$tx")
    rings=$({ flat "$j" | grep -o '"key_offsets":\[[^]]*\]' || true; } | sed 's/[^,]//g' | awk '{print length($0) + 1}' | sort -u | tr '\n' ' ')
    n=$(count "$j" '"key_offsets":')
    if [ "$(num "$j" version)" = 1 ] && [ "$n" -ge 1 ] && [ "$rings" = "$((mixin + 1)) " ]; then
        pass "$id" "f_transaction_json: version 1, $n input(s), ring size $((mixin + 1))"
    else
        fail "$id" "f_transaction_json: version $(num "$j" version), $n inputs, ring sizes [$rings], want $((mixin + 1))"
    fi
}

one_tx tx-a 1.5 0 "$SEED_RPC" "the C++ seed"
one_tx tx-b 2.5 1 "$S_RPC" "xkrd S"

want=$(( ${B0_S:-0} + 400000 ))
wait_same_top "$S_RPC" "$SEED_RPC" 120 || true
B1_S=$(unlocked_of "$W2_DIR" "$S_RPC")
B1_C=$(unlocked_of "$W2_DIR" "$SEED_RPC")
wallet "$W2_DIR" "$S_RPC" balance > "$SD/balance-w2-S.out" 2>&1 || true
wallet "$W2_DIR" "$SEED_RPC" balance > "$SD/balance-w2-seed.out" 2>&1 || true
if [ "$B1_S" = "$want" ] && [ "$((B1_C - B0_C))" = 400000 ]; then
    pass 3 "W2 unlocked +4.00000 XKR through S ($B0_S -> $B1_S) and through the C++ seed ($B0_C -> $B1_C)"
else
    fail 3 "W2 unlocked through S $B0_S -> $B1_S, through the C++ seed $B0_C -> $B1_C; want +400000 atomic units on both"
fi
finish
