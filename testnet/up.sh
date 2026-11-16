#!/usr/bin/env bash
# Starts the local testnet.
#   ./up.sh           use $XKR_CPP_IMAGE or the prebuilt ghcr image
#   ./up.sh --build   build the C++ master branch first and use that image
set -euo pipefail
cd "$(dirname "$0")"

if [ "${1:-}" = "--build" ]; then
    export XKR_CPP_IMAGE="${XKR_CPP_IMAGE:-xkr/kryptokrona-testnet:master}"
    # plain master (fork index 12); the fork-13 image is built separately, see patches/README.md
    export XKR_CPP_PATCHES="${XKR_CPP_PATCHES:-none}"
    ./build-cpp.sh
fi

mkdir -p data/node1 data/node2 data/node3 data/xkrd
echo "image: ${XKR_CPP_IMAGE:-ghcr.io/kryptokrona/kryptokrona-testnet:latest}"
docker compose up -d
echo "waiting for RPC..."
for _ in $(seq 1 30); do
    if curl -s --max-time 2 http://127.0.0.1:11899/getinfo >/dev/null 2>&1; then
        break
    fi
    sleep 2
done
./status.sh
