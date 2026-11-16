# shellcheck shell=bash
# testnet/lib-p2pool.sh: helpers of the p2pool scenarios (testnet/scenarios/32-p2pool.sh,
# 34-p2pool-mixed.sh; docs/testnet-suite.md "32-p2pool", "34-p2pool-mixed").
# Sourced after testnet/lib.sh; kept apart from lib.sh so that the shared library does not
# change for the optional scenarios.
#
# Nothing here builds anything. The image (p2pool at 8edf632 + p2pool/patches/0001 + 0002 + 0003,
# plus xmrig) comes from p2pool/build.sh; its absence is reported as MISSING with that command.

if [ -n "${XKR_SUITE_LIB_P2POOL:-}" ]; then
    return 0
fi
XKR_SUITE_LIB_P2POOL=1

P2POOL_IMAGE=${P2POOL_IMAGE:-xkr-p2pool:testnet}
P2POOL_UNPATCHED_IMAGE=${P2POOL_UNPATCHED_IMAGE:-xkr-p2pool:unpatched}
BUILD_P2POOL_IMAGE="p2pool/build.sh   (= nice -n 19 docker build --build-arg JOBS=3 -t $P2POOL_IMAGE p2pool/; a long C++ build, run it alone)"
BUILD_P2POOL_UNPATCHED="IMAGE=$P2POOL_UNPATCHED_IMAGE p2pool/build.sh --build-arg PATCHES="

# ------------------------------------------------------------------ ports (docs/testnet-suite.md §3)
# All 127.0.0.1, clear of every other scenario (the shared seed node keeps 16898/16899 busy
# between 10 and 90).

PORT_PP_A_P2P=22898; PORT_PP_A_RPC=22899    # xkrd A (miner in phase 0, then p2pool's daemon)
PORT_PP_B_RPC=22889                         # xkrd B (follower, no P2P listener)
PORT_PP_CPP_P2P=22868; PORT_PP_CPP_RPC=22869 # optional C++ fork-13 node
PORT_PP_POOL_A=22890; PORT_PP_STRATUM_A=22333 # p2pool A: sidechain P2P, stratum
PORT_PP_POOL_B=22891; PORT_PP_STRATUM_B=22334 # p2pool B
PORT_PP_POOL_C=22892; PORT_PP_STRATUM_C=22335 # optional unpatched p2pool C (negative check)
# 34-p2pool-mixed (own ports: it can run next to 32, 40 and 65)
PORT_PM_X_P2P=22798; PORT_PM_X_RPC=22799     # xkrd X (phase-0 miner, then C's daemon and R1's Levin peer)
PORT_PM_R1_RPC=22789                         # xkrd R1 (built-in p2pool over TCP and Hyperswarm)
PORT_PM_R2_RPC=22779                         # xkrd R2 (built-in p2pool over Hyperswarm only)
PORT_PM_POOL_C=22790; PORT_PM_STRATUM_C=22343 # C++ p2pool C: sidechain P2P, stratum
PORT_PM_POOL_R1=22791; PORT_PM_STRATUM_R1=22344 # R1's p2pool: sidechain TCP listener, stratum
PORT_PM_STRATUM_R2=22345                     # R2's p2pool: stratum (no sidechain TCP listener)
# RPC port of the xkrd that start_p2pool's containers use as their daemon (32, 40, 65: xkrd A)
P2POOL_RPC_PORT=$PORT_PP_A_RPC

P2POOL_LOGLEVEL=${P2POOL_LOGLEVEL:-3}  # 3 prints "new miner data", SHARE FOUND, BLOCK ACCEPTED
XMRIG_THREADS=${XMRIG_THREADS:-1}       # per xmrig container (one per p2pool node)
XMRIG_ALGO=${XMRIG_ALGO:-cn-pico}       # CN-Pico = CryptoNight-Turtle (p2pool's job says cryptonight_turtle)
# 1: XKR_TESTNET_UNCAPPED_PPLNS=1 for every p2pool node (patch 0003): without it the PPLNS
# window is capped at 2x the block difficulty, i.e. at one share on the testnet (difficulty 1),
# and every coinbase pays only the node that built it (p2pool/README.md, patch 0003)
PPLNS_UNCAPPED=${PPLNS_UNCAPPED:-1}
PPLNS_UNCAPPED_LINE="XKR testnet: PPLNS weight not capped at 2x the block difficulty"

C_POOL_PREFIX=xkr-suite-p2pool-
C_XMRIG_PREFIX=xkr-suite-xmrig-

# ------------------------------------------------------------------ containers

# require_p2pool_image: MISSING (exit 2) with the build command when the image is absent
require_p2pool_image() {
    require_docker
    docker image inspect "$P2POOL_IMAGE" > /dev/null 2>&1 \
        || missing_exit image "image $P2POOL_IMAGE (p2pool + patches 0001-0003 + xmrig) missing" "$BUILD_P2POOL_IMAGE"
}
image_present() { docker image inspect "$1" > /dev/null 2>&1; }

pool_container() { echo "$C_POOL_PREFIX$1"; }
xmrig_container() { echo "$C_XMRIG_PREFIX$1"; }
pool_dir() { echo "$SD/p2pool-$1"; }
pool_log() { echo "$SD/p2pool-$1/p2pool.log"; }

# start_p2pool <node a|b|c|...> <image> <wallet> <sidechain p2p port> <stratum port> [p2pool args...]
# Daemon: the xkrd RPC on 127.0.0.1:$P2POOL_RPC_PORT (xkrd A of 32 unless the scenario sets it).
# --network host (xkrd and the peers are on 127.0.0.1), runs as the calling user so the data
# dir (<sd>/p2pool-<node>, mounted at /data: p2pool.log, p2pool_cache.bin, api/) stays writable
# and removable, nice -n 19 inside the container, 1 CPU / 768 MiB. The fork heights go in as
# XKR_FORK13_HEIGHT / XKR_FORK14_HEIGHT (honoured by the patched p2pool because xkrd's get_info
# says "testnet": true; ignored by an unpatched one), and XKR_TESTNET_UNCAPPED_PPLNS =
# PPLNS_UNCAPPED (patch 0003, same condition). --no-upnp is NOT passed: the image is built
# with WITH_UPNP=OFF, and p2pool rejects options it was compiled without.
start_p2pool() {
    local n=$1 image=$2 w=$3 p=$4 s=$5 c dir
    shift 5
    c=$(pool_container "$n")
    dir=$(pool_dir "$n")
    rm_container "$c"
    mkdir -p "$dir/api"
    nice -n 19 docker run -d --name "$c" --label "$SUITE_LABEL=1" --label "$SUITE_LABEL-scenario=$SCEN" --network host \
        --cpus 1 --memory 768m --user "$(id -u):$(id -g)" \
        -e "XKR_FORK13_HEIGHT=$F13" -e "XKR_FORK14_HEIGHT=$F14" -e "XKR_TESTNET_UNCAPPED_PPLNS=$PPLNS_UNCAPPED" \
        -v "$dir:/data" -w /data "$image" \
        nice -n 19 p2pool --host 127.0.0.1 --rpc-port "$P2POOL_RPC_PORT" --wallet "$w" \
        --sidechain-config /etc/p2pool/sidechain-testnet.json \
        --p2p "127.0.0.1:$p" --stratum "127.0.0.1:$s" --p2p-allow-same-ip --no-dns --no-color \
        --data-dir /data --data-api /data/api --local-api --loglevel "$P2POOL_LOGLEVEL" "$@" > /dev/null \
        || die start "cannot start p2pool container $c from $image"
    _remember_container "$c"
    sleep 3
    if ! container_running "$c"; then
        docker logs --tail 30 "$c" 2>&1 || true
        die start "p2pool container $c exited at once (bad option or missing library? docker logs above)"
    fi
}

# start_xmrig <node> <stratum port>: xmrig from the same image, CN-Turtle (cn-pico), donation 0
start_xmrig() {
    local n=$1 s=$2 c
    c=$(xmrig_container "$n")
    rm_container "$c"
    nice -n 19 docker run -d --name "$c" --label "$SUITE_LABEL=1" --label "$SUITE_LABEL-scenario=$SCEN" --network host \
        --cpus "$XMRIG_THREADS" --memory 256m --user "$(id -u):$(id -g)" "$P2POOL_IMAGE" \
        nice -n 19 xmrig -o "127.0.0.1:$s" -u x -a "$XMRIG_ALGO" --threads "$XMRIG_THREADS" \
        --donate-level 0 --no-color --print-time 10 --cpu-priority 0 > /dev/null \
        || die start "cannot start xmrig container $c"
    _remember_container "$c"
}
stop_xmrig() { docker stop -t 5 "$(xmrig_container "$1")" > /dev/null 2>&1 || true; }
xmrig_accepted() { { docker logs "$(xmrig_container "$1")" 2>&1 | grep -ac "accepted (" || true; } | head -n 1; }
xmrig_rejected() { { docker logs "$(xmrig_container "$1")" 2>&1 | grep -ac "rejected (" || true; } | head -n 1; }

# ------------------------------------------------------------------ p2pool logs and data API

# pool_api <node> <category/name>: the local data API file (local/p2p, local/stratum, pool/stats)
pool_api() { cat "$(pool_dir "$1")/api/$2" 2> /dev/null || true; }
pool_connections() { num "$(pool_api "$1" local/p2p)" connections; }
pool_sidechain_height() { num "$(pool_api "$1" pool/stats)" sidechainHeight; }
pool_connected() { local c; c=$(pool_connections "$1"); [ -n "$c" ] && [ "$c" -ge 1 ]; }
pool_api_ready() { [ -s "$(pool_dir "$1")/api/local/p2p" ] && [ -s "$(pool_dir "$1")/api/network/stats" ]; }  # local/stratum only appears once a miner connects
pool_has() { log_has "$(pool_log "$1")" "$2"; }
pool_count() { local c; c=$(log_count "$(pool_log "$1")" "$2"); echo "${c:-0}"; }
# pool_count_re <node> <extended regex> [first line]: matching lines from line <first> on
pool_count_re() {
    { tail -n +"${3:-1}" "$(pool_log "$1")" 2> /dev/null | grep -acE -- "$2" || true; } | head -n 1
}
pool_lines() {
    local f
    f=$(pool_log "$1")
    if [ -f "$f" ]; then wc -l < "$f" | tr -d ' '; else echo 0; fi
}
# first matching line (for messages)
pool_first() { { grep -aE -m 1 -- "$2" "$(pool_log "$1")" 2> /dev/null || true; } | head -n 1 | cut -c1-220; }

# pool_templates <node>: one "height major" line per "new miner data" block of the log
# (p2pool prints "major_version = N" and then "height = H", one value per line)
pool_templates() {
    awk '/^major_version[ ]+= / {m = $NF} /^height[ ]+= / && m != "" {print $NF, m; m = ""}' \
        "$(pool_log "$1")" 2> /dev/null || true
}
# pool_template_seen <node> <major>: some template of that major version was built
pool_template_seen() { pool_templates "$1" | awk -v m="$2" '$2 == m {f = 1} END {exit !f}'; }

# pool_shares <node> [first line]: sidechain shares this node's miners found ("SHARE FOUND";
# NOT the "found a mainchain block" lines: with block difficulty 1 every stratum submission is
# a block, only one hash in ~sidechain-difficulty is also a share)
pool_shares() { pool_count_re "$1" 'SHARE FOUND' "${2:-1}"; }
# pool_peer_shares <node> [first line]: shares of other nodes this node added to its sidechain
# (add_block logs "verified = 0" for a received share, "verified = 1" for its own)
pool_peer_shares() { pool_count_re "$1" 'SideChain add_block: .*verified = 0' "${2:-1}"; }

# heights of the blocks p2pool reports as accepted by the daemon
pool_accepted_heights() {
    { grep -ao "submit_block: BLOCK ACCEPTED at height [0-9]*" "$(pool_log "$1")" 2> /dev/null || true; } \
        | awk '{print $NF}'
}

# Lines that must never appear: share/peer rejections, the fork-table mismatch, PoW or view-tag
# errors, a daemon that refused a block, RPC answers p2pool could not use.
P2POOL_BAD_RE='sent an invalid block|daemon wants block major version|wrong view tag|wrong PoW|failed to submit a share|submit_block: daemon returned error|get_miner_data RPC response (is invalid|failed to parse)|Couldn.t download block headers|invalid JSON response|pays out to a wrong'

# ------------------------------------------------------------------ blocks and coinbases

# coinbase_at <rpc> <height>: hash of the block's coinbase (xkrd f_block_json takes a height)
coinbase_at() {
    local r
    r=$(rpc "$1" f_block_json "{\"hash\":\"$2\"}")
    { flat "$r" | grep -o '"transactions":\[{[^}]*}' | grep -o '"hash":"[0-9a-f]*"' || true; } \
        | head -n 1 | cut -d'"' -f4
}
# header_vers <rpc> <height>: "major minor" or ""
header_vers() {
    local r
    r=$(rpc "$1" getblockheaderbyheight "{\"height\":$2}")
    [ -n "$(str "$r" hash)" ] || return 0
    echo "$(num "$r" major_version) $(num "$r" minor_version)"
}
# coinbase_shape <rpc> <height>: "<cb hash> <tx version> <outputs> <tagged 03> <view tags>"
coinbase_shape() {
    local cb j
    cb=$(coinbase_at "$1" "$2")
    [ -n "$cb" ] || return 0
    j=$(tx_json "$1" "$cb")
    echo "$cb $(num "$j" version) $(count "$j" '"amount":[0-9]*,"target":') $(count "$j" '"type":"03"') $(count "$j" '"view_tag":"[0-9a-f][0-9a-f]"')"
}
# owned_indices <tx hash> <balance file...>: the output indices of that transaction the
# wallets' `balance` listed ("... height H tx <hash>:<index>"), one per line, unsorted
owned_indices() {
    local tx=$1
    shift
    { cat "$@" 2> /dev/null | grep -o "tx $tx:[0-9]*" || true; } | cut -d: -f2
}
# scan_field <balance file> <"with view tags"|"skipped by view tag"|"full derivations">
scan_field() {
    { grep '^scan ' "$1" 2> /dev/null | grep -o "[0-9]* $2" || true; } | head -n 1 | cut -d' ' -f1
}
