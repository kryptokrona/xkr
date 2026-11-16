#!/usr/bin/env bash
# Stops the local testnet.  ./down.sh --wipe also deletes the chain data in testnet/data/.
set -euo pipefail
cd "$(dirname "$0")"

docker compose down
if [ "${1:-}" = "--wipe" ]; then
    # Containers run as root, so the files may not be ours: remove them from a container.
    docker run --rm -v "$PWD/data:/data" "${XKR_CPP_IMAGE:-ghcr.io/kryptokrona/kryptokrona-testnet:latest}" \
        sh -c 'rm -rf /data/node1 /data/node2 /data/node3 /data/xkrd'
    echo "chain data wiped"
fi
