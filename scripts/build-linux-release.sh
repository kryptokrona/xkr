#!/usr/bin/env bash
# Builds the portable Linux release archive (glibc 2.35: Ubuntu 22.04 and newer, Debian 12 and
# newer, ...) on a developer machine, in the same Ubuntu 22.04 environment as the Linux jobs of
# .github/workflows/build.yml. The target is the host's (x86_64 or aarch64).
#
#   scripts/build-linux-release.sh [--version <version>] [--out <dir>]
#
# Steps:
#   1. docker image xkr-linux-build:22.04 from packaging/linux/Dockerfile;
#   2. libbare-kit in that image (scripts/build-bare-kit.sh --docker), kept in
#      target/linux-release/bare-kit so it never mixes with the host's target/bare-kit;
#   3. the bridge bundle (packaging/scripts/bundle-bridge.sh) and the crates (`cargo fetch`),
#      both fetched on the host, through Socket Firewall when it is installed;
#   4. cargo build --release --offline in the image (Rust from rust-toolchain.toml, installed by
#      rustup into target/linux-release/rustup), target directory target/linux-release;
#   5. packaging/scripts/package-release.sh --max-glibc 2.35 in the image.
# The archive lands in --out (default dist/linux-release). The container runs with 3 CPUs and
# 5 GiB; cargo with -j 3. Needs docker, git, Node.js with npm and cargo on the host.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
out=$repo/dist/linux-release
version=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) version=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

IMAGE=xkr-linux-build:22.04
GLIBC_MAX=2.35
work=$repo/target/linux-release
case "$(uname -m)" in
  x86_64) target=x86_64-unknown-linux-gnu ;;
  aarch64|arm64) target=aarch64-unknown-linux-gnu ;;
  *) echo "build-linux-release: no Linux release target for $(uname -m)" >&2; exit 2 ;;
esac
for tool in docker git npm cargo; do
  command -v "$tool" >/dev/null 2>&1 || { echo "build-linux-release: $tool not found" >&2; exit 2; }
done

sfw="$(npm prefix -g 2>/dev/null)/bin/sfw"
fetch=()
if [[ -x "$sfw" ]]; then
  fetch=("$sfw")
  PATH="$(dirname "$sfw"):$PATH"
  export PATH
else
  echo "note: Socket Firewall (sfw) not found under \$(npm prefix -g)/bin; fetching without it"
fi

echo "1/5 build image $IMAGE"
docker build -q -t "$IMAGE" "$repo/packaging/linux" >/dev/null

echo "2/5 libbare-kit (glibc $GLIBC_MAX)"
XKR_BARE_KIT_DIR=$work/bare-kit NODE_IMAGE=$IMAGE "$repo/scripts/build-bare-kit.sh" --docker

echo "3/5 bridge bundle and crates"
rm -rf "$work/bridge"
"$repo/packaging/scripts/bundle-bridge.sh" --out "$work/bridge" --target "$target" >/dev/null
cargo_home=${CARGO_HOME:-$HOME/.cargo}
(cd "$repo" && CARGO_HOME=$cargo_home ${fetch[@]+"${fetch[@]}"} cargo fetch --locked)

echo "4/5 cargo build in $IMAGE"
mkdir -p "$work/rustup" "$work/rustup-cargo" "$out"
toolchain=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$repo/rust-toolchain.toml")
[[ -n "$toolchain" ]] || { echo "build-linux-release: no channel in rust-toolchain.toml" >&2; exit 2; }
# rustup and its toolchain live in target/linux-release (built for glibc systems, unlike a
# NixOS host's); the build reads the crates fetched in step 3 from the host's CARGO_HOME.
inner=$(cat <<'INNER'
set -euo pipefail
export RUSTUP_HOME=$WORK/rustup
if [[ ! -x "$WORK/rustup-cargo/bin/rustup" ]]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    CARGO_HOME=$WORK/rustup-cargo sh -s -- -y -q --no-modify-path --profile minimal --default-toolchain none
fi
CARGO_HOME=$WORK/rustup-cargo "$WORK/rustup-cargo/bin/rustup" toolchain install --profile minimal "$TOOLCHAIN"
cargo=$(CARGO_HOME=$WORK/rustup-cargo "$WORK/rustup-cargo/bin/rustup" which --toolchain "$TOOLCHAIN" cargo)
export PATH="$(dirname "$cargo"):$PATH" CARGO_TARGET_DIR=$WORK LIBBARE_DIR=$WORK/bare-kit/lib
cd "$REPO"
nice -n 19 cargo build --offline --locked --release -j 3 \
  -p xkrd -p xkr-wallet-api -p xkr-walletd -p xkr-wallet-cli -p xkr-miner
echo "5/5 package"
packaging/scripts/package-release.sh --target "$TARGET" ${VERSION:+--version "$VERSION"} \
  --bin-dir "$WORK/release" --bare-kit "$WORK/bare-kit/lib" --bridge "$WORK/bridge" \
  --out "$OUT" --max-glibc "$GLIBC_MAX"
INNER
)
docker run --rm --cpus 3 --memory 5g -u "$(id -u):$(id -g)" -e HOME=/tmp \
  -e REPO="$repo" -e WORK="$work" -e OUT="$out" -e TARGET="$target" -e VERSION="$version" \
  -e TOOLCHAIN="$toolchain" -e GLIBC_MAX="$GLIBC_MAX" -e CARGO_HOME="$cargo_home" \
  -v "$repo:$repo" -v "$cargo_home:$cargo_home" -w "$repo" "$IMAGE" bash -c "$inner"
