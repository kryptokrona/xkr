#!/usr/bin/env bash
# Builds the C++ master branch as a testnet image (heavy: C++ + vendored RocksDB, make -j3).
#   XKR_CPP_SRC   C++ checkout   (required: a clone of github.com/kryptokrona/kryptokrona)
#   XKR_CPP_IMAGE image tag      (default: xkr/kryptokrona-testnet:master)
#   XKR_CPP_PATCHES  which testnet/patches/*.patch to apply: "all" (default), "none", or a
#                 space-separated list of file names, e.g. "0001-fork13-testnet.patch"
#   FORK13_HEIGHT compiled-in fork-13 height (build arg; patched testnet daemons also read the
#                 XKR_FORK13_HEIGHT environment variable at start-up, see patches/README.md)
# The selected patches are applied inside the build (see patches/README.md).
#
#   XKR_CPP_IMAGE=xkr-cpp-testnet:master XKR_CPP_PATCHES=none ./build-cpp.sh   # old node (fork 12)
#   XKR_CPP_IMAGE=xkr-cpp-testnet:fork13 XKR_CPP_PATCHES=0001-fork13-testnet.patch ./build-cpp.sh
set -euo pipefail
cd "$(dirname "$0")"

SRC="${XKR_CPP_SRC:-}"
if [ -z "$SRC" ]; then
    echo "set XKR_CPP_SRC to a C++ checkout (github.com/kryptokrona/kryptokrona)" >&2
    exit 1
fi
IMAGE="${XKR_CPP_IMAGE:-xkr/kryptokrona-testnet:master}"

if [ ! -f "$SRC/CMakeLists.txt" ]; then
    echo "C++ source not found at $SRC (set XKR_CPP_SRC)" >&2
    exit 1
fi

PATCH_SET="${XKR_CPP_PATCHES:-all}"
PATCH_DIR=$(mktemp -d)
trap 'rm -rf "$PATCH_DIR"' EXIT
case "$PATCH_SET" in
    all) for p in patches/*.patch; do [ -e "$p" ] && cp "$p" "$PATCH_DIR/"; done ;;
    none) ;;
    *)
        for name in $PATCH_SET; do
            [ -f "patches/$name" ] || { echo "patch not found: patches/$name" >&2; exit 1; }
            cp "patches/$name" "$PATCH_DIR/"
        done
        ;;
esac
BUILD_ARGS=()
if [ -n "${FORK13_HEIGHT:-}" ]; then
    BUILD_ARGS+=(--build-arg "FORK13_HEIGHT=$FORK13_HEIGHT")
fi

echo "building $IMAGE from $SRC ($(git -C "$SRC" rev-parse --short HEAD 2>/dev/null || echo unknown))"
echo "patches: $(cd "$PATCH_DIR" && ls | tr '\n' ' ')${FORK13_HEIGHT:+(FORK13_HEIGHT=$FORK13_HEIGHT)}"
DOCKER_BUILDKIT=1 nice -n 19 docker build \
    -f Dockerfile.cpp-testnet \
    --build-context xkrpatches="$PATCH_DIR" \
    "${BUILD_ARGS[@]}" \
    -t "$IMAGE" \
    "$SRC"
