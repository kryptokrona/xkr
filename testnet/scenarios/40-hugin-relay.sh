#!/usr/bin/env bash
# 40-hugin-relay: xkrd as a hugin-node relay (on by default), end to end on a local HyperDHT
# (docs/hugin-relay.md, "Local test"):
#
#   DHT   bridge/test/testnet-bootstrap.js (local, nothing goes to the public DHT)
#   jobs  HUGIN_JOBS=auto (default): p2pool when the image xkr-p2pool:testnet is there (and
#         P2POOL is not 0), else the fake stratum pool. p2pool: A mines PHASE0_BLOCKS (10)
#         blocks with its built-in miner (no Hyperswarm), restarts as a relay; p2pool A runs on
#         A's RPC with wallet FEE_A, stratum 127.0.0.1:22333; both relays use it
#         (`--hugin-p2pool`, app difficulty `--hugin-share-difficulty 1000`, p2pool's least, so
#         every app share is also a p2pool share). fake: bridge/test/fake-stratum-pool.js
#         (127.0.0.1, difficulty 100, prints every login and submit; the default app
#         difficulty 200, every app share meets the pool target)
#   A     xkrd (relay by default) --hugin FEE_A: node mesh + public-node + private-node topics,
#         RPC 22899
#   B     xkrd --hugin-private, no --hugin: node mesh + private-node topic only; the default
#         donation address, said once as a warning (no terminal)
#   app   bridge/test/hugin-client-sim.js --testnet: a Hugin app doing what hugin-desktop and
#         hugin-native do, on the testnet Hugin topics (domain `hugin/testnet/topic`)
#   (A and B: testnet, no legacy peers; B follows A's chain over Hyperswarm)
#
# Checks: A announces 3 Hugin topics, B 2 (the public one after the node is synchronized);
# jobs: A's relay logs in to its p2pool node with FEE_A (`<FEE_A>+1000`); fee: B says it donates
# to nilsjr; public: the app finds A on the testnet public-node topic, gets a job, posts a paid
# DM, syncs it back and receives the new-message push; shares: A's /status counts an accepted
# share and the pool saw it under FEE_A (p2pool: "user <FEE_A, 31 characters> found a
# mainchain block", every share is a testnet block; fake: SUBMIT ok); mesh: B has the message;
# private: the app posts through A's private-node invite; restart: A restarted still has the
# message (<data dir>/hugin/messages.jsonl).
#
# Needs bridge/node_modules (the simulator imports bridge/src/swarm.js, mid.js and keccak.js).
# Env: HUGIN_NODE_MODULES (bare-rpc, keypear, hugin-utils, kryptokrona-utils; default
# ../hugin-desktop/node_modules next to this repo), HUGIN_JOBS (auto, p2pool or fake), P2POOL (0: never
# p2pool), PHASE0_BLOCKS (10), LOG_LEVEL (info,xkr_hugin=debug), TIMEOUT (180, per step),
# KEEP=1.
# suite: expected=180 timeout=1500
set -euo pipefail
: "${TIMEOUT:=180}"
: "${LOG_LEVEL:=info,xkr_hugin=debug}"
: "${HUGIN_JOBS:=auto}"
: "${P2POOL:=1}"
: "${PHASE0_BLOCKS:=10}"
. "$(dirname "$0")/../lib.sh"
. "$TESTNET_DIR/lib-p2pool.sh"
suite_init "${1:-}"

require_xkrd
require_hyper
require_node
require_bridge
require_cmd curl
for pkg in bare-rpc keypear hugin-utils kryptokrona-utils; do
    [ -d "$HUGIN_NODE_MODULES/$pkg" ] \
        || missing_exit prereq "$pkg missing in $HUGIN_NODE_MODULES" "set HUGIN_NODE_MODULES (hugin-desktop's node_modules has them)"
done
reclaim hugin-dht hugin-pool hugin-a hugin-b

# A's fee address (a standard testnet address; the same prefix as mainnet)
FEE_A=SEKReYU57DLLvUjNzmjVhaK7jqc8SdZZ3cyKJS5f4gWXK4NQQYChzKUUwzCGhgqUPkWQypeR94rqpgMPjXWG9ijnZKNw2LWXnZU
A_RPC=127.0.0.1:$PORT_PP_A_RPC
require_ports_free "$PORT_PP_A_RPC"

plain() { sed 's/\x1b\[[0-9;]*m//g' "$1" 2>/dev/null || true; }
# first value a sed expression prints from a file, waiting up to <secs>
wait_line() { # <file> <sed expr> <secs>
    local file=$1 expr=$2 secs=$3 v="" i
    for ((i = 0; i < secs * 10; i++)); do
        v=$({ sed -n "$expr" "$file" 2>/dev/null || true; } | head -n 1)
        [ -n "$v" ] && { echo "$v"; return 0; }
        sleep 0.1
    done
    return 1
}
# at least <n> lines matching <pattern> in <file> within <secs>
wait_count() { # <file> <n> <pattern> <secs>
    local file=$1 n=$2 pattern=$3 secs=$4 i c
    for ((i = 0; i < secs * 10; i++)); do
        c=$({ plain "$file" | grep -c -- "$pattern" || true; } | head -n 1)
        [ "${c:-0}" -ge "$n" ] && return 0
        sleep 0.1
    done
    return 1
}

if [ "$HUGIN_JOBS" = auto ]; then
    if [ "$P2POOL" != 0 ] && image_present "$P2POOL_IMAGE"; then
        HUGIN_JOBS=p2pool
    else
        HUGIN_JOBS=fake
        skip p2pool "image $P2POOL_IMAGE missing or P2POOL=0 ($BUILD_P2POOL_IMAGE): the fake stratum pool stands in for p2pool, no p2pool share check"
    fi
fi

start_bg hugin-dht "$SD/bootstrap.log" "$NODE" "$ROOT/bridge/test/testnet-bootstrap.js" --ignore-stdin
BOOT=$(wait_line "$SD/bootstrap.log" 's/^BOOTSTRAP //p' 10) || die setup "local DHT did not start: $(tail -n 5 "$SD/bootstrap.log" | tr '\n' ' ')"
note "DHT:  local testnet at $BOOT"

# No fork in this run (p2pool and both relays agree on the fork heights).
F13=${F13:-100000}
F14=${F14:-200000}
FORKS=(--fork13-height "$F13" --fork14-height "$F14")
case "$HUGIN_JOBS" in
    p2pool)
        reclaim pp-a
        rm_container "$(pool_container a)"
        require_ports_free "$PORT_PP_POOL_A" "$PORT_PP_STRATUM_A"
        # phase 0: blocks for p2pool to build on, mined by A without Hyperswarm
        start_xkrd hugin-a "$PORT_PP_A_RPC" "$SD/a" "$SD/a-phase0.log" "${FORKS[@]}" --no-hyper \
            --p2p-bind-port 0 --seed-node "$DEAD_PEER" --mine "$FEE_A" --mine-threads 1
        watch hugin-a
        wait_height "$A_RPC" "$PHASE0_BLOCKS" 180 || die setup "phase 0: A did not mine $PHASE0_BLOCKS blocks within 180 s"
        stop_proc hugin-a 20
        unwatch
        POOL=127.0.0.1:$PORT_PP_STRATUM_A
        note "jobs: p2pool A (stratum $POOL, wallet FEE_A) on A's RPC"
        ;;
    fake)
        start_bg hugin-pool "$SD/pool.out" "$NODE" "$ROOT/bridge/test/fake-stratum-pool.js" --job-interval 60
        POOL=$(wait_line "$SD/pool.out" 's/^POOL //p' 10) || die setup "fake stratum pool did not start: $(tail -n 5 "$SD/pool.out" | tr '\n' ' ')"
        note "jobs: fake stratum pool at $POOL"
        ;;
    *) die setup "HUGIN_JOBS must be auto, p2pool or fake, not $HUGIN_JOBS" ;;
esac

# xkrd runs bridge/ on Bare worker threads; the relay is on by default.
COMMON=(--hyper-bridge-dir "$ROOT/bridge" --hyper-bootstrap "$BOOT" --p2p-bind-port 0
    --seed-node "$DEAD_PEER" "${FORKS[@]}" --hugin-p2pool "$POOL")
if [ "$HUGIN_JOBS" = p2pool ]; then
    COMMON+=(--hugin-share-difficulty 1000)
fi
start_a() { start_xkrd hugin-a "$PORT_PP_A_RPC" "$SD/a" "$SD/a.log" "${COMMON[@]}" --hugin "$FEE_A"; }
start_a
start_xkrd hugin-b 0 "$SD/b" "$SD/b.log" "${COMMON[@]}" --hugin-private
watch hugin-a hugin-b hugin-dht hugin-pool
if [ "$HUGIN_JOBS" = p2pool ]; then
    start_p2pool a "$P2POOL_IMAGE" "$FEE_A" "$PORT_PP_POOL_A" "$PORT_PP_STRATUM_A"
    wait_for 120 pool_api_ready a || die setup "p2pool A wrote no local API within 120 s (log $(pool_log a))"
fi
note "A: $XKRD --hugin FEE_A, log $SD/a.log; B: --hugin-private (default fee), log $SD/b.log"

wait_count "$SD/a.log" 3 "Hugin topic announced" 120 || die topics "A did not announce its 3 Hugin topics: $(plain "$SD/a.log" | tail -n 5 | tr '\n' ' ')"
wait_count "$SD/b.log" 2 "Hugin topic announced" 120 || die topics "B did not announce its 2 Hugin topics: $(plain "$SD/b.log" | tail -n 5 | tr '\n' ' ')"
A_ADDR=$({ plain "$SD/a.log" | grep -o 'address=SEKR[A-Za-z0-9]*' || true; } | head -n 1 | cut -d= -f2)
A_INVITE=$({ plain "$SD/a.log" | grep -o 'invite=SEKR[A-Za-z0-9]*' || true; } | tail -n 1 | cut -d= -f2)
B_INVITE=$({ plain "$SD/b.log" | grep -o 'invite=SEKR[A-Za-z0-9]*' || true; } | tail -n 1 | cut -d= -f2)
[ -n "$A_ADDR" ] && [ -n "$A_INVITE" ] && [ -n "$B_INVITE" ] || die topics "no Hugin address/invite in the logs"
pass topics "A announced 3 Hugin topics (address $A_ADDR), B 2 (private)"

# jobs: A's relay is logged in to its p2pool node with the fee address
if wait_count "$SD/a.log" 1 "logged in to the stratum pool" 150 \
    && plain "$SD/a.log" | grep "Hugin relay ready" | grep -q "fee=$FEE_A"; then
    pass jobs "A's relay logged in to $POOL ($HUGIN_JOBS) with fee address FEE_A"
else
    fail jobs "A did not log in to $POOL with FEE_A: $(plain "$SD/a.log" | grep -E "Hugin relay ready|stratum|p2pool" | tail -n 3 | tr '\n' ' ')"
fi
if [ "$HUGIN_JOBS" = fake ]; then
    if wait_count "$SD/pool.out" 1 "^LOGIN $FEE_A+1000\$" 30; then
        pass login "the pool saw the login $FEE_A+1000"
    else
        fail login "no login $FEE_A+1000 in $SD/pool.out: $(grep '^LOGIN' "$SD/pool.out" | tail -n 2 | tr '\n' ' ')"
    fi
fi
if plain "$SD/b.log" | grep -q "You are donating your hugin node fee to nilsjr"; then
    pass fee "B (no --hugin, no terminal) says it donates its fee to nilsjr"
else
    fail fee "B did not say it donates its fee: $(plain "$SD/b.log" | grep -i "fee" | tail -n 2 | tr '\n' ' ')"
fi

SIM=("$NODE" "$ROOT/bridge/test/hugin-client-sim.js" --bootstrap "$BOOT" --testnet --timeout "$TIMEOUT")
# sim <check> <sim args...>: one run of the app simulator, output in sim-<check>.out
sim() {
    local name=$1
    shift
    if nice -n 19 "${SIM[@]}" "$@" > "$SD/sim-$name.out" 2>&1 < /dev/null; then
        pass "$name" "$(tail -n 1 "$SD/sim-$name.out")"
    else
        fail "$name" "$(tail -n 1 "$SD/sim-$name.out")"
    fi
}

# 1. public-node topic: post, sync, push
sim public --expect-address "$A_ADDR"
HASH=$({ sed -n 's/^POSTED //p' "$SD/sim-public.out" || true; } | head -n 1)

# 2. the share reached the pool under the fee address
if [ -n "$HASH" ]; then
    ST=$(get "$A_RPC" /status)
    acc=$(num "$ST" shares_accepted)
    fee=$(str "$ST" fee_address)
    if [ "${acc:-0}" -ge 1 ] && [ "$fee" = "$FEE_A" ]; then
        pass shares "A /status: shares_accepted $acc, fee_address FEE_A"
    else
        fail shares "A /status: shares_accepted [$acc], fee_address [$fee]"
    fi
    if [ "$HUGIN_JOBS" = p2pool ]; then
        user=${FEE_A:0:31}
        if wait_for 30 pool_has a "user $user found a mainchain block"; then
            pass p2pool "p2pool A: a share of user ${user} (FEE_A) became a testnet block"
        else
            fail p2pool "p2pool A has no share of user $user: $(pool_first a 'found a mainchain block|Stale|Invalid')"
        fi
    elif [ "$(grep -c '^SUBMIT ok' "$SD/pool.out" || true)" -ge 1 ]; then
        pass submit "the fake pool accepted the share"
    else
        fail submit "no accepted share in $SD/pool.out"
    fi
else
    fail shares "no message posted"
fi

# 3. B got it over the node mesh (gossip, or the xkrd sync once A and B meet); retried for a while
if [ -n "$HASH" ]; then
    ok=0
    for _ in 1 2 3 4 5 6; do
        if nice -n 19 "${SIM[@]}" --timeout 60 --invite "$B_INVITE" --expect-hash "$HASH" > "$SD/sim-mesh.out" 2>&1 < /dev/null; then
            ok=1
            break
        fi
        sleep 10
    done
    if [ "$ok" = 1 ]; then
        pass mesh "B has message $HASH (checked through B's private invite)"
    else
        fail mesh "$(tail -n 1 "$SD/sim-mesh.out")"
    fi
else
    fail mesh "no message posted"
fi

# 4. private-node topic of A (invite firewall: only A's key is accepted)
sim private --invite "$A_INVITE" --expect-address "$A_ADDR"

# 5. the pool survives a restart of A
if [ -n "$HASH" ]; then
    unwatch
    stop_proc hugin-a 20
    start_a
    watch hugin-a hugin-b
    wait_count "$SD/a.log" 6 "Hugin topic announced" 120 || true
    sim restart --invite "$A_INVITE" --expect-hash "$HASH"
else
    fail restart "no message posted"
fi

if [ "$HUGIN_JOBS" = fake ]; then
    note "pool submits: $(grep -c '^SUBMIT ok' "$SD/pool.out" || true) ok, $(grep -c '^SUBMIT bad' "$SD/pool.out" || true) bad"
else
    note "p2pool blocks from Hugin shares: $(pool_count a 'found a mainchain block')"
fi
plain "$SD/a.log" | grep -E "Hugin|p2pool|stratum" | tail -n 15 || true
finish
