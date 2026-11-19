#!/usr/bin/env bash
# Prepares an Ubuntu 22.04 system (glibc 2.35) for the Linux release build: compilers, git,
# patchelf, binutils and Node.js (for bare-make, which builds libbare-kit). Run as root, once:
# in the Linux jobs of .github/workflows/build.yml (inside the ubuntu:22.04 container) and in
# packaging/linux/Dockerfile, which scripts/build-linux-release.sh uses on a developer machine.
#
#   packaging/linux/setup-build-env.sh
#
# Programs built on Ubuntu 22.04 run on every distribution with glibc 2.35 or later (Ubuntu
# 22.04 and newer, Debian 12 and newer, Fedora 36 and newer, ...). Rust itself is not installed
# here: the workflow installs the toolchain from rust-toolchain.toml with rustup.
set -euo pipefail

NODE_VERSION=24.21.0
# sha256 of node-v$NODE_VERSION-linux-<arch>.tar.xz, from nodejs.org/dist/v$NODE_VERSION/SHASUMS256.txt
NODE_SHA256_X64=fd8e59d5a511510f6a298afb548f18c7d2b1be404d8b4a27d94fbe49f56cb2d6
NODE_SHA256_ARM64=6ad1325edbdb5649c379b75a237147a666c95d4f9ae8d340fef2d1575d289ad2

. /etc/os-release
[[ "${ID:-}" = ubuntu && "${VERSION_ID:-}" = 22.04 ]] ||
  echo "note: this script is written for Ubuntu 22.04, not ${PRETTY_NAME:-this system}" >&2

export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q --no-install-recommends \
  build-essential ca-certificates curl git patchelf pkg-config xz-utils zstd unzip
rm -rf /var/lib/apt/lists/*

case "$(uname -m)" in
  x86_64) node_arch=x64 node_sha=$NODE_SHA256_X64 ;;
  aarch64) node_arch=arm64 node_sha=$NODE_SHA256_ARM64 ;;
  *) echo "setup-build-env: no Node.js $NODE_VERSION for $(uname -m)" >&2; exit 1 ;;
esac
if [[ "$(node --version 2>/dev/null)" != "v$NODE_VERSION" ]]; then
  tmp=$(mktemp -d)
  tarball=node-v$NODE_VERSION-linux-$node_arch.tar.xz
  curl -fsSLo "$tmp/$tarball" "https://nodejs.org/dist/v$NODE_VERSION/$tarball"
  echo "$node_sha  $tmp/$tarball" | sha256sum -c --quiet
  tar -C /usr/local --strip-components=1 --no-same-owner -xJf "$tmp/$tarball" \
    --exclude='*/CHANGELOG.md' --exclude='*/README.md' --exclude='*/LICENSE'
  rm -rf "$tmp"
fi
node --version
ldd --version | head -n 1
