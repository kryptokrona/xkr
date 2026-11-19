#!/usr/bin/env bash
# 65-miner: the standalone xkr-miner solo against xkrd with the C++ miner's flags, and in a p2pool stratum (the C++ p2pool image xkr-p2pool:testnet, else xkrd's built-in p2pool).
#
#   C++ seed  (shared, see 10) <- xkrd S (P2P 17898, RPC 17899)
#   xkr-miner --network testnet --daemon-address=S --address=W1 --threads=1 --scan-time=1
#             --limit=SOLO_BLOCKS (5) --log-json                              (check 1, 2)
#   xkr-miner --daemon-host 127.0.0.1 --daemon-rpc-port 17899 --address W1 --threads 1
#             --limit 2                                                       (check 3, text output)
#   P2POOL=1: a fresh testnet xkrd A (P2P 22898, RPC 22899) mines PHASE0_BLOCKS (10) with its
#   built-in miner and restarts without it, then with the image: p2pool A (container, stratum
#   22333) on A; without it: A itself with --node --p2pool (p2pool/sidechain-testnet.json,
#   stratum 22333, payout W1 through --hugin-payout); xkr-miner --pool stratum+tcp://127.0.0.1:22333
#   --limit POOL_SHARES (3)
#
# Checks: (1) the miner exits 0 after --limit blocks; S is SOLO_BLOCKS higher and logs as many
# "submitted block accepted", none refused; the JSON lines have one "started", SOLO_BLOCKS
# "accepted" with a block hash, a "limit" and a "stopped"; (2) the seed takes the blocks: same
# top, headers equal; (3) the --daemon-host / --daemon-rpc-port spelling mines 2 more blocks
# and prints the C++ lines ("Block found! Hash:", "Mined requested amount of blocks (2).
# Quitting."); (4) xkr-miner logs in to p2pool, gets a job, POOL_SHARES shares are accepted (on
# the testnet every p2pool share is a main-chain block, so A grows) and it exits 0; with the
# built-in p2pool A also logs "p2pool: block found" for them.
#
# Env: SOLO_BLOCKS (5), P2POOL (1: run 4; 0: skip it), PHASE0_BLOCKS (10),
# POOL_SHARES (3), MINER_WAIT (300: longest wait for one miner run), XKR_MINER, TIMEOUT,
# LOG_LEVEL, KEEP=1.
# suite: expected=90 timeout=900
set -euo pipefail
. "$(dirname "$0")/../lib.sh"
. "$TESTNET_DIR/lib-p2pool.sh"
suite_init "${1:-}"

SOLO_BLOCKS=${SOLO_BLOCKS:-5}
P2POOL=${P2POOL:-1}
PHASE0_BLOCKS=${PHASE0_BLOCKS:-10}
POOL_SHARES=${POOL_SHARES:-3}
MINER_WAIT=${MINER_WAIT:-300}
require_xkrd
require_testwallet
require_miner
require_cmd curl
require_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE"

ensure_suite_chain "$SD/xkrd-S.log"

# run_miner <name> <log> <args...>: xkr-miner in the background (nice -n 19), waits up to
# MINER_WAIT for it to exit; sets MINER_RC (its exit code, "timeout" if it had to be stopped)
run_miner() {
    local name=$1 log=$2 pid i
    shift 2
    start_bg "$name" "$log" "$XKR_MINER" "$@"
    pid=$LAST_PID
    MINER_RC=timeout
    for ((i = 0; i < MINER_WAIT; i++)); do
        if ! kill -0 "$pid" 2>/dev/null; then
            MINER_RC=0
            wait "$pid" || MINER_RC=$?
            return 0
        fi
        check_alive
        sleep 1
    done
    stop_proc "$name"
}
json_count() { log_count "$1" "\"event\":\"$2\""; }

# ------------------------------------------------------------------ 1. solo, C++ flags, JSON

H0=$(height_of "$S_RPC")
acc0=$(log_count "$SD/xkrd-S.log" "submitted block accepted")
ref0=$(log_count "$SD/xkrd-S.log" "submitted block refused")
MLOG=$SD/xkr-miner-solo.log
run_miner miner-solo "$MLOG" --network testnet --daemon-address="$S_RPC" --address="$W1" \
    --threads=1 --scan-time=1 --limit="$SOLO_BLOCKS" --log-json
H1=$(height_of "$S_RPC")
if [ "$MINER_RC" = 0 ] && [ -n "$H1" ] && [ "$H1" -ge $((H0 + SOLO_BLOCKS)) ]; then
    pass 1 "xkr-miner (C++ flags, --limit=$SOLO_BLOCKS) exited 0; S from $H0 to $H1"
else
    fail 1 "xkr-miner exit [$MINER_RC], S from $H0 to [$H1], wanted $((H0 + SOLO_BLOCKS)) (log $MLOG)"
fi
acc=$(($(log_count "$SD/xkrd-S.log" "submitted block accepted") - ${acc0:-0}))
ref=$(($(log_count "$SD/xkrd-S.log" "submitted block refused") - ${ref0:-0}))
if [ "$acc" -ge "$SOLO_BLOCKS" ] && [ "$ref" = 0 ]; then
    pass 1 "S: $acc submitted blocks accepted, $ref refused"
else
    fail 1 "S: $acc submitted blocks accepted, $ref refused (want >= $SOLO_BLOCKS, 0)"
fi
started=$(json_count "$MLOG" started)
accepted=$(json_count "$MLOG" accepted)
hashes=$(log_count "$MLOG" '"block_hash":"')
limit=$(json_count "$MLOG" limit)
stopped=$(json_count "$MLOG" stopped)
if [ "$started" = 1 ] && [ "$accepted" = "$SOLO_BLOCKS" ] && [ "$hashes" = "$SOLO_BLOCKS" ] \
    && [ "$limit" = 1 ] && [ "$stopped" = 1 ]; then
    pass 1 "JSON lines: started 1, accepted $accepted (each with a block_hash), limit 1, stopped 1"
else
    fail 1 "JSON lines: started $started, accepted $accepted, block hashes $hashes, limit $limit, stopped $stopped (log $MLOG)"
fi

# ------------------------------------------------------------------ 2. the seed takes them

if wait_same_top "$SEED_RPC" "$S_RPC" 120; then
    T=$(height_of "$S_RPC")
    if compare_chains "$SEED_RPC" "$S_RPC" $((H0 + 1)) "$T"; then
        pass 2 "C++ seed accepted xkr-miner's blocks: headers $((H0 + 1))..$T identical"
    else
        fail 2 "$CMP_MISMATCHES headers differ in $((H0 + 1))..$T; first: $CMP_FIRST"
    fi
else
    fail 2 "seed [$(top_of "$SEED_RPC")] did not take S's blocks [$(top_of "$S_RPC")] within 120 s"
fi

# ------------------------------------------------------------------ 3. --daemon-host / --daemon-rpc-port, text

H2=$(height_of "$S_RPC")
TLOG=$SD/xkr-miner-text.log
run_miner miner-text "$TLOG" --network testnet --daemon-host 127.0.0.1 \
    --daemon-rpc-port "$PORT_S_RPC" --address "$W1" --threads 1 --limit 2
H3=$(height_of "$S_RPC")
found=$(log_count "$TLOG" "Block found! Hash:")
if [ "$MINER_RC" = 0 ] && [ -n "$H3" ] && [ "$H3" -ge $((H2 + 2)) ] && [ "$found" = 2 ] \
    && log_has "$TLOG" "Mined requested amount of blocks (2). Quitting."; then
    pass 3 "--daemon-host/--daemon-rpc-port: 2 blocks ($H2 -> $H3), \"Block found! Hash:\" x2, \"Mined requested amount of blocks (2). Quitting.\""
else
    fail 3 "exit [$MINER_RC], S $H2 -> [$H3], \"Block found\" x$found (log $TLOG)"
fi

# ------------------------------------------------------------------ 4. p2pool stratum

if [ "$P2POOL" != 1 ]; then
    skip 4 "P2POOL=$P2POOL: the p2pool stratum check is off"
else
    # No fork in this run: fork heights far above the heights mined here.
    F13=${F13:-100000}
    F14=${F14:-200000}
    A_RPC=127.0.0.1:$PORT_PP_A_RPC
    POOL_KIND=built-in
    image_present "$P2POOL_IMAGE" && POOL_KIND=image
    reclaim pp-a
    [ "$POOL_KIND" = image ] && rm_container "$(pool_container a)"
    require_ports_free "$PORT_PP_A_P2P" "$PORT_PP_A_RPC" "$PORT_PP_POOL_A" "$PORT_PP_STRATUM_A"
    A_ARGS=(--fork13-height "$F13" --fork14-height "$F14" --no-hyper --out-peers 0
        --p2p-bind-ip 127.0.0.1 --p2p-bind-port "$PORT_PP_A_P2P")
    start_xkrd pp-a "$PORT_PP_A_RPC" "$SD/xkrd-a" "$SD/xkrd-a-phase0.log" "${A_ARGS[@]}" \
        --mine "$W1" --mine-threads 1
    watch pp-a
    wait_height "$A_RPC" "$PHASE0_BLOCKS" 180 || die 4 "phase 0: A did not mine $PHASE0_BLOCKS blocks within 180 s"
    stop_proc pp-a
    unwatch
    watch S
    if [ "$POOL_KIND" = image ]; then
        start_xkrd pp-a "$PORT_PP_A_RPC" "$SD/xkrd-a" "$SD/xkrd-a.log" "${A_ARGS[@]}"
        watch pp-a
        start_p2pool a "$P2POOL_IMAGE" "$W1" "$PORT_PP_POOL_A" "$PORT_PP_STRATUM_A"
        wait_for 120 pool_api_ready a || die 4 "p2pool A wrote no local API within 120 s (log $(pool_log a))"
        POOL_LOG=$(pool_log a)
    else
        note "image $P2POOL_IMAGE missing: xkrd's built-in p2pool on A instead ($BUILD_P2POOL_IMAGE builds the image)"
        # --node --p2pool: no relay (and --no-hyper turns it off anyway); --hugin-payout sets the payout
        start_xkrd pp-a "$PORT_PP_A_RPC" "$SD/xkrd-a" "$SD/xkrd-a.log" "${A_ARGS[@]}" \
            --node --p2pool --hugin-payout "$W1" --p2pool-config "$ROOT/p2pool/sidechain-testnet.json" \
            --p2pool-stratum "127.0.0.1:$PORT_PP_STRATUM_A"
        watch pp-a
        a_running() { [ "$(str "$(obj "$(get "$A_RPC" /status)" p2pool)" state)" = running ]; }
        wait_for 60 a_running \
            || die 4 "xkrd A: built-in p2pool not running within 60 s (/status p2pool: $(obj "$(get "$A_RPC" /status)" p2pool | cut -c1-200))"
        POOL_LOG=$SD/xkrd-a.log
    fi
    HA=$(height_of "$A_RPC")
    PLOG=$SD/xkr-miner-pool.log
    run_miner miner-pool "$PLOG" --pool "stratum+tcp://127.0.0.1:$PORT_PP_STRATUM_A" \
        --pool-user x --rig-id suite --threads 1 --limit "$POOL_SHARES" --log-json
    HB=$(height_of "$A_RPC")
    jobs=$(json_count "$PLOG" job)
    shares=$(json_count "$PLOG" accepted)
    rejected=$(json_count "$PLOG" rejected)
    if [ "$MINER_RC" = 0 ] && [ "${jobs:-0}" -ge 1 ] && [ "$shares" = "$POOL_SHARES" ] && [ "${rejected:-0}" = 0 ]; then
        pass 4 "p2pool stratum ($POOL_KIND): $jobs job(s), $shares shares accepted, none rejected; xkr-miner exited 0"
    else
        fail 4 "p2pool stratum ($POOL_KIND): exit [$MINER_RC], $jobs job(s), $shares accepted, $rejected rejected (log $PLOG, p2pool $POOL_LOG)"
    fi
    if [ -n "$HB" ] && [ -n "$HA" ] && [ "$HB" -gt "$HA" ]; then
        pass 4 "A grew from $HA to $HB through p2pool ($POOL_KIND; testnet: every share is a block)"
    else
        fail 4 "A at [$HB], was [$HA]: no p2pool block from xkr-miner's shares ($POOL_KIND)"
    fi
    if [ "$POOL_KIND" = built-in ]; then
        found=$(log_count "$SD/xkrd-a.log" "p2pool: block found height=")
        if [ "${found:-0}" -ge 1 ]; then
            pass 4 "xkrd A: $found \"p2pool: block found\" (the built-in p2pool submitted xkr-miner's shares as blocks)"
        else
            fail 4 "xkrd A logged no \"p2pool: block found\" (log $SD/xkrd-a.log)"
        fi
    fi
fi
finish
