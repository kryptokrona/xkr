#!/usr/bin/env bash
# 32-p2pool: p2pool (kryptokrona/p2pool 8edf632 + p2pool/patches 0001, 0002, 0003) mines on xkrd through fork 13 and fork 14; optional (needs the image xkr-p2pool:testnet).
#
# docs/testnet-suite.md "32-p2pool" (recipe, assertions). A fresh testnet:
#
#   xkrd A    P2P 127.0.0.1:22898, RPC 22899, --fork13-height F13 --fork14-height F14,
#             --out-peers 0; phase 0 mines PHASE0_BLOCKS blocks with the built-in miner (W0),
#             then A restarts without a miner: from then on every block comes from p2pool
#       ^ Levin, B dials it
#   xkrd B    no P2P listener, RPC 22889, same fork flags, follower
#   p2pool A  container xkr-suite-p2pool-a, --network host, wallet W1, sidechain P2P 22890,
#             stratum 22333, XKR_FORK13_HEIGHT / XKR_FORK14_HEIGHT = F13 / F14,
#             XKR_TESTNET_UNCAPPED_PPLNS = PPLNS_UNCAPPED (patch 0003)
#   p2pool B  container xkr-suite-p2pool-b, wallet W2, P2P 22891, stratum 22334, --addpeers A
#             (P2POOL_B=0: no B; the share exchange checks are skipped)
#   xmrig     containers xkr-suite-xmrig-a / -b (same image), XMRIG_THREADS each, cn-pico
#   optional: C++ fork-13 node (OPTIONAL_CPP=1, image xkr-cpp-testnet:fork13, P2P 22868,
#             RPC 22869) and an unpatched p2pool C (NEGATIVE=1, image xkr-p2pool:unpatched)
#
# Testnet difficulty is 1 and the sidechain's minimum share difficulty 100 000: every share is
# a main-chain block, but not the other way round. Every stratum submission (xmrig's job
# difficulty is about 1000) is a block ("found a mainchain block", several per height until the
# next 2-s get_miner_data poll), so the chain advances about once per poll; a sidechain share
# ("SHARE FOUND") needs ~100 000 hashes, i.e. ~100 s per 1-kH/s xmrig thread. And upstream p2pool
# caps the PPLNS window at twice the main-chain difficulty (= 2 here), i.e. at one share: without
# patch 0003's XKR_TESTNET_UNCAPPED_PPLNS every coinbase pays only the node that built it.
#
# Checks: (1) xkrd serves get_miner_data / get_block_headers_range, deterministically;
# (2) p2pool starts, logs both fork heights, the signalled fork index and (PPLNS_UNCAPPED=1)
# the lifted PPLNS cap, polls get_miner_data, builds a v5 template, the nodes peer, xmrig's
# shares are accepted; (3) the blocks before F13 are p2pool's: major 5, minor EXPECT_MINOR (14
# with 0001 + 0002; a 0001-only build writes 13), version-1 coinbase, accepted by xkrd A
# ("submitted block accepted", none refused); (4) fork 13: F13 is v5, F13 + 1 .. F14 are v6
# with a version-1 coinbase, no peer rejects a v6 share, get_miner_data dedup holds; before the
# restart each node finds >= 1 share and adds the other's (SHARE_WAIT) and both report the same
# sidechain height; with PPLNS_UNCAPPED=1 coinbases pay both W1 and W2 (the shared window),
# with 0 every coinbase pays exactly one; (5) fork 14: F14 + 1 .. are v7 with a version-2
# coinbase whose outputs are all tagged (0x03 + view tag), accepted by A; every template p2pool
# built had the major version of its height; (6) the wallets W1 (+ W2) through A own every
# output of every p2pool coinbase exactly once, the v7 ones through the view-tag fast path
# (which skips an output whose tag is wrong, so full coverage proves every tag); (7) a
# version-2 transaction (W0 -> W2) appears in get_miner_data's tx_backlog and is mined in a
# p2pool v7 block; (8) p2pool A restarts, reloads its share cache without parser errors and
# mines again; (9a) optional: the unpatched p2pool rejects v6 shares (with PPLNS_UNCAPPED=1 it
# also rejects multi-payee shares: run NEGATIVE=1 with PPLNS_UNCAPPED=0 for a clean negative);
# (9b) optional: the C++ fork-13 node accepts p2pool's v5/v6 blocks and stops at F14; (10) xkrd
# B agrees with A on every header; summary table.
#
# Env: F13, F14, GRACE (16, 24, 10), PHASE0_BLOCKS (10), V7_BLOCKS (4), P2POOL_B (1),
# RESTART (1), OPTIONAL_CPP (0), NEGATIVE (0), EXPECT_MINOR (14), XMRIG_THREADS (1),
# XMRIG_ALGO (cn-pico), BLOCK_WAIT (300: longest wait for one block), RESTART_WAIT (180),
# CPP_SLACK (1), PPLNS_UNCAPPED (1), SHARE_WAIT (600: longest wait for both nodes' shares),
# POST_SHARE_BLOCKS (3: blocks mined after the share exchange), P2POOL_IMAGE,
# P2POOL_UNPATCHED_IMAGE, CPP_IMAGE, TIMEOUT, LOG_LEVEL, KEEP=1.
# suite: expected=600 timeout=1800 optional=1
set -euo pipefail
: "${TIMEOUT:=600}"
. "$(dirname "$0")/../lib.sh"
. "$TESTNET_DIR/lib-p2pool.sh"
suite_init "${1:-}"

F13=${F13:-16}
F14=${F14:-24}
GRACE=${GRACE:-10}
PHASE0_BLOCKS=${PHASE0_BLOCKS:-10}
V7_BLOCKS=${V7_BLOCKS:-4}
P2POOL_B=${P2POOL_B:-1}
RESTART=${RESTART:-1}
OPTIONAL_CPP=${OPTIONAL_CPP:-0}
NEGATIVE=${NEGATIVE:-0}
EXPECT_MINOR=${EXPECT_MINOR:-14}
BLOCK_WAIT=${BLOCK_WAIT:-300}
RESTART_WAIT=${RESTART_WAIT:-180}
CPP_SLACK=${CPP_SLACK:-1}
SHARE_WAIT=${SHARE_WAIT:-600}
POST_SHARE_BLOCKS=${POST_SHARE_BLOCKS:-3}
CPP_IMAGE=${CPP_IMAGE:-$CPP_FORK13_IMAGE}
C_CPP=xkr-suite-p2pool-cpp
A_RPC=127.0.0.1:$PORT_PP_A_RPC
B_RPC=127.0.0.1:$PORT_PP_B_RPC
CPP_RPC=127.0.0.1:$PORT_PP_CPP_RPC

# The image first: without it the scenario is skipped by the suite (optional=1).
require_p2pool_image
require_xkrd
require_testwallet
require_cmd curl awk
[ "$F13" -lt "$F14" ] || die 0 "F13 ($F13) must be below F14 ($F14)"
[ "$PHASE0_BLOCKS" -ge 10 ] || die 0 "PHASE0_BLOCKS ($PHASE0_BLOCKS) must be >= 10 (p2pool's fork table and the testnet daemon disagree below height 4)"
[ "$F13" -ge $((PHASE0_BLOCKS + 4)) ] || die 0 "F13 ($F13) must be at least PHASE0_BLOCKS + 4 = $((PHASE0_BLOCKS + 4)) (p2pool must mine v5 blocks first)"

POOLS=(a)
[ "$P2POOL_B" = 1 ] && POOLS+=(b)
USE_NEG=0
if [ "$NEGATIVE" = 1 ]; then
    [ "$PPLNS_UNCAPPED" = 1 ] && note "NEGATIVE=1 with PPLNS_UNCAPPED=1: the unpatched C also rejects shares whose PPLNS split it computes capped; PPLNS_UNCAPPED=0 gives a clean 9a"
    if image_present "$P2POOL_UNPATCHED_IMAGE"; then USE_NEG=1; else
        note "NEGATIVE=1 but $P2POOL_UNPATCHED_IMAGE is missing (build: $BUILD_P2POOL_UNPATCHED); 9a is skipped"
    fi
fi
USE_CPP=0
if [ "$OPTIONAL_CPP" = 1 ]; then
    if image_present "$CPP_IMAGE"; then USE_CPP=1; else
        note "OPTIONAL_CPP=1 but $CPP_IMAGE is missing (build: $BUILD_FORK13_IMAGE); 9b is skipped"
    fi
fi

reclaim pp-a pp-b
for n in a b c; do
    rm_container "$(pool_container "$n")"
    rm_container "$(xmrig_container "$n")"
done
rm_container "$C_CPP"
require_ports_free "$PORT_PP_A_P2P" "$PORT_PP_A_RPC" "$PORT_PP_B_RPC" "$PORT_PP_POOL_A" "$PORT_PP_STRATUM_A" \
    "$PORT_PP_POOL_B" "$PORT_PP_STRATUM_B" "$PORT_PP_POOL_C" "$PORT_PP_STRATUM_C" "$PORT_PP_CPP_P2P" "$PORT_PP_CPP_RPC"

# expected block major version at a height (a fork at F governs F + 1, ...)
want_major() {
    if [ "$1" -le "$F13" ]; then echo 5; elif [ "$1" -le "$F14" ]; then echo 6; else echo 7; fi
}
# every started p2pool / xmrig container must stay up while we wait
check_pools_alive() {
    local n c
    for n in "${POOLS[@]}"; do
        for c in "$(pool_container "$n")" "$(xmrig_container "$n")"; do
            container_exists "$c" || continue
            if ! container_running "$c"; then
                docker logs --tail 20 "$c" 2>&1 || true
                die alive "container $c exited unexpectedly (log $(pool_log "$n"))"
            fi
        done
    done
}
# wait_pool_height <height>: A's top >= height; 1 when the chain stalls for BLOCK_WAIT seconds
wait_pool_height() {
    local target=$1 last cur since
    last=$(height_of "$A_RPC")
    since=$(date +%s)
    while :; do
        cur=$(height_of "$A_RPC")
        if [ -n "$cur" ] && [ "$cur" -ge "$target" ]; then return 0; fi
        if [ -n "$cur" ] && [ "$cur" != "$last" ]; then
            last=$cur
            since=$(date +%s)
        fi
        [ $(($(date +%s) - since)) -ge "$BLOCK_WAIT" ] && return 1
        check_alive
        check_pools_alive
        sleep 2
    done
}
stall() { # <check> <target>: the chain did not get there
    local n
    for n in "${POOLS[@]}"; do
        note "p2pool $n: $(pool_count "$n" "new miner data") miner data, $(pool_count "$n" "SHARE FOUND") shares, xmrig accepted $(xmrig_accepted "$n") rejected $(xmrig_rejected "$n")"
    done
    die "$1" "no block for ${BLOCK_WAIT}s: A at $(height_of "$A_RPC"), wanted $2 (p2pool logs $(pool_log a); xmrig too slow? raise XMRIG_THREADS or BLOCK_WAIT)"
}
# vers_range <from> <to> <major> <coinbase version>: every block in range has that major, minor
# EXPECT_MINOR and that coinbase version (tagged outputs exactly for version 2); sets VR_BAD
# (number of bad blocks) and VR_FIRST
vers_range() {
    local from=$1 to=$2 maj=$3 cbv=$4 h v shape cb ver n tagged tags
    VR_BAD=0
    VR_FIRST=""
    for ((h = from; h <= to; h++)); do
        v=$(header_vers "$A_RPC" "$h")
        shape=$(coinbase_shape "$A_RPC" "$h")
        read -r cb ver n tagged tags <<< "$shape"
        local ok=1
        [ "$v" = "$maj $EXPECT_MINOR" ] || ok=0
        [ "${ver:-}" = "$cbv" ] && [ "${n:-0}" -gt 0 ] || ok=0
        if [ "$cbv" = 2 ]; then
            [ "${tagged:-}" = "$n" ] && [ "${tags:-}" = "$n" ] || ok=0
        else
            [ "${tagged:-}" = 0 ] || ok=0
        fi
        if [ "$ok" = 0 ]; then
            VR_BAD=$((VR_BAD + 1))
            [ -n "$VR_FIRST" ] || VR_FIRST="block $h: major/minor [$v], coinbase version ${ver:-?}, $n outputs, $tagged tagged, $tags view tags"
        fi
    done
    [ "$VR_BAD" = 0 ]
}
# bad_lines <node> [first line]: sets BAD_COUNT (P2POOL_BAD_RE lines from that line on) and
# BAD_LINE (the first of them)
bad_lines() {
    local f
    f=$(pool_log "$1")
    BAD_LINE=$({ tail -n +"${2:-1}" "$f" 2> /dev/null | grep -aE -m 1 -- "$P2POOL_BAD_RE" || true; } | cut -c1-200)
    BAD_COUNT=$(pool_count_re "$1" "$P2POOL_BAD_RE" "${2:-1}")
    BAD_COUNT=${BAD_COUNT:-0}
}

# ------------------------------------------------------------------ 1. wallets, xkrd, RPC

W0_DIR=$SD/w0
W1_DIR=$SD/w1
W2_DIR=$SD/w2
W0=$(wallet_addr "$W0_DIR")
W1=$(wallet_addr "$W1_DIR")
W2=$(wallet_addr "$W2_DIR")
[ -n "$W0" ] && [ -n "$W1" ] && [ -n "$W2" ] || die 1 "xkr-testwallet did not create the wallets"
pass 1 "wallets W0 (phase-0 miner) $W0, W1 (p2pool A) $W1, W2 (p2pool B) $W2"
echo "fork 13 at F13=$F13 (first v6 block $((F13 + 1))), fork 14 at F14=$F14 (first v7 block $((F14 + 1))), grace $GRACE; image $P2POOL_IMAGE"

FORK_ARGS=(--fork13-height "$F13" --fork14-height "$F14" --fork14-grace "$GRACE" --no-hyper --out-peers 0)
A_ARGS=(--p2p-bind-ip 127.0.0.1 --p2p-bind-port "$PORT_PP_A_P2P")
start_xkrd pp-a "$PORT_PP_A_RPC" "$SD/xkrd-a" "$SD/xkrd-a-phase0.log" "${FORK_ARGS[@]}" "${A_ARGS[@]}" \
    --mine "$W0" --mine-threads 1
watch pp-a
wait_height "$A_RPC" "$PHASE0_BLOCKS" 180 || die 1 "phase 0: A did not mine $PHASE0_BLOCKS blocks within 180 s"
stop_proc pp-a
unwatch
start_xkrd pp-a "$PORT_PP_A_RPC" "$SD/xkrd-a" "$SD/xkrd-a.log" "${FORK_ARGS[@]}" "${A_ARGS[@]}"
H0=$(height_of "$A_RPC")
[ -n "$H0" ] && [ "$H0" -le $((F13 - 3)) ] \
    || die 1 "phase 0 overshot: A at [$H0], F13=$F13 (raise F13 and F14)"
start_xkrd pp-b "$PORT_PP_B_RPC" "$SD/xkrd-b" "$SD/xkrd-b.log" "${FORK_ARGS[@]}" \
    --p2p-bind-port 0 --add-peer "127.0.0.1:$PORT_PP_A_P2P"
watch pp-a pp-b
if [ "$USE_CPP" = 1 ]; then
    start_cpp "$C_CPP" "$CPP_IMAGE" "$PORT_PP_CPP_P2P" "$PORT_PP_CPP_RPC" "XKR_FORK13_HEIGHT=$F13" \
        --add-exclusive-node="127.0.0.1:$PORT_PP_A_P2P"
fi
pass 1 "phase 0: A mined to $H0 with the built-in miner and restarted without it; B follows$([ "$USE_CPP" = 1 ] && echo ", C++ node $C_CPP started")"

md=$(rpc "$A_RPC" get_miner_data)
if [ -z "$(num "$md" height)" ]; then
    missing_exit rpc "xkrd A does not serve get_miner_data ($(flat "$md" | cut -c1-120))" "$BUILD_XKRD (crates/xkr-rpc/src/mining.rs)"
fi
hr=$(rpc "$A_RPC" get_block_headers_range "{\"start_height\":0,\"end_height\":$H0}")
nh=$(count "$hr" '"hash":"')
if [ "$nh" = 0 ]; then
    missing_exit rpc "xkrd A does not serve get_block_headers_range ($(flat "$hr" | cut -c1-120))" "$BUILD_XKRD (crates/xkr-rpc/src/mining.rs)"
fi
mdiff=$(str "$md" difficulty)
if [ "$(num "$md" height)" = $((H0 + 1)) ] && [ "$(num "$md" major_version)" = 5 ] \
    && [ "$(num "$md" unlock_window)" = 1 ] && [ "$(str "$md" prev_id)" = "$(top_hash_of "$A_RPC")" ] \
    && [[ $mdiff =~ ^[0-9a-f]+$ ]] && [ "$nh" = $((H0 + 1)) ]; then
    pass 1 "get_miner_data: height $((H0 + 1)) = top + 1, major 5, prev_id = top hash, difficulty \"$mdiff\" (hex), unlock_window 1; get_block_headers_range 0..$H0: $nh headers"
else
    fail 1 "get_miner_data/headers: $(flat "$md" | cut -c1-260); range 0..$H0 returned $nh headers"
fi
if [ "$(rpc "$A_RPC" get_miner_data)" = "$(rpc "$A_RPC" get_miner_data)" ]; then
    pass 1 "two get_miner_data answers on an unchanged chain are byte-identical (p2pool's dedup)"
else
    fail 1 "get_miner_data changes between polls without a new block"
fi

# ------------------------------------------------------------------ 2. p2pool start-up

t_start=$(date +%s)
start_p2pool a "$P2POOL_IMAGE" "$W1" "$PORT_PP_POOL_A" "$PORT_PP_STRATUM_A"
wait_for 120 pool_api_ready a || die 2 "p2pool A wrote no local API within 120 s (log $(pool_log a): $(pool_first a 'RPC response|Couldn|error'))"
if [ "$P2POOL_B" = 1 ]; then
    start_p2pool b "$P2POOL_IMAGE" "$W2" "$PORT_PP_POOL_B" "$PORT_PP_STRATUM_B" --addpeers "127.0.0.1:$PORT_PP_POOL_A"
    wait_for 120 pool_api_ready b || die 2 "p2pool B wrote no local API within 120 s (log $(pool_log b))"
fi
if [ "$USE_NEG" = 1 ]; then
    start_p2pool c "$P2POOL_UNPATCHED_IMAGE" "$W2" "$PORT_PP_POOL_C" "$PORT_PP_STRATUM_C" --addpeers "127.0.0.1:$PORT_PP_POOL_A"
fi
start_xmrig a "$PORT_PP_STRATUM_A"
[ "$P2POOL_B" = 1 ] && start_xmrig b "$PORT_PP_STRATUM_B"

for n in "${POOLS[@]}"; do
    l13="XKR fork 13 (block major version 6) at height $F13, signalling fork index $EXPECT_MINOR"
    l14="XKR fork 14 (block major version 7, version-2 coinbase with view tags) at height $F14"
    if pool_has "$n" "$l13" && pool_has "$n" "$l14"; then
        pass 2 "p2pool $n: \"$l13\", \"$l14\""
    else
        fail 2 "p2pool $n: fork lines missing: [$(pool_first "$n" 'XKR fork 13')] [$(pool_first "$n" 'XKR fork 14')] (heights from the environment need \"testnet\": true in get_info)"
    fi
    if [ "$PPLNS_UNCAPPED" = 1 ]; then
        if pool_has "$n" "$PPLNS_UNCAPPED_LINE"; then
            pass 2 "p2pool $n: \"$PPLNS_UNCAPPED_LINE\" (patch 0003, XKR_TESTNET_UNCAPPED_PPLNS=1)"
        else
            fail 2 "p2pool $n did not log \"$PPLNS_UNCAPPED_LINE\": image $P2POOL_IMAGE without patch 0003? (rebuild: $BUILD_P2POOL_IMAGE; or PPLNS_UNCAPPED=0)"
        fi
    elif pool_has "$n" "$PPLNS_UNCAPPED_LINE"; then
        fail 2 "p2pool $n lifted the PPLNS cap although PPLNS_UNCAPPED=0"
    fi
    if wait_for 60 pool_template_seen "$n" 5 && [ "$(pool_count "$n" "get_miner_data RPC response")" = 0 ]; then
        pass 2 "p2pool $n polls get_miner_data: $(pool_count "$n" "new miner data") \"new miner data\", template major 5 at height $((H0 + 1))"
    else
        fail 2 "p2pool $n: no major-5 template; $(pool_count "$n" "new miner data") miner data, [$(pool_first "$n" 'get_miner_data RPC response')]"
    fi
done
if [ "$P2POOL_B" = 1 ]; then
    if wait_for 60 pool_connected a && wait_for 60 pool_connected b; then
        pass 2 "sidechain peers: p2pool A $(pool_connections a) connection(s), B $(pool_connections b)"
    else
        fail 2 "p2pool A/B not peered: connections A [$(pool_connections a)] B [$(pool_connections b)]"
    fi
else
    skip 2 "P2POOL_B=0: no second p2pool node, no sidechain peering"
fi

wait_pool_height $((H0 + 1)) || stall 2 $((H0 + 1))
pass 2 "first p2pool block at height $((H0 + 1)) after $(($(date +%s) - t_start)) s"

# ------------------------------------------------------------------ 3. blocks before fork 13

wait_pool_height $((F13 + 1)) || stall 3 $((F13 + 1))
if vers_range $((H0 + 1)) "$F13" 5 1; then
    pass 3 "blocks $((H0 + 1))..$F13 (p2pool's): major 5, minor $EXPECT_MINOR, version-1 coinbase with untagged outputs"
else
    fail 3 "$VR_BAD of the blocks $((H0 + 1))..$F13 are not v5/minor $EXPECT_MINOR/coinbase v1; first: $VR_FIRST"
fi
acc=$(log_count "$SD/xkrd-a.log" "submitted block accepted")
ref=$(log_count "$SD/xkrd-a.log" "submitted block refused")
if [ "${acc:-0}" -ge $((F13 - H0)) ] && [ "${ref:-0}" = 0 ]; then
    pass 3 "xkrd A: $acc \"submitted block accepted\", $ref refused (p2pool's submitblock)"
else
    fail 3 "xkrd A: $acc accepted (want >= $((F13 - H0))), $ref refused: $({ grep -a -m 1 'submitted block refused' "$SD/xkrd-a.log" || true; } | cut -c1-200)"
fi

# ------------------------------------------------------------------ 4. fork 13

wait_pool_height $((F14 + 1)) || stall 4 $((F14 + 1))
v13=$(header_vers "$A_RPC" "$F13")
if [ "${v13%% *}" = 5 ] && vers_range $((F13 + 1)) "$F14" 6 1; then
    pass 4 "block $F13 is v5; blocks $((F13 + 1))..$F14 are major 6, minor $EXPECT_MINOR, version-1 coinbase (p2pool's v6 blocks accepted by A)"
else
    fail 4 "block $F13 [$v13]; $VR_BAD bad v6 blocks in $((F13 + 1))..$F14; first: $VR_FIRST"
fi
for n in "${POOLS[@]}"; do
    if pool_template_seen "$n" 6; then
        pass 4 "p2pool $n built a major-6 template (\"major_version = 6\")"
    else
        fail 4 "p2pool $n never logged a major-6 template"
    fi
done
if [ "$P2POOL_B" = 1 ]; then
    sa=$(pool_sidechain_height a); sb=$(pool_sidechain_height b)
    if [ -n "$sa" ] && [ -n "$sb" ] && [ $((sa > sb ? sa - sb : sb - sa)) -le 2 ] && pool_connected a; then
        pass 4 "still peered after fork 13: sidechainHeight A $sa, B $sb (0 = no share yet; the share exchange is checked before the restart), A has $(pool_connections a) peer(s)"
    else
        fail 4 "sidechains apart after fork 13: sidechainHeight A [$sa] B [$sb], connections A [$(pool_connections a)]"
    fi
else
    skip 4 "P2POOL_B=0: no sidechain to compare"
fi
nblocks=$(($(height_of "$A_RPC") - H0))
for n in "${POOLS[@]}"; do
    nmd=$(pool_count "$n" "new miner data")
    if [ "$nmd" -le $((2 * nblocks + 10)) ]; then
        pass 4 "p2pool $n dedup: $nmd \"new miner data\" for $nblocks blocks (no rebuild per 2-s poll)"
    else
        fail 4 "p2pool $n: $nmd \"new miner data\" for $nblocks blocks: the get_miner_data body changes between polls"
    fi
done

# ------------------------------------------------------------------ 7a. a version-2 transaction

# Sent as soon as fork 14 is active (tx_versions [1,2]: the wallet's default is 2), from W0 so
# that the p2pool wallets never spend and the coverage check (6) sees every output.
TXV2=""; HV2=""; IN_BACKLOG=0
if TXV2=$(send_tx "$W0_DIR" "$A_RPC" "$W2" v2 1.0) && [ -n "$TXV2" ] && grep -q '^version 2 ' "$SD/send-v2.out"; then
    pass 7 "W0 -> W2 1.0 XKR, version 2, at height $(height_of "$A_RPC"): $TXV2"
    in_backlog() { flat "$(rpc "$A_RPC" get_miner_data)" | grep -q "\"id\":\"$TXV2\""; }
    mined_now() { tx_in_block "$A_RPC" "$TXV2"; }
    if in_backlog; then
        IN_BACKLOG=1
    elif ! mined_now; then
        wait_for 30 in_backlog && IN_BACKLOG=1
    fi
else
    fail 7 "version-2 send W0 -> W2 failed or was not version 2: $(tail -n 3 "$SD/send-v2.out" 2> /dev/null | tr '\n' ' ')"
    TXV2=""
fi

# ------------------------------------------------------------------ 5. fork 14

wait_pool_height $((F14 + V7_BLOCKS)) || stall 5 $((F14 + V7_BLOCKS))
T1=$(height_of "$A_RPC")
v14=$(header_vers "$A_RPC" "$F14")
s14=$(coinbase_shape "$A_RPC" "$F14")
if [ "${v14%% *}" = 6 ] && [ "$(echo "$s14" | awk '{print $2, $4}')" = "1 0" ]; then
    pass 5 "block $F14 is v6 with a version-1 coinbase"
else
    fail 5 "block $F14: [$v14], coinbase [$s14] (want v6, coinbase version 1, no tagged outputs)"
fi
if vers_range $((F14 + 1)) "$T1" 7 2; then
    pass 5 "blocks $((F14 + 1))..$T1: major 7, minor $EXPECT_MINOR, version-2 coinbase with only tagged outputs (0x03 + view tag), accepted by A"
else
    fail 5 "$VR_BAD bad v7 blocks in $((F14 + 1))..$T1; first: $VR_FIRST"
fi
v7acc=0
for n in "${POOLS[@]}"; do
    c=$(pool_accepted_heights "$n" | awk -v f="$F14" '$1 > f' | wc -l | tr -d ' ')
    v7acc=$((v7acc + c))
done
ref=$(log_count "$SD/xkrd-a.log" "submitted block refused")
if [ "$v7acc" -ge 1 ] && [ "${ref:-0}" = 0 ]; then
    pass 5 "p2pool logged $v7acc \"submit_block: BLOCK ACCEPTED\" above F14; xkrd A refused none"
else
    fail 5 "BLOCK ACCEPTED above F14: $v7acc; xkrd A refused $ref"
fi
tbad=0; tfirst=""; seen=""
for n in "${POOLS[@]}"; do
    while read -r h m; do
        [ -n "$h" ] || continue
        [ "$h" -gt "$H0" ] || continue
        seen="$seen $m"
        if [ "$m" != "$(want_major "$h")" ]; then
            tbad=$((tbad + 1))
            [ -n "$tfirst" ] || tfirst="p2pool $n: template for height $h has major $m, want $(want_major "$h")"
        fi
    done < <(pool_templates "$n")
done
if [ "$tbad" = 0 ] && [[ $seen == *5* ]] && [[ $seen == *6* ]] && [[ $seen == *7* ]]; then
    pass 5 "every p2pool template had its height's major version (5 up to $F13, 6 up to $F14, 7 after)"
else
    fail 5 "templates: $tbad with the wrong major version (${tfirst:-none}); majors seen:$(echo "$seen" | tr ' ' '\n' | sort -u | tr '\n' ' ')"
fi
for n in "${POOLS[@]}"; do
    bad_lines "$n"
    if [ "$BAD_COUNT" = 0 ]; then
        pass 5 "p2pool $n log: no invalid share, fork-table mismatch, wrong view tag, PoW or submit error"
    else
        fail 5 "p2pool $n log: $BAD_COUNT bad line(s), first: $BAD_LINE"
    fi
done

# ------------------------------------------------------------------ 7b. the v2 transaction mined

if [ -n "$TXV2" ]; then
    if HV2=$(wait_mined "$A_RPC" "$TXV2" 180) && [ -n "$HV2" ]; then
        hv=$(header_vers "$A_RPC" "$HV2"); hs=$(coinbase_shape "$A_RPC" "$HV2")
        if [ "$HV2" -gt "$F14" ] && [ "${hv%% *}" = 7 ] && [ "$(echo "$hs" | awk '{print $2}')" = 2 ]; then
            pass 7 "v2 transaction mined in p2pool's v7 block $HV2 (coinbase version 2); in tx_backlog before: $([ "$IN_BACKLOG" = 1 ] && echo yes || echo "not observed (mined within one poll)")"
        else
            fail 7 "v2 transaction mined at $HV2: block [$hv], coinbase [$hs]"
        fi
        [ "$IN_BACKLOG" = 1 ] || note "the transaction was mined before a get_miner_data poll saw it"
    else
        HV2=""
        fail 7 "v2 transaction $TXV2 not mined within 180 s (in tx_backlog: $IN_BACKLOG): p2pool does not include backlog transactions?"
    fi
fi

# ------------------------------------------------------------------ 4. shares exchanged

# Shares are rare here (one per ~100 000 hashes, ~100 s per xmrig thread) while blocks come
# every poll, so the chain may pass F14 before the first share. Wait (before the restart, so
# that A's cache holds shares) until each node has found one and added the other's.
SH_H=""
if [ "$P2POOL_B" = 1 ]; then
    shares_both() {
        [ "$(pool_shares a)" -ge 1 ] && [ "$(pool_shares b)" -ge 1 ] \
            && [ "$(pool_peer_shares a)" -ge 1 ] && [ "$(pool_peer_shares b)" -ge 1 ]
    }
    t_sh=$(date +%s)
    while ! shares_both && [ $(($(date +%s) - t_sh)) -lt "$SHARE_WAIT" ]; do
        check_alive
        check_pools_alive
        sleep 5
    done
    same_sidechain() {
        local x y
        x=$(pool_sidechain_height a); y=$(pool_sidechain_height b)
        [ -n "$x" ] && [ "$x" = "$y" ]
    }
    sh_msg() {
        echo "A found $(pool_shares a) share(s) and added $(pool_peer_shares a) of B's, B found $(pool_shares b) and added $(pool_peer_shares b) of A's, sidechainHeight A $(pool_sidechain_height a) B $(pool_sidechain_height b), after $(($(date +%s) - t_sh)) s at height $(height_of "$A_RPC")"
    }
    if shares_both && wait_for 60 same_sidechain; then
        SH_H=$(height_of "$A_RPC")
        pass 4 "shares exchanged: $(sh_msg)"
        wait_pool_height $((SH_H + POST_SHARE_BLOCKS)) || stall 4 $((SH_H + POST_SHARE_BLOCKS))
    else
        fail 4 "share exchange incomplete within ${SHARE_WAIT}s: $(sh_msg) (xmrig too slow for sidechain difficulty 100 000? raise XMRIG_THREADS or SHARE_WAIT)"
    fi
else
    skip 4 "P2POOL_B=0: no share exchange"
fi

# ------------------------------------------------------------------ 8. restart p2pool A

if [ "$RESTART" = 1 ]; then
    from=$(($(pool_lines a) + 1))
    hr0=$(height_of "$A_RPC")
    nice -n 19 docker restart -t 15 "$(pool_container a)" > /dev/null 2>&1 || fail 8 "docker restart of p2pool A failed"
    loaded() { [ "$(pool_count_re a 'loaded [0-9]+ cached blocks' "$from")" -ge 1 ]; }
    shared() { [ "$(pool_count_re a 'SHARE FOUND|BLOCK ACCEPTED' "$from")" -ge 1 ]; }
    if wait_for 90 loaded; then
        line=$({ tail -n +"$from" "$(pool_log a)" | grep -aoE -m 1 'loaded [0-9]+ cached blocks' || true; } | head -n 1)
        nl=$(echo "$line" | awk '{print $2}')
        if [ "${nl:-0}" -gt 0 ]; then
            pass 8 "p2pool A restarted: \"$line\" (v5/v6/v7 shares from its cache)"
        else
            fail 8 "p2pool A restarted but \"$line\""
        fi
    else
        fail 8 "p2pool A logged no \"loaded N cached blocks\" within 90 s of the restart"
    fi
    if wait_for "$RESTART_WAIT" shared; then
        pass 8 "p2pool A mines again after the restart ($(pool_count_re a 'BLOCK ACCEPTED' "$from") block(s), $(pool_count_re a 'SHARE FOUND' "$from") share(s), A at $(height_of "$A_RPC"), was $hr0)"
    else
        fail 8 "p2pool A found no share within ${RESTART_WAIT}s of the restart (xmrig reconnected? $(xmrig_accepted a) accepted in total)"
    fi
    if [ "$P2POOL_B" = 1 ]; then
        if wait_for 60 pool_connected a; then
            pass 8 "p2pool A reconnected to B ($(pool_connections a) connection(s))"
        else
            fail 8 "p2pool A has no sidechain peer after the restart"
        fi
    fi
    bad_lines a "$from"
    if [ "$BAD_COUNT" = 0 ]; then
        pass 8 "no parser or share errors in A's log since the restart"
    else
        fail 8 "$BAD_COUNT bad line(s) since the restart, first: $BAD_LINE"
    fi
else
    skip 8 "RESTART=0"
fi

# ------------------------------------------------------------------ stop mining

for n in "${POOLS[@]}"; do
    xa=$(xmrig_accepted "$n"); xr=$(xmrig_rejected "$n")
    if [ "${xa:-0}" -ge 1 ] && [ "${xr:-0}" = 0 ]; then
        pass 2 "xmrig $n: $xa share(s) accepted by p2pool $n, $xr rejected"
    else
        fail 2 "xmrig $n: $xa accepted, $xr rejected (docker logs $(xmrig_container "$n"))"
    fi
    stop_xmrig "$n"
done
sleep 5
T=$(height_of "$A_RPC")

# ------------------------------------------------------------------ 6. wallets own every output

BAL_W1=$SD/balance-w1.out
BAL_W2=$SD/balance-w2.out
wallet "$W1_DIR" "$A_RPC" balance > "$BAL_W1" 2>&1 || true
wallet "$W2_DIR" "$A_RPC" balance > "$BAL_W2" 2>&1 || true
CV_FIRST=""; CV_BAD_PRE=0; CV_BAD_V7=0; CV_PRE=0; CV_V7=0; BOTH=0; OUT_W1=0; OUT_W2=0
BOTH_LATE=0; LATE=0; ONE=0
declare -A CV_ROW=()
for ((h = H0 + 1; h <= T; h++)); do
    shape=$(coinbase_shape "$A_RPC" "$h")
    read -r cb _ n _ _ <<< "$shape"
    idx=$(owned_indices "${cb:-x}" "$BAL_W1" "$BAL_W2")
    tot=$(printf '%s\n' "$idx" | grep -c . || true)
    uni=$({ printf '%s\n' "$idx" | grep . || true; } | sort -un | wc -l | tr -d ' ')
    mx=$({ printf '%s\n' "$idx" | grep . || true; } | sort -n | tail -n 1)
    w1=$(owned_indices "${cb:-x}" "$BAL_W1" | grep -c . || true)
    w2=$(owned_indices "${cb:-x}" "$BAL_W2" | grep -c . || true)
    OUT_W1=$((OUT_W1 + w1)); OUT_W2=$((OUT_W2 + w2))
    [ "$w1" -gt 0 ] && [ "$w2" -gt 0 ] && BOTH=$((BOTH + 1))
    if { [ "$w1" = 0 ] || [ "$w2" = 0 ]; } && [ $((w1 + w2)) -gt 0 ]; then ONE=$((ONE + 1)); fi
    if [ -n "$SH_H" ] && [ "$h" -gt $((SH_H + 1)) ]; then
        LATE=$((LATE + 1))
        [ "$w1" -gt 0 ] && [ "$w2" -gt 0 ] && BOTH_LATE=$((BOTH_LATE + 1))
    fi
    CV_ROW[$h]="W1 $w1 / W2 $w2"
    ok=0
    if [ -n "$cb" ] && [ "${n:-0}" -gt 0 ] && [ "$tot" = "$n" ] && [ "$uni" = "$n" ] && [ "${mx:-0}" -lt "$n" ]; then ok=1; fi
    if [ "$h" -gt "$F14" ]; then
        CV_V7=$((CV_V7 + 1)); [ "$ok" = 1 ] || CV_BAD_V7=$((CV_BAD_V7 + 1))
    else
        CV_PRE=$((CV_PRE + 1)); [ "$ok" = 1 ] || CV_BAD_PRE=$((CV_BAD_PRE + 1))
    fi
    [ "$ok" = 1 ] || [ -n "$CV_FIRST" ] || CV_FIRST="block $h coinbase ${cb:-?}: $n outputs, wallets own $tot ($uni distinct, max index ${mx:-none}; W1 $w1, W2 $w2)"
done
if [ "$CV_PRE" -gt 0 ] && [ "$CV_BAD_PRE" = 0 ]; then
    pass 6 "v5/v6 p2pool coinbases ($CV_PRE blocks $((H0 + 1))..$F14): every output owned exactly once by W1/W2 (full derivation)"
else
    fail 6 "$CV_BAD_PRE of $CV_PRE pre-fork-14 coinbases not covered; first: $CV_FIRST"
fi
if [ "$CV_V7" -gt 0 ] && [ "$CV_BAD_V7" = 0 ]; then
    pass 6 "v7 p2pool coinbases ($CV_V7 blocks $((F14 + 1))..$T): every tagged output owned exactly once by W1/W2 through the view-tag fast path (every view tag right)"
else
    fail 6 "$CV_BAD_V7 of $CV_V7 v7 coinbases not covered by the wallets (wrong view tag or payee); first: $CV_FIRST"
fi
tg=$(scan_field "$BAL_W1" "with view tags"); sk=$(scan_field "$BAL_W1" "skipped by view tag")
if [ "${tg:-0}" -gt 0 ] && { [ "${sk:-0}" -gt 0 ] || { [ "$P2POOL_B" = 0 ] && [ -z "$HV2" ]; }; }; then
    pass 6 "W1 through A: $OUT_W1 coinbase outputs; $(grep '^scan ' "$BAL_W1" | cut -c10-)"
else
    fail 6 "W1's scan line [$(grep '^scan ' "$BAL_W1" || true)]: want outputs with view tags and some skipped by view tag"
fi
NCB=$((T - H0))
if [ "$P2POOL_B" = 0 ]; then
    :
elif [ "$PPLNS_UNCAPPED" = 1 ]; then
    if [ "$BOTH" -gt 0 ] && { [ -z "$SH_H" ] || [ "$BOTH_LATE" -gt 0 ]; }; then
        pass 4 "shared PPLNS: $BOTH of $NCB coinbases pay both W1 and W2 ($BOTH_LATE of the $LATE after the share exchange at ${SH_H:-?}; W1 $OUT_W1, W2 $OUT_W2 outputs)"
    else
        fail 4 "no coinbase pays both W1 and W2 ($BOTH of $NCB; after the share exchange at ${SH_H:-?}: $BOTH_LATE of $LATE; W1 $OUT_W1, W2 $OUT_W2 outputs): the nodes do not share a PPLNS window (PPLNS cap still on? XKR_TESTNET_UNCAPPED_PPLNS, patch 0003)"
    fi
else
    if [ "$BOTH" = 0 ] && [ "$ONE" = "$NCB" ]; then
        pass 4 "capped PPLNS (PPLNS_UNCAPPED=0): each of the $NCB coinbases pays exactly one of W1/W2 (window = 2x block difficulty 1 = the template's own share)"
    else
        fail 4 "PPLNS_UNCAPPED=0: $BOTH of $NCB coinbases pay both wallets, $ONE pay exactly one (the upstream cap should leave one payee per coinbase)"
    fi
fi
if [ -n "$HV2" ]; then
    if grep -q "tx $TXV2:" "$BAL_W2"; then
        pass 7 "W2's balance through A lists the v2 transaction's output(s)"
    else
        fail 7 "W2's balance through A misses $TXV2 (see $BAL_W2)"
    fi
fi

# ------------------------------------------------------------------ 9. optional nodes

if [ "$USE_NEG" = 1 ]; then
    if pool_has c "sent an invalid block"; then
        pass 9a "unpatched p2pool C rejects the v6/v7 shares: $(pool_first c 'sent an invalid block')"
    else
        fail 9a "unpatched p2pool C logged no \"sent an invalid block\" after fork 13 (log $(pool_log c))"
    fi
else
    skip 9a "negative check off (NEGATIVE=1 and image $P2POOL_UNPATCHED_IMAGE: $BUILD_P2POOL_UNPATCHED)"
fi
if [ "$USE_CPP" = 1 ]; then
    c1=$(top_of "$CPP_RPC"); sleep 15; c2=$(top_of "$CPP_RPC")
    ch=${c2%% *}
    if [ -n "$ch" ] && [ "$ch" -le "$F14" ] && [ "$ch" -ge $((F14 - CPP_SLACK)) ] && [ "$c1" = "$c2" ]; then
        pass 9b "C++ fork-13 node stopped at $ch (F14=$F14)"
    else
        fail 9b "C++ node top [$c1] -> [$c2], want a constant height in [$((F14 - CPP_SLACK)), $F14]"
    fi
    if [ -n "$ch" ] && compare_chains "$A_RPC" "$CPP_RPC" $((H0 + 1)) "$ch"; then
        pass 9b "C++ node accepted p2pool's v5/v6 blocks: headers $((H0 + 1))..$ch identical to A's"
    else
        fail 9b "C++ node differs from A in $((H0 + 1))..${ch:-?}: ${CMP_FIRST:-no answer}"
    fi
else
    skip 9b "C++ fork-13 node off (OPTIONAL_CPP=1 and image $CPP_IMAGE)"
fi

# ------------------------------------------------------------------ 10. B agrees, summary

if wait_same_top "$A_RPC" "$B_RPC" 120; then
    if compare_chains "$A_RPC" "$B_RPC" 1 "$T"; then
        pass 10 "xkrd B follows A: headers 1..$T identical (v5, v6 and v7 p2pool blocks)"
    else
        fail 10 "$CMP_MISMATCHES headers differ between A and B in 1..$T; first: $CMP_FIRST"
    fi
else
    fail 10 "B [$(top_of "$B_RPC")] did not reach A's top [$(top_of "$A_RPC")] within 120 s"
fi

echo
printf '%-7s %-6s %-6s %-9s %-8s %-7s %s\n' height major minor "cb ver" outputs tagged payees
for h in "$F13" $((F13 + 1)) "$F14" $((F14 + 1)) "$T"; do
    read -r maj mnr <<< "$(header_vers "$A_RPC" "$h")"
    read -r _ cv n tg _ <<< "$(coinbase_shape "$A_RPC" "$h")"
    printf '%-7s %-6s %-6s %-9s %-8s %-7s %s\n' "$h" "${maj:-?}" "${mnr:-?}" "${cv:-?}" "${n:-?}" "${tg:-?}" "${CV_ROW[$h]:-?}"
done
echo
echo "phase 0 to $H0, p2pool blocks $((H0 + 1))..$T; v2 transaction ${TXV2:-none} at ${HV2:-?}"
for n in "${POOLS[@]}"; do
    echo "p2pool $n: $(pool_count "$n" "SHARE FOUND") shares, $(pool_accepted_heights "$n" | wc -l | tr -d ' ') BLOCK ACCEPTED, $(pool_count "$n" "new miner data") miner data; log $(pool_log "$n")"
done
echo "fork 13 at $F13, fork 14 at $F14; logs in $SD"
finish
