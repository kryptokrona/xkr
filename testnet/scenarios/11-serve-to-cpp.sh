#!/usr/bin/env bash
# 11-serve-to-cpp: a fresh old C++ node syncs the whole chain from xkrd only.
#
#   C++ seed  (shared, see 10) <- xkrd S (P2P 17898, RPC 17899) <- fresh C++ node
#   fresh     xkr-cpp-testnet:master, container xkr-suite-fresh, P2P 18898, RPC 18899,
#             --add-exclusive-node 127.0.0.1:17898 (xkrd is its only peer), empty data dir
#
# Checks: the fresh node reaches S's top height and hash; every header 0..top equal on the fresh
# node and S; S logged the fresh node as an inbound peer; the fresh node reports an outgoing peer.
# Nothing mines during the scenario, so the target is fixed.
# suite: expected=60 timeout=900
set -euo pipefail
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

require_xkrd
require_testwallet
require_cmd curl
require_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE"
FRESH_RPC=127.0.0.1:$PORT_FRESH_RPC

ensure_suite_chain "$SD/xkrd-S.log"
H=$(height_of "$S_RPC")

rm_container "$C_FRESH"
require_ports_free "$PORT_FRESH_P2P" "$PORT_FRESH_RPC"
t0=$(date +%s)
start_cpp "$C_FRESH" "$CPP_OLD_IMAGE" "$PORT_FRESH_P2P" "$PORT_FRESH_RPC" - \
    --add-exclusive-node="127.0.0.1:$PORT_S_P2P"
pass 1 "fresh C++ node $C_FRESH started with xkrd S as its only peer (S at $H)"

if wait_same_top "$FRESH_RPC" "$S_RPC"; then
    pass 2 "fresh C++ node synced from xkrd to $(top_of "$FRESH_RPC") in $(($(date +%s) - t0))s"
else
    die 2 "fresh C++ node [$(top_of "$FRESH_RPC")] did not reach S [$(top_of "$S_RPC")] within ${TIMEOUT}s (log $SD/$C_FRESH.log)"
fi

if compare_chains "$FRESH_RPC" "$S_RPC" 0 "$H"; then
    pass 3 "headers 0..$H identical on the fresh C++ node and S (hash $CMP_FIELDS)"
else
    fail 3 "$CMP_MISMATCHES of $((H + 1)) headers differ; first: $CMP_FIRST"
fi

line=$({ grep -a "peer connected" "$SD/xkrd-S.log" | grep -a "inbound=true" || true; } | head -n 1)
if [ -n "$line" ]; then
    pass 4 "S logged the inbound C++ peer: $(printf '%s' "$line" | sed 's/^.*peer connected/peer connected/' | cut -c1-160)"
else
    fail 4 "no \"peer connected ... inbound=true\" line in $SD/xkrd-S.log"
fi

fi_json=$(get "$FRESH_RPC" /getinfo)
out=$(num "$fi_json" outgoing_connections_count)
if [ -n "$out" ] && [ "$out" -ge 1 ]; then
    pass 5 "fresh C++ node /getinfo: height $(num "$fi_json" height), outgoing_connections_count $out"
else
    fail 5 "fresh C++ node /getinfo has no outgoing connection: $(flat "$fi_json" | cut -c1-200)"
fi
finish
