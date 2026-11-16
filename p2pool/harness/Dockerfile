# p2pool_vectors (p2pool_vectors.cpp): expected values of the C++ p2pool for the Rust p2pool's
# differential tests. Built on the `p2pool-build` stage of p2pool/Dockerfile (checkout at
# 8edf632 with the patches) by p2pool/harness/build.sh.
#
# Result: /usr/local/bin/p2pool_vectors. Run:
#   docker run --rm -v <dir>:/data xkr-p2pool-vectors \
#       p2pool_vectors /data/blocks.jsonl /data/seed-diffs.json [/data/sidechain-testnet.json]
#
# The runtime stage starts from DEBIAN (default debian:12-slim, a tag); with
# --build-arg DEBIAN_DIGEST=sha256:<digest> it starts from DEBIAN@DEBIAN_DIGEST and docker refuses
# an image with another digest (same arguments as p2pool/Dockerfile; maintainers: pin it).

ARG BASE=xkr-p2pool-build:vectors
# Current tag: debian:12-slim; pinned by digest when DEBIAN_DIGEST is set.
ARG DEBIAN=debian:12-slim
ARG DEBIAN_DIGEST=""
FROM ${BASE} AS vectors-build
ARG JOBS=3

COPY CMakeLists.txt p2pool_vectors.cpp /usr/src/p2pool/tools/vectors/
# get_shares / get_difficulty are private: this copy of the tree lets the dump read them.
RUN set -e && cd /usr/src/p2pool && \
    sed -i '/^class SideChain : public nocopy_nomove$/{n;s/^{$/{\n\tfriend struct VectorDump;/}' src/side_chain.h && \
    grep -q 'friend struct VectorDump;' src/side_chain.h && \
    cmake -S tools/vectors -B build-vectors && \
    nice -n 19 make -C build-vectors -j"$JOBS" p2pool_vectors && \
    cp build-vectors/p2pool_vectors /usr/local/bin/p2pool_vectors

FROM ${DEBIAN}${DEBIAN_DIGEST:+@}${DEBIAN_DIGEST}
RUN set -e && apt-get update -q && \
    DEBIAN_FRONTEND=noninteractive apt-get install -q -y --no-install-recommends libuv1 && \
    rm -rf /var/lib/apt/lists/*
COPY --from=vectors-build /usr/local/bin/p2pool_vectors /usr/local/bin/p2pool_vectors
WORKDIR /data
