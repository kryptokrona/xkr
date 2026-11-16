#!/usr/bin/env bash
# 20-hyper-two-nodes: two xkrd nodes sync over Hyperswarm (docs/hyper-transport.md, "Two-node
# test"):
#
#   A  testnet, Levin/TCP to a C++ node (the suite's seed, or $XKR_CPP_PEER) + Hyperswarm,
#      P2P 21897, RPC 21898
#   B  testnet, Hyperswarm only: no legacy listener (--p2p-bind-port 0), --out-peers 0, a dead
#      seed node, RPC 21888
#
# B can only reach A's height through Hyperswarm, i.e. Levin over HyperConn. Both use a local
# HyperDHT (bridge/test/testnet-bootstrap.js), so nothing goes to the public DHT; PUBLIC_DHT=1
# uses the public bootstrap nodes instead.
#
# Checks: local DHT up; A at the C++ node's top; B reaches A's top height and hash within
# TIMEOUT (900); B has no legacy connection (legacy_outgoing = legacy_incoming = 0) and at least
# one Hyperswarm peer.
# Env: XKR_CPP_PEER (C++ P2P host:port instead of the suite seed; XKR_CPP_RPC its RPC, optional),
# PUBLIC_DHT=1, TIMEOUT, LOG_LEVEL, KEEP=1.
# suite: expected=120 timeout=1500
set -euo pipefail
: "${TIMEOUT:=900}"
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

require_xkrd
require_hyper
require_node
require_bridge
require_cmd curl
RPC_A=127.0.0.1:$PORT_HA_RPC
RPC_B=127.0.0.1:$PORT_HB_RPC

if [ -n "${XKR_CPP_PEER:-}" ]; then
    CPP_PEER=$XKR_CPP_PEER
    CPP_RPC=${XKR_CPP_RPC:-}
    note "A peers with $CPP_PEER (XKR_CPP_PEER)"
else
    require_testwallet
    ensure_wallets
    ensure_seed
    CPP_PEER=127.0.0.1:$PORT_SEED_P2P
    CPP_RPC=$SEED_RPC
fi
reclaim hyper-dht hyper-a hyper-b
require_ports_free "$PORT_HA_P2P" "$PORT_HA_RPC" "$PORT_HB_RPC"

BOOT_ARGS=()
if [ "${PUBLIC_DHT:-0}" = 1 ]; then
    skip 1 "local DHT (PUBLIC_DHT=1: public HyperDHT bootstrap nodes)"
else
    start_bg hyper-dht "$SD/bootstrap.log" "$NODE" "$ROOT/bridge/test/testnet-bootstrap.js" --ignore-stdin
    BOOT=""
    for _ in $(seq 1 100); do
        BOOT=$({ sed -n 's/^BOOTSTRAP //p' "$SD/bootstrap.log" || true; } | head -n 1)
        [ -n "$BOOT" ] && break
        sleep 0.1
    done
    [ -n "$BOOT" ] || die 1 "local DHT did not start: $(tail -n 5 "$SD/bootstrap.log" | tr '\n' ' ')"
    pass 1 "local HyperDHT testnet at $BOOT"
    BOOT_ARGS=(--hyper-bootstrap "$BOOT")
fi

# xkrd runs bridge/ on Bare worker threads. --no-hugin: this scenario is about the XKR topic
# only (the Hugin relay, on by default, is scenario 40).
COMMON=(--hyper-bridge-dir "$ROOT/bridge" --no-hugin ${BOOT_ARGS[@]+"${BOOT_ARGS[@]}"})
start_xkrd hyper-a "$PORT_HA_RPC" "$SD/a" "$SD/a.log" "${COMMON[@]}" \
    --p2p-bind-ip 127.0.0.1 --p2p-bind-port "$PORT_HA_P2P" --add-peer "$CPP_PEER" --seed-node "$DEAD_PEER"
# B: no legacy peers at all. --seed-node replaces the compiled-in testnet seed (127.0.0.1:11898)
# with a port nothing listens on; --out-peers 0 keeps it from dialling addresses learned from A.
start_xkrd hyper-b "$PORT_HB_RPC" "$SD/b" "$SD/b.log" "${COMMON[@]}" \
    --p2p-bind-port 0 --seed-node "$DEAD_PEER" --out-peers 0
watch hyper-a hyper-b
note "A: rpc $RPC_A, levin $PORT_HA_P2P, peer $CPP_PEER; B: rpc $RPC_B, Hyperswarm only"

if [ -n "$CPP_RPC" ]; then
    if wait_same_top "$RPC_A" "$CPP_RPC"; then
        pass 2 "A at the C++ node's top $(top_of "$RPC_A")"
    else
        die 2 "A [$(top_of "$RPC_A")] did not reach the C++ node [$(top_of "$CPP_RPC")] within ${TIMEOUT}s"
    fi
else
    wait_for "$TIMEOUT" height_at_least "$RPC_A" 2 || die 2 "A did not sync from $CPP_PEER"
    skip 2 "A vs the C++ node (no XKR_CPP_RPC); A at $(top_of "$RPC_A")"
fi

t0=$(date +%s)
progress() {
    if same_top "$RPC_B" "$RPC_A"; then return 0; fi
    if [ $(( ($(date +%s) - t0) % 30 )) = 0 ]; then
        note "$(($(date +%s) - t0))s: A $(height_of "$RPC_A"), B $(height_of "$RPC_B")"
    fi
    return 1
}
if wait_for "$TIMEOUT" progress; then
    pass 3 "B reached A's top $(top_of "$RPC_B") over Hyperswarm in $(($(date +%s) - t0))s"
else
    fail 3 "B [$(top_of "$RPC_B")] did not reach A [$(top_of "$RPC_A")] within ${TIMEOUT}s"
fi

# /status counts peers from the network snapshot, which can lag a fast sync by a few seconds.
read_peers() {
    sb=$(obj "$(get "$RPC_B" /status)" peers)
    lo=$(num "$sb" legacy_outgoing)
    li=$(num "$sb" legacy_incoming)
    hy=$(num "$sb" hyper)
    [ -n "$hy" ] && [ "$hy" -ge 1 ]
}
wait_for 15 read_peers || true
if [ "$lo" = 0 ] && [ "$li" = 0 ] && [ -n "$hy" ] && [ "$hy" -ge 1 ]; then
    pass 4 "B /status.peers: legacy_outgoing 0, legacy_incoming 0, hyper $hy (the Hyperswarm path is the only one)"
else
    fail 4 "B /status.peers: legacy_outgoing $lo, legacy_incoming $li, hyper $hy (want 0, 0, >= 1)"
fi

echo "Hyperswarm lines from the logs:"
grep -h -i -E "hyperswarm|hyper bridge|bare worker|levin handshake" "$SD/a.log" | tail -n 8 | sed 's/^/  A /' || true
grep -h -i -E "hyperswarm|hyper bridge|bare worker|levin handshake" "$SD/b.log" | tail -n 8 | sed 's/^/  B /' || true
finish
