# shellcheck shell=bash
# testnet/lib.sh: shared helpers of the testnet regression suite (docs/testnet-suite.md).
#
# Sourced by testnet/suite.sh and every testnet/scenarios/NN-*.sh. Nothing here builds anything:
# a missing binary or image is reported with the exact build command and the scenario exits 2.
#
# Conventions
#   * every process runs under `nice -n 19` (inside containers too);
#   * every C++ container runs with `--network host` (the host firewall blocks docker-bridge ->
#     host) and binds 127.0.0.1 only; all suite ports are in the table below;
#   * all state lives under the suite work dir <wd>: <wd>/<scenario>/ per scenario (wiped when the
#     scenario starts), <wd>/chain/ (the shared seed chain and xkrd "S"), <wd>/wallets/,
#     <wd>/pids/, <wd>/checks.tsv;
#   * result lines: `PASS <scenario>/<check>: <text>` (also FAIL, SKIP, MISSING), one per check,
#     mirrored into <wd>/checks.tsv as `scenario<TAB>check<TAB>status<TAB>unix time<TAB>text`;
#   * exit status of a scenario: 0 all checks passed, 1 a check failed, 2 a prerequisite is
#     missing, 124/130/143 timeout or interrupt (the suite reports those as FAIL).

if [ -n "${XKR_SUITE_LIB:-}" ]; then
    return 0
fi
XKR_SUITE_LIB=1

if [ -z "${BASH_VERSINFO:-}" ] || [ "${BASH_VERSINFO[0]}" -lt 4 ]; then
    echo "bash >= 4 required" >&2
    exit 2
fi
set -o errtrace

TESTNET_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$TESTNET_DIR/.." && pwd)
export NO_COLOR=1

# ------------------------------------------------------------------ binaries and images

XKRD=${XKRD:-$ROOT/target/release/xkrd}
XKR_TESTWALLET=${XKR_TESTWALLET:-$ROOT/target/release/xkr-testwallet}
XKR_MINER=${XKR_MINER:-$ROOT/target/release/xkr-miner}
NODE=${NODE:-node}
CPP_OLD_IMAGE=${CPP_OLD_IMAGE:-xkr-cpp-testnet:master}
CPP_FORK13_IMAGE=${CPP_FORK13_IMAGE:-xkr-cpp-testnet:fork13}
HUGIN_NODE_MODULES=${HUGIN_NODE_MODULES:-$ROOT/../hugin-desktop/node_modules}
# kryptokrona-crypto 2.x for the app simulator's share search (native hashing); unset = kryptokrona-utils
if [ -z "${KRYPTOKRONA_CRYPTO:-}" ] && [ -f "$ROOT/../kryptokrona-crypto/package.json" ]; then
    KRYPTOKRONA_CRYPTO=$(cd "$ROOT/../kryptokrona-crypto" && pwd)
fi
export KRYPTOKRONA_CRYPTO
export HUGIN_NODE_MODULES

BUILD_XKRD="scripts/build-bare-kit.sh (once), then nice -n 19 cargo build --release -j 3 -p xkrd   (builds xkrd and xkr-testwallet)"
BUILD_MINER="nice -n 19 cargo build --release -j 3 -p xkr-miner"
BUILD_OLD_IMAGE="XKR_CPP_IMAGE=$CPP_OLD_IMAGE XKR_CPP_PATCHES=none testnet/build-cpp.sh"
BUILD_FORK13_IMAGE="XKR_CPP_IMAGE=$CPP_FORK13_IMAGE XKR_CPP_PATCHES=0001-fork13-testnet.patch testnet/build-cpp.sh"

# ------------------------------------------------------------------ knobs

LOG_LEVEL=${LOG_LEVEL:-info}
TIMEOUT=${TIMEOUT:-600}            # seconds for each wait inside a scenario
SEED_BLOCKS=${SEED_BLOCKS:-100}    # blocks the C++ seed node mines from genesis
DB_CACHE_MB=${DB_CACHE_MB:-64}     # --db-cache-mb of every suite xkrd (memory cap)
KEEP=${KEEP:-0}                    # 1: leave the scenario's nodes running at exit

# NixOS: the .bare addons that xkrd's Bare workers load (sodium-native, udx-native) link
# libstdc++.so.6, which is not on the default library path there. Started xkrd processes get
# gcc's lib directory on LD_LIBRARY_PATH unless it is there already.
XKRD_ENV=()
if [ -e /etc/NIXOS ] && command -v gcc >/dev/null 2>&1; then
    _stdcxx=$(gcc -print-file-name=libstdc++.so.6 2>/dev/null || true)
    case "$_stdcxx" in
        /*)
            _stdcxx_dir=$(dirname "$_stdcxx")
            case ":${LD_LIBRARY_PATH:-}:" in
                *":$_stdcxx_dir:"*) ;;
                *) XKRD_ENV=(env "LD_LIBRARY_PATH=$_stdcxx_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}") ;;
            esac
            ;;
    esac
fi

# ------------------------------------------------------------------ port table
# (docs/testnet-suite.md, "Ports"). Everything binds 127.0.0.1. The compose testnet
# (testnet/docker-compose.yml) uses 11898-13899 and is not used by the suite.

PORT_SEED_P2P=16898; PORT_SEED_RPC=16899    # 10-14, 20, 50: C++ seed node (old image)
PORT_S_P2P=17898; PORT_S_RPC=17899          # 10-14, 50: xkrd "S" on the seed chain
PORT_FRESH_P2P=18898; PORT_FRESH_RPC=18899  # 11: fresh C++ node (old image)
PORT_OLD_P2P=19898; PORT_OLD_RPC=19899      # 30: old C++ node (old image, fork index 12)
PORT_A_P2P=14898; PORT_A_RPC=14899          # 30, 31: xkrd A
PORT_B_RPC=14889                            # 31: xkrd B (no P2P listener)
PORT_CPP_P2P=15898; PORT_CPP_RPC=15899      # 30: patched C++ node, 31: fork-13 C++ node
PORT_HA_P2P=21897; PORT_HA_RPC=21898        # 20: xkrd A (Levin + Hyperswarm)
PORT_HB_RPC=21888                           # 20: xkrd B (Hyperswarm only)
PORT_PR_A_P2P=23898; PORT_PR_A_RPC=23899    # 33: xkrd A (full, the long chain)
PORT_PR_P_P2P=23888; PORT_PR_P_RPC=23889    # 33: xkrd P (stripe-pruned copy of A)
PORT_PR_F_RPC=23879                         # 33: xkrd F (fresh, no P2P listener)
PORT_PR_CPP_P2P=23868; PORT_PR_CPP_RPC=23869 # 33: fresh C++ node (old image) syncing from P
# testnet/fixtures/capture-*.sh (wallet fixtures, not suite scenarios; testnet/fixtures/lib-wallet.sh)
PORT_FX_P2P=24898; PORT_FX_RPC=24899        # fixtures: xkrd X, the fixture chain (own genesis)
PORT_FX_CPP_P2P=24888; PORT_FX_CPP_RPC=24889 # fixtures: C++ node following X (walletd, xkrwallet)
PORT_FX_GHCR_P2P=24878; PORT_FX_GHCR_RPC=24879 # fixtures: wallet-tools image's node (follow check)
PORT_FX_API=24070; PORT_FX_API_CORS=24071   # fixtures: C++ wallet-api, plain / --enable-cors
PORT_FX_WALLETD=24080; PORT_FX_WALLETD_ALT=24081 # fixtures: kryptokrona-service, main / legacy+CORS
PORT_FX_STUB=24090                          # fixtures: stub daemon for the C++ miner benchmark
# 60-64: the Rust wallet tools next to the C++ ones (testnet/fixtures/lib-wallet.sh, wl_*); each
# scenario has its own xkrd X and its own block, so they can run next to each other
PORT_W60_P2P=26098; PORT_W60_RPC=26099      # 60: xkrd X
PORT_W60_CPP_API=26070; PORT_W60_RUST_API=26071 # 60: C++ wallet-api / xkr-wallet-api
PORT_W61_P2P=26198; PORT_W61_RPC=26199      # 61: xkrd X
PORT_W61_CPP_WD=26180; PORT_W61_RUST_WD=26181 # 61: C++ kryptokrona-service / xkr-walletd
PORT_W62_P2P=26298; PORT_W62_RPC=26299      # 62: xkrd X
PORT_W62_CPP_API=26270; PORT_W62_RUST_API=26271 # 62: C++ wallet-api / xkr-wallet-api
PORT_W62_CPP_WD=26280; PORT_W62_RUST_WD=26281 # 62: C++ kryptokrona-service / xkr-walletd
PORT_W63_P2P=26398; PORT_W63_RPC=26399      # 63: xkrd X (fork 13 and fork 14 scheduled)
PORT_W63_CPP_API=26370; PORT_W63_RUST_API=26371 # 63: C++ wallet-api / xkr-wallet-api
PORT_W64_P2P=26498; PORT_W64_RPC=26499      # 64: xkrd X (terminal wallets)
DEAD_PEER=127.0.0.1:9                       # nothing listens: "connect to nobody"
SUITE_PORTS="$PORT_SEED_P2P $PORT_SEED_RPC $PORT_S_P2P $PORT_S_RPC $PORT_FRESH_P2P $PORT_FRESH_RPC
$PORT_OLD_P2P $PORT_OLD_RPC $PORT_A_P2P $PORT_A_RPC $PORT_B_RPC $PORT_CPP_P2P $PORT_CPP_RPC
$PORT_HA_P2P $PORT_HA_RPC $PORT_HB_RPC $PORT_PR_A_P2P $PORT_PR_A_RPC $PORT_PR_P_P2P $PORT_PR_P_RPC
$PORT_PR_F_RPC $PORT_PR_CPP_P2P $PORT_PR_CPP_RPC
$PORT_FX_P2P $PORT_FX_RPC $PORT_FX_CPP_P2P $PORT_FX_CPP_RPC $PORT_FX_GHCR_P2P $PORT_FX_GHCR_RPC
$PORT_FX_API $PORT_FX_API_CORS $PORT_FX_WALLETD $PORT_FX_WALLETD_ALT $PORT_FX_STUB
$PORT_W60_P2P $PORT_W60_RPC $PORT_W60_CPP_API $PORT_W60_RUST_API
$PORT_W61_P2P $PORT_W61_RPC $PORT_W61_CPP_WD $PORT_W61_RUST_WD
$PORT_W62_P2P $PORT_W62_RPC $PORT_W62_CPP_API $PORT_W62_RUST_API $PORT_W62_CPP_WD $PORT_W62_RUST_WD
$PORT_W63_P2P $PORT_W63_RPC $PORT_W63_CPP_API $PORT_W63_RUST_API $PORT_W64_P2P $PORT_W64_RPC"

SEED_RPC=127.0.0.1:$PORT_SEED_RPC
S_RPC=127.0.0.1:$PORT_S_RPC

# containers (all carry the label xkr-suite=1; 90-cleanup removes every labelled container)
SUITE_LABEL=xkr-suite
C_SEED=xkr-suite-seed
C_FRESH=xkr-suite-fresh
C_MINER=xkr-suite-miner

# ------------------------------------------------------------------ state

declare -A PID=()
_PROCS=()          # process names started by this scenario (stopped at exit)
_CONTAINERS=()     # containers started by this scenario (removed at exit)
_LOG_CONTAINERS=() # shared containers whose logs are saved at exit (not removed)
_WATCH=()          # processes that must stay alive while waiting
PASSES=0
FAILS=0
SKIPS=0
MISSINGS=0
_ERR_AT=""
WD=""
SCEN=""
SD=""

# suite_init <work dir> [scenario name]: sets WD, SCEN, SD (wiped), traps.
suite_init() {
    local wd=${1:-}
    if [ -z "$wd" ]; then
        echo "usage: $0 <work dir>" >&2
        exit 2
    fi
    mkdir -p "$wd"
    WD=$(cd "$wd" && pwd)
    SCEN=${2:-$(basename "$0" .sh)}
    SD=$WD/$SCEN
    rm -rf "$SD"
    mkdir -p "$SD" "$WD/pids"
    CHECKS_TSV=$WD/checks.tsv
    SCEN_START=$(date +%s)
    trap '_ERR_AT="${BASH_SOURCE[0]##*/}:$LINENO"' ERR
    trap _suite_exit EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    echo "=== $SCEN  $(date -u +%Y-%m-%dT%H:%M:%SZ)  work dir $WD"
}

# ------------------------------------------------------------------ results

_clean_text() { printf '%s' "$*" | tr '\t\r\n' '   '; }
_record() { # <status> <check> <text...>
    local st=$1 id=$2
    shift 2
    local msg
    msg=$(_clean_text "$*")
    printf '%-7s %s/%s: %s\n' "$st" "$SCEN" "$id" "$msg"
    if [ -n "${CHECKS_TSV:-}" ]; then
        printf '%s\t%s\t%s\t%s\t%s\n' "$SCEN" "$id" "$st" "$(date +%s)" "$msg" >> "$CHECKS_TSV"
    fi
}
pass() { PASSES=$((PASSES + 1)); _record PASS "$@"; }
fail() { FAILS=$((FAILS + 1)); _record FAIL "$@"; }
skip() { SKIPS=$((SKIPS + 1)); _record SKIP "$@"; }
note() { echo "        $*"; }
step() { echo "--- $*"; }
# die <check> <text>: a FAIL that ends the scenario
die() {
    fail "$@"
    echo "aborting $SCEN"
    exit 1
}
# missing <check> <what is missing> <how to get it>: records MISSING; `missing_exit` exits 2
missing() {
    MISSINGS=$((MISSINGS + 1))
    _record MISSING "$1" "$2; get it with: $3"
}
missing_exit() {
    missing "$@"
    exit 2
}
# finish: the scenario's last line
finish() {
    if [ "$MISSINGS" -gt 0 ]; then exit 2; fi
    if [ "$FAILS" -gt 0 ]; then exit 1; fi
    exit 0
}

_suite_exit() {
    local rc=$?
    trap - EXIT INT TERM ERR
    set +e
    if declare -F scenario_cleanup >/dev/null; then
        scenario_cleanup
    fi
    _save_container_logs
    if [ "$KEEP" = 1 ]; then
        echo "KEEP=1: left running: processes [${_PROCS[*]-}] containers [${_CONTAINERS[*]-}] (testnet/suite.sh $WD 90 cleans up)"
    else
        _stop_own
    fi
    if [ "$rc" = 0 ] && [ "$FAILS" -gt 0 ]; then rc=1; fi
    if [ "$rc" = 0 ] && [ "$MISSINGS" -gt 0 ]; then rc=2; fi
    if [ "$rc" != 0 ] && [ "$rc" != 2 ] && [ "$FAILS" = 0 ]; then
        local why="exit status $rc"
        case $rc in
            124 | 143) why="terminated (suite timeout or SIGTERM, exit $rc)" ;;
            130) why="interrupted (exit 130)" ;;
        esac
        FAILS=1
        _record FAIL exit "scenario aborted: $why${_ERR_AT:+, last failing command at $_ERR_AT}"
    fi
    local verdict=PASS
    if [ "$rc" = 2 ]; then verdict=MISSING; elif [ "$rc" != 0 ]; then verdict=FAIL; fi
    echo "=== $SCEN: $verdict ($PASSES passed, $FAILS failed, $SKIPS skipped, $(($(date +%s) - SCEN_START)) s; logs in $SD)"
    exit "$rc"
}

# ------------------------------------------------------------------ prerequisites

require_cmd() {
    local c
    for c in "$@"; do
        command -v "$c" >/dev/null 2>&1 || missing_exit prereq "command $c not found" "install $c"
    done
}
require_xkrd() {
    [ -x "$XKRD" ] || missing_exit prereq "xkrd not found at $XKRD" "$BUILD_XKRD (or set XKRD)"
}
# xkrd_finds_bare_kit: xkrd starts, i.e. the loader finds libbare-kit (the Bare workers of its
# Hyperswarm transport)
xkrd_finds_bare_kit() {
    ${XKRD_ENV[@]+"${XKRD_ENV[@]}"} "$XKRD" --version >/dev/null 2>&1
}
# require_hyper: what the Hyperswarm scenarios (20, 40) need from xkrd
require_hyper() {
    xkrd_finds_bare_kit || missing_exit prereq \
        "$XKRD does not start (libbare-kit not found)" \
        "scripts/build-bare-kit.sh, then $BUILD_XKRD"
}
require_testwallet() {
    [ -x "$XKR_TESTWALLET" ] || missing_exit prereq "xkr-testwallet not found at $XKR_TESTWALLET" "$BUILD_XKRD (or set XKR_TESTWALLET)"
}
require_miner() {
    [ -x "$XKR_MINER" ] || missing_exit prereq "xkr-miner not found at $XKR_MINER" "$BUILD_MINER (or set XKR_MINER)"
}
require_docker() {
    require_cmd docker
    docker info >/dev/null 2>&1 || missing_exit prereq "docker daemon not reachable" "start docker / join the docker group"
}
require_image() { # <image> <build command>
    require_docker
    docker image inspect "$1" >/dev/null 2>&1 || missing_exit prereq "docker image $1 missing" "$2"
}
require_node() {
    command -v "$NODE" >/dev/null 2>&1 || missing_exit prereq "node not found ($NODE)" "install Node.js >= 22 (or set NODE)"
}
require_bridge() {
    [ -d "$ROOT/bridge/node_modules" ] || missing_exit prereq "bridge/node_modules missing" "cd bridge && sfw npm ci"
}
port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
require_ports_free() {
    local p
    for p in "$@"; do
        if port_busy "$p"; then
            die ports "port $p is in use (an earlier run? testnet/suite.sh $WD 90; the compose testnet? testnet/down.sh)"
        fi
    done
}

# ------------------------------------------------------------------ processes

_remember_proc() {
    local x
    for x in ${_PROCS[@]+"${_PROCS[@]}"}; do
        [ "$x" = "$1" ] && return 0
    done
    _PROCS+=("$1")
}
_pid_is_ours() { # a pid from a pid file is still one of our processes (not a recycled pid)
    [ -n "$1" ] && [ -r "/proc/$1/cmdline" ] \
        && tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null \
        | grep -qE 'xkrd|xkr-testwallet|xkr-miner|xkr-wallet|xkrwallet|kryptokrona-service|wallet_sync|wallet_check|node|\.js'
}
# start_bg <name> <log> <command...>: nice -n 19, output appended to <log>; sets LAST_PID
start_bg() {
    local name=$1 log=$2
    shift 2
    stop_proc "$name"
    nice -n 19 "$@" >> "$log" 2>&1 < /dev/null &
    LAST_PID=$!
    PID[$name]=$LAST_PID
    echo "$LAST_PID" > "$WD/pids/$name.pid"
    _remember_proc "$name"
}
proc_alive() {
    local pid=${PID[$1]:-}
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}
# stop_proc <name> [seconds]: SIGTERM, then SIGKILL after <seconds> (30). Sets STOP_STATUS (exit
# status, 127 = unknown), STOP_SECS and STOP_KILLED (1 if it needed SIGKILL).
stop_proc() {
    local name=$1 secs=${2:-30} pid="" t0 i
    STOP_STATUS=""
    STOP_SECS=""
    STOP_KILLED=0
    pid=${PID[$name]:-}
    if [ -z "$pid" ] && [ -n "$WD" ] && [ -f "$WD/pids/$name.pid" ]; then
        pid=$(cat "$WD/pids/$name.pid" 2>/dev/null || true)
    fi
    [ -n "$WD" ] && rm -f "$WD/pids/$name.pid"
    unset "PID[$name]"
    [ -n "$pid" ] || return 0
    if ! kill -0 "$pid" 2>/dev/null || ! _pid_is_ours "$pid"; then
        # already gone: collect its status if it was our child
        if wait "$pid" 2>/dev/null; then STOP_STATUS=0; else STOP_STATUS=$?; fi
        return 0
    fi
    t0=$(date +%s)
    kill -TERM "$pid" 2>/dev/null || true
    for ((i = 0; i < secs * 2; i++)); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.5
    done
    if kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid" 2>/dev/null || true
        STOP_KILLED=1
    fi
    if wait "$pid" 2>/dev/null; then STOP_STATUS=0; else STOP_STATUS=$?; fi
    STOP_SECS=$(($(date +%s) - t0))
    return 0
}
# kill_proc <name>: SIGKILL at once (crash test)
kill_proc() {
    local pid=${PID[$1]:-}
    [ -n "$pid" ] || return 0
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    unset "PID[$1]"
    rm -f "$WD/pids/$1.pid"
}
# reclaim <name...>: stop same-named processes left by an earlier scenario (e.g. with --keep)
reclaim() {
    local n
    for n in "$@"; do stop_proc "$n"; done
}
# watch <name...>: wait loops die if one of these exits
watch() { _WATCH+=("$@"); }
unwatch() { _WATCH=(); }
check_alive() {
    local n
    for n in ${_WATCH[@]+"${_WATCH[@]}"}; do
        if [ -n "${PID[$n]:-}" ] && ! proc_alive "$n"; then
            echo "process $n exited; last lines of its log:"
            tail -n 20 "${LOGOF[$n]:-/dev/null}" 2>/dev/null || true
            die alive "process $n exited unexpectedly"
        fi
    done
}
declare -A LOGOF=()

_stop_own() {
    local i n c
    for ((i = ${#_PROCS[@]} - 1; i >= 0; i--)); do
        n=${_PROCS[$i]}
        stop_proc "$n"
    done
    for c in ${_CONTAINERS[@]+"${_CONTAINERS[@]}"}; do
        rm_container "$c"
    done
}

# ------------------------------------------------------------------ xkrd

# start_xkrd <name> <rpc port|0> <data dir> <log> [xkrd args...]: testnet, nice -n 19, waits for
# the RPC (unless the port is 0). The caller passes P2P, peer and fork flags.
start_xkrd() {
    local name=$1 port=$2 dir=$3 log=$4
    shift 4
    mkdir -p "$dir"
    LOGOF[$name]=$log
    start_bg "$name" "$log" ${XKRD_ENV[@]+"${XKRD_ENV[@]}"} "$XKRD" --network testnet --data-dir "$dir" --rpc-bind-port "$port" \
        --log-level "$LOG_LEVEL" --db-cache-mb "$DB_CACHE_MB" "$@"
    [ "$port" = 0 ] && return 0
    wait_rpc "127.0.0.1:$port" "$name"
}
wait_rpc() { # <host:port> <process name>
    local i
    for ((i = 0; i < 120; i++)); do
        [ -n "$(height_of "$1")" ] && return 0
        if ! proc_alive "$2"; then
            tail -n 20 "${LOGOF[$2]:-/dev/null}" 2>/dev/null || true
            die start "$2 exited before its RPC $1 came up (log ${LOGOF[$2]:-?})"
        fi
        sleep 0.5
    done
    die start "RPC $1 of $2 did not come up within 60 s (log ${LOGOF[$2]:-?})"
}
xkrd_version() { "$XKRD" --version 2>/dev/null | head -n 1 || true; }
# xkrd_db <log> <db args...>: `xkrd db ...` under nice; output in <log>; status of xkrd
xkrd_db() {
    local log=$1
    shift
    nice -n 19 "$XKRD" db "$@" > "$log" 2>&1 < /dev/null
}

# ------------------------------------------------------------------ C++ containers

container_running() { [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null || true)" = true ]; }
container_exists() { docker inspect "$1" >/dev/null 2>&1; }
rm_container() { nice -n 19 docker rm -f "$1" >/dev/null 2>&1 || true; }
_remember_container() {
    local x
    for x in ${_CONTAINERS[@]+"${_CONTAINERS[@]}"}; do
        [ "$x" = "$1" ] && return 0
    done
    _CONTAINERS+=("$1")
}
_use_shared() { # a shared container whose log this scenario saves (not removed at exit)
    _LOG_CONTAINERS+=("$1")
}
_save_container_logs() {
    local c
    # removed containers (the miner) already appended their log in mine_cpp
    for c in ${_CONTAINERS[@]+"${_CONTAINERS[@]}"}; do
        container_exists "$c" && { docker logs "$c" > "$SD/$c.log" 2>&1 || true; }
    done
    for c in ${_LOG_CONTAINERS[@]+"${_LOG_CONTAINERS[@]}"}; do
        container_exists "$c" && { docker logs --since "$SCEN_START" "$c" > "$SD/$c.log" 2>&1 || true; }
    done
    return 0
}
# start_cpp <container> <image> <p2p port> <rpc port> <ENV=value|-> [kryptokronad args...]
# Testnet C++ daemon, --network host, data on tmpfs, 2 CPUs / 1 GiB, nice -n 19, log level 2.
# Pass --add-exclusive-node (the compiled-in testnet seed is 127.0.0.1:11898). With
# CPP_SHARED=1 the container outlives the scenario (removed by 90-cleanup).
start_cpp() {
    local c=$1 image=$2 p2p=$3 rpc=$4 env=$5 i
    shift 5
    local envargs=()
    [ "$env" = - ] || envargs=(-e "$env")
    rm_container "$c"
    nice -n 19 docker run -d --name "$c" --label "$SUITE_LABEL=1" --network host \
        --cpus 2 --memory 1g --tmpfs /data:rw,size=512m ${envargs[@]+"${envargs[@]}"} \
        "$image" nice -n 19 /usr/src/kryptokrona/kryptokronad --data-dir=/data \
        --p2p-bind-ip=127.0.0.1 --p2p-bind-port="$p2p" \
        --rpc-bind-ip=127.0.0.1 --rpc-bind-port="$rpc" \
        --allow-local-ip --no-console --log-level=2 "$@" > /dev/null \
        || die start "cannot start container $c from $image"
    if [ "${CPP_SHARED:-0}" = 1 ]; then _use_shared "$c"; else _remember_container "$c"; fi
    for ((i = 0; i < 120; i++)); do
        [ -n "$(height_of "127.0.0.1:$rpc")" ] && return 0
        container_running "$c" || { docker logs --tail 20 "$c" 2>&1 || true; die start "container $c exited"; }
        sleep 0.5
    done
    die start "RPC 127.0.0.1:$rpc of $c did not come up within 60 s"
}
# run_cpp_miner <container> <daemon host:port> <address> <blocks>: the bundled C++ miner
# (getblocktemplate/submitblock), detached, stops by itself after <blocks> submitted blocks
run_cpp_miner() {
    local c=$1 daemon=$2 addr=$3 n=$4
    rm_container "$c"
    nice -n 19 docker run -d --name "$c" --label "$SUITE_LABEL=1" --network host \
        --cpus 1 --memory 512m "$CPP_OLD_IMAGE" nice -n 19 /usr/src/kryptokrona/miner \
        --daemon-address="$daemon" --address="$addr" --threads=1 --scan-time=1 --limit="$n" > /dev/null \
        || die miner "cannot start the C++ miner container $c"
    _remember_container "$c"
}
# container_exit_code <container> <seconds>: waits for it to stop; prints the exit code or ""
container_exit_code() {
    local c=$1 secs=$2 i
    for ((i = 0; i < secs; i++)); do
        container_running "$c" || break
        sleep 1
    done
    container_running "$c" && return 0
    docker inspect -f '{{.State.ExitCode}}' "$c" 2>/dev/null || true
}
# mine_cpp <daemon host:port> <address> <blocks> [container]: C++ miner, waits until the daemon
# is <blocks> higher (TIMEOUT, or 10 s after the miner stopped); 1 if it did not get there.
# Sets MINER_EXIT (the miner's exit code), appends its log to $SD/<container>.log, removes it.
mine_cpp() {
    local daemon=$1 addr=$2 n=$3 c=${4:-$C_MINER} h0 target deadline stopped=""
    MINER_EXIT=""
    h0=$(height_of "$daemon")
    [ -n "$h0" ] || return 1
    target=$((h0 + n))
    run_cpp_miner "$c" "$daemon" "$addr" "$n"
    deadline=$(($(date +%s) + TIMEOUT))
    while ! height_at_least "$daemon" "$target"; do
        if ! container_running "$c"; then
            if [ -z "$stopped" ]; then
                stopped=$(date +%s)
            elif [ $(($(date +%s) - stopped)) -ge 10 ]; then
                break
            fi
        fi
        [ "$(date +%s)" -ge "$deadline" ] && break
        check_alive
        sleep 1
    done
    # Once the chain is where we want it we stop the miner container ourselves and report exit 0;
    # a miner that died on its own keeps its real exit code.
    if container_running "$c"; then
        if height_at_least "$daemon" "$target"; then
            docker stop -t 5 "$c" >/dev/null 2>&1 || true
            MINER_EXIT=0
        else
            MINER_EXIT=$(container_exit_code "$c" 30)
        fi
    else
        MINER_EXIT=$(container_exit_code "$c" 30)
    fi
    docker logs "$c" >> "$SD/$c.log" 2>&1 || true
    rm_container "$c"
    height_at_least "$daemon" "$target"
}

# ------------------------------------------------------------------ RPC and JSON
# Every helper prints nothing when there is no answer or no match and always succeeds, so
# `x=$(helper ...)` never trips `set -e` / `pipefail`.

# rpc <host:port> <method> [params]: JSON-RPC 2.0 on /json_rpc
rpc() {
    local params=${3:-"{}"}
    curl -s --max-time 10 -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"$2\",\"params\":$params}" \
        "http://$1/json_rpc" 2>/dev/null || true
}
# post <host:port> <path> [body]
post() {
    curl -s --max-time 10 -H 'Content-Type: application/json' -d "${3:-"{}"}" \
        "http://$1$2" 2>/dev/null || true
}
# get <host:port> <path>
get() { curl -s --max-time 10 "http://$1$2" 2>/dev/null || true; }
flat() { printf '%s' "$1" | tr -d ' \n\r\t'; }
# first numeric / string / array / boolean value of a key in a JSON text
num() { { flat "$1" | grep -o "\"$2\":[0-9]*" || true; } | head -n 1 | cut -d: -f2; }
str() { { flat "$1" | grep -o "\"$2\":\"[^\"]*\"" || true; } | head -n 1 | cut -d'"' -f4; }
arr() { { flat "$1" | grep -o "\"$2\":\[[^]]*\]" || true; } | head -n 1 | cut -d: -f2-; }
bool() { { flat "$1" | grep -o "\"$2\":\(true\|false\)" || true; } | head -n 1 | cut -d: -f2; }
# a flat (not nested) object of a key, e.g. obj "$status" mining -> {"active":true,...}
obj() { { flat "$1" | grep -o "\"$2\":{[^{}]*}" || true; } | head -n 1 | cut -d: -f2-; }
# number of matches of a regular expression
count() { { flat "$1" | grep -o "$2" || true; } | wc -l | tr -d ' '; }

# top_of <rpc>: "height hash major" of the top block (height = top index), or ""
top_of() {
    local r h
    r=$(rpc "$1" getlastblockheader)
    h=$(num "$r" height)
    [ -n "$h" ] || return 0
    echo "$h $(str "$r" hash) $(num "$r" major_version)"
}
height_of() { local t; t=$(top_of "$1"); echo "${t%% *}"; }
top_hash_of() { local t; t=$(top_of "$1"); t=${t#* }; echo "${t%% *}"; }
# header_at <rpc> <height>: "hash major" or ""
header_at() {
    local r h
    r=$(rpc "$1" getblockheaderbyheight "{\"height\":$2}")
    h=$(str "$r" hash)
    [ -n "$h" ] || return 0
    echo "$h $(num "$r" major_version)"
}
header_hash_at() { local t; t=$(header_at "$1" "$2"); echo "${t%% *}"; }
# header_fields_at <rpc> <height>: the fields the differential walk compares
CMP_FIELDS=${CMP_FIELDS:-"major_version minor_version timestamp difficulty reward num_txes block_size"}
header_fields_at() {
    local r f out
    r=$(rpc "$1" getblockheaderbyheight "{\"height\":$2}")
    out=$(str "$r" hash)
    [ -n "$out" ] || return 0
    for f in $CMP_FIELDS; do
        out="$out $f=$(num "$r" "$f")"
    done
    echo "$out"
}
# compare_chains <rpc A> <rpc B> <from> <to>: every header field in CMP_FIELDS and the hash
# equal; sets CMP_MISMATCHES and CMP_FIRST (first difference)
compare_chains() {
    local a=$1 b=$2 from=$3 to=$4 h fa fb
    CMP_MISMATCHES=0
    CMP_FIRST=""
    for ((h = from; h <= to; h++)); do
        fa=$(header_fields_at "$a" "$h")
        fb=$(header_fields_at "$b" "$h")
        if [ -z "$fa" ] || [ "$fa" != "$fb" ]; then
            CMP_MISMATCHES=$((CMP_MISMATCHES + 1))
            [ -n "$CMP_FIRST" ] || CMP_FIRST="height $h: [$fa] vs [$fb]"
        fi
    done
    [ "$CMP_MISMATCHES" = 0 ]
}

# ------------------------------------------------------------------ waiting

# wait_for <seconds> <command...>: until the command succeeds (polls every second, checks the
# watched processes); 1 on timeout
wait_for() {
    local deadline
    deadline=$(($(date +%s) + $1))
    shift
    while :; do
        if "$@"; then return 0; fi
        [ "$(date +%s)" -ge "$deadline" ] && return 1
        check_alive
        sleep 1
    done
}
height_at_least() { local h; h=$(height_of "$1"); [ -n "$h" ] && [ "$h" -ge "$2" ]; }
same_top() {
    local a b
    a=$(top_of "$1")
    b=$(top_of "$2")
    [ -n "$a" ] && [ "${a% *}" = "${b% *}" ]
}
# wait_height <rpc> <height> [seconds]: top index >= height
wait_height() { wait_for "${3:-$TIMEOUT}" height_at_least "$1" "$2"; }
# wait_same_top <rpc A> <rpc B> [seconds]: same top height and hash
wait_same_top() { wait_for "${3:-$TIMEOUT}" same_top "$1" "$2"; }
# log_has <file> <fixed text>
log_has() { grep -aqF -- "$2" "$1" 2>/dev/null; }
log_count() { { grep -acF -- "$2" "$1" 2>/dev/null || true; } | head -n 1; }

# ------------------------------------------------------------------ wallets and transactions

# wallet <wallet dir> <daemon> args...
wallet() {
    local dir=$1 daemon=$2
    shift 2
    nice -n 19 "$XKR_TESTWALLET" --wallet-dir "$dir" --daemon "$daemon" "$@"
}
wallet_addr() { nice -n 19 "$XKR_TESTWALLET" --wallet-dir "$1" address 2> /dev/null | tail -n 1 || true; }
# unlocked balance in atomic units of a wallet through a daemon ("" if the scan failed)
unlocked_of() {
    local out
    out=$(wallet "$1" "$2" balance 2> /dev/null || true)
    { printf '%s\n' "$out" | grep '^unlocked [0-9]' || true; } | awk '{print $2}' | tr -d '.' | sed 's/^0*\([0-9]\)/\1/'
}
funds_at_least() { local u; u=$(unlocked_of "$1" "$2"); [ -n "$u" ] && [ "$u" -ge "$3" ]; }
# wait_funds <wallet> <daemon> <atomic units> [seconds]
wait_funds() { wait_for "${4:-$TIMEOUT}" funds_at_least "$1" "$2" "$3"; }
# send_tx <from wallet> <daemon> <to address> <label> <wallet send args...>: output in
# $SD/send-<label>.out; prints the transaction hash; 1 if the wallet or the daemon refused
send_tx() {
    local from=$1 daemon=$2 to=$3 label=$4
    shift 4
    local out="$SD/send-$label.out"
    if wallet "$from" "$daemon" send "$to" "$@" > "$out" 2>&1; then
        { grep -o '^transaction [0-9a-f]*' "$out" || true; } | head -n 1 | cut -d' ' -f2
        return 0
    fi
    return 1
}
tx_status() { post "$1" /get_transactions_status "{\"transactionHashes\":[\"$2\"]}"; }
tx_in_pool() { flat "$(tx_status "$1" "$2")" | grep -q "\"transactionsInPool\":\[\"$2\"\]"; }
tx_in_block() { flat "$(tx_status "$1" "$2")" | grep -q "\"transactionsInBlock\":\[\"$2\"\]"; }
tx_json() { rpc "$1" f_transaction_json "{\"hash\":\"$2\"}"; }
# wait_mined <xkrd rpc> <tx hash> [seconds]: prints the block height (xkrd f_transaction_json)
wait_mined() {
    wait_for "${3:-$TIMEOUT}" tx_in_block "$1" "$2" || return 1
    num "$(tx_json "$1" "$2")" height
}

# ------------------------------------------------------------------ the shared seed chain
#
# 10-14, 20 and 50 share one chain: the C++ seed node (old image, container xkr-suite-seed,
# P2P 16898, RPC 16899, no peers of its own) mines SEED_BLOCKS blocks from genesis with the
# bundled C++ miner to wallet W1; xkrd "S" (data dir <wd>/chain/xkrd, P2P 17898, RPC 17899)
# follows only the seed (--add-exclusive-node). <wd>/chain/seed.id ("<hash of block 1> <W1>")
# ties S's data to this seed; a new seed wipes <wd>/chain.

# ensure_wallets: W1 (seed-chain miner, sender) and W2 (receiver) in <wd>/wallets
ensure_wallets() {
    require_testwallet
    W1_DIR=$WD/wallets/w1
    W2_DIR=$WD/wallets/w2
    W1=$(wallet_addr "$W1_DIR")
    W2=$(wallet_addr "$W2_DIR")
    [ -n "$W1" ] && [ -n "$W2" ] || die setup "xkr-testwallet did not create the suite wallets in $WD/wallets"
}
# ensure_seed: reuse the running seed node if it matches <wd>/chain/seed.id, else create it
ensure_seed() {
    require_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE"
    local marker=$WD/chain/seed.id
    if container_running "$C_SEED" && [ -f "$marker" ] \
        && [ "$(header_hash_at "$SEED_RPC" 1) $W1" = "$(cat "$marker")" ] \
        && height_at_least "$SEED_RPC" "$SEED_BLOCKS"; then
        note "reusing seed node $C_SEED, top $(top_of "$SEED_RPC")"
        _use_shared "$C_SEED"
        SEED_CREATED=0
        return 0
    fi
    note "creating the seed chain: $C_SEED ($CPP_OLD_IMAGE) mines $SEED_BLOCKS blocks to W1"
    rm_container "$C_SEED"
    reclaim S
    rm -rf "$WD/chain"
    mkdir -p "$WD/chain"
    require_ports_free "$PORT_SEED_P2P" "$PORT_SEED_RPC"
    CPP_SHARED=1 start_cpp "$C_SEED" "$CPP_OLD_IMAGE" "$PORT_SEED_P2P" "$PORT_SEED_RPC" - \
        --add-exclusive-node="$DEAD_PEER"
    local t0
    t0=$(date +%s)
    mine_cpp "$SEED_RPC" "$W1" "$SEED_BLOCKS" \
        || die seed "the C++ miner did not mine $SEED_BLOCKS blocks on $C_SEED within ${TIMEOUT}s (log $SD/$C_MINER.log)"
    SEED_MINE_SECS=$(($(date +%s) - t0))
    echo "$(header_hash_at "$SEED_RPC" 1) $W1" > "$marker"
    SEED_CREATED=1
}
# start_suite_node <log> [xkrd args...]: xkrd S on the seed chain (exclusive peer: the seed)
start_suite_node() {
    local log=$1
    shift
    start_xkrd S "$PORT_S_RPC" "$WD/chain/xkrd" "$log" \
        --p2p-bind-ip 127.0.0.1 --p2p-bind-port "$PORT_S_P2P" \
        --add-exclusive-node "127.0.0.1:$PORT_SEED_P2P" --no-hyper "$@"
}
# ensure_suite_chain <log>: wallets, seed, S running and at the seed's top (setup, not a check)
ensure_suite_chain() {
    ensure_wallets
    ensure_seed
    reclaim S
    require_ports_free "$PORT_S_P2P" "$PORT_S_RPC"
    start_suite_node "$1"
    watch S
    wait_same_top "$S_RPC" "$SEED_RPC" \
        || die setup "xkrd S [$(top_of "$S_RPC")] did not reach the seed's top [$(top_of "$SEED_RPC")] within ${TIMEOUT}s"
    note "setup: S and the seed at $(top_of "$S_RPC")"
}

# ------------------------------------------------------------------ suite-wide cleanup

# suite_cleanup: stops every process in <wd>/pids and removes every labelled container (logs of
# shared containers are saved to <wd>/<container>.log first). Idempotent.
suite_cleanup() {
    local f pid name i alive
    for f in "$WD"/pids/*.pid; do
        [ -e "$f" ] || continue
        pid=$(cat "$f" 2>/dev/null || true)
        if _pid_is_ours "$pid"; then kill -TERM "$pid" 2>/dev/null || true; fi
    done
    for ((i = 0; i < 60; i++)); do
        alive=0
        for f in "$WD"/pids/*.pid; do
            [ -e "$f" ] || continue
            pid=$(cat "$f" 2>/dev/null || true)
            if kill -0 "$pid" 2>/dev/null && _pid_is_ours "$pid"; then alive=1; fi
        done
        [ "$alive" = 0 ] && break
        sleep 0.5
    done
    for f in "$WD"/pids/*.pid; do
        [ -e "$f" ] || continue
        pid=$(cat "$f" 2>/dev/null || true)
        name=$(basename "$f" .pid)
        if kill -0 "$pid" 2>/dev/null && _pid_is_ours "$pid"; then
            echo "cleanup: $name (pid $pid) ignored SIGTERM for 30 s, SIGKILL"
            kill -KILL "$pid" 2>/dev/null || true
        fi
        rm -f "$f"
    done
    if command -v docker >/dev/null 2>&1; then
        local ids c
        ids=$(docker ps -aq --filter "label=$SUITE_LABEL" 2>/dev/null || true)
        for c in $ids; do
            name=$(docker inspect -f '{{.Name}}' "$c" 2>/dev/null | tr -d / || true)
            [ -n "$name" ] && docker logs "$c" > "$WD/$name.log" 2>&1 || true
            nice -n 19 docker rm -f "$c" > /dev/null 2>&1 || true
        done
    fi
}
