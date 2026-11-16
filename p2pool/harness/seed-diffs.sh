#!/usr/bin/env bash
# p2pool/harness/seed-diffs.sh: writes seed-diffs.json for p2pool_vectors: the main-chain
# difficulty and hash at every seed height (multiples of 2048; a share at main-chain height h
# uses (h - 65) & ~2047) from FROM up to the node's tip, read with getblockheaderbyheight.
#
#   p2pool/harness/seed-diffs.sh [FROM] > seed-diffs.json
#   RPC=http://127.0.0.1:11898/json_rpc   (default, xkrd mainnet RPC)
#
# Output: {"tip": <height>, "rule": "...", "seeds": {"<height>": {"difficulty": n, "hash": "<hex>"}}}
set -euo pipefail
RPC=${RPC:-http://127.0.0.1:11898/json_rpc}
FROM=${1:-2420736}

call() {
    curl -sf --max-time 30 -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"$1\",\"params\":$2}" "$RPC"
}

tip=$(call getblockcount '{}' | jq -er '.result.count - 1')
from=$(( FROM & ~2047 ))
printf '{"tip": %d, "rule": "seed height = (main height - 65) & ~2047", "seeds": {' "$tip"
sep=""
for (( h = from; h <= tip; h += 2048 )); do
    header=$(call getblockheaderbyheight "{\"height\":$h}")
    difficulty=$(jq -er '.result.block_header.difficulty' <<< "$header")
    hash=$(jq -er '.result.block_header.hash' <<< "$header")
    printf '%s"%d": {"difficulty": %d, "hash": "%s"}' "$sep" "$h" "$difficulty" "$hash"
    sep=", "
done
printf '}}\n'
