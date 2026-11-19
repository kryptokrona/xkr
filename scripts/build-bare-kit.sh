#!/usr/bin/env bash
# Builds libbare-kit (bare-kit 3.1.0), which runs the Bare workers of xkrd's Hyperswarm transport.
#
#   scripts/build-bare-kit.sh [--docker] [--force] [--digest sha256:<64 hex>]
#
# Run it once from a checkout; afterwards plain `cargo build` / `cargo test` find the library,
# because .cargo/config.toml sets LIBBARE_DIR to target/bare-kit/lib (a LIBBARE_DIR in the
# environment wins). Steps (docs/hyper-transport.md, "Building"):
#   1. clone https://github.com/holepunchto/bare-kit at commit 64ca2a9 (v3.1.0) into
#      target/bare-kit/src and install its packages with `npm ci` from bare-kit's own
#      package-lock.json;
#   2. install bare-make 2.1.2 into target/bare-kit/tools (it brings CMake, Ninja and clang;
#      `npm ci` once target/bare-kit/tools/package-lock.json exists);
#   3. bare-make generate, build and install, natively or, on NixOS or with --docker, in a
#      Node.js container (bare-make's own clang cannot link on NixOS). bare-kit's CMakeLists.txt
#      fetches Bare 1.37.1 by its git tag (`fetch_package("github:holepunchto/bare@1.37.1")`),
#      and Bare's build fetches its prebuilt V8 and libuv; the script checks that the pinned
#      bare-kit commit still names that Bare version;
#   4. copy the shared library to target/bare-kit/lib: libbare-kit.so (Linux),
#      libbare-kit.dylib (macOS) or bare-kit.dll with its import library bare-kit.lib (Windows).
# npm fetches go through Socket Firewall ($(npm prefix -g)/bin/sfw) when it is installed.
# XKR_BARE_KIT_DIR moves target/bare-kit elsewhere (scripts/build-linux-release.sh keeps its
# glibc 2.35 build in target/linux-release/bare-kit); LIBBARE_DIR must then name its lib/.
# Container image: NODE_IMAGE (default node:24-bookworm, a tag). To pin it, set NODE_IMAGE_DIGEST
# (or pass --digest) to the image's `sha256:` digest, for example from
# `docker buildx imagetools inspect node:24-bookworm`; the script then runs
# NODE_IMAGE@<digest> and stops when the pulled image does not carry that digest. A NODE_IMAGE
# of your own (a local image, such as xkr-linux-build:22.04 from packaging/linux/Dockerfile)
# runs by its name unless NODE_IMAGE_DIGEST is set.
# Needs git, Node.js with npm, network access, and docker for --docker. The build uses at most
# 3 parallel jobs (CMAKE_BUILD_PARALLEL_LEVEL, default 3). --force rebuilds everything.
set -euo pipefail

REPO_URL=https://github.com/holepunchto/bare-kit
COMMIT=64ca2a9479db82fb3ce4d9e877c34e22cc492d72   # tag v3.1.0
BARE_VERSION=1.37.1                                # fetched by bare-kit's CMakeLists.txt, by tag
BARE_MAKE=bare-make@2.1.2
# node:24-bookworm, pinned by digest (override with NODE_IMAGE_DIGEST or --digest).
if [[ -z "${NODE_IMAGE:-}" ]]; then
  NODE_IMAGE=node:24-bookworm
  NODE_IMAGE_DIGEST=${NODE_IMAGE_DIGEST:-sha256:9d266751ca3d7fdbe651c878ffd5ad0a42fed49a18c283509267c97f025b7496}
fi
NODE_IMAGE_DIGEST=${NODE_IMAGE_DIGEST:-}

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
base=${XKR_BARE_KIT_DIR:-$root/target/bare-kit}
src=$base/src
tools=$base/tools
lib=$base/lib

docker_mode=0
force=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --docker) docker_mode=1; shift ;;
    --force) force=1; shift ;;
    --digest)
      [[ $# -ge 2 ]] || { echo "--digest needs a value (sha256:<64 hex>)" >&2; exit 2; }
      NODE_IMAGE_DIGEST=$2; shift 2 ;;
    --digest=*) NODE_IMAGE_DIGEST=${1#--digest=}; shift ;;
    -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [[ -e /etc/NIXOS ]]; then
  docker_mode=1
fi

if [[ -n "$NODE_IMAGE_DIGEST" ]]; then
  [[ "$NODE_IMAGE_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "build-bare-kit: NODE_IMAGE_DIGEST must be sha256:<64 hex>, not '$NODE_IMAGE_DIGEST'" >&2; exit 2; }
  IMAGE="${NODE_IMAGE%@*}@$NODE_IMAGE_DIGEST"
else
  IMAGE=$NODE_IMAGE
fi

case "$(uname -s)" in
  Linux) libfiles=(libbare-kit.so) ;;
  Darwin) libfiles=(libbare-kit.dylib) ;;
  MINGW*|MSYS*|CYGWIN*) libfiles=(bare-kit.dll bare-kit.lib) ;;
  *) libfiles=(libbare-kit.so) ;;
esac

if [[ $force = 0 && -f "$lib/${libfiles[0]}" ]]; then
  echo "libbare-kit is already built: $lib/${libfiles[0]} (--force rebuilds it)"
  exit 0
fi

need() { command -v "$1" >/dev/null 2>&1 || { echo "build-bare-kit: $1 not found; $2" >&2; exit 2; }; }
need git "install git"
need npm "install Node.js 22 or later with npm"
if [[ $docker_mode = 1 ]]; then
  need docker "install docker (used on NixOS and with --docker)"
fi

npm_cmd=(npm)
sfw="$(npm prefix -g 2>/dev/null)/bin/sfw"
if [[ -x "$sfw" ]]; then
  npm_cmd=("$sfw" npm)
else
  echo "note: Socket Firewall (sfw) not found under \$(npm prefix -g)/bin; using plain npm"
fi

# 1. bare-kit sources at the pinned commit
mkdir -p "$base"
if [[ $force = 1 ]]; then
  rm -rf "$src" "$tools" "$lib"
fi
if [[ ! -d "$src/.git" ]] || [[ "$(git -C "$src" rev-parse HEAD 2>/dev/null)" != "$COMMIT" ]]; then
  rm -rf "$src"
  git init -q "$src"
  git -C "$src" remote add origin "$REPO_URL"
  git -C "$src" fetch -q --depth 1 origin "$COMMIT"
  git -C "$src" checkout -q --detach FETCH_HEAD
fi
[[ "$(git -C "$src" rev-parse HEAD)" = "$COMMIT" ]] || { echo "build-bare-kit: $src is not at $COMMIT" >&2; exit 1; }
grep -qF "fetch_package(\"github:holepunchto/bare@$BARE_VERSION\")" "$src/CMakeLists.txt" ||
  { echo "build-bare-kit: $src/CMakeLists.txt does not fetch Bare $BARE_VERSION" >&2; exit 1; }
[[ -f "$src/package-lock.json" ]] || { echo "build-bare-kit: $src has no package-lock.json" >&2; exit 1; }
if [[ ! -d "$src/node_modules/cmake-bare" ]]; then
  (cd "$src" && "${npm_cmd[@]}" ci --no-audit --no-fund)
fi

# 2. bare-make, local to target/bare-kit/tools. In a container on a non-Linux host, its CMake,
# Ninja and clang packages must be the Linux ones.
if [[ ! -x "$tools/node_modules/.bin/bare-make" ]]; then
  mkdir -p "$tools"
  [[ -f "$tools/package.json" ]] || echo '{ "private": true }' > "$tools/package.json"
  os_flag=()
  if [[ $docker_mode = 1 && "$(uname -s)" != Linux ]]; then
    os_flag=(--os=linux)
  fi
  if [[ -f "$tools/package-lock.json" ]] && grep -qF "\"${BARE_MAKE#bare-make@}\"" "$tools/package.json"; then
    # the lockfile of the first install
    (cd "$tools" && "${npm_cmd[@]}" ci --no-audit --no-fund ${os_flag[@]+"${os_flag[@]}"})
  else
    (cd "$tools" && "${npm_cmd[@]}" install --no-audit --no-fund --save-exact ${os_flag[@]+"${os_flag[@]}"} "$BARE_MAKE")
  fi
fi

# 3. generate, build, install (into $src/prebuilds)
jobs=${CMAKE_BUILD_PARALLEL_LEVEL:-3}
steps='set -e; bm=$1; nice -n 19 "$bm" generate; nice -n 19 "$bm" build; nice -n 19 "$bm" install'
if [[ $docker_mode = 1 ]]; then
  if [[ -n "$NODE_IMAGE_DIGEST" ]]; then
    docker pull -q "$IMAGE" >/dev/null
    docker image inspect --format '{{join .RepoDigests "\n"}}' "$IMAGE" | grep -qF "@$NODE_IMAGE_DIGEST" ||
      { echo "build-bare-kit: $IMAGE does not carry digest $NODE_IMAGE_DIGEST" >&2; exit 1; }
  else
    echo "note: $IMAGE is used by tag; set NODE_IMAGE_DIGEST=sha256:<digest> (or --digest) to pin it"
  fi
  echo "building bare-kit in docker ($IMAGE, 3 CPUs, 4 GiB)"
  # Same path inside and outside, so CMake's absolute paths stay valid for a later native run.
  docker run --rm --cpus 3 --memory 4g -u "$(id -u):$(id -g)" \
    -e HOME=/tmp -e CMAKE_BUILD_PARALLEL_LEVEL="$jobs" \
    -v "$base:$base" -w "$src" "$IMAGE" \
    bash -c "$steps" bash "$tools/node_modules/.bin/bare-make"
else
  echo "building bare-kit natively"
  (cd "$src" && CMAKE_BUILD_PARALLEL_LEVEL="$jobs" bash -c "$steps" bash "$tools/node_modules/.bin/bare-make")
fi

# 4. the shared library into target/bare-kit/lib (symbolic links kept)
mkdir -p "$lib"
for f in "${libfiles[@]}"; do
  found=""
  for d in "$src/prebuilds/lib" "$src/prebuilds/bin"; do
    if [[ -e "$d/$f" ]]; then
      found=$d/$f
      break
    fi
  done
  [[ -n "$found" ]] || { echo "build-bare-kit: $f not found under $src/prebuilds" >&2; exit 1; }
  cp -a "$(dirname "$found")/$f"* "$lib/"
done

case "$(uname -s)" in
  Darwin)
    # xkrd finds the library through its @executable_path and LIBBARE_DIR search paths, which
    # needs an install name that starts with @rpath/.
    if ! otool -D "$lib/libbare-kit.dylib" | tail -n 1 | grep -q '^@rpath/'; then
      install_name_tool -id @rpath/libbare-kit.dylib "$lib/libbare-kit.dylib"
    fi
    ;;
  MINGW*|MSYS*|CYGWIN*)
    echo "note: Windows loads bare-kit.dll from the executable's directory or PATH;" \
      "copy $lib/bare-kit.dll next to xkrd.exe (target/<profile>/) or add $lib to PATH"
    ;;
esac

# target/<profile>/lib -> target/bare-kit/lib: xkrd finds libbare-kit at $ORIGIN/lib when run
# from target/debug or target/release (release binaries search only next to themselves).
for profile in debug release; do
  mkdir -p "$base/../$profile"
  ln -sfn ../bare-kit/lib "$base/../$profile/lib"
done

echo "libbare-kit in $lib:"
ls -l "$lib"
echo "next: cargo build --release -p xkrd (LIBBARE_DIR comes from .cargo/config.toml; target/<profile>/lib links to it)"
if [[ -e /etc/NIXOS ]]; then
  echo "NixOS: the .bare addons need libstdc++ at run time:" \
    "export LD_LIBRARY_PATH=\"\$(dirname \"\$(gcc -print-file-name=libstdc++.so.6)\")\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}\""
fi
