#!/usr/bin/env bash
# getinfo on every testnet node (host-published RPC ports).
set -uo pipefail

NODES="node1:11899 node2:12899 node3:13899 xkrd:14899"
have_jq=0
command -v jq >/dev/null 2>&1 && have_jq=1

for entry in $NODES; do
    name="${entry%%:*}"
    port="${entry##*:}"
    info=$(curl -s --max-time 3 "http://127.0.0.1:$port/getinfo") || info=""
    if [ -z "$info" ]; then
        [ "$name" = "xkrd" ] || echo "$name (rpc $port): unreachable"
        continue
    fi
    if [ "$have_jq" = 1 ]; then
        printf '%s (rpc %s): ' "$name" "$port"
        echo "$info" | jq -c '{height, network_height, difficulty, tx_pool_size,
            outgoing: .outgoing_connections_count, incoming: .incoming_connections_count,
            major_version, synced, version}'
    else
        echo "$name (rpc $port): $info"
    fi
done

# Top block hash per node: the three C++ nodes must agree.
for port in 11899 12899 13899; do
    curl -s --max-time 3 -d '{"jsonrpc":"2.0","id":0,"method":"getlastblockheader","params":{}}' \
        "http://127.0.0.1:$port/json_rpc" \
        | { if [ "$have_jq" = 1 ]; then jq -r --arg p "$port" '"rpc \($p) top: \(.result.block_header.height) \(.result.block_header.hash)"'; else cat; echo; fi; } \
        2>/dev/null || true
done
