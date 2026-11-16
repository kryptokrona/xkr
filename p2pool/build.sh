#!/usr/bin/env bash
# p2pool/build.sh: builds the p2pool test image (kryptokrona/p2pool 8edf632 + patches/0001,
# 0002 and 0003, plus xmrig) that testnet/scenarios/32-p2pool.sh needs.
#
#   p2pool/build.sh [extra docker build arguments...]
#
#   IMAGE=xkr-p2pool:testnet           tag (default)
#   JOBS=3                             parallel compile jobs inside the build (at most 3)
#   XKR_FORK13_HEIGHT / XKR_FORK14_HEIGHT   if set: compiled-in fork heights (--build-arg);
#                                      not needed for the testnet (the scenario passes them
#                                      as environment variables at run time)
#   CAPPED=<wrapper>                   optional wrapper for heavy steps, run in front of docker
#                                      build (default: none; e.g. CAPPED="systemd-run --user --scope"; NO_CAP=1 skips it)
#   MEMORY=4g                          optional hard memory limit for the build containers;
#                                      uses the legacy builder (DOCKER_BUILDKIT=0 and
#                                      `docker build --memory`), because BuildKit has no limit
#
# Examples:
#   p2pool/build.sh                                               # xkr-p2pool:testnet
#   IMAGE=xkr-p2pool:unpatched p2pool/build.sh --build-arg PATCHES=  # negative check (9a)
#   p2pool/build.sh --build-arg WERROR=0         # if the untested patches fail only on -Werror
#
# This is a long C++ build (p2pool with grpc's BoringSSL sources, xmrig): run it alone, never
# next to a cargo build, a suite run or another image build. The build runs inside the docker
# daemon, so a CAPPED wrapper's limits apply to the docker client only; the Dockerfile itself runs every `make` under
# `nice -n 19` with JOBS jobs, which is what bounds the build's CPU and memory (expect up to
# about 1 GiB per job), and MEMORY=4g adds a hard limit.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
IMAGE=${IMAGE:-xkr-p2pool:testnet}
JOBS=${JOBS:-3}
CAPPED=${CAPPED:-}

case $JOBS in
    1 | 2 | 3) ;;
    *) echo "JOBS=$JOBS: use 1-3 parallel jobs" >&2; exit 2 ;;
esac
command -v docker > /dev/null 2>&1 || { echo "docker not found" >&2; exit 2; }

args=(--build-arg "JOBS=$JOBS" -t "$IMAGE" -f "$HERE/Dockerfile")
[ -n "${XKR_FORK13_HEIGHT:-}" ] && args+=(--build-arg "XKR_FORK13_HEIGHT=$XKR_FORK13_HEIGHT")
[ -n "${XKR_FORK14_HEIGHT:-}" ] && args+=(--build-arg "XKR_FORK14_HEIGHT=$XKR_FORK14_HEIGHT")
if [ -n "${MEMORY:-}" ]; then
    export DOCKER_BUILDKIT=0
    args+=(--memory "$MEMORY" --memory-swap "$MEMORY")
fi
args+=("$@" "$HERE")

echo "building $IMAGE from $HERE (JOBS=$JOBS) at $(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [ "${NO_CAP:-0}" != 1 ] && [ -n "$CAPPED" ]; then
    # shellcheck disable=SC2086 # CAPPED is a command prefix
    exec $CAPPED nice -n 19 docker build "${args[@]}"
fi
exec nice -n 19 docker build "${args[@]}"
