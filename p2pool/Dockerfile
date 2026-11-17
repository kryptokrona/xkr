# Kryptokrona p2pool with the xkr fork patches (patches/), plus xmrig as the CN-Turtle
# stratum miner for the testnet. Build it alone (no other heavy build at the same time), from
# the repository root:
#
#   p2pool/build.sh
#   # = nice -n 19 docker build --build-arg JOBS=3 -t xkr-p2pool:testnet -f p2pool/Dockerfile p2pool
#
# The compilers run inside the docker daemon, not under the calling shell: limits on the
# calling shell (build.sh's optional CAPPED wrapper) and `nice` on `docker build` do not reach
# them, so every `make` below runs under `nice -n 19` with JOBS (default 3) parallel jobs.
#
# Build arguments:
#   P2POOL_COMMIT      base commit of kryptokrona/p2pool the patches were written against
#   PATCHES            patch files applied in order with `patch -p1` (default: 0001, 0002 and
#                      0003, the testnet-only XKR_TESTNET_UNCAPPED_PPLNS switch that changes
#                      nothing unless set against a testnet daemon;
#                      "0001-fork13-v6.patch" alone gives a fork-13-only build that writes minor
#                      version 13, "" the unpatched p2pool for the negative check of 32-p2pool:
#                      IMAGE=xkr-p2pool:unpatched p2pool/build.sh --build-arg PATCHES=)
#   XKR_FORK13_HEIGHT  compiled-in fork heights (-DXKR_FORK13_HEIGHT / -DXKR_FORK14_HEIGHT;
#   XKR_FORK14_HEIGHT  empty = the patches' defaults 2 700 000 / 2 750 000, the mainnet heights) for a
#                      release with fixed heights. Testnet nodes need no rebuild: the patched
#                      p2pool reads the environment variables of the same names at start-up when
#                      the daemon's get_info reports "testnet": true (README.md)
#   CXX_DEFS           further compile flags appended to CMAKE_CXX_FLAGS
#   WERROR             1 (default): p2pool's -Werror stays on; 0: -DDISABLE_WARNINGS=ON, for a
#                      build that fails only on a warning
#   JOBS               parallel compile jobs (default 3)
#   XMRIG_VERSION      xmrig tag (CN-Pico = CN-Turtle is part of the default build)
#   DEBIAN             base image of every stage (default debian:12-slim, a tag)
#   DEBIAN_DIGEST      sha256:<digest> of that image; when set every stage starts from
#                      DEBIAN@DEBIAN_DIGEST and docker refuses an image with another digest.
#                      Maintainers: pin it (`docker buildx imagetools inspect debian:12-slim`
#                      shows the current digest), e.g. --build-arg DEBIAN_DIGEST=sha256:...
#
# Result: /usr/local/bin/p2pool and /usr/local/bin/xmrig, /etc/p2pool/sidechain-testnet.json; no
# entrypoint (pick the binary in `docker run`). Use --network host to reach xkrd on the host;
# testnet/scenarios/32-p2pool.sh runs both binaries with --user <uid>:<gid> on a mounted /data.

# Current tag: debian:12-slim; pinned by digest when DEBIAN_DIGEST is set.
ARG DEBIAN=debian:12-slim
ARG DEBIAN_DIGEST=""

# --- p2pool -----------------------------------------------------------------------------------
FROM ${DEBIAN}${DEBIAN_DIGEST:+@}${DEBIAN_DIGEST} AS p2pool-build
ARG P2POOL_REPO=https://github.com/kryptokrona/p2pool.git
ARG P2POOL_COMMIT=8edf632fae7ebcbb29d3525144ac77b0dbd6477e
ARG PATCHES="0001-fork13-v6.patch 0002-fork14-tx-v2-coinbase.patch 0003-testnet-uncapped-pplns.patch"
ARG XKR_FORK13_HEIGHT=""
ARG XKR_FORK14_HEIGHT=""
ARG CXX_DEFS=""
ARG WERROR=1
ARG JOBS=3

# libzmq/libsodium/libpgm/libnorm/libgss are what upstream's Dockerfile installs; this port has
# no ZMQ any more and links only libuv, libcurl and pthread, but keeping the list costs little.
RUN set -e && apt-get update -q && \
    DEBIAN_FRONTEND=noninteractive apt-get install -q -y --no-install-recommends \
        git build-essential ca-certificates cmake patch \
        libuv1-dev libzmq3-dev libsodium-dev libpgm-dev libnorm-dev libgss-dev \
        libcurl4-openssl-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src
# The upstream Dockerfile initialises all submodules recursively; the build needs at least
# rapidjson, robin-hood-hashing and grpc's vendored BoringSSL (SHA-256/curve25519 sources with
# WITH_GRPC=OFF and WITH_TLS=OFF, CMakeLists.txt "ED25519_SOURCES").
RUN set -e && git init -q p2pool && cd p2pool && \
    git remote add origin "$P2POOL_REPO" && \
    git fetch -q --depth 1 origin "$P2POOL_COMMIT" && \
    git checkout -q --detach FETCH_HEAD && \
    test "$(git rev-parse HEAD)" = "$P2POOL_COMMIT" && \
    git submodule update --init --recursive && \
    test -f external/src/grpc/third_party/boringssl-with-bazel/crypto/sha/sha256.cc

COPY patches/ /usr/src/xkr-patches/
RUN set -e && cd /usr/src/p2pool && \
    for p in $PATCHES; do echo "applying $p"; patch -p1 --forward < "/usr/src/xkr-patches/$p"; done && \
    if find src -name '*.rej' | grep -q .; then echo "rejected hunks" >&2; exit 1; fi

# gRPC/Tari, TLS, UPnP, the merge-mining donation and LTO are not needed for XKR and cost
# memory. Without UPnP p2pool does not know --no-upnp (and exits on unknown options).
RUN set -e && cd /usr/src/p2pool && mkdir build && cd build && \
    defs="$CXX_DEFS"; \
    for v in "XKR_FORK13_HEIGHT=$XKR_FORK13_HEIGHT" "XKR_FORK14_HEIGHT=$XKR_FORK14_HEIGHT"; do \
        h=${v#*=}; \
        [ -z "$h" ] && continue; \
        case $h in *[!0-9]*) echo "build argument ${v%%=*} must be a block height, not \"$h\"" >&2; exit 1 ;; esac; \
        defs="$defs -D$v"; \
    done; \
    werror=""; \
    if [ "$WERROR" = 0 ]; then werror="-DDISABLE_WARNINGS=ON"; fi; \
    echo "CMAKE_CXX_FLAGS: [$defs] $werror" && \
    cmake .. -DCMAKE_BUILD_TYPE=Release -DWITH_GRPC=OFF -DWITH_TLS=OFF -DWITH_UPNP=OFF \
        -DWITH_MERGE_MINING_DONATION=OFF -DWITH_LTO=OFF -DWITH_RANDOMX=OFF $werror \
        -DCMAKE_CXX_FLAGS="$defs" && \
    nice -n 19 make -j"$JOBS" p2pool && \
    ./p2pool --version

# --- xmrig (CN-Pico / CN-Turtle stratum miner; testnet only) ---------------------------------
FROM ${DEBIAN}${DEBIAN_DIGEST:+@}${DEBIAN_DIGEST} AS xmrig-build
ARG XMRIG_VERSION=v6.21.0
ARG JOBS=3

RUN set -e && apt-get update -q && \
    DEBIAN_FRONTEND=noninteractive apt-get install -q -y --no-install-recommends \
        git build-essential ca-certificates cmake libuv1-dev libssl-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src
# Donation level 0: the test network is offline, nothing must dial donate.xmrig.com.
RUN set -e && git clone -q --depth 1 --branch "$XMRIG_VERSION" https://github.com/xmrig/xmrig.git && \
    cd xmrig && \
    sed -i 's/kDefaultDonateLevel = 1;/kDefaultDonateLevel = 0;/; s/kMinimumDonateLevel = 1;/kMinimumDonateLevel = 0;/' src/donate.h && \
    grep -q 'kMinimumDonateLevel = 0;' src/donate.h && \
    mkdir build && cd build && \
    cmake .. -DCMAKE_BUILD_TYPE=Release -DWITH_HWLOC=OFF -DWITH_OPENCL=OFF -DWITH_CUDA=OFF && \
    nice -n 19 make -j"$JOBS" xmrig

# --- runtime ----------------------------------------------------------------------------------
FROM ${DEBIAN}${DEBIAN_DIGEST:+@}${DEBIAN_DIGEST}
RUN set -e && apt-get update -q && \
    DEBIAN_FRONTEND=noninteractive apt-get install -q -y --no-install-recommends \
        libzmq5 libuv1 libcurl4 libsodium23 libssl3 ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=p2pool-build /usr/src/p2pool/build/p2pool /usr/local/bin/p2pool
COPY --from=xmrig-build /usr/src/xmrig/build/xmrig /usr/local/bin/xmrig
COPY sidechain-testnet.json /etc/p2pool/sidechain-testnet.json
COPY patches/ /usr/share/doc/xkr-p2pool/patches/

# Fails the build if a shared library is missing at run time.
RUN p2pool --version && xmrig --version

RUN groupadd -r p2pool -g 1000 && \
    useradd -u 1000 -r -g p2pool -s /sbin/nologin -c "p2pool user" p2pool && \
    mkdir -p /home/p2pool/.p2pool && chown p2pool:p2pool /home/p2pool /home/p2pool/.p2pool
USER p2pool
WORKDIR /home/p2pool/.p2pool
