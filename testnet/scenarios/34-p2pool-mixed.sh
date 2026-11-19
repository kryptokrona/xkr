#!/usr/bin/env bash
# 34-p2pool-mixed: one sidechain of the C++ p2pool and xkrd's built-in p2pool (TCP and Hyperswarm only), through fork 13 and fork 14; optional (needs the image xkr-p2pool:testnet).
#
# docs/testnet-suite.md "34-p2pool-mixed". A fresh testnet; every p2pool node uses
# p2pool/sidechain-testnet.json (consensus id 9b8712b7…53248686, minimum share difficulty
# 100 000, PPLNS window 60) with the PPLNS weight cap lifted (XKR_TESTNET_UNCAPPED_PPLNS=1 for
# C, --p2pool-uncapped-pplns for R1 and R2):
#
#   xkrd X     P2P 127.0.0.1:22798, RPC 22799, --no-hyper --out-peers 0, fork flags; phase 0
#              mines PHASE0_BLOCKS blocks with the built-in miner (W0), then X restarts without
#              it: every later block comes from p2pool
#   p2pool C   C++ (container xkr-suite-p2pool-mixed-c, image xkr-p2pool:testnet), daemon X,
#              wallet WC, sidechain P2P 22790, stratum 22343, fork heights from the environment
#       ^ sidechain TCP: R1 dials C (--p2pool-addpeers)
#   xkrd R1    RPC 22789, Levin to X only (--add-peer, no listener), Hyperswarm on a local DHT,
#              --node --p2pool: stratum 22344, sidechain TCP listener 22791, payout WR1
#              (--hugin-payout)
#       ^ Hyperswarm only: the main chain (Levin) and the sidechain topic
#   xkrd R2    RPC 22779, no legacy peer at all (--p2p-bind-port 0, dead seed, --out-peers 0),
#              --node --p2pool: stratum 22345, no TCP listener, no --p2pool-addpeers, payout WR2
#   miners     xmrig (container xkr-suite-xmrig-mixed-c) on C's stratum; xkr-miner on R1's
#              stratum, and on R2's with R2_MINER=1
#
# Testnet difficulty is 1: every stratum submission (job difficulty 1000) is a main-chain block,
# a sidechain share needs ~100 000 hashes. C reads new main-chain blocks from X's
# get_miner_data every 2 s, xkrd's p2pool on every new block. Both implementations drop a
# broadcast share whose main-chain parent is 2 or more blocks behind (5 or more: the peer is
# banned), so a main chain moving several blocks per C poll would make C's shares stale on
# arrival. The xkr-miners therefore pause R_MINER_PAUSE seconds (SIGSTOP / SIGCONT) after each
# accepted submission, which keeps xkrd's blocks at about one per R_MINER_PAUSE seconds; a
# share of R1 then takes ~100 x R_MINER_PAUSE seconds.
#
# Checks: (1) wallets; phase 0 on X; R1 reaches X's top over Levin, R2 over Hyperswarm only;
# both p2pools run. (2) One sidechain: C logs "consensus ID = 9b8712b7…53248686" (masked
# middle), R1 and R2 log "p2pool: consensus id 9b8712b7…53248686" and report it in /status;
# all three lifted the PPLNS cap. (3) Peers: R1 has C as an outgoing TCP peer and R2 as a
# Hyperswarm peer, C has a connection, R2 has a Hyperswarm peer and no TCP peer (peers_out =
# peers_in = 0) and no legacy main-chain peer. (4) Forks: every block after phase 0 has its
# height's major version (5 up to F13, 6 up to F14, 7 after), from F14 + 1 a version-2 coinbase
# with only tagged outputs; X refused no submitted block. (5) Shares cross both implementations
# and both transports: shares C found are added by R1 and R2, R1's by C and R2, R2's (R2_MINER=1)
# by R1 and C. (6) The sidechains agree: heights within SC_SLACK of each other, a share at the
# lowest of the three heights known to all three, R1 and R2 logged "p2pool: synchronized".
# (7) Blocks found by p2pool: C logs "BLOCK ACCEPTED", R1's "p2pool: block found" blocks are on
# X's chain (same hash at that height). (8) PPLNS payouts: the wallets WC, WR1 and WR2 through X
# own every output of every p2pool coinbase exactly once (v7: through the view-tag fast path);
# after the share exchange a coinbase built by xkrd and one built by C pay both WC and WR1 (and
# WR2 once its share is in). (9) No rejection or ban between the nodes, the miners' shares
# accepted and none rejected, X, R1 and R2 have the same headers; summary table.
#
# Env: F13, F14, GRACE (16, 24, 10), PHASE0_BLOCKS (10), V7_BLOCKS (4), R2_MINER (0),
# R_MINER_THREADS (1), R_MINER_PAUSE (2), SHARE_WAIT (1200: longest wait for the share
# exchange), POST_SHARE_BLOCKS (5), SC_SLACK (1), BLOCK_WAIT (300), XMRIG_THREADS, XMRIG_ALGO,
# P2POOL_IMAGE, P2POOL_LOGLEVEL, TIMEOUT, LOG_LEVEL, KEEP=1.
# suite: expected=900 timeout=2700 optional=1
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
R2_MINER=${R2_MINER:-0}
R_MINER_THREADS=${R_MINER_THREADS:-1}
R_MINER_PAUSE=${R_MINER_PAUSE:-2}
SHARE_WAIT=${SHARE_WAIT:-1200}
POST_SHARE_BLOCKS=${POST_SHARE_BLOCKS:-5}
SC_SLACK=${SC_SLACK:-1}
BLOCK_WAIT=${BLOCK_WAIT:-300}
# every node of the sidechain must agree on the PPLNS cap; C's container reads PPLNS_UNCAPPED
PPLNS_UNCAPPED=1
# C's daemon is X (start_p2pool)
P2POOL_RPC_PORT=$PORT_PM_X_RPC
C=mixed-c
X_RPC=127.0.0.1:$PORT_PM_X_RPC
R1_RPC=127.0.0.1:$PORT_PM_R1_RPC
R2_RPC=127.0.0.1:$PORT_PM_R2_RPC
X_LOG=$SD/xkrd-x.log
R1_LOG=$SD/xkrd-r1.log
R2_LOG=$SD/xkrd-r2.log
M1_LOG=$SD/xkr-miner-r1.log
M2_LOG=$SD/xkr-miner-r2.log
SIDECHAIN_JSON=$ROOT/p2pool/sidechain-testnet.json
CID_HEAD=9b8712b7
CID_TAIL=53248686

# The image first: without it the scenario is skipped by the suite (optional=1).
require_p2pool_image
require_xkrd
require_hyper
require_node
require_bridge
require_testwallet
require_miner
require_cmd curl awk sed sort comm
[ -f "$SIDECHAIN_JSON" ] || missing_exit prereq "$SIDECHAIN_JSON missing" "git checkout p2pool/sidechain-testnet.json"
[ "$F13" -lt "$F14" ] || die 0 "F13 ($F13) must be below F14 ($F14)"
[ "$PHASE0_BLOCKS" -ge 10 ] || die 0 "PHASE0_BLOCKS ($PHASE0_BLOCKS) must be >= 10 (p2pool's fork table and the testnet daemon disagree below height 4)"
[ "$F13" -ge $((PHASE0_BLOCKS + 4)) ] || die 0 "F13 ($F13) must be at least PHASE0_BLOCKS + 4 = $((PHASE0_BLOCKS + 4)) (p2pool must mine v5 blocks first)"

MINERS=(pm-miner-r1)
[ "$R2_MINER" = 1 ] && MINERS+=(pm-miner-r2)
reclaim pm-miner-r1 pm-miner-r2 pm-r2 pm-r1 pm-x pm-dht
rm_container "$(pool_container "$C")"
rm_container "$(xmrig_container "$C")"
require_ports_free "$PORT_PM_X_P2P" "$PORT_PM_X_RPC" "$PORT_PM_R1_RPC" "$PORT_PM_R2_RPC" \
    "$PORT_PM_POOL_C" "$PORT_PM_STRATUM_C" "$PORT_PM_POOL_R1" "$PORT_PM_STRATUM_R1" "$PORT_PM_STRATUM_R2"

# ------------------------------------------------------------------ helpers

# expected block major version at a height (a fork at F governs F + 1, ...)
want_major() {
    if [ "$1" -le "$F13" ]; then echo 5; elif [ "$1" -le "$F14" ]; then echo 6; else echo 7; fi
}
# /status p2pool object of an xkrd, one number or string of it
r_p2pool() { obj "$(get "$1" /status)" p2pool; }
r_num() { num "$(r_p2pool "$1")" "$2"; }
r_str() { str "$(r_p2pool "$1")" "$2"; }
r_running() { [ "$(r_str "$1" state)" = running ]; }

# c_shares [0|1]: "height id" of C's add_block lines (verified = 0: a peer's share, 1: its own)
c_shares() {
    { grep -aoE "add_block: height = [0-9]+, id = [0-9a-f]{64}, mainchain height = [0-9]+, verified = ${1:-[01]}" \
        "$(pool_log "$C")" 2> /dev/null || true; } | awk '{gsub(",", ""); print $4, $7}'
}
# r_shares <xkrd log> <found|added|any>: "height id" of the "p2pool: share found/added" lines
r_shares() {
    local what=$2
    [ "$what" = any ] && what='(found|added)'
    { grep -aoE "p2pool: share $what height=[0-9]+ (diff=[0-9]+ )?id=[0-9a-f]{64}" "$1" 2> /dev/null || true; } \
        | sed -E 's/.* height=([0-9]+) .*id=([0-9a-f]{64})$/\1 \2/'
}
# r_blocks <xkrd log>: "height hash" of the main-chain blocks this node's miners found
# ("p2pool: block found height=H id=<block hash>"; not the "(share of a peer)" lines)
r_blocks() {
    { grep -a "p2pool: block found height=" "$1" 2> /dev/null | grep -av "share of a peer" \
        | grep -aoE "block found height=[0-9]+ id=[0-9a-f]{64}" || true; } \
        | sed -E 's/block found height=([0-9]+) id=([0-9a-f]{64})/\1 \2/'
}
ids() { awk '{print $2}' | sort -u; }
# ids_at <height>: the ids of "height id" lines (stdin) at exactly that height
ids_at() { awk -v h="$1" '$1 == h {print $2}' | sort -u; }
# common <ids> <ids>: how many ids both lists hold
common() {
    comm -12 <(printf '%s\n' "$1" | sed '/^$/d' | sort -u) <(printf '%s\n' "$2" | sed '/^$/d' | sort -u) \
        | wc -l | tr -d ' '
}
nlines() { printf '%s\n' "$1" | sed '/^$/d' | wc -l | tr -d ' '; }

# The C container and the xmrig container must stay up while we wait.
check_pool_alive() {
    local c
    for c in "$(pool_container "$C")" "$(xmrig_container "$C")"; do
        container_exists "$c" || continue
        if ! container_running "$c"; then
            docker logs --tail 20 "$c" 2>&1 || true
            die alive "container $c exited unexpectedly (log $(pool_log "$C"))"
        fi
    done
}
# wait_x_height <height>: X's top >= height; 1 when the chain stalls for BLOCK_WAIT seconds
wait_x_height() {
    local target=$1 last cur since
    last=$(height_of "$X_RPC")
    since=$(date +%s)
    while :; do
        cur=$(height_of "$X_RPC")
        if [ -n "$cur" ] && [ "$cur" -ge "$target" ]; then return 0; fi
        if [ -n "$cur" ] && [ "$cur" != "$last" ]; then
            last=$cur
            since=$(date +%s)
        fi
        [ $(($(date +%s) - since)) -ge "$BLOCK_WAIT" ] && return 1
        check_alive
        check_pool_alive
        sleep 2
    done
}
stall() { # <check> <target>
    die "$1" "no block for ${BLOCK_WAIT}s: X at $(height_of "$X_RPC"), wanted $2 (C: $(pool_count "$C" "SHARE FOUND") shares, xmrig accepted $(xmrig_accepted "$C"); R1 log $R1_LOG)"
}

# Pacing of the xkr-miners: after each accepted submission (= a main-chain block on this
# testnet) the miner is stopped for R_MINER_PAUSE seconds. PACERS holds "<loop pid>:<miner pid>".
PACERS=()
pace() { # <process name> <miner log>
    local pid=${PID[$1]:-} log=$2
    [ -n "$pid" ] || return 0
    (
        set +e
        trap - ERR EXIT INT TERM
        n=0
        while kill -0 "$pid" 2> /dev/null; do
            c=$(grep -ac '"event":"accepted"' "$log" 2> /dev/null)
            if [ "${c:-0}" -gt "$n" ]; then
                n=$c
                kill -STOP "$pid" 2> /dev/null || break
                sleep "$R_MINER_PAUSE"
                kill -CONT "$pid" 2> /dev/null || break
            fi
            sleep 0.2
        done
    ) &
    PACERS+=("$!:$pid")
}
unpace() {
    local p
    for p in ${PACERS[@]+"${PACERS[@]}"}; do
        kill "${p%%:*}" 2> /dev/null || true
        wait "${p%%:*}" 2> /dev/null || true
        kill -CONT "${p##*:}" 2> /dev/null || true
    done
    PACERS=()
}
# run by _suite_exit before the processes are stopped: no miner may stay stopped
scenario_cleanup() { unpace; }

# Lines that must not appear in the xkrd logs of R1 and R2: a rejected or banned sidechain
# peer, a found block the chain refused for another reason than coming too late.
R_BAD_RE='p2pool: peer .* rejected|p2pool: [0-9]+ found blocks? refused|p2pool could not start|p2pool: no payout address'
C_BAD_RE="$P2POOL_BAD_RE|banned for"
r_bad_count() { { grep -acE -- "$R_BAD_RE" "$1" 2> /dev/null || true; } | head -n 1; }
r_bad_first() { { grep -aE -m 1 -- "$R_BAD_RE" "$1" 2> /dev/null || true; } | head -n 1 | cut -c1-220; }

# ------------------------------------------------------------------ 1. wallets, X, R1, R2

W0_DIR=$SD/w0
WC_DIR=$SD/wc
WR1_DIR=$SD/wr1
WR2_DIR=$SD/wr2
W0=$(wallet_addr "$W0_DIR")
WC=$(wallet_addr "$WC_DIR")
WR1=$(wallet_addr "$WR1_DIR")
WR2=$(wallet_addr "$WR2_DIR")
[ -n "$W0" ] && [ -n "$WC" ] && [ -n "$WR1" ] && [ -n "$WR2" ] || die 1 "xkr-testwallet did not create the wallets"
pass 1 "wallets W0 (phase 0) $W0, WC (C++ p2pool C) $WC, WR1 (xkrd R1) $WR1, WR2 (xkrd R2) $WR2"
echo "fork 13 at F13=$F13, fork 14 at F14=$F14 (grace $GRACE); image $P2POOL_IMAGE; xkr-miner pause ${R_MINER_PAUSE}s, R2 miner $R2_MINER"

FORK_ARGS=(--fork13-height "$F13" --fork14-height "$F14" --fork14-grace "$GRACE")
X_ARGS=(--no-hyper --out-peers 0 --p2p-bind-ip 127.0.0.1 --p2p-bind-port "$PORT_PM_X_P2P")
start_xkrd pm-x "$PORT_PM_X_RPC" "$SD/xkrd-x" "$SD/xkrd-x-phase0.log" "${FORK_ARGS[@]}" "${X_ARGS[@]}" \
    --mine "$W0" --mine-threads 1
watch pm-x
wait_height "$X_RPC" "$PHASE0_BLOCKS" 180 || die 1 "phase 0: X did not mine $PHASE0_BLOCKS blocks within 180 s"
stop_proc pm-x
unwatch
start_xkrd pm-x "$PORT_PM_X_RPC" "$SD/xkrd-x" "$X_LOG" "${FORK_ARGS[@]}" "${X_ARGS[@]}"
watch pm-x
H0=$(height_of "$X_RPC")
[ -n "$H0" ] && [ "$H0" -le $((F13 - 3)) ] || die 1 "phase 0 overshot: X at [$H0], F13=$F13 (raise F13 and F14)"

# local HyperDHT (as 20 and 40): nothing goes to the public DHT
start_bg pm-dht "$SD/bootstrap.log" "$NODE" "$ROOT/bridge/test/testnet-bootstrap.js" --ignore-stdin
watch pm-dht
BOOT=""
for _ in $(seq 1 100); do
    BOOT=$({ sed -n 's/^BOOTSTRAP //p' "$SD/bootstrap.log" || true; } | head -n 1)
    [ -n "$BOOT" ] && break
    sleep 0.1
done
[ -n "$BOOT" ] || die 1 "local DHT did not start: $(tail -n 5 "$SD/bootstrap.log" | tr '\n' ' ')"

# C first, so that R1 finds it at once
start_p2pool "$C" "$P2POOL_IMAGE" "$WC" "$PORT_PM_POOL_C" "$PORT_PM_STRATUM_C"
wait_for 120 pool_api_ready "$C" || die 1 "p2pool C wrote no local API within 120 s (log $(pool_log "$C"): $(pool_first "$C" 'RPC response|Couldn|error'))"

HYPER=(--hyper-bridge-dir "$ROOT/bridge" --hyper-bootstrap "$BOOT")
# --node --p2pool: the node and p2pool, no Hugin relay
P2POOL_ARGS=(--node --p2pool --p2pool-config "$SIDECHAIN_JSON" --p2pool-uncapped-pplns --p2pool-allow-same-ip)
start_xkrd pm-r1 "$PORT_PM_R1_RPC" "$SD/xkrd-r1" "$R1_LOG" "${FORK_ARGS[@]}" "${HYPER[@]}" \
    --p2p-bind-port 0 --add-peer "127.0.0.1:$PORT_PM_X_P2P" --seed-node "$DEAD_PEER" \
    "${P2POOL_ARGS[@]}" --p2pool-p2p "127.0.0.1:$PORT_PM_POOL_R1" \
    --p2pool-stratum "127.0.0.1:$PORT_PM_STRATUM_R1" --p2pool-addpeers "127.0.0.1:$PORT_PM_POOL_C" \
    --hugin-payout "$WR1"
# R2: no legacy peers (dead seed, --out-peers 0, no listener) and no sidechain TCP peers (no
# listener, no --p2pool-addpeers; the custom sidechain has no seeds)
start_xkrd pm-r2 "$PORT_PM_R2_RPC" "$SD/xkrd-r2" "$R2_LOG" "${FORK_ARGS[@]}" "${HYPER[@]}" \
    --p2p-bind-port 0 --seed-node "$DEAD_PEER" --out-peers 0 \
    "${P2POOL_ARGS[@]}" --p2pool-stratum "127.0.0.1:$PORT_PM_STRATUM_R2" --hugin-payout "$WR2"
watch pm-r1 pm-r2

wait_same_top "$R1_RPC" "$X_RPC" || die 1 "R1 [$(top_of "$R1_RPC")] did not reach X's top [$(top_of "$X_RPC")] over Levin within ${TIMEOUT}s"
wait_same_top "$R2_RPC" "$X_RPC" || die 1 "R2 [$(top_of "$R2_RPC")] did not reach X's top [$(top_of "$X_RPC")] over Hyperswarm within ${TIMEOUT}s"
if wait_for 120 r_running "$R1_RPC" && wait_for 120 r_running "$R2_RPC"; then
    pass 1 "phase 0: X mined to $H0 and restarted without a miner; R1 (Levin) and R2 (Hyperswarm only) at X's top $(top_of "$X_RPC"); p2pool running on both; local DHT $BOOT"
else
    die 1 "p2pool not running: R1 state [$(r_str "$R1_RPC" state)] error [$(r_str "$R1_RPC" error)], R2 state [$(r_str "$R2_RPC" state)] error [$(r_str "$R2_RPC" error)]"
fi

# ------------------------------------------------------------------ 2. one sidechain

C_CID=$(pool_first "$C" "consensus ID = $CID_HEAD\*+$CID_TAIL")
if [ -n "$C_CID" ]; then
    pass 2 "C: \"${C_CID##*SideChain }\""
else
    fail 2 "C did not log the consensus id $CID_HEAD…$CID_TAIL: [$(pool_first "$C" 'consensus ID')]"
fi
for n in r1 r2; do
    if [ "$n" = r1 ]; then rpc_=$R1_RPC log_=$R1_LOG; else rpc_=$R2_RPC log_=$R2_LOG; fi
    cid=$(r_str "$rpc_" consensus_id)
    if log_has "$log_" "p2pool: consensus id $CID_HEAD…$CID_TAIL" && [[ $cid == "$CID_HEAD"*"$CID_TAIL" ]] && [ "${#cid}" = 64 ]; then
        pass 2 "${n^^}: \"p2pool: consensus id $CID_HEAD…$CID_TAIL\", /status consensus_id $cid, sidechain $(r_str "$rpc_" sidechain)"
    else
        fail 2 "${n^^}: consensus id line [$({ grep -a -m 1 'p2pool: consensus id' "$log_" || true; } | cut -c1-160)], /status [$cid]"
    fi
    if log_has "$log_" "p2pool: testnet sidechain, PPLNS weight not capped"; then
        pass 2 "${n^^}: \"p2pool: testnet sidechain, PPLNS weight not capped\""
    else
        fail 2 "${n^^} did not lift the PPLNS cap (no \"p2pool: testnet sidechain, PPLNS weight not capped\")"
    fi
done
if pool_has "$C" "$PPLNS_UNCAPPED_LINE"; then
    pass 2 "C: \"$PPLNS_UNCAPPED_LINE\" (patch 0003)"
else
    fail 2 "C did not log \"$PPLNS_UNCAPPED_LINE\": image $P2POOL_IMAGE without patch 0003? (rebuild: $BUILD_P2POOL_IMAGE)"
fi

# ------------------------------------------------------------------ 3. peers

r1_peered() { [ "$(r_num "$R1_RPC" peers_out)" -ge 1 ] && [ "$(r_num "$R1_RPC" peers_hyper)" -ge 1 ]; } 2> /dev/null
r2_peered() { [ "$(r_num "$R2_RPC" peers_hyper)" -ge 1 ]; } 2> /dev/null
if wait_for 120 r1_peered && wait_for 60 r2_peered && wait_for 60 pool_connected "$C"; then
    pass 3 "R1: peers_out $(r_num "$R1_RPC" peers_out) (C over TCP), peers_in $(r_num "$R1_RPC" peers_in), peers_hyper $(r_num "$R1_RPC" peers_hyper) (R2); C: $(pool_connections "$C") connection(s)"
else
    fail 3 "sidechain peers: R1 out [$(r_num "$R1_RPC" peers_out)] hyper [$(r_num "$R1_RPC" peers_hyper)], R2 hyper [$(r_num "$R2_RPC" peers_hyper)], C connections [$(pool_connections "$C")]"
fi
r2_out=$(r_num "$R2_RPC" peers_out); r2_in=$(r_num "$R2_RPC" peers_in); r2_hy=$(r_num "$R2_RPC" peers_hyper)
peers2=$(obj "$(get "$R2_RPC" /status)" peers)
lo2=$(num "$peers2" legacy_outgoing); li2=$(num "$peers2" legacy_incoming); hy2=$(num "$peers2" hyper)
if [ "$r2_out" = 0 ] && [ "$r2_in" = 0 ] && [ -n "$r2_hy" ] && [ "$r2_hy" -ge 1 ] \
    && [ "$lo2" = 0 ] && [ "$li2" = 0 ] && [ -n "$hy2" ] && [ "$hy2" -ge 1 ]; then
    pass 3 "R2 over Hyperswarm only: p2pool peers_out 0, peers_in 0, peers_hyper $r2_hy; main chain legacy_outgoing 0, legacy_incoming 0, hyper $hy2"
else
    fail 3 "R2: p2pool peers_out [$r2_out] peers_in [$r2_in] peers_hyper [$r2_hy], main chain legacy_outgoing [$lo2] legacy_incoming [$li2] hyper [$hy2] (want 0, 0, >= 1, 0, 0, >= 1)"
fi

# ------------------------------------------------------------------ miners

t_start=$(date +%s)
start_xmrig "$C" "$PORT_PM_STRATUM_C"
start_bg pm-miner-r1 "$M1_LOG" "$XKR_MINER" --pool "stratum+tcp://127.0.0.1:$PORT_PM_STRATUM_R1" \
    --pool-user x --rig-id r1 --threads "$R_MINER_THREADS" --log-json
pace pm-miner-r1 "$M1_LOG"
if [ "$R2_MINER" = 1 ]; then
    start_bg pm-miner-r2 "$M2_LOG" "$XKR_MINER" --pool "stratum+tcp://127.0.0.1:$PORT_PM_STRATUM_R2" \
        --pool-user x --rig-id r2 --threads "$R_MINER_THREADS" --log-json
    pace pm-miner-r2 "$M2_LOG"
fi
watch "${MINERS[@]}"

# ------------------------------------------------------------------ 4. forks

wait_x_height $((F14 + V7_BLOCKS)) || stall 4 $((F14 + V7_BLOCKS))
T4=$(height_of "$X_RPC")
FB=0; FB_FIRST=""; MINORS=""
for ((h = H0 + 1; h <= T4; h++)); do
    read -r maj mnr <<< "$(header_vers "$X_RPC" "$h")"
    read -r _ cv n tg vt <<< "$(coinbase_shape "$X_RPC" "$h")"
    MINORS="$MINORS ${mnr:-?}"
    ok=1
    [ "${maj:-}" = "$(want_major "$h")" ] && [ "${n:-0}" -gt 0 ] || ok=0
    if [ "$h" -gt "$F14" ]; then
        [ "${cv:-}" = 2 ] && [ "${tg:-}" = "$n" ] && [ "${vt:-}" = "$n" ] || ok=0
    else
        [ "${cv:-}" = 1 ] && [ "${tg:-}" = 0 ] || ok=0
    fi
    if [ "$ok" = 0 ]; then
        FB=$((FB + 1))
        [ -n "$FB_FIRST" ] || FB_FIRST="block $h: major ${maj:-?} (want $(want_major "$h")), coinbase version ${cv:-?}, ${n:-?} outputs, ${tg:-?} tagged, ${vt:-?} view tags"
    fi
done
ref=$(log_count "$X_LOG" "submitted block refused")
if [ "$FB" = 0 ] && [ "${ref:-0}" = 0 ]; then
    pass 4 "blocks $((H0 + 1))..$T4: major 5 up to $F13, 6 up to $F14, 7 after with a version-2 coinbase of tagged outputs; minors seen:$(echo "$MINORS" | tr ' ' '\n' | sed '/^$/d' | sort -u | tr '\n' ' '); X refused no submitted block"
else
    fail 4 "$FB bad block(s) in $((H0 + 1))..$T4, first: ${FB_FIRST:-none}; X refused ${ref:-?} submitted block(s): $({ grep -a -m 1 'submitted block refused' "$X_LOG" || true; } | cut -c1-200)"
fi

# ------------------------------------------------------------------ 5. shares cross both ways

collect() {
    C_OWN=$(c_shares 1 | ids); C_PEER=$(c_shares 0 | ids)
    R1_OWN=$(r_shares "$R1_LOG" found | ids); R1_ADDED=$(r_shares "$R1_LOG" added | ids)
    R2_OWN=$(r_shares "$R2_LOG" found | ids); R2_ADDED=$(r_shares "$R2_LOG" added | ids)
    C_TO_R1=$(common "$C_OWN" "$R1_ADDED"); C_TO_R2=$(common "$C_OWN" "$R2_ADDED")
    R1_TO_C=$(common "$R1_OWN" "$C_PEER"); R1_TO_R2=$(common "$R1_OWN" "$R2_ADDED")
    R2_TO_R1=$(common "$R2_OWN" "$R1_ADDED"); R2_TO_C=$(common "$R2_OWN" "$C_PEER")
}
main_exchange() { [ "$C_TO_R1" -ge 1 ] && [ "$C_TO_R2" -ge 1 ] && [ "$R1_TO_C" -ge 1 ] && [ "$R1_TO_R2" -ge 1 ]; }
r2_exchange() { [ "$R2_TO_R1" -ge 1 ] && [ "$R2_TO_C" -ge 1 ]; }
sh_msg() {
    echo "C found $(nlines "$C_OWN") (R1 added $C_TO_R1, R2 added $C_TO_R2), R1 found $(nlines "$R1_OWN") (C added $R1_TO_C, R2 added $R1_TO_R2), R2 found $(nlines "$R2_OWN") (R1 added $R2_TO_R1, C added $R2_TO_C) after $(($(date +%s) - t_start)) s, X at $(height_of "$X_RPC")"
}
t_sh=$(date +%s)
while :; do
    collect
    if main_exchange && { [ "$R2_MINER" != 1 ] || r2_exchange; }; then break; fi
    [ $(($(date +%s) - t_sh)) -ge "$SHARE_WAIT" ] && break
    check_alive
    check_pool_alive
    sleep 5
done
SH_H=""
if main_exchange; then
    SH_H=$(height_of "$X_RPC")
    pass 5 "shares crossed: $(sh_msg)"
else
    fail 5 "share exchange incomplete within ${SHARE_WAIT}s: $(sh_msg) (slow miners? raise SHARE_WAIT; C log $(pool_log "$C"), R1 log $R1_LOG)"
fi
R2_SH=""
if [ "$R2_MINER" = 1 ]; then
    if r2_exchange; then
        R2_SH=$(height_of "$X_RPC")
        pass 5 "R2's shares (Hyperswarm only) reached R1 ($R2_TO_R1) and, through R1, C ($R2_TO_C)"
    elif [ "$(nlines "$R2_OWN")" = 0 ]; then
        skip 5 "R2's miner found no share within ${SHARE_WAIT}s: the Hyperswarm -> TCP direction of R2's own shares is not shown"
    else
        fail 5 "R2 found $(nlines "$R2_OWN") share(s) but R1 added $R2_TO_R1 and C added $R2_TO_C of them"
    fi
else
    skip 5 "R2_MINER=0: R2 receives shares over Hyperswarm but mines none"
fi
if [ -n "$SH_H" ]; then
    wait_x_height $((SH_H + POST_SHARE_BLOCKS)) || stall 5 $((SH_H + POST_SHARE_BLOCKS))
fi

# ------------------------------------------------------------------ 6. the sidechains agree

# Heights within SC_SLACK, and a share at the lowest of the three heights known to all three.
# (/status reports no tip id and C's log only its "new chain tip" height, so the share ids of
# the add_block / "share found|added" lines stand in for the tips.)
agree() {
    HC=$(pool_sidechain_height "$C"); HR1=$(r_num "$R1_RPC" height); HR2=$(r_num "$R2_RPC" height)
    [ -n "$HC" ] && [ -n "$HR1" ] && [ -n "$HR2" ] || return 1
    LO=$HC; HI=$HC
    for v in "$HR1" "$HR2"; do
        [ "$v" -lt "$LO" ] && LO=$v
        [ "$v" -gt "$HI" ] && HI=$v
    done
    [ "$LO" -ge 1 ] && [ "$LO" -ge $((HI - SC_SLACK)) ] || return 1
    AT_C=$(c_shares | ids_at "$LO"); AT_R1=$(r_shares "$R1_LOG" any | ids_at "$LO"); AT_R2=$(r_shares "$R2_LOG" any | ids_at "$LO")
    SHARED=$(comm -12 <(printf '%s\n' "$AT_C" | sed '/^$/d') <(printf '%s\n' "$AT_R1" | sed '/^$/d') \
        | comm -12 - <(printf '%s\n' "$AT_R2" | sed '/^$/d') | head -n 1)
    [ -n "$SHARED" ]
}
if wait_for 120 agree; then
    pass 6 "sidechain heights C $HC, R1 $HR1, R2 $HR2 (within $SC_SLACK); share $SHARED at height $LO is on all three"
else
    fail 6 "sidechains apart: heights C [$HC] R1 [$HR1] R2 [$HR2] (slack $SC_SLACK); ids at the lowest height ${LO:-?}: C [$(echo "${AT_C:-}" | tr '\n' ' ')] R1 [$(echo "${AT_R1:-}" | tr '\n' ' ')] R2 [$(echo "${AT_R2:-}" | tr '\n' ' ')]"
fi
for n in r1 r2; do
    if [ "$n" = r1 ]; then log_=$R1_LOG; rpc_=$R1_RPC; else log_=$R2_LOG; rpc_=$R2_RPC; fi
    if log_has "$log_" "p2pool: synchronized" && [ "$(bool "$(r_p2pool "$rpc_")" synced)" = true ]; then
        pass 6 "${n^^}: \"p2pool: synchronized\", /status synced true, $(r_num "$rpc_" shares) shares stored, $(r_num "$rpc_" miners) payout wallet(s) in the window"
    else
        fail 6 "${n^^}: \"p2pool: synchronized\" logged: $(log_has "$log_" "p2pool: synchronized" && echo yes || echo no), /status synced [$(bool "$(r_p2pool "$rpc_")" synced)]"
    fi
done

# ------------------------------------------------------------------ stop mining

unpace
for m in "${MINERS[@]}"; do stop_proc "$m"; done
unwatch
watch pm-x pm-r1 pm-r2
xa=$(xmrig_accepted "$C"); xr=$(xmrig_rejected "$C")
stop_xmrig "$C"
sleep 5
T=$(height_of "$X_RPC")

# ------------------------------------------------------------------ 7. blocks found by p2pool

c_acc=$(pool_accepted_heights "$C" | wc -l | tr -d ' ')
if [ "${c_acc:-0}" -ge 1 ]; then
    pass 7 "C: $c_acc \"submit_block: BLOCK ACCEPTED\" from X"
else
    fail 7 "C logged no \"BLOCK ACCEPTED\" (xmrig accepted ${xa:-0})"
fi
declare -A RBUILT=()
r1_found=0; r1_on_x=0; r2_on_x=0
while read -r h id; do
    [ -n "${h:-}" ] || continue
    r1_found=$((r1_found + 1))
    if [ "$(header_hash_at "$X_RPC" "$h")" = "$id" ]; then
        r1_on_x=$((r1_on_x + 1))
        RBUILT[$h]=R1
    fi
done < <(r_blocks "$R1_LOG")
while read -r h id; do
    [ -n "${h:-}" ] || continue
    if [ "$(header_hash_at "$X_RPC" "$h")" = "$id" ]; then
        r2_on_x=$((r2_on_x + 1))
        RBUILT[$h]=R2
    fi
done < <(r_blocks "$R2_LOG")
lost=$(log_count "$R1_LOG" "p2pool: found block not accepted")
if [ "$r1_on_x" -ge 1 ]; then
    pass 7 "R1: $r1_found \"p2pool: block found\", $r1_on_x of them on X's chain (same hash at that height); $lost lost the race for their height; R2's blocks on X: $r2_on_x"
elif [ "$r1_found" = 0 ] && [ "${lost:-0}" -ge 1 ] && [ "$(r_bad_count "$R1_LOG")" = 0 ]; then
    skip 7 "every block of R1's miner ($lost) lost the race for its height to a block of C (\"found block not accepted\" as an alternative block): no xkrd-built block on X"
else
    fail 7 "R1: $r1_found \"p2pool: block found\", $r1_on_x on X's chain, $lost not accepted; first error: [$(r_bad_first "$R1_LOG")]"
fi

# ------------------------------------------------------------------ 8. PPLNS payouts

BAL_C=$SD/balance-wc.out; BAL_1=$SD/balance-wr1.out; BAL_2=$SD/balance-wr2.out
wallet "$WC_DIR" "$X_RPC" balance > "$BAL_C" 2>&1 || true
wallet "$WR1_DIR" "$X_RPC" balance > "$BAL_1" 2>&1 || true
wallet "$WR2_DIR" "$X_RPC" balance > "$BAL_2" 2>&1 || true
CV_BAD=0; CV_N=0; CV_FIRST=""; OUT_C=0; OUT_1=0; OUT_2=0
BOTH_R=0; BOTH_C=0; LATE_R=0; LATE_C=0; PAYS_2=0; LATE_2=0
declare -A ROW=()
for ((h = H0 + 1; h <= T; h++)); do
    read -r cb _ n _ _ <<< "$(coinbase_shape "$X_RPC" "$h")"
    idx=$(owned_indices "${cb:-x}" "$BAL_C" "$BAL_1" "$BAL_2")
    tot=$(nlines "$idx")
    uni=$(printf '%s\n' "$idx" | sed '/^$/d' | sort -un | wc -l | tr -d ' ')
    mx=$(printf '%s\n' "$idx" | sed '/^$/d' | sort -n | tail -n 1)
    wc_=$(nlines "$(owned_indices "${cb:-x}" "$BAL_C")")
    w1=$(nlines "$(owned_indices "${cb:-x}" "$BAL_1")")
    w2=$(nlines "$(owned_indices "${cb:-x}" "$BAL_2")")
    OUT_C=$((OUT_C + wc_)); OUT_1=$((OUT_1 + w1)); OUT_2=$((OUT_2 + w2))
    by=${RBUILT[$h]:-C}
    ROW[$h]="$by: WC $wc_ / WR1 $w1 / WR2 $w2"
    CV_N=$((CV_N + 1))
    if [ -n "$cb" ] && [ "${n:-0}" -gt 0 ] && [ "$tot" = "$n" ] && [ "$uni" = "$n" ] && [ "${mx:-0}" -lt "$n" ]; then
        :
    else
        CV_BAD=$((CV_BAD + 1))
        [ -n "$CV_FIRST" ] || CV_FIRST="block $h ($by) coinbase ${cb:-?}: ${n:-?} outputs, wallets own $tot ($uni distinct, max index ${mx:-none}; WC $wc_, WR1 $w1, WR2 $w2)"
    fi
    if [ -n "$SH_H" ] && [ "$h" -gt $((SH_H + 1)) ]; then
        if [ "$by" = C ]; then
            LATE_C=$((LATE_C + 1))
            [ "$wc_" -gt 0 ] && [ "$w1" -gt 0 ] && BOTH_C=$((BOTH_C + 1))
        else
            LATE_R=$((LATE_R + 1))
            [ "$wc_" -gt 0 ] && [ "$w1" -gt 0 ] && BOTH_R=$((BOTH_R + 1))
        fi
    fi
    if [ -n "$R2_SH" ] && [ "$h" -gt $((R2_SH + 1)) ]; then
        LATE_2=$((LATE_2 + 1))
        [ "$w2" -gt 0 ] && PAYS_2=$((PAYS_2 + 1))
    fi
done
if [ "$CV_N" -gt 0 ] && [ "$CV_BAD" = 0 ]; then
    pass 8 "$CV_N p2pool coinbases ($((H0 + 1))..$T, built by C and xkrd): every output owned exactly once by WC/WR1/WR2 (WC $OUT_C, WR1 $OUT_1, WR2 $OUT_2 outputs; v7 through the view-tag fast path)"
else
    fail 8 "$CV_BAD of $CV_N coinbases not covered by WC/WR1/WR2 (wrong payee, amount split or view tag); first: $CV_FIRST"
fi
if [ -z "$SH_H" ]; then
    skip 8 "no share exchange (5 failed): no shared PPLNS window to check"
else
    if [ "$BOTH_R" -ge 1 ]; then
        pass 8 "xkrd-built coinbases after the share exchange (X height $SH_H): $BOTH_R of $LATE_R pay both WC (C++ share) and WR1"
    elif [ "$LATE_R" = 0 ]; then
        skip 8 "no xkrd-built block on X after the share exchange at $SH_H (see 7)"
    else
        fail 8 "none of the $LATE_R xkrd-built coinbases after the share exchange at $SH_H pays both WC and WR1: C's share is not in xkrd's PPLNS window"
    fi
    if [ "$BOTH_C" -ge 1 ]; then
        pass 8 "C-built coinbases after the share exchange: $BOTH_C of $LATE_C pay both WC and WR1 (xkrd's share)"
    else
        fail 8 "none of the $LATE_C C-built coinbases after the share exchange at $SH_H pays both WC and WR1: R1's share is not in C's PPLNS window"
    fi
fi
if [ -n "$R2_SH" ]; then
    if [ "$PAYS_2" -ge 1 ]; then
        pass 8 "$PAYS_2 of $LATE_2 coinbases after R2's share reached C pay WR2"
    else
        fail 8 "none of the $LATE_2 coinbases after R2's share reached C ($R2_SH) pays WR2"
    fi
fi

# ------------------------------------------------------------------ 9. no rejections, miners, chains

C_BAD=$(pool_count_re "$C" "$C_BAD_RE")
if [ "${C_BAD:-0}" = 0 ]; then
    pass 9 "C log: no invalid share, ban, fork-table mismatch, wrong view tag, PoW or submit error"
else
    fail 9 "C log: $C_BAD bad line(s), first: $(pool_first "$C" "$C_BAD_RE")"
fi
for n in r1 r2; do
    if [ "$n" = r1 ]; then log_=$R1_LOG; else log_=$R2_LOG; fi
    nb=$(r_bad_count "$log_")
    if [ "${nb:-0}" = 0 ]; then
        pass 9 "${n^^} log: no rejected sidechain peer and no refused found block"
    else
        fail 9 "${n^^} log: $nb bad line(s), first: $(r_bad_first "$log_")"
    fi
done
if [ "${xa:-0}" -ge 1 ] && [ "${xr:-0}" = 0 ]; then
    pass 9 "xmrig on C: $xa submission(s) accepted, $xr rejected"
else
    fail 9 "xmrig on C: ${xa:-0} accepted, ${xr:-?} rejected (docker logs $(xmrig_container "$C"))"
fi
for m in "${MINERS[@]}"; do
    if [ "$m" = pm-miner-r1 ]; then ml=$M1_LOG; else ml=$M2_LOG; fi
    ma=$(log_count "$ml" '"event":"accepted"'); mr=$(log_count "$ml" '"event":"rejected"')
    if [ "${ma:-0}" -ge 1 ] && [ "${mr:-0}" = 0 ]; then
        pass 9 "xkr-miner ${m#pm-miner-}: $ma submission(s) accepted by xkrd's stratum, $mr rejected"
    else
        fail 9 "xkr-miner ${m#pm-miner-}: ${ma:-0} accepted, ${mr:-?} rejected (log $ml)"
    fi
done
for r in "$R1_RPC" "$R2_RPC"; do
    nm=R1; [ "$r" = "$R2_RPC" ] && nm=R2
    if wait_same_top "$X_RPC" "$r" 120; then
        if compare_chains "$X_RPC" "$r" 1 "$T"; then
            pass 9 "$nm has X's headers 1..$T (all p2pool blocks of both implementations)"
        else
            fail 9 "$CMP_MISMATCHES headers differ between X and $nm in 1..$T; first: $CMP_FIRST"
        fi
    else
        fail 9 "$nm [$(top_of "$r")] did not reach X's top [$(top_of "$X_RPC")] within 120 s"
    fi
done

# ------------------------------------------------------------------ summary

echo
printf '%-7s %-6s %-6s %-7s %-8s %s\n' height major minor "cb ver" outputs "built by: payees"
for h in "$F13" $((F13 + 1)) "$F14" $((F14 + 1)) ${SH_H:+$((SH_H + 2))} "$T"; do
    [ "$h" -gt "$H0" ] && [ "$h" -le "$T" ] || continue
    read -r maj mnr <<< "$(header_vers "$X_RPC" "$h")"
    read -r _ cv n _ _ <<< "$(coinbase_shape "$X_RPC" "$h")"
    printf '%-7s %-6s %-6s %-7s %-8s %s\n' "$h" "${maj:-?}" "${mnr:-?}" "${cv:-?}" "${n:-?}" "${ROW[$h]:-?}"
done
echo
echo "phase 0 to $H0, p2pool blocks $((H0 + 1))..$T (xkrd-built on X: R1 $r1_on_x, R2 $r2_on_x); share exchange at X height ${SH_H:-none}"
echo "C: $(pool_count "$C" "SHARE FOUND") shares, $c_acc BLOCK ACCEPTED, sidechain height $(pool_sidechain_height "$C"); log $(pool_log "$C")"
echo "R1: $(r_num "$R1_RPC" shares_found) shares, $(r_num "$R1_RPC" blocks_found) blocks, sidechain height $(r_num "$R1_RPC" height); log $R1_LOG"
echo "R2: $(r_num "$R2_RPC" shares_found) shares, $(r_num "$R2_RPC" blocks_found) blocks, sidechain height $(r_num "$R2_RPC" height); log $R2_LOG"
echo "fork 13 at $F13, fork 14 at $F14; logs in $SD"
finish
