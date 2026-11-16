# shellcheck shell=bash
# testnet/fixtures/lib-wallet.sh: shared helpers of the wallet fixture capture scripts
# (testnet/fixtures/capture-*.sh). They record what the C++ wallet tools answer so that the Rust
# wallet-api, walletd, wallet-file code, terminal wallet and miner can be tested without docker.
#
# Sourced after testnet/lib.sh (this file sources it). Conventions on top of lib.sh:
#   * every capture script starts its own fixture chain: xkrd "X" (testnet, own genesis, no peers,
#     P2P 24898 / RPC 24899), mined only with `xkr-testwallet mine --to`, so heights, rewards and
#     balances are the same on every run; only hashes, keys of random wallets and timestamps change
#     (and those are normalised, see fx_normalise);
#   * the C++ tools run from docker images (--cpus 2 --memory 1g, nice -n 19, --network host,
#     label xkr-suite, the uid/gid of the caller so their files stay removable); wallet-api,
#     kryptokrona-service and xkrwallet talk to xkrd X directly (xkrd serves /queryblockslite and
#     /get_blocks_hashes_by_timestamps); FX_WALLETD_DAEMON=cpp puts walletd and xkrwallet behind
#     a C++ kryptokronad that follows X instead (24888 / 24889);
#   * HTTP exchanges are recorded as JSON lines
#       {"name", "request": {"method", "path", "headers", "body"},
#                "response": {"status", "headers", "body"}}
#     where a body is the parsed JSON when the text is JSON, the raw text otherwise, "" when the
#     response had no body and null when the request had none; response headers without Date;
#   * raw captures and logs stay under <work dir>/<script>/; fixtures are written below
#     $FX_OUT_ROOT (default: this checkout);
#   * never builds anything: a missing binary or image is reported (MISSING) with the command
#     that provides it, and the script exits 2.
#
# The last section (wl_*, rapi_*, rwd_*, rcli_*) serves the suite scenarios 60-64, which run the
# Rust wallet tools next to the C++ ones: see "Rust next to C++" at the end of this file.

if [ -n "${XKR_FX_LIB:-}" ]; then
    return 0
fi
XKR_FX_LIB=1

FX_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
. "$FX_DIR/../lib.sh"

# ------------------------------------------------------------------ images and knobs

# FX_IMAGE: the image with the C++ wallet tools. Default: the published 2025 testnet build;
# FX_IMAGE=xkr-cpp-wallets:master selects a local testnet build of the current C++ checkout with
# every tool (wallet-api, kryptokrona-service, xkrwallet, xkrwallet-beta, miner, kryptokronad).
FX_IMAGE=${FX_IMAGE:-ghcr.io/kryptokrona/kryptokrona-testnet:latest}  # wallet-api, walletd, xkrwallet, miner
CPP_WALLETS_IMAGE=${CPP_WALLETS_IMAGE:-xkr-cpp-wallets:master}       # looked at for xkrwallet-beta
WALLETD_IMAGE=${WALLETD_IMAGE:-$FX_IMAGE}       # kryptokrona-service for capture-walletd / -wallet-files
CLI_IMAGE=${CLI_IMAGE:-$FX_IMAGE}               # xkrwallet (and xkrwallet-beta if present)
MINER_IMAGES=${MINER_IMAGES:-"$FX_IMAGE $CPP_OLD_IMAGE"}  # C++ miners compared by capture-miner
FX_FOLLOWER_IMAGE=${FX_FOLLOWER_IMAGE:-auto}    # C++ node following X: auto = FX_IMAGE if it follows, else CPP_OLD_IMAGE
FX_WALLETD_DAEMON=${FX_WALLETD_DAEMON:-xkrd}    # xkrd | cpp: daemon of kryptokrona-service and xkrwallet
FX_OUT_ROOT=${FX_OUT_ROOT:-$ROOT}
FX_BIN=/usr/src/kryptokrona
FX_HTTP_TIMEOUT=${FX_HTTP_TIMEOUT:-60}
FX_SYNC_WAIT=${FX_SYNC_WAIT:-240}
FX_FUND_TO=${FX_FUND_TO:-30}       # blocks 1..FX_FUND_TO pay the script's main wallet
FX_FILLER_TO=${FX_FILLER_TO:-130}  # then filler blocks to FX_FILLER_TO: decoys, and past the
                                   # 20-block coinbase window of the 2025 wallet builds
FX_CONFIRM=${FX_CONFIRM:-12}       # blocks mined after sends (spendable age 10 + slack)
FX_CORS=https://fixture.example
FX_API_KEY=fixture-api-key
FX_WD_RPC_PASS=fixture-rpc-pass
FX_UIDGID="$(id -u):$(id -g)"
PULL_FX_IMAGE="docker pull $FX_IMAGE   (a download of the published image, not a build)"

FX_RPC=127.0.0.1:$PORT_FX_RPC
FX_CPP_RPC=127.0.0.1:$PORT_FX_CPP_RPC
C_FX_CPP=xkr-suite-fx-cpp
C_FX_GHCR=xkr-suite-fx-ghcr
C_FX_API=xkr-suite-fx-api
C_FX_API_CORS=xkr-suite-fx-api-cors
C_FX_WD=xkr-suite-fx-walletd
C_FX_WD_ALT=xkr-suite-fx-walletd-alt
C_FX_ONCE=xkr-suite-fx-once

# ------------------------------------------------------------------ fixed key material
# Seeds and keys from the kryptokrona-wallet-backend-js tests (tests/tests.ts), so the same values
# already appear in Rust unit tests (crates/xkr-wallet-core/src/keys.rs).

FX_SEED_A="skulls woozy ouch summon gifts huts waffle ourselves obtains hexagon tadpoles hacksaw dormant hence abort listen history atom cadets stylishly snout vegan girth guest history"
FX_A_SPEND=d61a57a59318d70ff77cc7f8ad7f62887c828da1d5d3f3b0d2f7d3fa596c2904
FX_A_VIEW=688e5229df6463ec4c27f6ee11c3f1d3d4b4d2480c0aabe64fb807182cfdc801
FX_SEED_B="nugget lazy gang sonic vulture exit veteran poverty affair ringing opus soapy sonic afield dating lectures worry tuxedo ruffled rated locker bested aunt bifocals opus"
FX_SEED_BAD_CHECKSUM="nugget lazy gang sonic vulture exit veteran poverty affair ringing opus soapy sonic afield dating lectures worry tuxedo ruffled rated locker bested aunt bifocals soapy"
FX_SEED_UNKNOWN_WORD="a lazy gang sonic vulture exit veteran poverty affair ringing opus soapy sonic afield dating lectures worry tuxedo ruffled rated locker bested aunt bifocals opus"
FX_SEED_24_WORDS="nugget lazy gang sonic vulture exit veteran poverty affair ringing opus soapy sonic afield dating lectures worry tuxedo ruffled rated locker bested aunt bifocals"
# non-deterministic pair (view key not derived from the spend key)
FX_K_SPEND=55e0aa4ca65c0ae016c7364eec313f56fc162901ead0e38a9f846686ac78560f
FX_K_VIEW=1f3f6c220dd9f97619dbf44d967f79f3041b9b1c63da2c895f980f1411d5d704
FX_K_SPEND_PUB=ff9b6e048297ee435d6219005974c2c8df620a4aca9ca5c4e13f071823482029
# a valid foreign address (JS view-wallet test); filler blocks pay it
FX_DUMMY_ADDR=SEKReTRGXxc41SGrKyw5ucHKoM4nhMYazTwseWGB181H9zRB68oSApmjSaUS8yPjgEGEnH4WesJEW1zaCmEB5ykSLQZvfs7CFTU
FX_PID1=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
FX_PID2=fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210
FX_ZERO_HASH=0000000000000000000000000000000000000000000000000000000000000000
FX_UNKNOWN_HASH=abababababababababababababababababababababababababababababababab

# fx_fixed_key <label>: a fixed, valid private key (sha256 of the label with the top byte < 0x10,
# so the scalar is below the group order)
fx_fixed_key() { printf '%s0a' "$(printf 'xkr-fixture-%s' "$1" | sha256sum | cut -c1-62)"; }
FX_SUB1=$(fx_fixed_key sub1)
FX_SUB2=$(fx_fixed_key sub2)
FX_SUB3=$(fx_fixed_key sub3)

# fx_alt_address <standard address>: the same keys under the other prefix (SEKR <-> Xkr, C++
# utilities::alternateAddressForm, src/utilities/addresses.cpp); empty when the checksum is wrong
fx_alt_address() {
    [ -n "${1:-}" ] || return 0
    python3 -I - "$1" 2> /dev/null <<'PY' || true
import sys

ALPHA = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
SIZES = [0, 2, 3, 5, 6, 7, 9, 10, 11]
RC = [0x0000000000000001, 0x0000000000008082, 0x800000000000808A, 0x8000000080008000,
      0x000000000000808B, 0x0000000080000001, 0x8000000080008081, 0x8000000000008009,
      0x000000000000008A, 0x0000000000000088, 0x0000000080008009, 0x000000008000000A,
      0x000000008000808B, 0x800000000000008B, 0x8000000000008089, 0x8000000000008003,
      0x8000000000008002, 0x8000000000000080, 0x000000000000800A, 0x800000008000000A,
      0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008]
ROT = [[0, 36, 3, 41, 18], [1, 44, 10, 45, 2], [62, 6, 43, 15, 61], [28, 55, 25, 21, 56],
       [27, 20, 39, 8, 14]]
M = (1 << 64) - 1


def rol(v, n):
    return ((v << n) | (v >> (64 - n))) & M if n else v


def keccak_f(a):
    for rc in RC:
        c = [a[x][0] ^ a[x][1] ^ a[x][2] ^ a[x][3] ^ a[x][4] for x in range(5)]
        d = [c[(x - 1) % 5] ^ rol(c[(x + 1) % 5], 1) for x in range(5)]
        a = [[a[x][y] ^ d[x] for y in range(5)] for x in range(5)]
        b = [[0] * 5 for _ in range(5)]
        for x in range(5):
            for y in range(5):
                b[y][(2 * x + 3 * y) % 5] = rol(a[x][y], ROT[x][y])
        a = [[b[x][y] ^ (~b[(x + 1) % 5][y] & b[(x + 2) % 5][y]) for y in range(5)] for x in range(5)]
        a[0][0] ^= rc
    return a


def keccak256(data):
    rate = 136
    msg = bytearray(data) + b'\x01'
    while len(msg) % rate:
        msg.append(0)
    msg[-1] |= 0x80
    a = [[0] * 5 for _ in range(5)]
    for off in range(0, len(msg), rate):
        for i in range(rate // 8):
            x, y = i % 5, i // 5
            a[x][y] ^= int.from_bytes(msg[off + 8 * i:off + 8 * i + 8], 'little')
        a = keccak_f(a)
    out = b''.join(a[i % 5][i // 5].to_bytes(8, 'little') for i in range(4))
    return out


def enc_block(b):
    n = int.from_bytes(b, 'big')
    s = ''
    for _ in range(SIZES[len(b)]):
        n, r = divmod(n, 58)
        s = ALPHA[r] + s
    return s


def dec_block(s):
    n = 0
    for ch in s:
        n = n * 58 + ALPHA.index(ch)
    return n.to_bytes(SIZES.index(len(s)), 'big')


def b58enc(b):
    return ''.join(enc_block(b[i:i + 8]) for i in range(0, len(b), 8))


def b58dec(s):
    return b''.join(dec_block(s[i:i + 11]) for i in range(0, len(s), 11))


def varint(n):
    out = bytearray()
    while True:
        c = n & 0x7f
        n >>= 7
        if n:
            out.append(c | 0x80)
        else:
            out.append(c)
            return bytes(out)


def read_varint(b):
    n = shift = i = 0
    while True:
        c = b[i]
        i += 1
        n |= (c & 0x7f) << shift
        shift += 7
        if not c & 0x80:
            return n, i


SEKR, XKR = 2239254, 45239
raw = b58dec(sys.argv[1])
body, check = raw[:-4], raw[-4:]
if keccak256(body)[:4] != check:
    sys.exit(0)
prefix, n = read_varint(body)
other = XKR if prefix == SEKR else SEKR
nb = varint(other) + body[n:]
print(b58enc(nb + keccak256(nb)[:4]))
PY
}

# fx_image_note <image>: one line for _meta.json about what the image is
fx_image_note() {
    case $1 in
        ghcr.io/kryptokrona/kryptokrona-testnet:*)
            echo "the published image is a 2025-03-31 testnet build: coinbase unlock window 20 in the wallet (the chain uses 1), no Xkr address prefix (Xkr cases show the rejection)" ;;
        *)
            echo "image $1: a testnet build of the C++ checkout (versions in _image.json); it knows the Xkr prefix if the checkout does" ;;
    esac
}

# ------------------------------------------------------------------ init

# fx_init <work dir> <name>: suite_init plus the fixture state (keep list, raw dir)
fx_init() {
    suite_init "$1" "$2"
    require_cmd curl jq python3 docker timeout nice sha256sum
    require_docker
    require_xkrd
    require_testwallet
    mkdir -p "$SD/raw" "$SD/w"
    FX_KEEP=$SD/keep.txt
    : > "$FX_KEEP"
    FX_JSONL=$SD/raw/default.jsonl
    FX_RPC_ID=0
    FX_STUB_PID=""
    fx_keep "$FX_SEED_A" "$FX_A_SPEND" "$FX_A_VIEW" "$FX_SEED_B" "$FX_SEED_BAD_CHECKSUM" \
        "$FX_SEED_UNKNOWN_WORD" "$FX_SEED_24_WORDS" "$FX_K_SPEND" "$FX_K_VIEW" "$FX_K_SPEND_PUB" \
        "$FX_DUMMY_ADDR" "$FX_PID1" "$FX_PID2" "$FX_ZERO_HASH" "$FX_UNKNOWN_HASH" \
        "$FX_SUB1" "$FX_SUB2" "$FX_SUB3"
    note "fixtures go below $FX_OUT_ROOT/crates"
}

# fx_require_image <image> [get-it command]
fx_require_image() {
    require_image "$1" "${2:-docker pull $1   (a download of the published image, not a build)}"
}

# fx_keep <value...>: values that are deterministic and must not become placeholders
fx_keep() {
    local v
    for v in "$@"; do
        [ -n "$v" ] && printf '%s\n' "$v" >> "$FX_KEEP"
    done
    return 0
}

# fx_group <name>: following fx_record lines go to <sd>/raw/<name>.jsonl
fx_group() {
    FX_JSONL=$SD/raw/$1.jsonl
    touch "$FX_JSONL"
    step "group $1"
}

scenario_cleanup() {
    if [ -n "${FX_STUB_PID:-}" ]; then
        kill "$FX_STUB_PID" 2>/dev/null || true
        wait "$FX_STUB_PID" 2>/dev/null || true
    fi
    rm_container "$C_FX_ONCE"
    return 0
}

# ------------------------------------------------------------------ containers

FX_DOCKER_LIMITS=(--cpus 2 --memory 1g)

# fx_docker_once <image> <entrypoint> [args...]: a short one-off run (no network), output to stdout
fx_docker_once() {
    local img=$1 ep=$2
    shift 2
    timeout --kill-after=10 60 nice -n 19 docker run --rm --network none "${FX_DOCKER_LIMITS[@]}" \
        --entrypoint "$ep" "$img" "$@" 2>&1 < /dev/null || true
}

# fx_run_tool <container> <image> <stdin 0|1> <binary> [args...]: a detached C++ tool with
# <sd>/w mounted at /w (its working directory)
fx_run_tool() {
    local c=$1 img=$2 stdin=$3 bin=$4
    shift 4
    local iflag=()
    [ "$stdin" = 1 ] && iflag=(-i)
    rm_container "$c"
    nice -n 19 docker run -d ${iflag[@]+"${iflag[@]}"} --name "$c" --label "$SUITE_LABEL=1" \
        --network host "${FX_DOCKER_LIMITS[@]}" --user "$FX_UIDGID" -v "$SD/w:/w" --workdir /w \
        -e HOME=/w --entrypoint nice "$img" -n 19 "$FX_BIN/$bin" "$@" > /dev/null \
        || die start "cannot start $bin from $img (container $c)"
    _remember_container "$c"
}

# fx_tool_fg <log> <image> <timeout s> <binary> [args...]: a foreground C++ tool run (stdin from
# /dev/null), output to <log>; prints its exit status
fx_tool_fg() {
    local log=$1 img=$2 secs=$3 bin=$4 rc=0
    shift 4
    rm_container "$C_FX_ONCE"
    timeout --kill-after=10 "$secs" nice -n 19 docker run --name "$C_FX_ONCE" --label "$SUITE_LABEL=1" \
        --network host "${FX_DOCKER_LIMITS[@]}" --user "$FX_UIDGID" -v "$SD/w:/w" --workdir /w \
        -e HOME=/w --entrypoint nice "$img" -n 19 "$FX_BIN/$bin" "$@" > "$log" 2>&1 < /dev/null || rc=$?
    rm_container "$C_FX_ONCE"
    echo "$rc"
}

# fx_tool_timed <log> <image> <max s> <stop pattern|-> <grace s> <binary> [args...]: a detached
# run with line-buffered output (stdbuf -oL; FX_TIMED_TTY=1 uses a pseudo-tty instead), so
# nothing is lost when it is stopped. It is stopped (docker stop) after <max s>, or <grace s>
# after a line matching <stop pattern> appeared. Whole output in <log>; prints the exit status,
# or "stopped" when it was still running at the end.
fx_tool_timed() {
    local log=$1 img=$2 max=$3 pat=$4 grace=$5 bin=$6 t0 now seen="" rc
    shift 6
    local wrap=(stdbuf -oL -eL) tty=()
    if [ "${FX_TIMED_TTY:-0}" = 1 ]; then wrap=(); tty=(-t); fi
    rm_container "$C_FX_ONCE"
    if ! nice -n 19 docker run -d ${tty[@]+"${tty[@]}"} --name "$C_FX_ONCE" --label "$SUITE_LABEL=1" \
        --network host "${FX_DOCKER_LIMITS[@]}" --user "$FX_UIDGID" -v "$SD/w:/w" --workdir /w \
        -e HOME=/w --entrypoint nice "$img" -n 19 ${wrap[@]+"${wrap[@]}"} "$FX_BIN/$bin" "$@" > /dev/null; then
        echo start-failed
        return 0
    fi
    t0=$(date +%s)
    while container_running "$C_FX_ONCE"; do
        now=$(date +%s)
        [ $((now - t0)) -ge "$max" ] && break
        if [ "$pat" != - ] && [ -z "$seen" ] && docker logs "$C_FX_ONCE" 2>&1 | grep -aq -- "$pat"; then
            seen=$now
        fi
        [ -n "$seen" ] && [ $((now - seen)) -ge "$grace" ] && break
        sleep 1
    done
    if container_running "$C_FX_ONCE"; then
        rc=stopped
        docker stop -t 5 "$C_FX_ONCE" > /dev/null 2>&1 || true
    else
        rc=$(docker inspect -f '{{.State.ExitCode}}' "$C_FX_ONCE" 2> /dev/null || echo unknown)
    fi
    docker logs "$C_FX_ONCE" > "$log" 2>&1 || true
    rm_container "$C_FX_ONCE"
    echo "$rc"
}

# fx_stop_tool <container>: docker stop (SIGTERM, the tools save on it), logs to <sd>/<c>.log, rm
fx_stop_tool() {
    docker stop -t 30 "$1" > /dev/null 2>&1 || true
    docker logs "$1" > "$SD/$1.log" 2>&1 || true
    rm_container "$1"
}

# fx_start_cpp_node <container> <image> <p2p> <rpc> [kryptokronad args...]: like start_cpp, but
# with an explicit entrypoint (the published image may carry one) and without dying: 0 when the
# RPC came up, 1 otherwise
fx_start_cpp_node() {
    local c=$1 img=$2 p2p=$3 rpc=$4 i
    shift 4
    rm_container "$c"
    nice -n 19 docker run -d --name "$c" --label "$SUITE_LABEL=1" --network host \
        "${FX_DOCKER_LIMITS[@]}" --tmpfs /data:rw,size=512m --entrypoint nice "$img" -n 19 \
        "$FX_BIN/kryptokronad" --data-dir=/data --p2p-bind-ip=127.0.0.1 --p2p-bind-port="$p2p" \
        --rpc-bind-ip=127.0.0.1 --rpc-bind-port="$rpc" --allow-local-ip --no-console --log-level=2 \
        "$@" > /dev/null || return 1
    _remember_container "$c"
    for ((i = 0; i < 120; i++)); do
        [ -n "$(height_of "127.0.0.1:$rpc")" ] && return 0
        container_running "$c" || return 1
        sleep 0.5
    done
    return 1
}

# ------------------------------------------------------------------ the fixture chain

# fx_fresh_chain [xkrd args...]: xkrd X on an empty data dir (genesis only, dials nobody); the
# process name is FX_XNAME (default fx-x), extra arguments go to xkrd (e.g. fork heights)
fx_fresh_chain() {
    local x=${FX_XNAME:-fx-x}
    reclaim "$x"
    rm -rf "$SD/xkrd"
    require_ports_free "$PORT_FX_P2P" "$PORT_FX_RPC"
    start_xkrd "$x" "$PORT_FX_RPC" "$SD/xkrd" "$SD/xkrd.log" --p2p-bind-ip 127.0.0.1 \
        --p2p-bind-port "$PORT_FX_P2P" --add-exclusive-node "$DEAD_PEER" --no-hyper "$@"
    watch "$x"
    note "chain: xkrd X on $FX_RPC, top $(top_of "$FX_RPC")"
}

# fx_mine <address> <top index>: exact blocks with xkr-testwallet mine (template + submitblock)
fx_mine() {
    local out=$SD/mine.out
    echo "### mine --address $1 --to $2" >> "$out"
    nice -n 19 timeout 600 "$XKR_TESTWALLET" --wallet-dir "$SD/miner-wallet" --daemon "$FX_RPC" \
        mine --address "$1" --to "$2" >> "$out" 2>&1 < /dev/null \
        || die mine "xkr-testwallet mine to $2 failed (top $(height_of "$FX_RPC")): $(tail -n 2 "$out" | tr '\n' ' ')"
}
# fx_mine_count <address> <blocks>
fx_mine_count() {
    local h
    h=$(height_of "$FX_RPC")
    fx_mine "$1" $((h + $2))
}

# fx_wait_pool <tx hash...>: every hash in X's pool (60 s each); 1 if one is missing
fx_wait_pool() {
    local t ok=0
    for t in "$@"; do
        [ -n "$t" ] && [ "$t" != null ] || continue
        if ! wait_for 60 tx_in_pool "$FX_RPC" "$t"; then
            note "transaction $t not in X's pool: $(flat "$(tx_status "$FX_RPC" "$t")" | cut -c1-160)"
            ok=1
        fi
    done
    return "$ok"
}

# fx_confirm <tx hash...>: wait for the pool, mine 1 block (filler address), then FX_CONFIRM - 1 more
fx_confirm() {
    fx_wait_pool "$@" || note "mining anyway"
    fx_mine_count "$FX_DUMMY_ADDR" 1
    fx_mine_count "$FX_DUMMY_ADDR" $((FX_CONFIRM - 1))
    fx_wait_follower
}

# ------------------------------------------------------------------ C++ follower node

# fx_choose_follower: sets FX_FOLLOWER, the image of the C++ node that follows X (see
# FX_FOLLOWER_IMAGE); uses the verdict of fx_verify_image cached in <wd>/fx-follow.json, or runs
# it (it restarts X: call it before fx_fresh_chain)
fx_choose_follower() {
    if [ "$FX_FOLLOWER_IMAGE" != auto ]; then
        FX_FOLLOWER=$FX_FOLLOWER_IMAGE
        return 0
    fi
    if [ ! -f "$WD/fx-follow.json" ] || [ "$(jq -r '.image' "$WD/fx-follow.json" 2> /dev/null)" != "$FX_IMAGE" ]; then
        fx_verify_image "$FX_IMAGE"
    fi
    if [ "$(jq -r '.follows' "$WD/fx-follow.json" 2> /dev/null)" = true ]; then
        FX_FOLLOWER=$FX_IMAGE
    else
        FX_FOLLOWER=$CPP_OLD_IMAGE
    fi
    note "C++ node image for following X: $FX_FOLLOWER"
}

# fx_start_follower <image>: C++ node following X (24888/24889); dies if it does not sync
fx_start_follower() {
    local img=$1
    fx_require_image "$img"
    require_ports_free "$PORT_FX_CPP_P2P" "$PORT_FX_CPP_RPC"
    fx_start_cpp_node "$C_FX_CPP" "$img" "$PORT_FX_CPP_P2P" "$PORT_FX_CPP_RPC" \
        --add-exclusive-node="127.0.0.1:$PORT_FX_P2P" \
        || die follower "C++ node $C_FX_CPP ($img) did not start (log: docker logs $C_FX_CPP)"
    FX_FOLLOWER_RUNNING=1
    fx_wait_follower
    note "C++ node $C_FX_CPP ($img) follows X at $(top_of "$FX_CPP_RPC")"
}
fx_wait_follower() {
    [ "${FX_FOLLOWER_RUNNING:-0}" = 1 ] || return 0
    wait_same_top "$FX_CPP_RPC" "$FX_RPC" 180 \
        || die follower "C++ node [$(top_of "$FX_CPP_RPC")] did not reach X [$(top_of "$FX_RPC")] within 180 s"
}

# fx_walletd_daemon_port: RPC port kryptokrona-service and xkrwallet use (FX_WALLETD_DAEMON)
fx_walletd_daemon_port() {
    if [ "$FX_WALLETD_DAEMON" = cpp ]; then echo "$PORT_FX_CPP_RPC"; else echo "$PORT_FX_RPC"; fi
}

# ------------------------------------------------------------------ image report and follow check

# fx_image_report <image> <out json>: id, creation time, binaries, versions, default ports
fx_image_report() {
    local img=$1 out=$2 files b vers=() have=()
    files=$(fx_docker_once "$img" /bin/ls "$FX_BIN" | tr '\n' ' ')
    for b in kryptokronad wallet-api kryptokrona-service xkrwallet xkrwallet-beta zedwallet miner; do
        case " $files " in
            *" $b "*)
                have+=("$b")
                vers+=("$b" "$(fx_docker_once "$img" "$FX_BIN/$b" --version | tr -d '\r' | { grep -v '^\s*$' || true; } | head -n 4 | tr '\n' ' ')")
                ;;
        esac
    done
    local dhelp mhelp shelp ahelp
    dhelp=$(fx_docker_once "$img" "$FX_BIN/kryptokronad" --help | { grep -E 'bind-port' || true; } | tr -s ' ' | head -n 4 | tr '\n' '|')
    mhelp=$(fx_docker_once "$img" "$FX_BIN/miner" --help | { grep -E 'daemon-rpc-port' || true; } | tr -s ' ' | head -n 2 | tr '\n' '|')
    shelp=$(fx_docker_once "$img" "$FX_BIN/kryptokrona-service" --help | { grep -E 'daemon-port|bind-port' || true; } | tr -s ' ' | head -n 4 | tr '\n' '|')
    ahelp=$(fx_docker_once "$img" "$FX_BIN/wallet-api" --help | { grep -E 'port' || true; } | tr -s ' ' | head -n 3 | tr '\n' '|')
    jq -n --arg image "$img" \
        --arg id "$(docker image inspect -f '{{.Id}}' "$img" 2>/dev/null || true)" \
        --arg created "$(docker image inspect -f '{{.Created}}' "$img" 2>/dev/null || true)" \
        --arg files "$files" --arg dhelp "$dhelp" --arg mhelp "$mhelp" --arg shelp "$shelp" \
        --arg ahelp "$ahelp" \
        '{image: $image, id: $id, created: $created,
          files: ($files | split(" ") | map(select(length > 0))),
          versions: ([$ARGS.positional as $p | range(0; $p | length; 2) | {key: $p[.], value: $p[. + 1]}] | from_entries),
          xkrwalletBeta: ($files | split(" ") | index("xkrwallet-beta") != null),
          helpDefaults: {kryptokronad: $dhelp, miner: $mhelp, "kryptokrona-service": $shelp, "wallet-api": $ahelp},
          testnetDefaultPorts: (($dhelp | test("11898")) and ($dhelp | test("11899")))}' \
        --args ${vers[@]+"${vers[@]}"} > "$out"
    note "image $img: $(jq -c '{id: .id[7:19], created, files, xkrwalletBeta, testnetDefaultPorts}' "$out")"
}

# fx_verify_image <image>: does the image's kryptokronad follow a chain made by xkrd testnet? Own
# fresh X, FX_VERIFY_BLOCKS blocks, the image's node with X as its only peer for FX_FOLLOW_WAIT s.
# Network id: xkrd logs "peer connected ... inbound=true" only after a handshake with the same
# network id. Writes <sd>/follow.json and <wd>/fx-follow.json (cache). Stops X.
fx_verify_image() {
    local img=$1 blocks=${FX_VERIFY_BLOCKS:-20} wait=${FX_FOLLOW_WAIT:-90} started=false follows=false
    local t0 inbound="" cinfo="" ctop="" xtop="" errs=""
    fx_require_image "$img"
    step "follow check: $img"
    fx_fresh_chain
    fx_mine "$FX_DUMMY_ADDR" "$blocks"
    xtop=$(top_of "$FX_RPC")
    require_ports_free "$PORT_FX_GHCR_P2P" "$PORT_FX_GHCR_RPC"
    t0=$(date +%s)
    if fx_start_cpp_node "$C_FX_GHCR" "$img" "$PORT_FX_GHCR_P2P" "$PORT_FX_GHCR_RPC" \
        --add-exclusive-node="127.0.0.1:$PORT_FX_P2P"; then
        started=true
        if wait_for "$wait" same_top "127.0.0.1:$PORT_FX_GHCR_RPC" "$FX_RPC"; then follows=true; fi
        ctop=$(top_of "127.0.0.1:$PORT_FX_GHCR_RPC")
        cinfo=$(get "127.0.0.1:$PORT_FX_GHCR_RPC" /getinfo)
    fi
    inbound=$({ grep -a "peer connected" "$SD/xkrd.log" | grep -a "inbound=true" || true; } | tail -n 1 | sed 's/^.*peer connected/peer connected/' | cut -c1-300)
    docker logs "$C_FX_GHCR" > "$SD/$C_FX_GHCR.log" 2>&1 || true
    errs=$({ grep -aiE 'error|reject|fail|invalid|alternative|wrong' "$SD/$C_FX_GHCR.log" || true; } | tail -n 15)
    rm_container "$C_FX_GHCR"
    stop_proc "${FX_XNAME:-fx-x}"
    unwatch
    jq -n --arg image "$img" --argjson started "$started" --argjson follows "$follows" \
        --arg xtop "$xtop" --arg ctop "$ctop" --arg inbound "$inbound" --arg errs "$errs" \
        --arg info "$cinfo" --argjson secs $(($(date +%s) - t0)) --argjson wait "$wait" \
        '{image: $image, nodeStarted: $started, follows: $follows, waitedSeconds: $secs, limit: $wait,
          xkrdTop: $xtop, imageNodeTop: $ctop,
          handshakeWithXkrd: ($inbound | length > 0), xkrdPeerLine: $inbound,
          imageNodeGetinfo: ($info | try fromjson catch null), imageNodeLogErrors: ($errs | split("\n") | map(select(length > 0)))}' \
        > "$SD/follow.json"
    cp "$SD/follow.json" "$WD/fx-follow.json"
    if [ "$follows" = true ]; then
        pass follow "$img's kryptokronad follows the xkrd testnet chain (top $ctop)"
    else
        note "NOTE: $img's kryptokronad does not follow the xkrd testnet chain (started $started, handshake $([ -n "$inbound" ] && echo yes || echo no), its top [$ctop], X [$xtop]); details in $SD/follow.json"
        note "      (the 2025 testnet build predates UPGRADE_HEIGHT_V2..V5 = 0 and the fixed testnet difficulty)"
    fi
}

# ------------------------------------------------------------------ recording

# fx_record <name> <METHOD> <host:port> <path> <body|-> [header...]: one JSON line to $FX_JSONL;
# sets FX_STATUS (0 = no answer) and FX_BODY
fx_record() {
    local name=$1 method=$2 hp=$3 path=$4 body=$5
    shift 5
    local rq=$SD/.req rh=$SD/.resp-headers rb=$SD/.resp-body code h hasbody=1
    local args=(-s --max-time "$FX_HTTP_TIMEOUT" -X "$method" -H 'Expect:' -D "$rh" -o "$rb" -w '%{http_code}')
    for h in "$@"; do args+=(-H "$h"); done
    : > "$rh"
    : > "$rb"
    if [ "$body" = - ]; then
        : > "$rq"
        hasbody=0
    else
        printf '%s' "$body" > "$rq"
        args+=(--data-binary "@$rq")
    fi
    code=$(curl "${args[@]}" "http://$hp$path" 2> /dev/null) || true
    [[ $code =~ ^[0-9]+$ ]] || code=000
    FX_STATUS=$((10#$code))
    FX_BODY=$(cat "$rb")
    jq -cn --arg name "$name" --arg method "$method" --arg path "$path" --arg hasbody "$hasbody" \
        --rawfile reqb "$rq" --argjson status "$FX_STATUS" --rawfile resph "$rh" --rawfile respb "$rb" '
        def body($t): if $t == "" then "" else ($t | try fromjson catch $t) end;
        def kv: {key: .[0:index(":")], value: (.[index(":") + 1:] | sub("^\\s+"; "") | sub("\\s+$"; ""))};
        def hdrs($t): ($t | split("\r\n\r\n") | map(select(length > 0)) | last // "")
            | [split("\r\n")[1:][] | select(index(":") != null) | kv]
            | map(select(.key | ascii_downcase != "date")) | from_entries;
        {name: $name,
         request: {method: $method, path: $path,
                   headers: ([$ARGS.positional[] | select(index(":") != null) | kv] | from_entries),
                   body: (if $hasbody == "1" then body($reqb) else null end)},
         response: {status: $status, headers: hdrs($resph), body: body($respb)}}' \
        --args "$@" >> "$FX_JSONL"
    return 0
}

# ------------------------------------------------------------------ wallet-api

# fx_api_start <container> <port> [wallet-api args...]: C++ wallet-api from FX_IMAGE (stdin kept
# open: the console exits on end of input)
fx_api_start() {
    local c=$1 port=$2 i
    shift 2
    require_ports_free "$port"
    fx_run_tool "$c" "$FX_IMAGE" 1 wallet-api --port "$port" --rpc-bind-ip 127.0.0.1 \
        --rpc-password "$FX_API_KEY" "$@"
    for ((i = 0; i < 60; i++)); do
        port_busy "$port" && return 0
        container_running "$c" || break
        sleep 0.5
    done
    docker logs --tail 20 "$c" 2>&1 || true
    die start "wallet-api $c did not listen on $port"
}

# fx_api <name> <METHOD> <path> [body|-] [port]: recorded wallet-api call with the right key
fx_api() {
    local name=$1 method=$2 path=$3 body=${4:--} port=${5:-$PORT_FX_API}
    local hdr=("X-API-KEY: $FX_API_KEY")
    [ "$body" = - ] || hdr+=("Content-Type: application/json")
    fx_record "$name" "$method" "127.0.0.1:$port" "$path" "$body" "${hdr[@]}"
}
# fx_api_q <METHOD> <path> [body] [port]: unrecorded call, prints the body
fx_api_q() {
    local args=(-s --max-time "$FX_HTTP_TIMEOUT" -X "$1" -H "X-API-KEY: $FX_API_KEY" -H 'Expect:')
    [ -n "${3:-}" ] && args+=(-H 'Content-Type: application/json' --data-binary "$3")
    curl "${args[@]}" "http://127.0.0.1:${4:-$PORT_FX_API}$2" 2> /dev/null || true
}
# fx_wparams <file under /w> <password> [extra json object]: the wallet params of open/import/create
fx_wparams() {
    local extra=${3:-}
    [ -n "$extra" ] || extra='{}'
    jq -cn --arg f "/w/$1" --arg p "$2" --argjson port "$PORT_FX_RPC" --argjson extra "$extra" \
        '{filename: $f, password: $p, daemonHost: "127.0.0.1", daemonPort: $port} + $extra'
}
fx_api_synced() {
    local s w n l top
    s=$(fx_api_q GET /status "" "${1:-$PORT_FX_API}")
    w=$(num "$s" walletBlockCount)
    n=$(num "$s" networkBlockCount)
    l=$(num "$s" localDaemonBlockCount)
    top=$(height_of "$FX_RPC")
    # the wallet has scanned what the node reports; the 2025 build counts one block more than
    # C++ master and xkrd's /height, so only "at least the top" is checked
    [ -n "$w" ] && [ -n "$n" ] && [ -n "$l" ] && [ -n "$top" ] && [ "$l" -ge "$top" ] \
        && [ "$w" -ge "$n" ] && [ "$w" -ge "$l" ]
}
# fx_api_wait_synced [port]: the open wallet has scanned X's top block
fx_api_wait_synced() {
    wait_for "$FX_SYNC_WAIT" fx_api_synced "${1:-$PORT_FX_API}" \
        || die sync "wallet-api did not sync to X's top $(height_of "$FX_RPC") within ${FX_SYNC_WAIT}s: $(flat "$(fx_api_q GET /status "" "${1:-$PORT_FX_API}")")"
    sleep 2
}
# fx_api_dump <out json>: what a reader of the open wallet's file must find (raw, not normalised)
fx_api_dump() {
    local addrs a keys=() v
    addrs=$(fx_api_q GET /addresses)
    jq -e . > /dev/null 2>&1 <<< "$addrs" || addrs=null
    for a in $(printf '%s' "$addrs" | jq -r '.addresses[]? // empty'); do
        keys+=("$a" "$(fx_api_q GET "/keys/$a")" "$(fx_api_q GET "/keys/mnemonic/$a")" "$(fx_api_q GET "/balance/$a")")
    done
    v=$(fx_api_q GET /keys)
    local txs
    txs=$(fx_api_q GET /transactions)
    if ! jq -e '.transactions | type == "array"' > /dev/null 2>&1 <<< "$txs"; then
        fail dump "$(basename "$1"): GET /transactions did not answer a list: $(printf '%s' "$txs" | head -c 300)"
    fi
    jq -n --argjson addrs "${addrs:-null}" --arg view "$v" --arg primary "$(fx_api_q GET /addresses/primary)" \
        --arg bal "$(fx_api_q GET /balance)" --arg bals "$(fx_api_q GET /balances)" \
        --arg txs "$txs" --arg st "$(fx_api_q GET /status)" '
        def j: try fromjson catch .;
        {primaryAddress: ($primary | j | .address? // null), addresses: ($addrs.addresses? // null),
         privateViewKey: ($view | j | .privateViewKey? // null),
         subWallets: [$ARGS.positional as $p | range(0; $p | length; 4) as $i | $p[$i:$i + 4] | {address: .[0], keys: (.[1] | j),
                      mnemonic: (.[2] | j), balance: (.[3] | j)}],
         balance: ($bal | j), balances: ($bals | j), transactions: ($txs | j | .transactions? // .),
         status: ($st | j)}' --args ${keys[@]+"${keys[@]}"} > "$1"
}

# ------------------------------------------------------------------ kryptokrona-service (walletd)

# fx_wd_generate <log> <file under /w> <password> [args...]: kryptokrona-service -g (exits);
# prints its exit status
fx_wd_generate() {
    local log=$1 file=$2 pass=$3
    shift 3
    fx_tool_fg "$log" "$WALLETD_IMAGE" 120 kryptokrona-service -g -w "/w/$file" -p "$pass" \
        --rpc-password "$FX_WD_RPC_PASS" --daemon-address 127.0.0.1 \
        --daemon-port "$(fx_walletd_daemon_port)" -l "/w/$file.generate.log" "$@"
}
# fx_wd_start <container> <port> <file> <password> [args...]: walletd on the container file
fx_wd_start() {
    local c=$1 port=$2 file=$3 pass=$4 i r
    shift 4
    require_ports_free "$port"
    fx_run_tool "$c" "$WALLETD_IMAGE" 0 kryptokrona-service -w "/w/$file" -p "$pass" \
        --rpc-password "$FX_WD_RPC_PASS" --bind-address 127.0.0.1 --bind-port "$port" \
        --daemon-address 127.0.0.1 --daemon-port "$(fx_walletd_daemon_port)" -l "/w/$file.log" "$@"
    for ((i = 0; i < 120; i++)); do
        r=$(curl -s --max-time 5 -d '{"jsonrpc":"2.0","id":0,"method":"getStatus","params":{},"password":"'"$FX_WD_RPC_PASS"'"}' \
            "http://127.0.0.1:$port/json_rpc" 2> /dev/null || true)
        [ -n "$r" ] && return 0
        container_running "$c" || break
        sleep 0.5
    done
    docker logs --tail 30 "$c" 2>&1 || true
    die start "kryptokrona-service $c did not answer on $port"
}
# fx_wd <name> <method> [params json] [port] [password | -none]: recorded JSON-RPC call
fx_wd() {
    local name=$1 method=$2 params=${3:-} port=${4:-$PORT_FX_WALLETD} pw=${5-$FX_WD_RPC_PASS} body
    [ -n "$params" ] || params='{}'
    FX_RPC_ID=$((FX_RPC_ID + 1))
    if [ "$pw" = -none ]; then
        body=$(jq -cn --arg m "$method" --argjson p "$params" --argjson id "$FX_RPC_ID" \
            '{jsonrpc: "2.0", id: $id, method: $m, params: $p}')
    else
        body=$(jq -cn --arg m "$method" --argjson p "$params" --argjson id "$FX_RPC_ID" --arg pw "$pw" \
            '{jsonrpc: "2.0", id: $id, password: $pw, method: $m, params: $p}')
    fi
    fx_record "$name" POST "127.0.0.1:$port" /json_rpc "$body" "Content-Type: application/json"
}
# fx_wd_raw <name> <body> [port] [path] [METHOD]: recorded raw request (transport errors)
fx_wd_raw() {
    fx_record "$1" "${5:-POST}" "127.0.0.1:${3:-$PORT_FX_WALLETD}" "${4:-/json_rpc}" "$2" "Content-Type: application/json"
}
# fx_wd_q <method> [params] [port]: unrecorded call, prints the "result" (or the whole answer)
fx_wd_q() {
    local params=${2:-}
    [ -n "$params" ] || params='{}'
    curl -s --max-time "$FX_HTTP_TIMEOUT" -H 'Content-Type: application/json' \
        -d "$(jq -cn --arg m "$1" --argjson p "$params" --arg pw "$FX_WD_RPC_PASS" '{jsonrpc: "2.0", id: 0, password: $pw, method: $m, params: $p}')" \
        "http://127.0.0.1:${3:-$PORT_FX_WALLETD}/json_rpc" 2> /dev/null | jq -c '.result // .' 2> /dev/null || true
}
fx_wd_synced() {
    local s b k top
    s=$(fx_wd_q getStatus "" "${1:-$PORT_FX_WALLETD}")
    b=$(num "$s" blockCount)
    k=$(num "$s" knownBlockCount)
    top=$(height_of "$FX_RPC")
    [ -n "$b" ] && [ -n "$k" ] && [ -n "$top" ] && [ "$b" -gt "$top" ] && [ "$b" -ge "$k" ]
}
fx_wd_wait_synced() {
    fx_wait_follower
    wait_for "$FX_SYNC_WAIT" fx_wd_synced "${1:-$PORT_FX_WALLETD}" \
        || die sync "kryptokrona-service did not sync to X's top $(height_of "$FX_RPC") within ${FX_SYNC_WAIT}s: $(fx_wd_q getStatus "" "${1:-$PORT_FX_WALLETD}")"
    sleep 2
}
# fx_wd_dump <out json> [port]: what a reader of the container must find (raw, not normalised).
# getTransactions starts at firstBlockIndex 1: C++ WalletGreen::getTransactionsInBlocks
# (src/wallet/wallet_green.cpp) refuses 0 with WRONG_PARAMETERS (application_code 22). An error
# answer is kept in "transactionsError" and is a FAIL, never an empty list.
fx_wd_dump() {
    local port=${2:-$PORT_FX_WALLETD} addrs a keys=() top txs
    addrs=$(fx_wd_q getAddresses "" "$port")
    jq -e . > /dev/null 2>&1 <<< "$addrs" || addrs=null
    top=$(height_of "$FX_RPC")
    for a in $(printf '%s' "$addrs" | jq -r '.addresses[]? // empty'); do
        keys+=("$a" "$(fx_wd_q getSpendKeys "{\"address\":\"$a\"}" "$port")" \
            "$(fx_wd_q getMnemonicSeed "{\"address\":\"$a\"}" "$port")" \
            "$(fx_wd_q getBalance "{\"address\":\"$a\"}" "$port")")
    done
    txs=$(fx_wd_q getTransactions "{\"firstBlockIndex\":1,\"blockCount\":$((top + 1))}" "$port")
    if ! jq -e '.items | type == "array"' > /dev/null 2>&1 <<< "$txs"; then
        fail dump "$(basename "$1"): getTransactions did not answer items: $(printf '%s' "$txs" | head -c 300)"
    fi
    jq -n --argjson addrs "${addrs:-null}" --arg view "$(fx_wd_q getViewKey "" "$port")" \
        --arg bal "$(fx_wd_q getBalance "" "$port")" --arg st "$(fx_wd_q getStatus "" "$port")" \
        --arg txs "$txs" '
        def j: try fromjson catch .;
        {addresses: ($addrs.addresses? // null), viewSecretKey: ($view | j | .viewSecretKey? // null),
         spendKeys: [$ARGS.positional as $p | range(0; $p | length; 4) as $i | $p[$i:$i + 4] | {address: .[0], keys: (.[1] | j),
                     mnemonic: (.[2] | j), balance: (.[3] | j)}],
         balance: ($bal | j), status: ($st | j),
         transactions: ($txs | j | if (type == "object" and (.items | type) == "array")
                                    then [.items[] | .transactions[]?] else null end),
         transactionsError: ($txs | j | if (type == "object" and (.items | type) == "array")
                                         then null else . end)}' \
        --args ${keys[@]+"${keys[@]}"} > "$1"
}

# ------------------------------------------------------------------ CLI sessions

FX_CLI_DELAY=${FX_CLI_DELAY:-1.5}      # seconds between two input lines
FX_CLI_START=${FX_CLI_START:-4}        # seconds before the first line
FX_CLI_TIMEOUT=${FX_CLI_TIMEOUT:-240}  # hard limit of one session
FX_CLI_MAX_BYTES=${FX_CLI_MAX_BYTES:-1048576}  # transcript size limit (a prompt loop on end of input)
FX_CLI_TRUNCATED="[transcript truncated by testnet/fixtures at FX_CLI_MAX_BYTES bytes: the session did not end on its input]"

# fx_feed <script file>: its lines on stdout, one every FX_CLI_DELAY s; "@sleep N" waits N s
fx_feed() {
    local line
    trap '' PIPE
    sleep "$FX_CLI_START"
    while IFS= read -r line || [ -n "$line" ]; do
        case $line in
            "@sleep "*) sleep "${line#@sleep }" ;;
            *) printf '%s\n' "$line" || return 0; sleep "$FX_CLI_DELAY" ;;
        esac
    done < "$1"
    sleep 5
}
# fx_cli_session <out> <image> <binary> <script file> [args...]: a terminal wallet fed through
# stdin (not a tty: linenoise and the password prompt then read plain lines); prints the status,
# or "truncated" when the output reached FX_CLI_MAX_BYTES (it is cut there, the session stopped
# and a FX_CLI_TRUNCATED line appended)
fx_cli_session() {
    local out=$1 img=$2 bin=$3 script=$4 rc=0
    shift 4
    rm_container "$C_FX_ONCE"
    set +e
    fx_feed "$script" | timeout --kill-after=10 "$FX_CLI_TIMEOUT" nice -n 19 docker run -i \
        --name "$C_FX_ONCE" --label "$SUITE_LABEL=1" --network host "${FX_DOCKER_LIMITS[@]}" \
        --user "$FX_UIDGID" -v "$SD/w:/w" --workdir /w -e HOME=/w --entrypoint nice "$img" -n 19 \
        "$FX_BIN/$bin" "$@" 2>&1 | head -c "$FX_CLI_MAX_BYTES" > "$out"
    rc=${PIPESTATUS[1]}
    set -e
    rm_container "$C_FX_ONCE"
    if [ "$(wc -c < "$out")" -ge "$FX_CLI_MAX_BYTES" ]; then
        printf '\n%s\n' "$FX_CLI_TRUNCATED" >> "$out"
        rc=truncated
    fi
    echo "$rc"
}

# ------------------------------------------------------------------ normalisation and output

# fx_normalise <jsonl|text> <in> <out>: placeholders for values that change from run to run.
#   * 64-hex values not in the keep list -> "<kind:N>", numbered per kind in order of first
#     appearance in the file; the kind comes from the JSON key (tx, block, spendkey, spendpub,
#     viewkey, txkey, txpub) or is "hex";
#   * addresses not in the keep list -> "<address:N>" / "<integrated:N>"; random 25-word seeds
#     (key mnemonicSeed, or a line of their own in text) -> "<mnemonic:N>";
#   * "extra": tag 01 public key -> "01<txpub:N>", other fields kept; other hex runs longer than
#     64 -> "<hexdata:N>";
#   * timestamp keys (> 0) -> "<timestamp>", peerCount -> "<peers>", walletd change transfers
#     (type 2) amount -> "<change>";
#   * entries whose name ends in "~shape" (timing-dependent answers): every number in the
#     response body -> "<n>";
#   * text: ANSI colour codes removed, CR -> LF, the same token rules.
fx_normalise() {
    python3 -I - "$1" "$2" "$3" "$FX_KEEP" <<'PY'
import json
import re
import sys

mode, src, dst, keepf = sys.argv[1:5]
keep = set()
with open(keepf, encoding='utf-8') as f:
    for line in f:
        line = line.rstrip('\n')
        if line:
            keep.add(line)

HEX = re.compile(r'(?<![0-9a-fA-F])[0-9a-fA-F]{64}(?![0-9a-fA-F])')
LONGHEX = re.compile(r'^[0-9a-fA-F]{65,}$')
ADDR = re.compile(r'(?<![A-Za-z0-9])(?:SEKR|Xkr)[1-9A-HJ-NP-Za-km-z]{90,}(?![A-Za-z0-9])')
SEED = re.compile(r'^(?:[a-z]+ ){24}[a-z]+$')
SEED_LINE = re.compile(r'(?m)^[ \t]*((?:[a-z]+ ){24}[a-z]+)[ \t]*$')
ANSI = re.compile(r'\x1b\[[0-9;?]*[A-Za-z]')
KIND = {
    'transactionHash': 'tx', 'hash': 'tx', 'transactionHashes': 'tx', 'parentTransactionHash': 'tx',
    'blockHash': 'block', 'lastBlockHash': 'block', 'blockHashes': 'block', 'blockId': 'block',
    'blockIds': 'block', 'top': 'block', 'top_id': 'block',
    'privateSpendKey': 'spendkey', 'spendSecretKey': 'spendkey', 'spendSecretKeys': 'spendkey',
    'publicSpendKey': 'spendpub', 'spendPublicKey': 'spendpub', 'publicKey': 'spendpub',
    'privateViewKey': 'viewkey', 'viewSecretKey': 'viewkey',
    'transactionPrivateKey': 'txkey', 'txHash': 'tx',
}
TIME_KEYS = {'timestamp', 'syncStartTimestamp', 'creationTimestamp', 'startTimestamp',
             'creationTime', 'timestampBegin'}
mapping = {}
counters = {}


def assign(value, kind):
    if value in keep or value in mapping:
        return
    counters[kind] = counters.get(kind, 0) + 1
    mapping[value] = '<%s:%d>' % (kind, counters[kind])


def get(value, kind):
    if value in keep:
        return value
    assign(value, kind)
    return mapping[value]


def classify(obj, key=None):
    if isinstance(obj, dict):
        for k, v in obj.items():
            classify(v, k)
    elif isinstance(obj, list):
        for v in obj:
            classify(v, key)
    elif isinstance(obj, str):
        if key == 'extra':
            return
        if key == 'mnemonicSeed' and SEED.match(obj.strip()):
            assign(obj.strip(), 'mnemonic')
        for m in HEX.findall(obj):
            assign(m, KIND.get(key, 'hex'))
        for m in ADDR.findall(obj):
            assign(m, 'integrated' if len(m) > 150 else 'address')


def repl(s):
    for v, p in mapping.items():
        if p.startswith('<mnemonic:') and v in s:
            s = s.replace(v, p)
    s = HEX.sub(lambda m: get(m.group(0), 'hex'), s)
    s = ADDR.sub(lambda m: get(m.group(0), 'integrated' if len(m.group(0)) > 150 else 'address'), s)
    return s


def varint(b, i):
    n = shift = 0
    while i < len(b):
        c = b[i]
        i += 1
        n |= (c & 0x7f) << shift
        shift += 7
        if not c & 0x80:
            return n, i
    raise ValueError('varint')


def norm_extra(h):
    try:
        b = bytes.fromhex(h)
    except ValueError:
        return repl(h)
    out = []
    i = 0
    try:
        while i < len(b):
            t = b[i]
            if t == 0x01 and i + 33 <= len(b):
                out.append('01' + get(b[i + 1:i + 33].hex(), 'txpub'))
                i += 33
            elif t in (0x02, 0x03):
                n, j = varint(b, i + 1)
                out.append(b[i:j + n].hex())
                i = j + n
            else:
                out.append(b[i:].hex())
                break
    except ValueError:
        return '<hexdata>'
    return ''.join(out)


def norm(obj, key=None):
    if isinstance(obj, dict):
        out = {k: norm(v, k) for k, v in obj.items()}
        if obj.get('type') == 2 and 'amount' in obj and 'address' in obj:
            out['amount'] = '<change>'
        return out
    if isinstance(obj, list):
        return [norm(v, key) for v in obj]
    if isinstance(obj, bool) or obj is None:
        return obj
    if isinstance(obj, (int, float)):
        if key in TIME_KEYS and obj > 0:
            return '<timestamp>'
        if key == 'peerCount':
            return '<peers>'
        return obj
    if isinstance(obj, str):
        if key == 'extra' and obj and re.fullmatch(r'[0-9a-fA-F]+', obj):
            return norm_extra(obj)
        if LONGHEX.match(obj) and obj not in keep:
            return get(obj, 'hexdata')
        return repl(obj)
    return obj


def shape(obj):
    if isinstance(obj, dict):
        return {k: shape(v) for k, v in obj.items()}
    if isinstance(obj, list):
        return [shape(v) for v in obj]
    if isinstance(obj, (int, float)) and not isinstance(obj, bool):
        return '<n>'
    return obj


if mode == 'jsonl':
    entries = []
    with open(src, encoding='utf-8') as f:
        for line in f:
            line = line.strip()
            if line:
                entries.append(json.loads(line))
    for e in entries:
        classify(e)
    with open(dst, 'w', encoding='utf-8') as f:
        for e in entries:
            n = norm(e)
            for part in ('request', 'response'):
                h = n.get(part, {}).get('headers')
                if isinstance(h, dict):
                    n[part]['headers'] = {k: v for k, v in h.items() if k.lower() != 'date'}
            if str(n.get('name', '')).endswith('~shape') and 'response' in n:
                n['response']['body'] = shape(n['response']['body'])
            f.write(json.dumps(n, ensure_ascii=False, separators=(',', ':')) + '\n')
else:
    with open(src, encoding='utf-8', errors='replace') as f:
        text = f.read()
    text = ANSI.sub('', text).replace('\r\n', '\n').replace('\r', '\n')
    for m in SEED_LINE.finditer(text):
        assign(m.group(1), 'mnemonic')
    for m in HEX.finditer(text):
        assign(m.group(0), 'hex')
    for m in ADDR.finditer(text):
        assign(m.group(0), 'integrated' if len(m.group(0)) > 150 else 'address')
    with open(dst, 'w', encoding='utf-8') as f:
        f.write(repl(text))
PY
}

# fx_publish_jsonl <dest dir>: every <sd>/raw/*.jsonl normalised into <dest dir> (old *.jsonl
# there removed first, so a re-run leaves no stale group)
fx_publish_jsonl() {
    local dest=$1 f n=0
    mkdir -p "$dest"
    rm -f "$dest"/*.jsonl
    for f in "$SD"/raw/*.jsonl; do
        [ -s "$f" ] || continue
        fx_normalise jsonl "$f" "$dest/$(basename "$f")"
        n=$((n + $(wc -l < "$f")))
    done
    pass publish "$n exchanges in $(ls "$dest"/*.jsonl 2> /dev/null | wc -l) file(s) under ${dest#"$FX_OUT_ROOT"/}"
}

# fx_app_codes <dir> <out json>: every distinct JSON-RPC error (code, message, application_code)
# in the normalised JSONL files of <dir>, with the first fixture name that shows it
fx_app_codes() {
    python3 -I - "$1" "$2" <<'PY'
import glob
import json
import os
import sys

src, dst = sys.argv[1:3]
seen = {}
for path in sorted(glob.glob(os.path.join(src, '*.jsonl'))):
    with open(path, encoding='utf-8') as f:
        for line in f:
            e = json.loads(line)
            body = e.get('response', {}).get('body')
            err = body.get('error') if isinstance(body, dict) else None
            if not isinstance(err, dict):
                continue
            data = err.get('data') if isinstance(err.get('data'), dict) else {}
            key = (err.get('code'), data.get('application_code'), err.get('message'))
            if key not in seen:
                seen[key] = {'code': key[0], 'application_code': key[1], 'message': key[2],
                             'first': os.path.basename(path) + ':' + e.get('name', '')}
rows = sorted(seen.values(), key=lambda r: (r['code'] if isinstance(r['code'], int) else 0,
                                           r['application_code'] if isinstance(r['application_code'], int) else -1))
with open(dst, 'w', encoding='utf-8') as f:
    json.dump(rows, f, ensure_ascii=False, indent=2)
    f.write('\n')
print(len(rows))
PY
}

# fx_meta <out json> <tool> <image report json> [notes...]: _meta.json of a fixture directory
fx_meta() {
    local out=$1 tool=$2 rep=$3
    shift 3
    jq -n --arg tool "$tool" --slurpfile img "$rep" --arg xkrd "$(xkrd_version)" \
        --arg date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg script "$SCEN" \
        --argjson fund "$FX_FUND_TO" --argjson filler "$FX_FILLER_TO" --argjson confirm "$FX_CONFIRM" \
        --arg daemon "$FX_WALLETD_DAEMON" \
        '{capturedBy: ("testnet/fixtures/" + ($script | sub("^fx-"; "capture-")) + ".sh"), captured: $date,
          tool: $tool, image: $img[0], xkrd: $xkrd,
          chain: {network: "testnet", genesisOnly: true, mainWalletBlocks: [1, $fund],
                  fillerBlocksTo: $filler, blocksAfterSends: $confirm,
                  walletdAndXkrwalletDaemon: $daemon},
          format: {line: "{name, request: {method, path, headers, body}, response: {status, headers, body}}",
                   body: "parsed JSON if the text is JSON, else the raw text; \"\" = empty response body; null = no request body",
                   headers: "response headers without Date; Content-Length is that of the original (not normalised) body"},
          normalisation: {hex64: "<kind:N> per kind (tx, block, spendkey, spendpub, viewkey, txkey, txpub, hex), numbered by first appearance",
                          addresses: "<address:N> / <integrated:N> unless derived from fixed keys",
                          seeds: "<mnemonic:N> unless fixed", extra: "01<txpub:N> + the other fields",
                          timestamps: "<timestamp>", peerCount: "<peers>", walletdChange: "<change>",
                          shape: "names ending in ~shape: every number in the response body is <n>"},
          notes: $ARGS.positional}' --args "$@" > "$out"
}

# ------------------------------------------------------------------ Rust next to C++ (scenarios 60-64)
#
# testnet/scenarios/60-64 run the Rust wallet tools (xkr-wallet-api, xkr-walletd,
# xkrwallet) next to the C++ ones from WL_IMAGE (default xkr-cpp-wallets:master, the -O2
# reference build of testnet/build-cpp.sh) against one xkrd X per scenario (own ports, see
# testnet/lib.sh). Conventions on top of the ones above:
#   * wl_init sets the scenario's X ports, the C++ images and the container label WALLET_LABEL
#     (default xkr-wallets, not xkr-suite: a suite or fixture run that cleans up its own
#     containers leaves a wallet scenario running next to it alone; every scenario removes its
#     containers by name when it ends; after a hard kill:
#     docker rm -f $(docker ps -aq --filter label=xkr-wallets));
#   * the Rust programs run as local processes under nice -n 19 with their working directory in
#     <sd>/w, the directory the C++ containers see as /w, so both sides open the same files;
#   * the two wallet-api programs read a console on stdin and stop at the end of input, so they
#     get a FIFO this shell holds open (wl_start);
#   * sync checks use >= (the node and the tools may count blocks differently); JSON is read
#     with jq -e and an answer that is not what was asked for is a FAIL showing the answer,
#     never an empty result.

XKR_WALLET_API=${XKR_WALLET_API:-$ROOT/target/release/xkr-wallet-api}
XKR_WALLETD=${XKR_WALLETD:-$ROOT/target/release/xkr-walletd}
XKR_WALLET_CLI=${XKR_WALLET_CLI:-$ROOT/target/release/xkrwallet}
WL_IMAGE=${WL_IMAGE:-$CPP_WALLETS_IMAGE}
WALLET_LABEL=${WALLET_LABEL:-xkr-wallets}
BUILD_WALLET_API="nice -n 19 cargo build --release -j 3 -p xkr-wallet-api"
BUILD_WALLETD="nice -n 19 cargo build --release -j 3 -p xkr-walletd"
BUILD_WALLET_CLI="nice -n 19 cargo build --release -j 3 -p xkr-wallet-cli"
BUILD_CPP_WALLETS="XKR_CPP_IMAGE=$WL_IMAGE XKR_CPP_PATCHES=none testnet/build-cpp.sh   (the -O2 reference build of every C++ tool)"
WL_PASS=${WL_PASS:-wl-pass}   # password of the wallets the scenarios create

# wl_init <work dir> <X p2p port> <X rpc port>: suite_init plus the state of a wallet scenario
wl_init() {
    suite_init "$1"
    require_cmd curl jq python3 docker timeout nice sha256sum mkfifo cmp diff
    require_xkrd
    require_testwallet
    SUITE_LABEL=$WALLET_LABEL
    FX_IMAGE=$WL_IMAGE
    WALLETD_IMAGE=$WL_IMAGE
    CLI_IMAGE=$WL_IMAGE
    FX_WALLETD_DAEMON=xkrd
    PORT_FX_P2P=$2
    PORT_FX_RPC=$3
    FX_RPC=127.0.0.1:$3
    FX_XNAME=$SCEN-x
    C_FX_ONCE=xkr-wl-$SCEN-once
    mkdir -p "$SD/w" "$SD/snap" "$SD/in" "$SD/tx"
    FX_KEEP=$SD/keep.txt
    : > "$FX_KEEP"
    FX_JSONL=$SD/requests.jsonl
    FX_RPC_ID=0
    FX_STUB_PID=""
    note "C++ tools from $WL_IMAGE (label $WALLET_LABEL); Rust tools from $(dirname "$XKR_WALLET_API")"
}

require_wallet_api() {
    [ -x "$XKR_WALLET_API" ] || missing_exit prereq "xkr-wallet-api not found at $XKR_WALLET_API" "$BUILD_WALLET_API (or set XKR_WALLET_API)"
}
require_walletd() {
    [ -x "$XKR_WALLETD" ] || missing_exit prereq "xkr-walletd not found at $XKR_WALLETD" "$BUILD_WALLETD (or set XKR_WALLETD)"
}
require_wallet_cli() {
    [ -x "$XKR_WALLET_CLI" ] || missing_exit prereq "xkrwallet not found at $XKR_WALLET_CLI" "$BUILD_WALLET_CLI (or set XKR_WALLET_CLI)"
}
# cpp_wallets_have <binary...>: WL_IMAGE exists and has every binary (no result line)
cpp_wallets_have() {
    local files b
    docker image inspect "$WL_IMAGE" > /dev/null 2>&1 || return 1
    files=" $(fx_docker_once "$WL_IMAGE" /bin/ls "$FX_BIN" | tr '\n' ' ') "
    for b in "$@"; do
        case $files in
            *" $b "*) ;;
            *) return 1 ;;
        esac
    done
}
# require_cpp_wallets <binary...>: MISSING (exit 2) with the build command unless WL_IMAGE has them
require_cpp_wallets() {
    require_image "$WL_IMAGE" "$BUILD_CPP_WALLETS"
    cpp_wallets_have "$@" || missing_exit prereq "$WL_IMAGE lacks one of: $*" "$BUILD_CPP_WALLETS"
}

# wl_hold_stdin: a FIFO this shell holds open read-write; a program with it as stdin never sees
# the end of its input
wl_hold_stdin() {
    [ -n "${WL_STDIN_FD:-}" ] && return 0
    rm -f "$SD/.stdin"
    mkfifo "$SD/.stdin"
    exec {WL_STDIN_FD}<> "$SD/.stdin"
}
# wl_start <name> <log> <command...>: like start_bg (nice -n 19, pid file, stopped at exit), in
# <sd>/w, stdin the held FIFO
wl_start() {
    local name=$1 log=$2
    shift 2
    wl_hold_stdin
    stop_proc "$name"
    LOGOF[$name]=$log
    (cd "$SD/w" && exec nice -n 19 "$@") >> "$log" 2>&1 <&"$WL_STDIN_FD" &
    LAST_PID=$!
    PID[$name]=$LAST_PID
    echo "$LAST_PID" > "$WD/pids/$name.pid"
    _remember_proc "$name"
}

# wl_jq <check> <out file> <filter> <json text>: jq -S -e <filter> into <out file>; text that is
# not JSON or a filter error is a FAIL showing the start of the text (the raw text then goes into
# <out file>, so a later comparison fails too); 1 on failure
wl_jq() {
    local id=$1 out=$2 filter=$3 text=$4
    if ! printf '%s' "$text" | jq -S -e "$filter" > "$out" 2> "$out.err"; then
        fail "$id" "unexpected answer ($(head -c 200 "$out.err" | tr '\n' ' ')): $(printf '%s' "$text" | head -c 300)"
        printf '%s\n' "$text" > "$out"
        return 1
    fi
}
# wl_same <check> <what> <file a> <file b> <name a> <name b>: PASS when the files are equal, else
# FAIL with the start of the difference
wl_same() {
    if cmp -s "$3" "$4"; then
        pass "$1" "$2: the same in $5 and $6"
    else
        fail "$1" "$2 differ between $5 and $6 ($3 vs $4): $(diff "$3" "$4" | head -n 14 | tr '\n' ' ' | cut -c1-600)"
    fi
}
# wl_atomic <amount text>: "1 234.56789" or "1,234.5" -> atomic units (5 decimals)
wl_atomic() {
    local v int dec
    v=$(printf '%s' "$1" | tr -d ' ,')
    [[ $v =~ ^[0-9]+(\.[0-9]{1,5})?$ ]] || { echo "not an amount: $1"; return 0; }
    int=${v%%.*}
    dec=0
    if [[ $v == *.* ]]; then dec=${v#*.}; fi
    dec=$(printf '%-5s' "$dec" | tr ' ' 0)
    echo $((10#$int * 100000 + 10#$dec))
}
# wl_tx_version <tx hash>: the version xkrd X reports for a transaction ("" if unknown)
wl_tx_version() { num "$(tx_json "$FX_RPC" "$1")" version; }
# wl_tx_height <tx hash>: the block height of a mined transaction ("" if not mined)
wl_tx_height() { if tx_in_block "$FX_RPC" "$1"; then num "$(tx_json "$FX_RPC" "$1")" height; fi; }
# wl_sha <file>: sha256 of a file ("missing" if it does not exist)
wl_sha() { if [ -f "$1" ]; then sha256sum "$1" | cut -c1-64; else echo missing; fi; }

# ------------------------------------------------------------------ wallet-api, both sides

# rapi_start <name> <port>: the Rust xkr-wallet-api with the C++ flags (plus --network testnet)
rapi_start() {
    local name=$1 port=$2 i
    require_ports_free "$port"
    wl_start "$name" "$SD/$name.log" "$XKR_WALLET_API" --port "$port" --rpc-bind-ip 127.0.0.1 \
        --rpc-password "$FX_API_KEY" --network testnet
    for ((i = 0; i < 60; i++)); do
        port_busy "$port" && return 0
        proc_alive "$name" || break
        sleep 0.5
    done
    tail -n 20 "$SD/$name.log" 2> /dev/null || true
    die start "xkr-wallet-api $name did not listen on $port (log $SD/$name.log)"
}
# wl_wparams <cpp|rust> <file in <sd>/w> <password> [extra json object]: the body of
# /wallet/open, /wallet/import/* and /wallet/create for that side (C++ sees <sd>/w as /w)
wl_wparams() {
    local f extra=${4:-}
    [ -n "$extra" ] || extra='{}'
    if [ "$1" = cpp ]; then f=/w/$2; else f=$SD/w/$2; fi
    jq -cn --arg f "$f" --arg p "$3" --argjson port "$PORT_FX_RPC" --argjson extra "$extra" \
        '{filename: $f, password: $p, daemonHost: "127.0.0.1", daemonPort: $port} + $extra'
}
# wl_api <port> <METHOD> <path> [body]: one call; sets WL_CODE (HTTP status, 000 = no answer)
# and WL_BODY
wl_api() {
    local args=(-s --max-time "$FX_HTTP_TIMEOUT" -X "$2" -H "X-API-KEY: $FX_API_KEY" -H 'Expect:'
        -o "$SD/.api-body" -w '%{http_code}')
    [ -n "${4:-}" ] && args+=(-H 'Content-Type: application/json' --data-binary "$4")
    : > "$SD/.api-body"
    WL_CODE=$(curl "${args[@]}" "http://127.0.0.1:$1$3" 2> /dev/null) || true
    [[ $WL_CODE =~ ^[0-9]+$ ]] || WL_CODE=000
    WL_BODY=$(cat "$SD/.api-body")
}
wl_api_synced() { # <port>
    local s top w l
    s=$(fx_api_q GET /status "" "$1")
    top=$(height_of "$FX_RPC")
    w=$(num "$s" walletBlockCount)
    l=$(num "$s" localDaemonBlockCount)
    [ -n "$top" ] && [ -n "$w" ] && [ -n "$l" ] && [ "$w" -ge "$top" ] && [ "$l" -ge "$top" ]
}
# wl_api_wait <port> <name>: the open wallet has scanned X's top; 1 (no result line) if not
wl_api_wait() {
    sleep 2
    if wait_for "$FX_SYNC_WAIT" wl_api_synced "$1"; then
        sleep 2
        return 0
    fi
    note "$2 did not sync to X's top $(height_of "$FX_RPC") within ${FX_SYNC_WAIT}s: $(flat "$(fx_api_q GET /status "" "$1")" | cut -c1-300)"
    return 1
}
# wl_api_wait_or_die <port> <name>
wl_api_wait_or_die() { wl_api_wait "$1" "$2" || die sync "$2 did not sync (see the line above)"; }
# wl_api_total <port>: unlocked + locked of the open wallet, or a text starting with "no balance"
wl_api_total() {
    local b t
    b=$(fx_api_q GET /balance "" "$1")
    if t=$(jq -e '.unlocked + .locked' <<< "$b" 2> /dev/null); then echo "$t"; else echo "no balance: $(printf '%s' "$b" | head -c 200)"; fi
}
# wl_api_open <check> <port> <cpp|rust> <file> <password> <name>: /wallet/open; FAIL and 1 when
# it does not answer 200
wl_api_open() {
    wl_api "$2" POST /wallet/open "$(wl_wparams "$3" "$4" "$5")"
    if [ "$WL_CODE" != 200 ]; then
        fail "$1" "$6 could not open $4: HTTP $WL_CODE $(printf '%s' "$WL_BODY" | head -c 300)"
        return 1
    fi
}
# wl_api_close <port>: DELETE /wallet (saves and closes)
wl_api_close() { fx_api_q DELETE /wallet "" "$1" > /dev/null; }
# wl_api_send <check> <name> <port> <path> <body>: a send; the transaction hash in WL_TX; FAIL
# and 1 when there is none (not for $(...): the result line must reach the scenario)
wl_api_send() {
    WL_TX=""
    wl_api "$3" POST "$4" "$5"
    if WL_TX=$(jq -re '.transactionHash | select(type == "string" and length == 64)' <<< "$WL_BODY" 2> /dev/null); then
        return 0
    fi
    WL_TX=""
    fail "$1" "$2 $4 gave no transaction: HTTP $WL_CODE $(printf '%s' "$WL_BODY" | head -c 300)"
    return 1
}

WL_API_BAL='if (.unlocked | type) == "number" and (.locked | type) == "number" then {unlocked, locked} else error("no balance") end'
WL_API_BALS='if type == "array" then sort_by(.address) else error("not a list") end'
WL_API_TXS='.transactions | if type == "array" then . else error("no transactions list") end
    | map(.transfers |= sort_by(.address, .amount)) | sort_by(.blockHeight, .hash)'
WL_API_STATUS='if (.walletBlockCount | type) == "number" then {walletBlockCount, localDaemonBlockCount, networkBlockCount, isViewWallet, subWalletCount} else error("no status") end'
WL_API_ADDRS='.addresses | if type == "array" then sort else error("no address list") end'
# wl_api_snapshot <check> <port> <out prefix>: balance, balances, transactions (transfers and
# transactions sorted), status (block counts, view flag, subwallet count) and the address list
# of the open wallet as <out prefix>.<part>.json
wl_api_snapshot() {
    local id=$1 port=$2 out=$3
    wl_jq "$id" "$out.balance.json" "$WL_API_BAL" "$(fx_api_q GET /balance "" "$port")" || true
    wl_jq "$id" "$out.balances.json" "$WL_API_BALS" "$(fx_api_q GET /balances "" "$port")" || true
    wl_jq "$id" "$out.transactions.json" "$WL_API_TXS" "$(fx_api_q GET /transactions "" "$port")" || true
    wl_jq "$id" "$out.status.json" "$WL_API_STATUS" "$(fx_api_q GET /status "" "$port")" || true
    wl_jq "$id" "$out.addresses.json" "$WL_API_ADDRS" "$(fx_api_q GET /addresses "" "$port")" || true
}
# wl_api_compare <check> <prefix a> <prefix b> <name a> <name b> [parts]: wl_same per part
# (default: balance balances transactions status addresses)
wl_api_compare() {
    local id=$1 a=$2 b=$3 na=$4 nb=$5 parts=${6:-"balance balances transactions status addresses"} p
    for p in $parts; do
        wl_same "$id-$p" "$p" "$a.$p.json" "$b.$p.json" "$na" "$nb"
    done
}

# ------------------------------------------------------------------ walletd, both sides

# rwd_generate <log> <file> <password> [args...]: xkr-walletd -g (exits); prints its exit status
rwd_generate() {
    local log=$1 file=$2 pass=$3 rc=0
    shift 3
    (cd "$SD/w" && exec timeout --kill-after=10 120 nice -n 19 "$XKR_WALLETD" -g -w "$SD/w/$file" \
        -p "$pass" --rpc-password "$FX_WD_RPC_PASS" --daemon-address 127.0.0.1 \
        --daemon-port "$PORT_FX_RPC" -l "$SD/w/$file.generate.log" --network testnet "$@") \
        > "$log" 2>&1 < /dev/null || rc=$?
    echo "$rc"
}
# rwd_start <name> <port> <file> <password> [args...]: xkr-walletd on a wallet file in <sd>/w,
# with the C++ flags (plus --network testnet); waits until it answers
rwd_start() {
    local name=$1 port=$2 file=$3 pass=$4 i r
    shift 4
    require_ports_free "$port"
    wl_start "$name" "$SD/$name.log" "$XKR_WALLETD" -w "$SD/w/$file" -p "$pass" \
        --rpc-password "$FX_WD_RPC_PASS" --bind-address 127.0.0.1 --bind-port "$port" \
        --daemon-address 127.0.0.1 --daemon-port "$PORT_FX_RPC" -l "$SD/w/$file.log" \
        --network testnet "$@"
    for ((i = 0; i < 120; i++)); do
        r=$(curl -s --max-time 5 -d '{"jsonrpc":"2.0","id":0,"method":"getStatus","params":{},"password":"'"$FX_WD_RPC_PASS"'"}' \
            "http://127.0.0.1:$port/json_rpc" 2> /dev/null || true)
        [ -n "$r" ] && return 0
        proc_alive "$name" || break
        sleep 0.5
    done
    tail -n 20 "$SD/$name.log" 2> /dev/null || true
    die start "xkr-walletd $name did not answer on $port (log $SD/$name.log)"
}
# wl_wd_start <cpp|rust> <name> <port> <file> <password>: either walletd on a file in <sd>/w
wl_wd_start() {
    if [ "$1" = cpp ]; then
        fx_wd_start "$2" "$3" "$4" "$5"
    else
        rwd_start "$2" "$3" "$4" "$5"
    fi
}
# wl_wd_stop <cpp|rust> <name>: SIGTERM (both save on it)
wl_wd_stop() {
    if [ "$1" = cpp ]; then fx_stop_tool "$2"; else stop_proc "$2" 60; fi
}
wl_wd_synced() { # <port>
    local s top b k
    s=$(fx_wd_q getStatus "" "$1")
    top=$(height_of "$FX_RPC")
    b=$(num "$s" blockCount)
    k=$(num "$s" knownBlockCount)
    [ -n "$top" ] && [ -n "$b" ] && [ -n "$k" ] && [ "$b" -ge "$top" ] && [ "$k" -ge "$top" ]
}
# wl_wd_wait <port> <name>: walletd has scanned X's top; 1 (no result line) if not
wl_wd_wait() {
    sleep 2
    if wait_for "$FX_SYNC_WAIT" wl_wd_synced "$1"; then
        sleep 2
        return 0
    fi
    note "$2 did not sync to X's top $(height_of "$FX_RPC") within ${FX_SYNC_WAIT}s: $(fx_wd_q getStatus "" "$1" | cut -c1-300)"
    return 1
}
wl_wd_wait_or_die() { wl_wd_wait "$1" "$2" || die sync "$2 did not sync (see the line above)"; }
# wl_wd_total <port> [address]: availableBalance + lockedAmount, or a text starting "no balance"
wl_wd_total() {
    local p='{}' b t
    [ -n "${2:-}" ] && p=$(jq -cn --arg a "$2" '{address: $a}')
    b=$(fx_wd_q getBalance "$p" "$1")
    if t=$(jq -e '.availableBalance + .lockedAmount' <<< "$b" 2> /dev/null); then echo "$t"; else echo "no balance: $(printf '%s' "$b" | head -c 200)"; fi
}
# wl_wd_call <port> <method> [params]: the whole JSON-RPC answer (result or error) in WL_BODY
wl_wd_call() {
    local params=${3:-}
    [ -n "$params" ] || params='{}'
    WL_BODY=$(curl -s --max-time "$FX_HTTP_TIMEOUT" -H 'Content-Type: application/json' \
        -d "$(jq -cn --arg m "$2" --argjson p "$params" --arg pw "$FX_WD_RPC_PASS" '{jsonrpc: "2.0", id: 0, password: $pw, method: $m, params: $p}')" \
        "http://127.0.0.1:$1/json_rpc" 2> /dev/null || true)
}
# wl_wd_hash <check> <name> <port> <method> <params>: a call that answers a transactionHash; the
# hash in WL_TX; FAIL and 1 when there is none (not for $(...))
wl_wd_hash() {
    WL_TX=""
    wl_wd_call "$3" "$4" "$5"
    if WL_TX=$(jq -re '.result.transactionHash | select(type == "string" and length == 64)' <<< "$WL_BODY" 2> /dev/null); then
        return 0
    fi
    WL_TX=""
    fail "$1" "$2 $4 gave no transaction: $(printf '%s' "$WL_BODY" | head -c 300)"
    return 1
}
# wl_wd_ok <check> <name> <port> <method> [params]: a call that must answer a result (no error)
wl_wd_ok() {
    wl_wd_call "$3" "$4" "${5:-}"
    if jq -e 'has("result") and (has("error") | not)' <<< "$WL_BODY" > /dev/null 2>&1; then
        return 0
    fi
    fail "$1" "$2 $4: $(printf '%s' "$WL_BODY" | head -c 300)"
    return 1
}

WL_WD_BAL='if (.availableBalance | type) == "number" then {availableBalance, lockedAmount} else error("no balance") end'
WL_WD_ADDRS='.addresses | if type == "array" then sort else error("no address list") end'
WL_WD_TXS='.items | if type == "array" then . else error("no items") end
    | map(.transactions |= (map(.transfers |= sort_by(.address, .type, .amount)) | sort_by(.transactionHash)))'
WL_WD_STATUS='if (.blockCount | type) == "number" then {blockCount, knownBlockCount, lastBlockHash} else error("no status") end'
WL_WD_DELAYED='.transactionHashes | if type == "array" then sort else error("no hash list") end'
# wl_wd_snapshot <check> <port> <out prefix>: balance (whole wallet and per address), addresses,
# getTransactions from block index 1 over the whole chain (C++ refuses index 0), status and the
# delayed transaction hashes as <out prefix>.<part>.json
wl_wd_snapshot() {
    local id=$1 port=$2 out=$3 top a addrs
    top=$(height_of "$FX_RPC")
    wl_jq "$id" "$out.balance.json" "$WL_WD_BAL" "$(fx_wd_q getBalance "" "$port")" || true
    wl_jq "$id" "$out.addresses.json" "$WL_WD_ADDRS" "$(fx_wd_q getAddresses "" "$port")" || true
    : > "$out.balances.json"
    addrs=$(jq -r '.[]?' "$out.addresses.json" 2> /dev/null || true)
    for a in $addrs; do
        wl_jq "$id" "$out.one.json" "($WL_WD_BAL) + {address: \"$a\"}" \
            "$(fx_wd_q getBalance "$(jq -cn --arg a "$a" '{address: $a}')" "$port")" || true
        cat "$out.one.json" >> "$out.balances.json"
    done
    rm -f "$out.one.json" "$out.one.json.err"
    wl_jq "$id" "$out.transactions.json" "$WL_WD_TXS" \
        "$(fx_wd_q getTransactions "{\"firstBlockIndex\":1,\"blockCount\":$((${top:-0} + 1))}" "$port")" || true
    wl_jq "$id" "$out.status.json" "$WL_WD_STATUS" "$(fx_wd_q getStatus "" "$port")" || true
    wl_jq "$id" "$out.delayed.json" "$WL_WD_DELAYED" "$(fx_wd_q getDelayedTransactionHashes "" "$port")" || true
}
# wl_wd_compare <check> <prefix a> <prefix b> <name a> <name b> [parts]
wl_wd_compare() {
    local id=$1 a=$2 b=$3 na=$4 nb=$5 parts=${6:-"balance addresses balances transactions status delayed"} p
    for p in $parts; do
        wl_same "$id-$p" "$p" "$a.$p.json" "$b.$p.json" "$na" "$nb"
    done
}

# ------------------------------------------------------------------ terminal wallets

# rcli_session <out> <script file> [xkrwallet args...]: the Rust xkrwallet without a terminal
# (script mode) in <sd>/w with -r X and --network testnet, fed like fx_cli_session (FX_CLI_DELAY
# between lines, "@sleep N"), FX_CLI_TIMEOUT, output cut at FX_CLI_MAX_BYTES; prints the exit
# status or "truncated"
rcli_session() {
    local out=$1 script=$2 rc
    shift 2
    set +e
    fx_feed "$script" | (cd "$SD/w" && exec timeout --kill-after=10 "$FX_CLI_TIMEOUT" nice -n 19 \
        "$XKR_WALLET_CLI" -r "$FX_RPC" --network testnet "$@") 2>&1 | head -c "$FX_CLI_MAX_BYTES" > "$out"
    rc=${PIPESTATUS[1]}
    set -e
    if [ "$(wc -c < "$out")" -ge "$FX_CLI_MAX_BYTES" ]; then
        printf '\n%s\n' "$FX_CLI_TRUNCATED" >> "$out"
        rc=truncated
    fi
    echo "$rc"
}
# cpp_cli_session <out> <binary> <script file> [args...]: a C++ terminal wallet from WL_IMAGE
# against X (fx_cli_session); prints the exit status or "truncated"
cpp_cli_session() {
    local out=$1 bin=$2 script=$3
    shift 3
    fx_cli_session "$out" "$WL_IMAGE" "$bin" "$script" -r "$FX_RPC" "$@"
}
# wl_script <file> <line...>: a session script, one argument per line
wl_script() {
    local f=$1
    shift
    printf '%s\n' "$@" > "$f"
}
# rcli_total <transcript>: available + locked from the last `balance` of the Rust xkrwallet
# ("  1 234.56789 XKR", then "  locked 0.5 XKR" when something is locked); "no balance" if none
rcli_total() {
    local line av lo
    line=$(sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$1" | awk '
        /^  [0-9][0-9 ]*\.[0-9]+ XKR$/ { a = $0; l = ""; want = 1; next }
        want && /^  locked [0-9][0-9 ]*\.[0-9]+ XKR$/ { l = $0 }
        { want = 0 }
        END { print a "|" l }')
    av=${line%%|*}
    lo=${line#*|}
    [ -n "$av" ] || { echo "no balance in $1"; return 0; }
    av=${av#  }
    av=${av% XKR}
    lo=${lo#  locked }
    lo=${lo% XKR}
    av=$(wl_atomic "$av")
    if [ -n "$lo" ]; then lo=$(wl_atomic "$lo"); else lo=0; fi
    [[ $av =~ ^[0-9]+$ && $lo =~ ^[0-9]+$ ]] || { echo "no balance in $1 ($av / $lo)"; return 0; }
    echo $((av + lo))
}
# cpp_cli_total <transcript>: "Total balance: 14,304.01516 XKR" of the C++ terminal wallets (the
# last one); "no balance" if none
cpp_cli_total() {
    local v
    v=$(sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$1" | { grep -aoE 'Total balance: [0-9,]+\.[0-9]+ XKR' || true; } \
        | tail -n 1 | sed 's/^Total balance: //; s/ XKR$//')
    if [ -n "$v" ]; then wl_atomic "$v"; else echo "no balance in $1"; fi
}
# wl_short <address>: as the Rust xkrwallet prints it (first 8, "…", last 5)
wl_short() { printf '%s…%s' "${1:0:8}" "${1: -5}"; }
