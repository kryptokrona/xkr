#!/usr/bin/env bash
# 00-preflight: everything the suite needs is present; prints versions. Never builds anything.
#
# Checks: commands (curl, docker + daemon, timeout, nice), target/release/xkrd and
# xkr-testwallet (with versions; a NOTE when a source file is newer than the binary), xkrd finds
# libbare-kit (the Bare workers of its Hyperswarm transport), node >= 22, bridge/node_modules,
# the Hugin node_modules of 40-hugin-relay, the images xkr-cpp-testnet:master and :fork13 (with
# kryptokronad and miner inside), every suite port free, free disk and memory. Prints the git
# commit when the tree is a git checkout.
#
# Env: MIN_DISK_MB (default 2048), MIN_MEM_MB (default 4096).
# suite: expected=10 timeout=180
set -euo pipefail
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

MIN_DISK_MB=${MIN_DISK_MB:-2048}
MIN_MEM_MB=${MIN_MEM_MB:-4096}

# ------------------------------------------------------------------ commands

for c in curl docker timeout nice df awk sed grep tr; do
    if command -v "$c" >/dev/null 2>&1; then
        :
    else
        missing cmd "command $c not found" "install $c"
    fi
done
pass cmd "bash ${BASH_VERSION}, curl $(curl --version 2>/dev/null | head -n 1 | awk '{print $2}')"
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    pass docker "docker $(docker version --format '{{.Server.Version}}' 2>/dev/null || echo '?') reachable"
else
    missing docker "docker daemon not reachable" "start docker / join the docker group"
fi

# ------------------------------------------------------------------ source tree

if command -v git >/dev/null 2>&1 && git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1; then
    dirty=$(git -C "$ROOT" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
    pass source "git $(git -C "$ROOT" rev-parse --short=12 HEAD) ($(git -C "$ROOT" log -1 --format=%cs 2>/dev/null)), $dirty changed files"
else
    pass source "$ROOT is not a git checkout (no commit to record; binary checksums below identify the build)"
fi

# ------------------------------------------------------------------ binaries

bin_info() { # <path>
    local sum mtime
    sum=$(sha256sum "$1" 2>/dev/null | cut -c1-16 || true)
    mtime=$(date -u -r "$1" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || true)
    echo "sha256 $sum…, built $mtime"
}
if [ -x "$XKRD" ]; then
    pass xkrd "$(xkrd_version) at $XKRD ($(bin_info "$XKRD"))"
    newer=$(find "$ROOT/crates" -name '*.rs' -newer "$XKRD" -print 2>/dev/null | head -n 3 | tr '\n' ' ')
    if [ -n "$newer" ]; then
        note "NOTE: sources newer than xkrd (rebuild before trusting the run?): $newer"
    fi
else
    missing xkrd "xkrd not found at $XKRD" "$BUILD_XKRD"
fi
if [ -x "$XKRD" ]; then
    if xkrd_finds_bare_kit; then
        pass bare-kit "xkrd finds libbare-kit (Hyperswarm on Bare workers)"
    else
        missing bare-kit "$XKRD does not start (libbare-kit not found)" \
            "scripts/build-bare-kit.sh, then $BUILD_XKRD"
    fi
    if [ -e /etc/NIXOS ] && [ "${#XKRD_ENV[@]}" = 0 ] && ! command -v gcc >/dev/null 2>&1; then
        note "NOTE: NixOS without gcc on PATH: the .bare addons may not find libstdc++.so.6 (put gcc's lib directory on LD_LIBRARY_PATH)"
    fi
fi
if [ -x "$XKR_TESTWALLET" ]; then
    pass testwallet "$("$XKR_TESTWALLET" --version 2>/dev/null | head -n 1) at $XKR_TESTWALLET ($(bin_info "$XKR_TESTWALLET"))"
else
    missing testwallet "xkr-testwallet not found at $XKR_TESTWALLET" "$BUILD_XKRD"
fi

# ------------------------------------------------------------------ node, bridge, Hugin modules

if command -v "$NODE" >/dev/null 2>&1; then
    v=$("$NODE" --version 2>/dev/null || true)
    major=${v#v}
    major=${major%%.*}
    if [ -n "$major" ] && [ "$major" -ge 22 ] 2>/dev/null; then
        pass node "node $v ($(command -v "$NODE"))"
    else
        fail node "node $v is older than 22 (bridge/package.json engines)"
    fi
else
    missing node "node not found ($NODE)" "install Node.js >= 22 (or set NODE)"
fi
if [ -d "$ROOT/bridge/node_modules/hyperswarm" ] && [ -d "$ROOT/bridge/node_modules/hyperdht" ]; then
    pass bridge "bridge/node_modules (hyperswarm $(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$ROOT/bridge/node_modules/hyperswarm/package.json" | head -n 1))"
else
    missing bridge "bridge/node_modules incomplete (needed by 20, 40)" "cd bridge && sfw npm ci"
fi
hmiss=""
for pkg in bare-rpc keypear hugin-utils kryptokrona-utils; do
    [ -d "$HUGIN_NODE_MODULES/$pkg" ] || hmiss="$hmiss $HUGIN_NODE_MODULES/$pkg"
done
if [ -z "$hmiss" ]; then
    pass hugin "Hugin app modules in $HUGIN_NODE_MODULES"
else
    missing hugin "Hugin modules missing (needed by 40):$hmiss" "install hugin-desktop's node_modules or set HUGIN_NODE_MODULES"
fi

# ------------------------------------------------------------------ images

check_image() { # <image> <build command> <role>
    local id created files
    if ! docker image inspect "$1" >/dev/null 2>&1; then
        missing "image" "docker image $1 ($3) missing" "$2"
        return 0
    fi
    id=$(docker image inspect -f '{{.Id}}' "$1" 2>/dev/null | cut -c8-19)
    created=$(docker image inspect -f '{{.Created}}' "$1" 2>/dev/null | cut -c1-19)
    files=$(nice -n 19 docker run --rm --network none "$1" ls /usr/src/kryptokrona 2>/dev/null | tr '\n' ' ' || true)
    # Two separate tests: a single glob such as `*" kryptokronad "*" miner "*` cannot match
    # "kryptokronad miner" because both alternatives need the one space between the names.
    local have_daemon=0 have_miner=0
    case " $files " in *" kryptokronad "*) have_daemon=1 ;; esac
    case " $files " in *" miner "*) have_miner=1 ;; esac
    if [ "$have_daemon" = 1 ] && [ "$have_miner" = 1 ]; then
        pass image "$1 ($3): id $id, created $created, has kryptokronad and miner"
    else
        fail image "$1 ($3): kryptokronad/miner not found in /usr/src/kryptokrona ($files)"
    fi
}
if docker info >/dev/null 2>&1; then
    check_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE" "old node, fork index 12"
    check_image "$CPP_FORK13_IMAGE" "$BUILD_FORK13_IMAGE" "patched node, fork index 13"
fi

# ------------------------------------------------------------------ ports, disk, memory

busy=""
for p in $SUITE_PORTS; do
    if port_busy "$p"; then busy="$busy $p"; fi
done
if [ -z "$busy" ]; then
    pass ports "all suite ports free: $(echo $SUITE_PORTS)"
else
    fail ports "ports in use:$busy (earlier run: testnet/suite.sh $WD 90; see docs/testnet-suite.md, Ports)"
fi
if port_busy 11898 || port_busy 11899; then
    note "NOTE: 11898/11899 in use (compose testnet or xkrd defaults); the suite does not use them and never peers with them"
fi
disk=$(df -Pk "$WD" 2>/dev/null | awk 'NR == 2 {print int($4 / 1024)}')
if [ -n "$disk" ] && [ "$disk" -ge "$MIN_DISK_MB" ]; then
    pass disk "$disk MB free under $WD (need $MIN_DISK_MB)"
else
    fail disk "only ${disk:-?} MB free under $WD, need $MIN_DISK_MB (MIN_DISK_MB)"
fi
mem=$(awk '/^MemAvailable:/ {print int($2 / 1024)}' /proc/meminfo 2>/dev/null || true)
if [ -n "$mem" ] && [ "$mem" -ge "$MIN_MEM_MB" ]; then
    pass memory "$mem MB available (need $MIN_MEM_MB: up to 3 C++ containers at 1 GiB and 2 xkrd)"
else
    fail memory "only ${mem:-?} MB available, need $MIN_MEM_MB (MIN_MEM_MB); close something first"
fi

left=$(docker ps -aq --filter "label=$SUITE_LABEL" 2>/dev/null | wc -l | tr -d ' ' || true)
if [ "${left:-0}" != 0 ]; then
    note "NOTE: $left suite container(s) from an earlier run exist; scenarios replace them, 90-cleanup removes them"
fi
finish
