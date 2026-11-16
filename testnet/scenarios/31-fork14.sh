#!/usr/bin/env bash
# 31-fork14: transaction version 2 (view tags, single-key CLSAG) and block major version 7 on a
# fresh testnet (docs/forks.md §5, docs/tx-v2.md). Two xkrd nodes and the patched C++ fork-13
# node start from genesis; the miner node schedules fork 13 at F13 and fork 14 at F14 with a
# GRACE-block version-1 grace period. Expected: xkrd A and B switch to v7 blocks and version-2
# coinbases at F14 + 1, a v1 transaction before F14 and in the grace period and a v2 transaction
# after F14 are mined and seen by B, a v1 transaction after the grace is refused by the pool, and
# the C++ node (which knows major 6 but not 7) follows through fork 13 and stops at F14.
#
#   xkrd A    P2P 127.0.0.1:14898, RPC 14899, --mine W1 (testnet difficulty 1: 1 block/s),
#             --out-peers 0 (dials nobody: the chain is its own)
#       ^ Levin, B dials it (--add-peer 127.0.0.1:14898)
#   xkrd B    no P2P listener, RPC 14889, no miner, --out-peers 0
#       ^ Levin, the C++ node dials A (--add-exclusive-node 127.0.0.1:14898)
#   C++       container xkr-suite-f14-cpp (CPP_CONTAINER; image xkr-cpp-testnet:fork13),
#             --network host, P2P 15898, RPC 15899, XKR_FORK13_HEIGHT=F13
#
# Steps: (1) wallets, nodes, fork tables; (2) before F14 a v1 transaction W1 -> W2 through A,
# mined, A/B/C++ agree; (3) after F14 block F14 + 1 is v7 with a version-2 coinbase (tagged
# outputs) on A and B; (4a) in the grace period a v1 transaction is still mined (sent first:
# the grace is short); (3b) a v2 transaction W1 -> W2 (default version from /getinfo
# tx_versions, MIXIN_V2 decoys) is mined, B agrees, W2's balance through B shows it and reports
# outputs skipped by view tag; (4b) after F14 + GRACE a v1 send is refused ("transaction version
# not allowed at this height"); (5) the C++ node sits at F14 with the deserialisation error in
# its log, A drops it as "on an older fork" and /status.fork.active_index = 14; (6) summary.
#
# Env: F13, F14, GRACE (60, 140, 20), MIXIN_V2 (2), CPP_SLACK (1), TIMEOUT (600),
# OLDER_FORK_WAIT (300), CPP_IMAGE (= CPP_FORK13_IMAGE), CPP_CONTAINER, LOG_LEVEL, KEEP=1.
# suite: expected=480 timeout=1800
set -euo pipefail
: "${TIMEOUT:=600}"
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

CPP_IMAGE=${CPP_IMAGE:-$CPP_FORK13_IMAGE}
CPP_CONTAINER=${CPP_CONTAINER:-xkr-suite-f14-cpp}
F13=${F13:-60}
F14=${F14:-140}
GRACE=${GRACE:-20}
MIXIN_V2=${MIXIN_V2:-2}
CPP_SLACK=${CPP_SLACK:-1}
OLDER_FORK_WAIT=${OLDER_FORK_WAIT:-300}
A_RPC=127.0.0.1:$PORT_A_RPC
B_RPC=127.0.0.1:$PORT_B_RPC
CPP_RPC=127.0.0.1:$PORT_CPP_RPC

require_xkrd
require_testwallet
require_cmd curl
require_image "$CPP_IMAGE" "$BUILD_FORK13_IMAGE"
[ "$F13" -lt "$F14" ] || die 0 "F13 ($F13) must be below F14 ($F14)"
[ "$F13" -ge 10 ] || die 0 "F13 ($F13) is too low: the v1 send needs a few mined blocks first"
reclaim f14-a f14-b
rm_container "$CPP_CONTAINER"
require_ports_free "$PORT_A_P2P" "$PORT_A_RPC" "$PORT_B_RPC" "$PORT_CPP_P2P" "$PORT_CPP_RPC"

# Output targets in the explorer JSON: a plain key is `"data":{"key":"…"}`, a tagged key has
# `"view_tag"` next to it (key order differs with serde_json's preserve_order, the closing brace
# right after the key does not).
PLAIN_OUT='"data":{"key":"[0-9a-f]*"}'

# hash of the coinbase transaction of the block at a height (xkrd f_block_json takes a height
# in "hash" too)
coinbase_at() {
    local r
    r=$(rpc "$1" f_block_json "{\"hash\":\"$2\"}")
    { flat "$r" | grep -o '"transactions":\[{[^}]*}' | grep -o '"hash":"[0-9a-f]*"' || true; } \
        | head -n 1 | cut -d'"' -f4
}
# send <label> <wallet args...>: W1 -> W2 through A; prints the tx hash
send() {
    local label=$1
    shift
    send_tx "$WORK_W1" "$A_RPC" "$W2" "$label" "$@"
}

# ------------------------------------------------------------------ 1. wallets and nodes

WORK_W1=$SD/w1
WORK_W2=$SD/w2
W1=$(wallet_addr "$WORK_W1")
W2=$(wallet_addr "$WORK_W2")
[ -n "$W1" ] && [ -n "$W2" ] || die 1 "xkr-testwallet did not create the wallets"
pass 1 "wallets W1 $W1, W2 $W2"
echo "fork 13 at F13=$F13 (first v6 block $((F13 + 1))), fork 14 at F14=$F14 (first v7 block $((F14 + 1))), v1 grace until $((F14 + GRACE))"

FORK_ARGS=(--fork13-height "$F13" --fork14-height "$F14" --fork14-grace "$GRACE" --no-hyper --out-peers 0)
# A binds P2P to loopback only: nothing outside this host can connect and hand it a longer chain.
start_xkrd f14-a "$PORT_A_RPC" "$SD/a" "$SD/a.log" "${FORK_ARGS[@]}" \
    --p2p-bind-ip 127.0.0.1 --p2p-bind-port "$PORT_A_P2P" --mine "$W1" --mine-threads 1
start_xkrd f14-b "$PORT_B_RPC" "$SD/b" "$SD/b.log" "${FORK_ARGS[@]}" \
    --p2p-bind-port 0 --add-peer "127.0.0.1:$PORT_A_P2P"
watch f14-a f14-b
start_cpp "$CPP_CONTAINER" "$CPP_IMAGE" "$PORT_CPP_P2P" "$PORT_CPP_RPC" "XKR_FORK13_HEIGHT=$F13" \
    --add-exclusive-node="127.0.0.1:$PORT_A_P2P"
pass 1 "xkrd A (miner, pid ${PID[f14-a]}), xkrd B (follower, pid ${PID[f14-b]}) and C++ node $CPP_CONTAINER started"

for node in "A:$A_RPC" "B:$B_RPC"; do
    name=${node%%:*}; addr=${node#*:}
    info=$(get "$addr" /getinfo)
    heights=$(arr "$info" upgrade_heights)
    if printf '%s' "$heights" | grep -q ",$F13,$F14\]$" \
        && [ "$(num "$info" supported_height)" = "$F14" ] \
        && [ "$(arr "$info" tx_versions)" = "[1]" ]; then
        pass 1 "$name /getinfo: upgrade_heights end with $F13,$F14, supported_height $F14, tx_versions [1]"
    else
        fail 1 "$name /getinfo: upgrade_heights $heights supported_height $(num "$info" supported_height) tx_versions $(arr "$info" tx_versions)"
    fi
done
CPP_INFO=$(get "$CPP_RPC" /getinfo)
if [ "$(num "$CPP_INFO" supported_height)" = "$F13" ]; then
    pass 1 "C++ node reports supported_height $F13 (fork 13 from XKR_FORK13_HEIGHT)"
else
    fail 1 "C++ node supported_height=$(num "$CPP_INFO" supported_height), expected $F13 (is $CPP_IMAGE built with 0001-fork13-testnet.patch?)"
fi

# ------------------------------------------------------------------ 2. v1 before fork 14

wait_height "$A_RPC" $((F13 + 3)) || die 2 "A did not reach $((F13 + 3)) within ${TIMEOUT}s"
wait_funds "$WORK_W1" "$A_RPC" 500000 || die 2 "W1 has no 5 XKR unlocked within ${TIMEOUT}s"
H=$(height_of "$A_RPC")
[ "$H" -lt $((F14 - 15)) ] || die 2 "A is already at $H, too close to F14=$F14 for the v1 check (raise F14)"
TX1=$(send v1-before --tx-version 1 2.5) \
    || die 2 "v1 send failed: $(tail -n 3 "$SD/send-v1-before.out" | tr '\n' ' ')"
pass 2 "W1 -> W2 2.5 XKR, version 1, at height $H: $TX1 ($(grep -o '^version.*' "$SD/send-v1-before.out"))"
H1=""
if H1=$(wait_mined "$A_RPC" "$TX1") && [ -n "$H1" ]; then
    if [ "$H1" -le "$F14" ]; then
        pass 2 "v1 transaction mined at height $H1 <= F14"
    else
        fail 2 "v1 transaction mined at height $H1 > F14=$F14"
    fi
    wait_height "$B_RPC" "$H1" || fail 2 "B did not reach $H1"
    wait_height "$CPP_RPC" "$H1" || fail 2 "C++ node did not reach $H1"
    ha=$(header_at "$A_RPC" "$H1"); hb=$(header_at "$B_RPC" "$H1"); hc=$(header_at "$CPP_RPC" "$H1")
    if [ -n "$ha" ] && [ "$ha" = "$hb" ] && [ "$hb" = "$hc" ]; then
        pass 2 "A, B and C++ agree on block $H1 (${ha%% *}, v${ha#* })"
    else
        fail 2 "block $H1 differs: A=[$ha] B=[$hb] C++=[$hc]"
    fi
    if tx_in_block "$B_RPC" "$TX1"; then
        pass 2 "B has the v1 transaction in a block"
    else
        fail 2 "B does not have $TX1 in a block: $(tx_status "$B_RPC" "$TX1")"
    fi
else
    H1=""
    fail 2 "v1 transaction $TX1 not mined within ${TIMEOUT}s"
fi

# ------------------------------------------------------------------ 3. the fork-14 block

wait_height "$A_RPC" $((F14 + 1)) || die 3 "A did not reach $((F14 + 1)) within ${TIMEOUT}s"
wait_height "$B_RPC" $((F14 + 1)) || fail 3 "B did not reach $((F14 + 1))"
for node in "A:$A_RPC" "B:$B_RPC"; do
    name=${node%%:*}; addr=${node#*:}
    before=$(header_at "$addr" "$F14"); after=$(header_at "$addr" $((F14 + 1)))
    if [ "${before#* }" = 6 ] && [ "${after#* }" = 7 ]; then
        pass 3 "$name: block $F14 is v6, block $((F14 + 1)) is v7"
    else
        fail 3 "$name: block $F14 major ${before#* }, block $((F14 + 1)) major ${after#* } (want 6, 7)"
    fi
    cb=$(coinbase_at "$addr" $((F14 + 1)))
    j=$(tx_json "$addr" "$cb")
    tagged=$(count "$j" '"type":"03"'); plain=$(count "$j" "$PLAIN_OUT"); tags=$(count "$j" '"view_tag":"[0-9a-f][0-9a-f]"')
    if [ -n "$cb" ] && [ "$(num "$j" version)" = 2 ] && [ "$tagged" -gt 0 ] && [ "$plain" = 0 ] && [ "$tags" = "$tagged" ]; then
        pass 3 "$name: coinbase $cb of block $((F14 + 1)) is version 2 with $tagged tagged outputs (type 03 + view_tag)"
    else
        fail 3 "$name: coinbase [$cb] version $(num "$j" version), $tagged tagged / $plain plain outputs, $tags view tags"
    fi
    cb0=$(coinbase_at "$addr" "$F14"); j0=$(tx_json "$addr" "$cb0")
    if [ "$(num "$j0" version)" = 1 ] && [ "$(count "$j0" '"type":"03"')" = 0 ]; then
        pass 3 "$name: coinbase of block $F14 is still version 1 (untagged)"
    else
        fail 3 "$name: coinbase of block $F14 is version $(num "$j0" version) with $(count "$j0" '"type":"03"') tagged outputs"
    fi
done
info=$(get "$A_RPC" /getinfo)
tv=$(arr "$info" tx_versions)
if [ "$tv" = "[1,2]" ]; then
    pass 3 "A /getinfo tx_versions $tv (grace period)"
else
    fail 3 "A /getinfo tx_versions $tv during the grace period (height $(num "$info" height)), want [1,2]"
fi

# ------------------------------------------------------------------ 4a. v1 in the grace period

# Sent first: the grace period is GRACE blocks (~GRACE seconds), the v2 round trip can wait.
wait_funds "$WORK_W1" "$A_RPC" 300000 || fail 4 "W1 has no 3 XKR unlocked"
H=$(height_of "$A_RPC")
TX3=""; H3=""
if TX3=$(send v1-grace --tx-version 1 1.5); then
    pass 4 "v1 send at height $H (grace until $((F14 + GRACE))) accepted by the pool: $TX3"
    if H3=$(wait_mined "$A_RPC" "$TX3") && [ -n "$H3" ]; then
        v=$(num "$(tx_json "$A_RPC" "$TX3")" version)
        if [ "$H3" -gt "$F14" ] && [ "$H3" -le $((F14 + GRACE)) ] && [ "$v" = 1 ]; then
            pass 4 "version-$v transaction mined at height $H3 in the grace period ($((F14 + 1))..$((F14 + GRACE)))"
        else
            fail 4 "grace transaction version $v mined at $H3, want version 1 in $((F14 + 1))..$((F14 + GRACE))"
        fi
    else
        H3=""
        fail 4 "grace v1 transaction $TX3 not mined (evicted after the grace? raise GRACE)"
    fi
else
    TX3=""
    fail 4 "v1 send in the grace period refused: $(tail -n 3 "$SD/send-v1-grace.out" | tr '\n' ' ')"
fi

# ------------------------------------------------------------------ 3b. v2 transaction

wait_funds "$WORK_W1" "$A_RPC" 300000 || fail 3b "W1 has no 3 XKR unlocked"
TX2=""; H2=""
if TX2=$(send v2 --mixin "$MIXIN_V2" 2.0); then
    :
elif grep -qE "decoys|getrandom_outs" "$SD/send-v2.out"; then
    note "not enough decoys for mixin $MIXIN_V2 ($(tail -n 1 "$SD/send-v2.out")); default mixin"
    TX2=$(send v2 2.0) || TX2=""
else
    TX2=""
fi
if [ -n "$TX2" ] && grep -q '^version 2 ' "$SD/send-v2.out"; then
    pass 3b "W1 -> W2 2.0 XKR with the default version (2, from /getinfo): $TX2 ($(grep -o '^version.*' "$SD/send-v2.out"))"
else
    fail 3b "v2 send failed or was not version 2: $(tail -n 3 "$SD/send-v2.out" | tr '\n' ' ')"
fi
if [ -n "$TX2" ] && H2=$(wait_mined "$A_RPC" "$TX2") && [ -n "$H2" ]; then
    j=$(tx_json "$A_RPC" "$TX2")
    if [ "$(num "$j" version)" = 2 ] && [ "$(count "$j" '"type":"03"')" -gt 0 ] \
        && [ "$(count "$j" "$PLAIN_OUT")" = 0 ] && flat "$j" | grep -q '"clsag_signatures":\[{'; then
        pass 3b "v2 transaction mined at height $H2: tagged outputs, CLSAG signatures"
    else
        fail 3b "transaction $TX2 at $H2 does not look like version 2: $(flat "$j" | cut -c1-300)"
    fi
    wait_height "$B_RPC" "$H2" || fail 3b "B did not reach $H2"
    ha=$(header_at "$A_RPC" "$H2"); hb=$(header_at "$B_RPC" "$H2")
    if [ -n "$ha" ] && [ "$ha" = "$hb" ] && tx_in_block "$B_RPC" "$TX2"; then
        pass 3b "B agrees on block $H2 (${ha%% *}) and has the v2 transaction"
    else
        fail 3b "B: block $H2 A=[$ha] B=[$hb], status $(tx_status "$B_RPC" "$TX2")"
    fi
    bal="$SD/balance-w2-b.out"
    wallet "$WORK_W2" "$B_RPC" balance > "$bal" 2>&1 || true
    if grep -q "tx $TX2:" "$bal"; then
        pass 3b "W2's balance through B lists the v2 output(s): $(grep -E '^(unlocked|locked) +[0-9]+\.[0-9]+$' "$bal" | tr '\n' ' ')"
    else
        fail 3b "W2's balance through B has no output of $TX2 (see $bal)"
    fi
    for t in "$TX1" "$TX3"; do
        [ -n "$t" ] || continue
        grep -q "tx $t:" "$bal" || fail 3b "W2's balance through B misses the v1 transaction $t"
    done
    line=$(grep '^scan ' "$bal" || true)
    skipped=$({ printf '%s' "$line" | grep -o '[0-9]* skipped by view tag' || true; } | cut -d' ' -f1)
    if [ -n "$skipped" ] && [ "$skipped" -gt 0 ]; then
        pass 3b "view tags: $line"
    else
        fail 3b "W2's scan reports no output skipped by view tag: [$line]"
    fi
else
    [ -z "$TX2" ] || fail 3b "v2 transaction $TX2 not mined within ${TIMEOUT}s"
    H2=""
fi

# ------------------------------------------------------------------ 4b. v1 after the grace

wait_height "$A_RPC" $((F14 + GRACE)) || fail 4b "A did not reach $((F14 + GRACE))"
info=$(get "$A_RPC" /getinfo)
tv=$(arr "$info" tx_versions)
if [ "$tv" = "[2]" ]; then
    pass 4b "A /getinfo tx_versions $tv after the grace period"
else
    fail 4b "A /getinfo tx_versions $tv after the grace period, want [2]"
fi
out="$SD/send-v1-late.out"
if send v1-late --tx-version 1 1.0 > /dev/null; then
    fail 4b "a v1 send after F14 + GRACE was accepted: $(tail -n 3 "$out" | tr '\n' ' ')"
elif grep -q "transaction version not allowed at this height" "$out"; then
    pass 4b "v1 send after F14 + GRACE refused by the pool: $(grep -o 'invalid transaction: transaction version not allowed at this height' "$out" | head -n 1)"
else
    fail 4b "v1 send after the grace failed for another reason: $(tail -n 3 "$out" | tr '\n' ' ')"
fi

# ------------------------------------------------------------------ 5. the C++ node stops at F14

sleep 10
c1=$(top_of "$CPP_RPC"); sleep 15; c2=$(top_of "$CPP_RPC")
ch=${c2%% *}
if [ -n "$ch" ] && [ "$ch" -le "$F14" ] && [ "$ch" -ge $((F14 - CPP_SLACK)) ] && [ "$c1" = "$c2" ]; then
    pass 5 "C++ node followed fork 13 and stopped at $ch (F14=$F14)"
else
    fail 5 "C++ node top [$c1] -> [$c2], expected a constant height in [$((F14 - CPP_SLACK)), $F14]"
fi
if [ -n "$ch" ]; then
    hc=$(header_at "$CPP_RPC" "$ch"); ha=$(header_at "$A_RPC" "$ch")
    if [ -n "$hc" ] && [ "$hc" = "$ha" ] && [ "${hc#* }" = 6 ]; then
        pass 5 "C++ top block $ch is A's v6 block (${hc%% *})"
    else
        fail 5 "C++ block $ch [$hc] differs from A's [$ha]"
    fi
fi
msg=$(docker logs --since "$SCEN_START" "$CPP_CONTAINER" 2>&1 \
    | grep -E -m 1 -o "failed to parse and validate block|Deserialization of Block Template failed" || true)
if [ -n "$msg" ]; then
    pass 5 "C++ node logged \"$msg\" (cannot deserialise block major version 7)"
else
    fail 5 "C++ log has neither \"failed to parse and validate block\" nor \"Deserialization of Block Template failed\""
fi
older_line() {
    line=$({ grep -a "peer is on an older fork" "$SD/a.log" | grep -a "fork 14" || true; } | head -n 1)
    [ -n "$line" ]
}
line=""
if wait_for "$OLDER_FORK_WAIT" older_line; then
    pass 5 "A dropped the C++ node: $(printf '%s' "$line" | sed 's/^.*peer is on an older fork/peer is on an older fork/' | cut -c1-200)"
else
    fail 5 "no \"peer is on an older fork\" line for fork 14 in $SD/a.log within ${OLDER_FORK_WAIT}s"
fi
if grep -a "peer is on an older fork" "$SD/b.log" > /dev/null; then
    fail 5 "B logged an older-fork peer (it only talks to A)"
fi
for node in "A:$A_RPC" "B:$B_RPC"; do
    name=${node%%:*}; addr=${node#*:}
    status=$(get "$addr" /status)
    if flat "$status" | grep -q '"active_index":14' && flat "$status" | grep -q '"supported_index":14'; then
        pass 5 "$name /status fork.active_index = 14, supported_index = 14"
    else
        fail 5 "$name /status does not report fork 14 as active and supported"
    fi
done

# ------------------------------------------------------------------ 6. summary

echo
printf '%-5s %-8s %-66s %s\n' node height "top hash" major
for node in "A:$A_RPC" "B:$B_RPC" "C++:$CPP_RPC"; do
    name=${node%%:*}; addr=${node#*:}
    t=$(top_of "$addr")
    if [ -n "$t" ]; then
        read -r th thash tmaj <<< "$t"
        printf '%-5s %-8s %-66s %s\n' "$name" "$th" "$thash" "$tmaj"
    else
        printf '%-5s %s\n' "$name" "(no answer)"
    fi
done
echo
printf '%-12s %-66s %s\n' transaction hash "mined at"
printf '%-12s %-66s %s\n' "v1 before" "${TX1:-}" "${H1:-}"
printf '%-12s %-66s %s\n' "v1 grace" "${TX3:-}" "${H3:-}"
printf '%-12s %-66s %s\n' "v2" "${TX2:-}" "${H2:-}"
echo "fork 13 at $F13, fork 14 at $F14, grace $GRACE; logs in $SD"
finish
