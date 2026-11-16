#!/usr/bin/env bash
# p2pool/harness/build.sh: builds the image xkr-p2pool-vectors (the C++ p2pool_vectors dump at
# -O2) in two steps: the `p2pool-build` stage of p2pool/Dockerfile with its default patches
# 0001-0003 (0003 changes nothing without its testnet variable; the stage is cached if
# xkr-p2pool:testnet was built before), then p2pool/harness/Dockerfile on top.
# A long C++ build: run it alone, never next to a cargo build or another image build.
#
#   p2pool/harness/build.sh
#   IMAGE=xkr-p2pool-vectors JOBS=3 CAPPED=<wrapper> NO_CAP=1   (as p2pool/build.sh)
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
P2POOL=$(cd "$HERE/.." && pwd)
IMAGE=${IMAGE:-xkr-p2pool-vectors}
BASE=${BASE:-xkr-p2pool-build:vectors}
JOBS=${JOBS:-3}
CAPPED=${CAPPED:-}   # optional wrapper for heavy steps (default: none)

case $JOBS in
    1 | 2 | 3) ;;
    *) echo "JOBS=$JOBS: use 1-3 parallel jobs" >&2; exit 2 ;;
esac

run() {
    if [ "${NO_CAP:-0}" != 1 ] && [ -n "$CAPPED" ]; then
        # shellcheck disable=SC2086 # CAPPED is a command prefix
        $CAPPED nice -n 19 "$@"
    else
        nice -n 19 "$@"
    fi
}

echo "building $BASE (p2pool-build stage) at $(date -u +%Y-%m-%dT%H:%M:%SZ)"
run docker build --target p2pool-build \
    --build-arg JOBS="$JOBS" \
    -t "$BASE" -f "$P2POOL/Dockerfile" "$P2POOL"

echo "building $IMAGE at $(date -u +%Y-%m-%dT%H:%M:%SZ)"
run docker build --build-arg JOBS="$JOBS" --build-arg BASE="$BASE" \
    -t "$IMAGE" -f "$HERE/Dockerfile" "$HERE"
