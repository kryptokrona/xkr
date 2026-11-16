#!/usr/bin/env bash
# 12-mining: xkrd's getblocktemplate/submitblock (the API pools and p2pool use) and the built-in
# miner, with the C++ seed node as the judge.
#
#   C++ seed  (shared, see 10) <- xkrd S (P2P 17898, RPC 17899)
#   C++ miner container xkr-suite-miner (old image) -> S's RPC, --limit CPP_MINE_BLOCKS (10)
#   then S restarts with --mine W1 --mine-threads 1 for BUILTIN_BLOCKS (10) blocks
#
# Checks: getblocktemplate on S and on the C++ seed agree on height, difficulty and
# reserved_offset; the C++ miner's blocks are accepted by S ("submitted block accepted", none
# refused, miner exit 0) and by the seed (same top, headers equal); the built-in miner finds
# blocks (/status.mining, "new block ... mined=true", none rejected) that the seed accepts
# (headers equal after S restarts without the miner); W1's balance through the C++ node lists the
# coinbase outputs of the first block mined via S's template and of the last built-in block.
# suite: expected=90 timeout=900
set -euo pipefail
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

CPP_MINE_BLOCKS=${CPP_MINE_BLOCKS:-10}
BUILTIN_BLOCKS=${BUILTIN_BLOCKS:-10}
require_xkrd
require_testwallet
require_cmd curl
require_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE"

ensure_suite_chain "$SD/xkrd-S.log"

# ------------------------------------------------------------------ 1. template differential

params="{\"wallet_address\":\"$W1\",\"reserve_size\":8}"
gc=$(rpc "$SEED_RPC" getblocktemplate "$params")
gx=$(rpc "$S_RPC" getblocktemplate "$params")
ct="height $(num "$gc" height) difficulty $(num "$gc" difficulty) reserved_offset $(num "$gc" reserved_offset)"
xt="height $(num "$gx" height) difficulty $(num "$gx" difficulty) reserved_offset $(num "$gx" reserved_offset)"
if [ -n "$(num "$gx" height)" ] && [ "$ct" = "$xt" ]; then
    pass 1 "getblocktemplate agrees with C++: $xt (blob $(($(str "$gx" blocktemplate_blob | wc -c) / 2)) bytes, C++ $(($(str "$gc" blocktemplate_blob | wc -c) / 2)))"
else
    fail 1 "getblocktemplate: C++ [$ct] xkrd [$xt] ($(flat "$gx" | cut -c1-160))"
fi

# ------------------------------------------------------------------ 2. C++ miner against xkrd

H0=$(height_of "$S_RPC")
if mine_cpp "$S_RPC" "$W1" "$CPP_MINE_BLOCKS"; then
    pass 2 "C++ miner mined $CPP_MINE_BLOCKS blocks through S's getblocktemplate/submitblock: S at $(height_of "$S_RPC")"
else
    fail 2 "S did not reach $((H0 + CPP_MINE_BLOCKS)) with the C++ miner (S at $(height_of "$S_RPC"), miner log $SD/$C_MINER.log)"
fi
acc=$(log_count "$SD/xkrd-S.log" "submitted block accepted")
ref=$(log_count "$SD/xkrd-S.log" "submitted block refused")
if [ "${acc:-0}" -ge "$CPP_MINE_BLOCKS" ] && [ "${ref:-0}" = 0 ] && [ "$MINER_EXIT" = 0 ]; then
    pass 2 "S: $acc submitted blocks accepted, $ref refused; C++ miner exit 0"
else
    fail 2 "S: $acc accepted, $ref refused (want >= $CPP_MINE_BLOCKS, 0); C++ miner exit [$MINER_EXIT]"
fi
if wait_same_top "$SEED_RPC" "$S_RPC" 120; then
    T=$(height_of "$S_RPC")
    if compare_chains "$SEED_RPC" "$S_RPC" $((H0 + 1)) "$T"; then
        pass 2 "C++ seed accepted the blocks: headers $((H0 + 1))..$T identical"
    else
        fail 2 "$CMP_MISMATCHES headers differ in $((H0 + 1))..$T; first: $CMP_FIRST"
    fi
else
    fail 2 "seed [$(top_of "$SEED_RPC")] did not take S's blocks [$(top_of "$S_RPC")] within 120 s"
fi

# ------------------------------------------------------------------ 3. built-in miner

stop_proc S
H1=$(height_of "$SEED_RPC")
start_suite_node "$SD/xkrd-S-mine.log" --mine "$W1" --mine-threads 1
watch S
t0=$(date +%s)
if wait_height "$S_RPC" $((H1 + BUILTIN_BLOCKS)); then
    pass 3 "built-in miner: $BUILTIN_BLOCKS blocks in $(($(date +%s) - t0))s (S at $(height_of "$S_RPC"))"
else
    fail 3 "built-in miner: S at $(height_of "$S_RPC") after ${TIMEOUT}s, wanted $((H1 + BUILTIN_BLOCKS))"
fi
st=$(obj "$(get "$S_RPC" /status)" mining)
found=$(num "$st" blocks_found)
rej=$(num "$st" blocks_rejected)
mined=$({ grep -a "new block" "$SD/xkrd-S-mine.log" | grep -ac "mined=true" || true; } | head -n 1)
if [ "$(bool "$st" active)" = true ] && [ "${found:-0}" -ge "$BUILTIN_BLOCKS" ] && [ "${rej:-1}" = 0 ] \
    && [ "${mined:-0}" -ge "$BUILTIN_BLOCKS" ]; then
    pass 3 "/status.mining active, blocks_found $found, blocks_rejected $rej; $mined \"new block ... mined=true\" lines"
else
    fail 3 "/status.mining active $(bool "$st" active) blocks_found $found blocks_rejected $rej; mined=true lines $mined"
fi

# stop the miner, then S without it: nobody mines, so the comparison is stable
stop_proc S
start_suite_node "$SD/xkrd-S-after.log"
watch S
if wait_same_top "$SEED_RPC" "$S_RPC" 120; then
    T=$(height_of "$S_RPC")
    if compare_chains "$SEED_RPC" "$S_RPC" $((H1 + 1)) "$T"; then
        pass 3 "C++ seed accepted the built-in miner's blocks: headers $((H1 + 1))..$T identical"
    else
        fail 3 "$CMP_MISMATCHES headers differ in $((H1 + 1))..$T; first: $CMP_FIRST"
    fi
else
    fail 3 "seed [$(top_of "$SEED_RPC")] and S [$(top_of "$S_RPC")] differ after the built-in miner"
    T=$(height_of "$S_RPC")
fi

# ------------------------------------------------------------------ 4. the coinbase pays W1

bal="$SD/balance-w1-seed.out"
wallet "$W1_DIR" "$SEED_RPC" balance > "$bal" 2>&1 || true
for h in $((H0 + 1)) "$T"; do
    if grep -qE "height $h tx " "$bal"; then
        pass 4 "W1's balance through the C++ node has the coinbase output(s) of block $h"
    else
        fail 4 "W1's balance through the C++ node has no output at height $h (see $bal)"
    fi
done
finish
