#!/usr/bin/env bash
# 30-fork13: fork 13 (block major version 6) between an unpatched C++ node (fork index 12), xkrd
# and a patched C++ node (fork index 13), docs/forks.md §2 and §4. Fork 13 is scheduled
# FORK_OFFSET (60) blocks above the old node's tip. Expected: xkrd and the patched node switch to
# block major version 6 together, the old node stops at the fork height, and xkrd drops it as a
# peer "on an older fork".
#
#   old node  container xkr-suite-f13-old (xkr-cpp-testnet:master), P2P 19898, RPC 19899, no
#             peers of its own; mines OLD_BLOCKS (30) blocks from genesis with the C++ miner, then
#             stops mining (or an external node: OLD_NODE_PEER + OLD_NODE_RPC [+ OLD_NODE_CONTAINER])
#       ^ Levin, xkrd dials it (--add-peer)
#   xkrd A    P2P 14898, RPC 14899, --fork13-height F, phase B mines 1 block/s (difficulty 1)
#       ^ Levin, the patched node dials it (--add-exclusive-node 127.0.0.1:14898)
#   patched   container xkr-suite-f13-patched (xkr-cpp-testnet:fork13), P2P 15898, RPC 15899,
#             XKR_FORK13_HEIGHT=F
#
# Phases: (A) xkrd without the miner syncs the old node's chain and the patched node syncs from
# xkrd, so all three agree on height H < F with nothing being mined; (B) xkrd restarts with
# --mine and the same F, mines past F + AFTER, and the checks run.
#
# Checks: 1 F = H + FORK_OFFSET, old top major 5; 2 patched node reports supported_height F;
# 3 all three agree below F; 4 xkrd and the patched node pass F + AFTER, block F is v5 and F + 1
# v6 on both, they agree at their common height (v6), the old node sits at F (or up to OLD_SLACK
# below) and logged the deserialisation error; 5 the old node's top block is xkrd's, xkrd logs
# "peer is on an older fork" for it (within OLDER_FORK_WAIT, the stall rule needs 150 s),
# /status.fork.active_index = 13.
#
# Env: OLD_BLOCKS (30), FORK_OFFSET (60), AFTER (20), OLD_SLACK (1), TIMEOUT (900),
# OLDER_FORK_WAIT (300), MINE_ADDRESS, LOG_LEVEL, KEEP=1, FORK13_IMAGE (= CPP_FORK13_IMAGE),
# FORK13_CONTAINER, OLD_NODE_PEER / OLD_NODE_RPC / OLD_NODE_CONTAINER (external old node).
# suite: expected=330 timeout=1800
set -euo pipefail
: "${TIMEOUT:=900}"
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

FORK13_IMAGE=${FORK13_IMAGE:-$CPP_FORK13_IMAGE}
FORK13_CONTAINER=${FORK13_CONTAINER:-xkr-suite-f13-patched}
OLD_CONTAINER_OWN=xkr-suite-f13-old
OLD_BLOCKS=${OLD_BLOCKS:-30}
FORK_OFFSET=${FORK_OFFSET:-60}
AFTER=${AFTER:-20}
OLD_SLACK=${OLD_SLACK:-1}
OLDER_FORK_WAIT=${OLDER_FORK_WAIT:-300}
XKRD_RPC=127.0.0.1:$PORT_A_RPC
NEW_RPC=127.0.0.1:$PORT_CPP_RPC

require_xkrd
require_testwallet
require_cmd curl
require_image "$FORK13_IMAGE" "$BUILD_FORK13_IMAGE"
reclaim f13-xkrd
rm_container "$FORK13_CONTAINER"

# ------------------------------------------------------------------ 0. the old node

ADDR=${MINE_ADDRESS:-$(wallet_addr "$SD/wallet")}
[ -n "$ADDR" ] || die 0 "no mining address (xkr-testwallet in $SD/wallet)"
if [ -n "${OLD_NODE_RPC:-}" ]; then
    OLD_NODE_PEER=${OLD_NODE_PEER:?set OLD_NODE_PEER with OLD_NODE_RPC}
    OLD_NODE_CONTAINER=${OLD_NODE_CONTAINER-}
    note "external old node: P2P $OLD_NODE_PEER, RPC $OLD_NODE_RPC, container [${OLD_NODE_CONTAINER}]"
    require_ports_free "$PORT_A_P2P" "$PORT_A_RPC" "$PORT_CPP_P2P" "$PORT_CPP_RPC"
else
    require_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE"
    rm_container "$OLD_CONTAINER_OWN"
    require_ports_free "$PORT_OLD_P2P" "$PORT_OLD_RPC" "$PORT_A_P2P" "$PORT_A_RPC" "$PORT_CPP_P2P" "$PORT_CPP_RPC"
    OLD_NODE_PEER=127.0.0.1:$PORT_OLD_P2P
    OLD_NODE_RPC=127.0.0.1:$PORT_OLD_RPC
    OLD_NODE_CONTAINER=$OLD_CONTAINER_OWN
    start_cpp "$OLD_NODE_CONTAINER" "$CPP_OLD_IMAGE" "$PORT_OLD_P2P" "$PORT_OLD_RPC" - \
        --add-exclusive-node="$DEAD_PEER"
    mine_cpp "$OLD_NODE_RPC" "$ADDR" "$OLD_BLOCKS" xkr-suite-f13-miner \
        || die 0 "the C++ miner did not mine $OLD_BLOCKS blocks on the old node"
    pass 0 "old node $OLD_NODE_CONTAINER ($CPP_OLD_IMAGE) mined $OLD_BLOCKS blocks from genesis, miner stopped"
fi

# ------------------------------------------------------------------ 1. fork height

OLD_TOP=$(top_of "$OLD_NODE_RPC")
[ -n "$OLD_TOP" ] || die 1 "old node RPC $OLD_NODE_RPC does not answer"
read -r H _ OLD_MAJOR <<< "$OLD_TOP"
F=$((H + FORK_OFFSET))
pass 1 "old node top index H=$H (major $OLD_MAJOR); fork 13 at F=$F, first v6 block $((F + 1))"
[ "$OLD_MAJOR" = 5 ] || fail 1 "old node top block has major $OLD_MAJOR, expected 5"

# ------------------------------------------------------------------ 2. start the nodes

start_a() { # <log> [extra args...]
    local log=$1
    shift
    start_xkrd f13-xkrd "$PORT_A_RPC" "$SD/xkrd" "$log" \
        --fork13-height "$F" --p2p-bind-ip 127.0.0.1 --p2p-bind-port "$PORT_A_P2P" \
        --add-peer "$OLD_NODE_PEER" --seed-node "$DEAD_PEER" --no-hyper "$@"
    watch f13-xkrd
}
echo "phase A: xkrd without the miner, log $SD/xkrd-sync.log"
start_a "$SD/xkrd-sync.log"
start_cpp "$FORK13_CONTAINER" "$FORK13_IMAGE" "$PORT_CPP_P2P" "$PORT_CPP_RPC" "XKR_FORK13_HEIGHT=$F" \
    --add-exclusive-node="127.0.0.1:$PORT_A_P2P"
pass 2 "xkrd (--fork13-height $F) and patched node $FORK13_CONTAINER (XKR_FORK13_HEIGHT=$F) started"

NEW_INFO=$(get "$NEW_RPC" /getinfo)
if [ "$(num "$NEW_INFO" supported_height)" = "$F" ]; then
    pass 2 "patched node reports supported_height $F (fork 13 read from XKR_FORK13_HEIGHT)"
else
    fail 2 "patched node supported_height=$(num "$NEW_INFO" supported_height), expected $F (is $FORK13_IMAGE built with 0001-fork13-testnet.patch?)"
fi

# ------------------------------------------------------------------ 3. same height < F

three_agree() {
    o=$(top_of "$OLD_NODE_RPC")
    x=$(top_of "$XKRD_RPC")
    n=$(top_of "$NEW_RPC")
    [ -n "$o" ] && [ "${o% *}" = "${x% *}" ] && [ "${x% *}" = "${n% *}" ]
}
o=""; x=""; n=""
if wait_for "$TIMEOUT" three_agree; then
    SYNC_H=${x%% *}
    if [ "$SYNC_H" -lt "$F" ]; then
        pass 3 "old node, xkrd and patched node agree on height $SYNC_H < F (top ${x#* })"
    else
        die 3 "nodes agree at $SYNC_H, not below F=$F"
    fi
else
    die 3 "nodes did not agree below F within ${TIMEOUT}s: old=[$o] xkrd=[$x] patched=[$n]"
fi

echo "phase B: xkrd restarts with --mine $ADDR --mine-threads 1, log $SD/xkrd.log"
unwatch
stop_proc f13-xkrd
start_a "$SD/xkrd.log" --mine "$ADDR" --mine-threads 1

# ------------------------------------------------------------------ 4. through the fork

TARGET=$((F + AFTER))
wait_height "$XKRD_RPC" "$TARGET" || true
wait_height "$NEW_RPC" "$TARGET" 120 || true
xh=$(height_of "$XKRD_RPC"); nh=$(height_of "$NEW_RPC")
xh=${xh:-0}; nh=${nh:-0}
if [ "$xh" -ge "$TARGET" ] && [ "$nh" -ge "$TARGET" ]; then
    pass 4 "xkrd ($xh) and patched node ($nh) passed F + $AFTER = $TARGET"
else
    fail 4 "heights after ${TIMEOUT}s: xkrd=$xh patched=$nh, wanted >= $TARGET"
fi

for node in "xkrd:$XKRD_RPC" "patched:$NEW_RPC"; do
    name=${node%%:*}; addr=${node#*:}
    before=$(header_at "$addr" "$F"); after=$(header_at "$addr" $((F + 1)))
    if [ "${before#* }" = 5 ] && [ "${after#* }" = 6 ]; then
        pass 4 "$name: block $F is v5, block $((F + 1)) is v6"
    else
        fail 4 "$name: block $F major ${before#* }, block $((F + 1)) major ${after#* } (want 5, 6)"
    fi
done
COMMON=$(( xh < nh ? xh : nh ))
hx=$(header_at "$XKRD_RPC" "$COMMON"); hn=$(header_at "$NEW_RPC" "$COMMON")
if [ -n "$hx" ] && [ "$hx" = "$hn" ] && [ "${hx#* }" = 6 ]; then
    pass 4 "xkrd and patched node agree at height $COMMON (${hx%% *}, v6)"
else
    fail 4 "xkrd [$hx] and patched node [$hn] differ at height $COMMON"
fi

# Old node: give it time to stall, then it must sit at F (or up to OLD_SLACK below: C++ drops a
# whole sync response when one block in it does not parse).
sleep 10
o1=$(top_of "$OLD_NODE_RPC"); sleep 15; o2=$(top_of "$OLD_NODE_RPC")
oh=${o2%% *}
if [ -n "$oh" ] && [ "$oh" -le "$F" ] && [ "$oh" -ge $((F - OLD_SLACK)) ] && [ "$o1" = "$o2" ]; then
    pass 4 "old node stopped at $oh (F=$F) and stays there"
else
    fail 4 "old node top [$o1] -> [$o2], expected a constant height in [$((F - OLD_SLACK)), $F]"
fi
if [ -z "$OLD_NODE_CONTAINER" ]; then
    skip 4 "old node log (OLD_NODE_CONTAINER empty)"
elif ! container_exists "$OLD_NODE_CONTAINER"; then
    skip 4 "old node log (no container $OLD_NODE_CONTAINER)"
else
    msg=$(docker logs --since "$SCEN_START" "$OLD_NODE_CONTAINER" 2>&1 \
        | grep -E -m 1 -o "failed to parse and validate block|Deserialization of Block Template failed" || true)
    if [ -n "$msg" ]; then
        pass 4 "old node logged \"$msg\" (cannot deserialise block major version 6)"
    else
        fail 4 "old node log has neither \"failed to parse and validate block\" nor \"Deserialization of Block Template failed\" (log level >= 2?)"
    fi
fi

# ------------------------------------------------------------------ 5. old node isolated

if [ -n "$oh" ]; then
    ho=$(header_at "$OLD_NODE_RPC" "$oh"); hx=$(header_at "$XKRD_RPC" "$oh")
    if [ -n "$ho" ] && [ "${ho%% *}" = "${hx%% *}" ]; then
        pass 5 "old node's top block $oh is xkrd's block $oh (no blocks of its own)"
    else
        fail 5 "old node block $oh [$ho] differs from xkrd's [$hx]"
    fi
fi
older_line() {
    line=$({ grep -a "peer is on an older fork" "$SD/xkrd.log" | grep -aF "$OLD_PEER_MATCH" || true; } | head -n 1)
    [ -n "$line" ]
}
# our own old node is on loopback like the patched node: match its full address; an external
# one by IP (as before)
if [ "$OLD_NODE_CONTAINER" = "$OLD_CONTAINER_OWN" ]; then OLD_PEER_MATCH=$OLD_NODE_PEER; else OLD_PEER_MATCH=${OLD_NODE_PEER%:*}; fi
line=""
if wait_for "$OLDER_FORK_WAIT" older_line; then
    pass 5 "xkrd dropped the old node: $(printf '%s' "$line" | sed 's/^.*peer is on an older fork/peer is on an older fork/' | cut -c1-160)"
else
    fail 5 "no \"peer is on an older fork\" line for $OLD_PEER_MATCH in $SD/xkrd.log within ${OLDER_FORK_WAIT}s"
fi
status=$(get "$XKRD_RPC" /status)
if flat "$status" | grep -q '"active_index":13'; then
    pass 5 "xkrd /status fork.active_index = 13"
else
    fail 5 "xkrd /status does not report fork 13 as active"
fi

# ------------------------------------------------------------------ 6. summary

echo
printf '%-9s %-10s %-66s %s\n' node height "top hash" major
for node in "old:$OLD_NODE_RPC" "xkrd:$XKRD_RPC" "patched:$NEW_RPC"; do
    name=${node%%:*}; addr=${node#*:}
    t=$(top_of "$addr")
    if [ -n "$t" ]; then
        read -r th thash tmaj <<< "$t"
        printf '%-9s %-10s %-66s %s\n' "$name" "$th" "$thash" "$tmaj"
    else
        printf '%-9s %s\n' "$name" "(no answer)"
    fi
done
echo "fork 13 at F=$F; logs in $SD"
finish
