#!/usr/bin/env bash
# 62-wallet-migration: wallet files of all four C++ wallet tools open in the three Rust tools with the same balance, and the files the Rust tools leave behind open in the C++ tools again.
#
#   xkrd X       own testnet chain from genesis, P2P 26298 / RPC 26299, mined only with
#                `xkr-testwallet mine`
#   C++          from WL_IMAGE (xkr-cpp-wallets:master, -O2 reference build): wallet-api (26270,
#                container xkr-wl-62-api), kryptokrona-service (26280, xkr-wl-62-walletd),
#                xkrwallet and xkrwallet-beta (one-off containers, fed through stdin)
#   Rust         target/release/xkr-wallet-api (26271), xkr-walletd (26281), xkrwallet (script
#                mode, no terminal)
#
# Four C++ files, all from seed A (FX_SEED_A), so all hold the same balance B:
#   api   cpp-api.wallet         wallet-api /wallet/import/seed          WalletBackend file
#   wd    cpp-wd.wallet          kryptokrona-service -g, plus S1         WalletGreen container
#   xw    xkrwallet-seed.wallet  xkrwallet seed_restore                  WalletGreen container
#   beta  beta-seed.wallet       xkrwallet-beta seed_restore             WalletBackend file
# The chain pays A, then the C++ wallet-api sends 1 XKR away, so every file has incoming and
# outgoing history; B is the C++ wallet-api's balance after that send.
# Checks:
#   cpp-*        the C++ tools agree on B (walletd, xkrwallet, xkrwallet-beta on their own files)
#   rust-api-*   xkr-wallet-api opens a copy of each file: balance B
#   rust-wd-*    xkr-walletd opens a copy of each file: balance B
#   rust-cli-*   xkrwallet -w <copy> -p <password>: balance B, exit 0
#   sidecar-*    a container opened by a Rust tool is left byte for byte as it was and gets
#                <file>.xkrstate next to it; xkr-walletd reopens it from that state with balance
#                B; without the state file it scans again and reaches B; after createAddress the
#                container is rewritten and the C++ walletd opens it with all three addresses
#   back-api-*   the C++ wallet-api opens every WalletBackend file a Rust tool saved: balance B
#   back-wd-*    the C++ walletd opens every container a Rust tool used: balance B
#   back-cli-*   the C++ xkrwallet opens the container xkrwallet used, the C++ xkrwallet-beta the
#                file xkrwallet saved: "Total balance" B
#
# Env: WL_IMAGE, XKR_WALLET_API, XKR_WALLETD, XKR_WALLET_CLI, FX_SYNC_WAIT (240), FX_CLI_DELAY,
# FX_CLI_TIMEOUT, WALLET_LABEL, TIMEOUT, KEEP=1.
# suite: expected=780 timeout=2700
set -euo pipefail
: "${TIMEOUT:=300}"
. "$(dirname "$0")/../fixtures/lib-wallet.sh"
wl_init "${1:-}" "$PORT_W62_P2P" "$PORT_W62_RPC"
require_wallet_api
require_walletd
require_wallet_cli
require_cpp_wallets wallet-api kryptokrona-service xkrwallet xkrwallet-beta

C_API=xkr-wl-62-api
C_WD=xkr-wl-62-walletd
CAP=$PORT_W62_CPP_API
RAP=$PORT_W62_RUST_API
CWP=$PORT_W62_CPP_WD
RWP=$PORT_W62_RUST_WD
rm_container "$C_API"
rm_container "$C_WD"
reclaim w62-rapi w62-rwd

declare -A FILE=([api]=cpp-api.wallet [wd]=cpp-wd.wallet [xw]=xkrwallet-seed.wallet [beta]=beta-seed.wallet)
declare -A PASSW=([api]=$WL_PASS [wd]=$WL_PASS [xw]="" [beta]="")
declare -A FORMAT=([api]=A [wd]=B [xw]=B [beta]=A)
KINDS="api wd xw beta"

# expect_b <check> <text> <total>: PASS when <total> is B
expect_b() {
    if [ "$3" = "$B" ]; then pass "$1" "$2: balance $3"; else fail "$1" "$2: balance [$3], expected $B"; fi
}

# ------------------------------------------------------------------ the four C++ files

fx_fresh_chain
fx_mine "$FX_DUMMY_ADDR" 5
fx_api_start "$C_API" "$CAP"
wl_api "$CAP" POST /wallet/import/seed \
    "$(wl_wparams cpp "${FILE[api]}" "$WL_PASS" "$(jq -cn --arg s "$FX_SEED_A" '{mnemonicSeed: $s, scanHeight: 0}')")"
[ "$WL_CODE" = 200 ] || die cpp-api "C++ wallet-api import: HTTP $WL_CODE $(head -c 300 <<< "$WL_BODY")"
A=$(fx_api_q GET /addresses/primary "" "$CAP" | jq -r '.address' 2> /dev/null || true)
[ "${#A}" = 99 ] || die cpp-api "no primary address: $A"
rc=$(fx_wd_generate "$SD/generate-wd.log" "${FILE[wd]}" "$WL_PASS" --mnemonic-seed "$FX_SEED_A")
[ -s "$SD/w/${FILE[wd]}" ] || die cpp-wd "kryptokrona-service -g (exit $rc) wrote nothing: $(tail -n 3 "$SD/generate-wd.log" | tr '\n' ' ')"
for k in xw beta; do
    bin=xkrwallet
    [ "$k" = beta ] && bin=xkrwallet-beta
    wl_script "$SD/in/$k-restore.in" seed_restore "$FX_SEED_A" "${FILE[$k]%.wallet}" "" "" "@sleep 10" address exit
    rc=$(cpp_cli_session "$SD/tx/$k-restore.txt" "$bin" "$SD/in/$k-restore.in")
    if [ -s "$SD/w/${FILE[$k]}" ] && grep -aqF "$A" "$SD/tx/$k-restore.txt"; then
        pass "cpp-$k" "$bin seed_restore wrote ${FILE[$k]} for $A (exit $rc)"
    else
        die "cpp-$k" "$bin seed_restore (exit $rc) did not write ${FILE[$k]} for A ($SD/tx/$k-restore.txt)"
    fi
done
pass cpp-files "four C++ files from seed A: ${FILE[*]}"

# fund A, one send from the C++ wallet-api, B
fx_mine "$A" 35
fx_mine "$FX_DUMMY_ADDR" 140
wl_api_wait_or_die "$CAP" "C++ wallet-api"
wl_api_send cpp-api "C++ wallet-api" "$CAP" /transactions/send/basic \
    "$(jq -cn --arg d "$FX_DUMMY_ADDR" '{destination: $d, amount: 100000}')" || die cpp-api "the C++ send failed"
SEND=$WL_TX
fx_confirm "$SEND"
wl_api_wait_or_die "$CAP" "C++ wallet-api"
B=$(wl_api_total "$CAP")
[[ $B =~ ^[0-9]+$ ]] || die cpp-api "C++ wallet-api balance: $B"
pass cpp-api "C++ wallet-api sent 1 XKR away ($SEND); B = $B"
fx_api_q PUT /save "" "$CAP" > /dev/null
wl_api_close "$CAP"
fx_stop_tool "$C_API"

# the other C++ tools on their own files
fx_wd_start "$C_WD" "$CWP" "${FILE[wd]}" "$WL_PASS"
wl_wd_ok cpp-wd "C++ walletd" "$CWP" createAddress "$(jq -cn --arg k "$FX_SUB1" '{spendSecretKey: $k}')" || true
S1=$(jq -r '.result.address // empty' <<< "$WL_BODY" 2> /dev/null || true)
if wl_wd_wait "$CWP" "C++ walletd"; then
    expect_b cpp-wd "C++ walletd on ${FILE[wd]} (A and S1)" "$(wl_wd_total "$CWP")"
else
    fail cpp-wd "C++ walletd did not sync on ${FILE[wd]}"
fi
fx_stop_tool "$C_WD"
for k in xw beta; do
    bin=xkrwallet
    [ "$k" = beta ] && bin=xkrwallet-beta
    wl_script "$SD/in/$k-open.in" "" "@sleep 20" balance exit
    rc=$(cpp_cli_session "$SD/tx/$k-open.txt" "$bin" "$SD/in/$k-open.in" -w "${FILE[$k]}")
    expect_b "cpp-$k" "$bin on ${FILE[$k]} (exit $rc)" "$(cpp_cli_total "$SD/tx/$k-open.txt")"
done
declare -A ORIG_SHA=()
for k in $KINDS; do ORIG_SHA[$k]=$(wl_sha "$SD/w/${FILE[$k]}"); done

# container_kept <check> <copy> <kind>: a container copy is unchanged and has its state file
container_kept() {
    local id=$1 f=$2 k=$3
    [ "${FORMAT[$k]}" = B ] || return 0
    if [ "$(wl_sha "$SD/w/$f")" = "${ORIG_SHA[$k]}" ] && [ -s "$SD/w/$f.xkrstate" ]; then
        pass "$id" "$f left as the C++ tool wrote it; state in $f.xkrstate"
    else
        fail "$id" "$f: sha $(wl_sha "$SD/w/$f" | cut -c1-12) (C++ ${ORIG_SHA[$k]:0:12}), state file $([ -s "$SD/w/$f.xkrstate" ] && echo present || echo missing)"
    fi
}

# ------------------------------------------------------------------ Rust wallet-api on each file

rapi_start w62-rapi "$RAP"
watch w62-rapi
for k in $KINDS; do
    f=ra-${FILE[$k]}
    cp "$SD/w/${FILE[$k]}" "$SD/w/$f"
    if wl_api_open "rust-api-$k" "$RAP" rust "$f" "${PASSW[$k]}" "xkr-wallet-api"; then
        if wl_api_wait "$RAP" "xkr-wallet-api on $f"; then
            expect_b "rust-api-$k" "xkr-wallet-api on a copy of the ${k} file (format ${FORMAT[$k]})" "$(wl_api_total "$RAP")"
        else
            fail "rust-api-$k" "xkr-wallet-api did not sync on $f"
        fi
        wl_api_close "$RAP"
        sleep 1
        container_kept "sidecar-api-$k" "$f" "$k"
    fi
done
stop_proc w62-rapi
unwatch
watch "$FX_XNAME"

# ------------------------------------------------------------------ Rust walletd on each file

for k in $KINDS; do
    f=rw-${FILE[$k]}
    cp "$SD/w/${FILE[$k]}" "$SD/w/$f"
    rwd_start w62-rwd "$RWP" "$f" "${PASSW[$k]}"
    if wl_wd_wait "$RWP" "xkr-walletd on $f"; then
        expect_b "rust-wd-$k" "xkr-walletd on a copy of the ${k} file (format ${FORMAT[$k]})" "$(wl_wd_total "$RWP")"
    else
        fail "rust-wd-$k" "xkr-walletd did not sync on $f"
    fi
    stop_proc w62-rwd 60
    container_kept "sidecar-wd-$k" "$f" "$k"
done

# ------------------------------------------------------------------ Rust xkrwallet on each file

for k in $KINDS; do
    f=rx-${FILE[$k]}
    cp "$SD/w/${FILE[$k]}" "$SD/w/$f"
    wl_script "$SD/in/rust-cli-$k.in" balance exit
    rc=$(rcli_session "$SD/tx/rust-cli-$k.txt" "$SD/in/rust-cli-$k.in" -w "$f" -p "${PASSW[$k]}")
    t=$(rcli_total "$SD/tx/rust-cli-$k.txt")
    if [ "$rc" = 0 ] && [ "$t" = "$B" ]; then
        pass "rust-cli-$k" "xkrwallet on a copy of the ${k} file (format ${FORMAT[$k]}): balance $t, exit 0"
    else
        fail "rust-cli-$k" "xkrwallet on $f: exit [$rc], balance [$t], expected $B ($SD/tx/rust-cli-$k.txt)"
    fi
    container_kept "sidecar-cli-$k" "$f" "$k"
done

# ------------------------------------------------------------------ the container state file

f=rw-${FILE[wd]}
rwd_start w62-rwd "$RWP" "$f" "$WL_PASS"
if wl_wd_wait "$RWP" "xkr-walletd on $f again"; then
    expect_b sidecar-reopen "xkr-walletd reopened $f with its state file" "$(wl_wd_total "$RWP")"
else
    fail sidecar-reopen "xkr-walletd did not sync on $f with its state file"
fi
stop_proc w62-rwd 60
rm -f "$SD/w/$f.xkrstate"
rwd_start w62-rwd "$RWP" "$f" "$WL_PASS"
if wl_wd_wait "$RWP" "xkr-walletd on $f without its state file"; then
    expect_b sidecar-rescan "xkr-walletd without $f.xkrstate scanned again" "$(wl_wd_total "$RWP")"
else
    fail sidecar-rescan "xkr-walletd did not sync on $f without its state file"
fi
[ -s "$SD/w/$f.xkrstate" ] && pass sidecar-rescan "the state file is written again" || true
wl_wd_ok sidecar-keys "Rust walletd" "$RWP" createAddress "$(jq -cn --arg k "$FX_SUB2" '{spendSecretKey: $k}')" || true
S2=$(jq -r '.result.address // empty' <<< "$WL_BODY" 2> /dev/null || true)
stop_proc w62-rwd 60
if [ -n "$S2" ] && [ "$(wl_sha "$SD/w/$f")" != "${ORIG_SHA[wd]}" ]; then
    pass sidecar-keys "createAddress $S2 rewrote the container (new keys)"
else
    fail sidecar-keys "after createAddress [$S2] the container sha is $(wl_sha "$SD/w/$f" | cut -c1-12) (before ${ORIG_SHA[wd]:0:12})"
fi
fx_wd_start "$C_WD" "$CWP" "$f" "$WL_PASS"
addrs=$(fx_wd_q getAddresses "" "$CWP" | jq -c '.addresses | sort' 2> /dev/null || echo "no list")
want=$(jq -cn --arg a "$A" --arg b "$S1" --arg c "$S2" '[$a, $b, $c] | sort')
if [ "$addrs" = "$want" ]; then
    pass sidecar-keys "the C++ walletd opens the rewritten container with A, S1 and S2"
else
    fail sidecar-keys "C++ walletd addresses $addrs, expected $want"
fi
if wl_wd_wait "$CWP" "C++ walletd on $f"; then
    expect_b sidecar-keys "C++ walletd on the rewritten container" "$(wl_wd_total "$CWP")"
else
    fail sidecar-keys "C++ walletd did not sync on $f"
fi
fx_stop_tool "$C_WD"

# ------------------------------------------------------------------ back to C++

# WalletBackend files a Rust tool saved -> C++ wallet-api
fx_api_start "$C_API" "$CAP"
for t in ra rw rx; do
    for k in api beta; do
        f=$t-${FILE[$k]}
        if wl_api_open "back-api-$t-$k" "$CAP" cpp "$f" "${PASSW[$k]}" "C++ wallet-api"; then
            if wl_api_wait "$CAP" "C++ wallet-api on $f"; then
                expect_b "back-api-$t-$k" "C++ wallet-api on $f" "$(wl_api_total "$CAP")"
            else
                fail "back-api-$t-$k" "C++ wallet-api did not sync on $f"
            fi
            wl_api_close "$CAP"
            sleep 1
        fi
    done
done
fx_stop_tool "$C_API"

# containers a Rust tool used -> C++ walletd (rw-cpp-wd.wallet was checked above)
for item in ra:wd rx:wd ra:xw rw:xw rx:xw; do
    t=${item%%:*}
    k=${item#*:}
    f=$t-${FILE[$k]}
    fx_wd_start "$C_WD" "$CWP" "$f" "${PASSW[$k]}"
    if wl_wd_wait "$CWP" "C++ walletd on $f"; then
        expect_b "back-wd-$t-$k" "C++ walletd on $f" "$(wl_wd_total "$CWP")"
    else
        fail "back-wd-$t-$k" "C++ walletd did not sync on $f"
    fi
    fx_stop_tool "$C_WD"
done

# the terminal wallets on what xkrwallet left
wl_script "$SD/in/back-xw.in" "" "@sleep 20" balance exit
rc=$(cpp_cli_session "$SD/tx/back-xw.txt" xkrwallet "$SD/in/back-xw.in" -w "rx-${FILE[xw]}")
expect_b back-cli-xw "C++ xkrwallet on the container xkrwallet used (exit $rc)" "$(cpp_cli_total "$SD/tx/back-xw.txt")"
wl_script "$SD/in/back-beta.in" "" "@sleep 20" balance exit
rc=$(cpp_cli_session "$SD/tx/back-beta.txt" xkrwallet-beta "$SD/in/back-beta.in" -w "rx-${FILE[beta]}")
expect_b back-cli-beta "C++ xkrwallet-beta on the file xkrwallet saved (exit $rc)" "$(cpp_cli_total "$SD/tx/back-beta.txt")"
finish
