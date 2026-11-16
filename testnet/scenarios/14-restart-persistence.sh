#!/usr/bin/env bash
# 14-restart-persistence: xkrd keeps its chain across a clean shutdown and survives a crash.
#
#   C++ seed  (shared, see 10) <- xkrd S (data dir <wd>/chain/xkrd)
#
# Checks: (1) SIGTERM: S exits with status 0 within 30 s, logs "xkrd stopped" with its height, and
# wrote p2pstate.json; (2) restarted with no reachable peer (--add-exclusive-node 127.0.0.1:9)
# S serves the same top height and hash at once, without checking the database again; (3)
# `xkrd db verify` on the stopped data dir finds 0 problems; (4) SIGKILL while running (a
# crash): S restarts, `db verify` finds 0 problems, and S catches up with the seed again (same
# top).
# suite: expected=60 timeout=900
set -euo pipefail
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

require_xkrd
require_testwallet
require_cmd curl
require_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE"
DATA=$WD/chain/xkrd

ensure_suite_chain "$SD/xkrd-S-1.log"
read -r H HASH _ <<< "$(top_of "$S_RPC")"
note "S at $H $HASH"

# ------------------------------------------------------------------ 1. clean shutdown

unwatch
stop_proc S 30
stopped=$({ grep -a "xkrd stopped" "$SD/xkrd-S-1.log" || true; } | tail -n 1)
sh=$({ printf '%s' "$stopped" | grep -o 'height=[0-9]*' || true; } | cut -d= -f2)
if [ "$STOP_STATUS" = 0 ] && [ "$STOP_KILLED" = 0 ] && [ "$sh" = "$H" ]; then
    pass 1 "SIGTERM: exit status 0 after ${STOP_SECS}s, \"xkrd stopped\" at height $sh"
else
    fail 1 "SIGTERM: exit status $STOP_STATUS after ${STOP_SECS}s (SIGKILL needed: $STOP_KILLED), stopped line [$stopped], want height $H"
fi
if [ -s "$DATA/p2pstate.json" ]; then
    pass 1 "p2pstate.json written ($(wc -c < "$DATA/p2pstate.json") bytes)"
else
    fail 1 "no p2pstate.json in $DATA after the shutdown"
fi

# ------------------------------------------------------------------ 2. restart without peers

start_xkrd S "$PORT_S_RPC" "$DATA" "$SD/xkrd-S-2.log" \
    --p2p-bind-ip 127.0.0.1 --p2p-bind-port "$PORT_S_P2P" --add-exclusive-node "$DEAD_PEER" --no-hyper
read -r H2 HASH2 _ <<< "$(top_of "$S_RPC")"
if [ "$H2" = "$H" ] && [ "$HASH2" = "$HASH" ]; then
    pass 2 "restarted S (no peers) serves the same top $H2 $HASH2"
else
    fail 2 "restarted S top [$H2 $HASH2], before the shutdown [$H $HASH]"
fi
if grep -aq "did not stop cleanly" "$SD/xkrd-S-2.log"; then
    fail 2 "the restart after a clean stop checked the database again (it was not closed cleanly)"
else
    pass 2 "no database check after the clean stop"
fi
hdr=$(header_hash_at "$S_RPC" $((H / 2)))
if [ -n "$hdr" ] && [ "$hdr" = "$(header_hash_at "$SEED_RPC" $((H / 2)))" ]; then
    pass 2 "block $((H / 2)) after the restart equals the seed's ($hdr)"
else
    fail 2 "block $((H / 2)) after the restart [$hdr] differs from the seed's"
fi
stop_proc S 30

# ------------------------------------------------------------------ 3. db verify

if xkrd_db "$SD/db-verify-1.out" verify --network testnet --data-dir "$DATA" \
    && grep -q ': 0 problems$' "$SD/db-verify-1.out"; then
    pass 3 "xkrd db verify: $(grep '^verified blocks' "$SD/db-verify-1.out" | tail -n 1)"
else
    fail 3 "xkrd db verify failed: $(tail -n 3 "$SD/db-verify-1.out" | tr '\n' ' ')"
fi

# ------------------------------------------------------------------ 4. crash (SIGKILL)

start_suite_node "$SD/xkrd-S-3.log"
watch S
wait_same_top "$S_RPC" "$SEED_RPC" 120 || true
sleep 6  # past one durable-commit interval (--db-durable-secs 5)
kill_proc S
unwatch
start_xkrd S "$PORT_S_RPC" "$DATA" "$SD/xkrd-S-4.log" \
    --p2p-bind-ip 127.0.0.1 --p2p-bind-port "$PORT_S_P2P" --add-exclusive-node "$DEAD_PEER" --no-hyper
HK=$(height_of "$S_RPC")
if [ -n "$HK" ]; then
    pass 4 "after SIGKILL S opens its database again at height $HK (seed at $(height_of "$SEED_RPC"))"
else
    fail 4 "S does not answer after the SIGKILL restart"
fi
stop_proc S 30
if xkrd_db "$SD/db-verify-2.out" verify --network testnet --data-dir "$DATA" \
    && grep -q ': 0 problems$' "$SD/db-verify-2.out"; then
    pass 4 "xkrd db verify after the crash: $(grep '^verified blocks' "$SD/db-verify-2.out" | tail -n 1)"
else
    fail 4 "xkrd db verify after the crash failed: $(tail -n 3 "$SD/db-verify-2.out" | tr '\n' ' ')"
fi
start_suite_node "$SD/xkrd-S-5.log"
watch S
if wait_same_top "$S_RPC" "$SEED_RPC" 120; then
    pass 4 "S caught up with the seed after the crash: $(top_of "$S_RPC")"
else
    fail 4 "S [$(top_of "$S_RPC")] did not catch up with the seed [$(top_of "$SEED_RPC")]"
fi
finish
