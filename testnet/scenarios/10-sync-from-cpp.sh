#!/usr/bin/env bash
# 10-sync-from-cpp: a fresh xkrd syncs a chain mined by an old C++ node.
#
#   C++ seed  xkr-cpp-testnet:master, container xkr-suite-seed, P2P 16898, RPC 16899, no peers;
#             the bundled C++ miner mines SEED_BLOCKS (100) blocks from genesis to wallet W1
#   xkrd S    P2P 16898 <- dials it (--add-exclusive-node), P2P 17898, RPC 17899, empty data dir
#
# Checks: seed chain mined (top major 5); S reaches the seed's top height and hash; every header
# 0..top equal on both (hash, major, minor, timestamp, difficulty, reward, num_txes, block_size);
# /status synced with a legacy peer; /getinfo upgrade_heights and supported_height equal to the
# C++ node's; 3 more C++-mined blocks reach S live (lite-block relay).
# Leaves the seed running for 11-14, 20, 50 (90-cleanup removes it).
# suite: expected=90 timeout=900
set -euo pipefail
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

LIVE_BLOCKS=${LIVE_BLOCKS:-3}
require_xkrd
require_testwallet
require_cmd curl
require_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE"

ensure_wallets
ensure_seed
read -r SH SHASH SMAJ <<< "$(top_of "$SEED_RPC")"
if [ "${SEED_CREATED:-0}" = 1 ]; then
    pass 1 "C++ seed $C_SEED mined $SEED_BLOCKS blocks from genesis in ${SEED_MINE_SECS}s: top $SH $SHASH"
else
    pass 1 "C++ seed $C_SEED reused: top $SH $SHASH"
fi
if [ "$SMAJ" = 5 ]; then
    pass 1 "seed top block has major version 5 (testnet: V2..V5 at height 0)"
else
    fail 1 "seed top block has major version $SMAJ, expected 5"
fi

# ------------------------------------------------------------------ fresh xkrd syncs

reclaim S
rm -rf "$WD/chain/xkrd"
require_ports_free "$PORT_S_P2P" "$PORT_S_RPC"
t0=$(date +%s)
start_suite_node "$SD/xkrd-S.log"
watch S
if wait_same_top "$S_RPC" "$SEED_RPC"; then
    pass 2 "fresh xkrd S synced to the seed's top $(top_of "$S_RPC") in $(($(date +%s) - t0))s"
else
    die 2 "S [$(top_of "$S_RPC")] did not reach the seed's top [$(top_of "$SEED_RPC")] within ${TIMEOUT}s"
fi

H=$(height_of "$S_RPC")
if compare_chains "$SEED_RPC" "$S_RPC" 0 "$H"; then
    pass 3 "headers 0..$H identical on the C++ seed and S (hash $CMP_FIELDS)"
else
    fail 3 "$CMP_MISMATCHES of $((H + 1)) headers differ; first: $CMP_FIRST"
fi

st=$(get "$S_RPC" /status)
synced=$(bool "$st" synced)
lout=$(num "$st" legacy_outgoing)
if [ "$synced" = true ] && [ -n "$lout" ] && [ "$lout" -ge 1 ] && [ "$(str "$st" network)" = testnet ]; then
    pass 4 "S /status: network testnet, synced true, legacy_outgoing $lout"
else
    fail 4 "S /status: network $(str "$st" network), synced $synced, legacy_outgoing $lout"
fi

ci=$(get "$SEED_RPC" /getinfo)
xi=$(get "$S_RPC" /getinfo)
cu=$(arr "$ci" upgrade_heights)
xu=$(arr "$xi" upgrade_heights)
cs=$(num "$ci" supported_height)
xs=$(num "$xi" supported_height)
if [ -n "$cu" ] && [ "$cu" = "$xu" ] && [ "$cs" = "$xs" ]; then
    pass 5 "/getinfo upgrade_heights and supported_height ($xs) equal on C++ and xkrd"
else
    fail 5 "/getinfo differs: C++ upgrade_heights $cu supported_height $cs; xkrd $xu $xs"
fi

# ------------------------------------------------------------------ live relay C++ -> xkrd

if mine_cpp "$SEED_RPC" "$W1" "$LIVE_BLOCKS"; then
    if wait_same_top "$S_RPC" "$SEED_RPC" 120; then
        pass 6 "$LIVE_BLOCKS blocks mined on the C++ seed reached S live: top $(top_of "$S_RPC")"
    else
        fail 6 "S [$(top_of "$S_RPC")] did not follow the seed [$(top_of "$SEED_RPC")] within 120 s"
    fi
else
    fail 6 "the C++ miner did not mine $LIVE_BLOCKS blocks on the seed"
fi
finish
