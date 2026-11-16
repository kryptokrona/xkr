#!/usr/bin/env bash
# 63-wallet-fork14: the Rust xkr-wallet-api through fork 14 (v1 before F, the version the node allows in the grace window, v2 after, v2 coinbase received) and the deadline of the C++ wallet-api on xkrd.
#
#   xkrd X       own testnet chain from genesis, P2P 26398 / RPC 26399, --fork13-height F13
#                --fork14-height F14 --fork14-grace GRACE, mined only with `xkr-testwallet mine`
#                (exact heights); xkr_rpc handler debug lines on, so a refused transaction is logged
#   Rust         target/release/xkr-wallet-api, port 26371, seed A
#   C++          wallet-api from WL_IMAGE (xkr-cpp-wallets:master, -O2 reference build), container
#                xkr-wl-63-api, port 26370, seed B; it syncs from xkrd like an exchange that
#                replaced only the node
#
# Heights (defaults F13 40, F14 160, GRACE 20): blocks 6..35 pay A, 36..65 pay B, filler to
# F14 - 20; /getinfo tx_versions is [1] up to F14, [1,2] from F14 + 1 to F14 + GRACE, [2] after.
# Checks:
#   setup        both wallets synced; X's /getinfo has the fork heights and tx_versions [1]
#   before       Rust and C++ send (Rust to B, C++ to A): both version 1, mined at or below F14
#   grace        at F14 + 1 (tx_versions [1,2]): the Rust send has a version the node lists (the
#                engine takes the highest: 2) and is mined in the window; the C++ send (version 1)
#                is still mined in the window
#   coinbase     the next block in the window pays A with a version-2 coinbase (tagged
#                outputs); the Rust wallet lists it as a coinbase transaction
#   after        after F14 + GRACE (tx_versions [2]): the Rust send is version 2 and mined; the C++
#                wallet-api send is refused with "transaction version not allowed at this height"
#                (in its answer or X's log) and nothing reaches the pool: the last height at which
#                C++ wallets can send on xkrd is F14 + GRACE
# The C++ sends use the default ring size; when the wallet picks an input whose amount has too
# few outputs on this short chain for that ring, it sends again with mixin 0 (cpp_send).
#   receive      the C++ wallet-api, attached to xkrd, still syncs past F14 and lists the version-2
#                transfers the Rust wallet sent to B in the window and after it
#
# Env: F13 (40), F14 (160), GRACE (20), WL_IMAGE, XKR_WALLET_API, FX_SYNC_WAIT (240),
# FX_CONFIRM (12), F14_LOG_LEVEL (info,xkr_rpc::handlers=debug), WALLET_LABEL, TIMEOUT, KEEP=1.
# suite: expected=360 timeout=1800
set -euo pipefail
: "${TIMEOUT:=300}"
. "$(dirname "$0")/../fixtures/lib-wallet.sh"
wl_init "${1:-}" "$PORT_W63_P2P" "$PORT_W63_RPC"
require_wallet_api
require_cpp_wallets wallet-api

F13=${F13:-40}
F14=${F14:-160}
GRACE=${GRACE:-20}
LOG_LEVEL=${F14_LOG_LEVEL:-info,xkr_rpc::handlers=debug}
[ "$F13" -ge 36 ] && [ "$F14" -ge $((F13 + 100)) ] && [ "$GRACE" -ge 5 ] \
    || die setup "F13 ($F13) >= 36, F14 ($F14) >= F13 + 100 and GRACE ($GRACE) >= 5 are needed for the funding and the windows"
C_API=xkr-wl-63-api
CP=$PORT_W63_CPP_API
RP=$PORT_W63_RUST_API
rm_container "$C_API"
reclaim w63-rapi

# coinbase_at <height>: the coinbase transaction hash of a block (f_block_json, as in 31-fork14)
coinbase_at() {
    local r
    r=$(rpc "$FX_RPC" f_block_json "{\"hash\":\"$1\"}")
    { flat "$r" | grep -o '"transactions":\[{[^}]*}' | grep -o '"hash":"[0-9a-f]*"' || true; } \
        | head -n 1 | cut -d'"' -f4
}
tx_versions() { arr "$(get "$FX_RPC" /getinfo)" tx_versions; }
# mined_in <check> <name> <hash> <lowest> <highest> <wanted versions, e.g. "1" or "1 2">
mined_in() {
    local id=$1 name=$2 h=$3 lo=$4 hi=$5 want=$6 ht v
    ht=$(wl_tx_height "$h")
    v=$(wl_tx_version "$h")
    if [ -n "$ht" ] && [ "$ht" -ge "$lo" ] && [ "$ht" -le "$hi" ] && [[ " $want " == *" $v "* ]]; then
        pass "$id" "$name: version $v, mined at $ht (window $lo..$hi): $h"
    else
        fail "$id" "$name: version [$v] (wanted $want), height [$ht] (window $lo..$hi): $h"
    fi
}
api_tx() { # <port> <hash>: the wallet's record of a transaction, compact, or ""
    fx_api_q GET /transactions "" "$1" | jq -c --arg h "$2" '.transactions[] | select(.hash == $h)' 2> /dev/null || true
}
# cpp_send <amount>: the C++ wallet-api sends <amount> to A with /transactions/send/basic (default
# ring size). The wallet picks its inputs at random; when one of them has an amount with too few
# outputs on this short chain to fill the ring (errorCode 28), the same send is made again with
# /transactions/send/advanced and mixin 0 (testnet heights are below the mixin limits), so the
# transaction always reaches xkrd. Sets WL_CODE and WL_BODY.
cpp_send() {
    wl_api "$CP" POST /transactions/send/basic "$(jq -cn --arg d "$A" --argjson a "$1" '{destination: $d, amount: $a}')"
    if [ "$(jq -r '.errorCode // empty' <<< "$WL_BODY" 2> /dev/null || true)" = 28 ]; then
        note "C++ wallet-api: $(jq -r '.errorMessage' <<< "$WL_BODY" 2> /dev/null | cut -c1-110); sending again with mixin 0"
        wl_api "$CP" POST /transactions/send/advanced \
            "$(jq -cn --arg d "$A" --argjson a "$1" '{destinations: [{address: $d, amount: $a}], mixin: 0}')"
    fi
}
# cpp_send_tx <check> <amount>: cpp_send that must give a transaction (hash in WL_TX, else FAIL)
cpp_send_tx() {
    WL_TX=""
    cpp_send "$2"
    if WL_TX=$(jq -re '.transactionHash | select(type == "string" and length == 64)' <<< "$WL_BODY" 2> /dev/null); then
        return 0
    fi
    WL_TX=""
    fail "$1" "C++ send of $2 gave no transaction: HTTP $WL_CODE $(printf '%s' "$WL_BODY" | head -c 300)"
    return 1
}

# ------------------------------------------------------------------ setup

fx_fresh_chain --fork13-height "$F13" --fork14-height "$F14" --fork14-grace "$GRACE"
info=$(get "$FX_RPC" /getinfo)
if printf '%s' "$(arr "$info" upgrade_heights)" | grep -q ",$F13,$F14\]$" && [ "$(arr "$info" tx_versions)" = "[1]" ]; then
    pass setup "X: upgrade_heights end with $F13,$F14, tx_versions [1]"
else
    fail setup "X /getinfo: upgrade_heights $(arr "$info" upgrade_heights), tx_versions $(arr "$info" tx_versions)"
fi
fx_mine "$FX_DUMMY_ADDR" 5
fx_api_start "$C_API" "$CP"
rapi_start w63-rapi "$RP"
watch w63-rapi
wl_api "$RP" POST /wallet/import/seed \
    "$(wl_wparams rust rust-a.wallet "$WL_PASS" "$(jq -cn --arg s "$FX_SEED_A" '{mnemonicSeed: $s, scanHeight: 0}')")"
[ "$WL_CODE" = 200 ] || die setup "Rust import: HTTP $WL_CODE $(head -c 300 <<< "$WL_BODY")"
wl_api "$CP" POST /wallet/import/seed \
    "$(wl_wparams cpp cpp-b.wallet "$WL_PASS" "$(jq -cn --arg s "$FX_SEED_B" '{mnemonicSeed: $s, scanHeight: 0}')")"
[ "$WL_CODE" = 200 ] || die setup "C++ import: HTTP $WL_CODE $(head -c 300 <<< "$WL_BODY")"
A=$(fx_api_q GET /addresses/primary "" "$RP" | jq -r '.address' 2> /dev/null || true)
B=$(fx_api_q GET /addresses/primary "" "$CP" | jq -r '.address' 2> /dev/null || true)
[ "${#A}" = 99 ] && [ "${#B}" = 99 ] || die setup "addresses: Rust [$A] C++ [$B]"
fx_mine "$A" 35
fx_mine "$B" 65
fx_mine "$FX_DUMMY_ADDR" $((F14 - 20))
wl_api_wait_or_die "$RP" "xkr-wallet-api"
wl_api_wait_or_die "$CP" "C++ wallet-api"
pass setup "Rust (seed A, $A) and C++ (seed B, $B) synced at $(height_of "$FX_RPC")"

# ------------------------------------------------------------------ before F14: version 1

RUST_TO_B=()
if wl_api_send before "Rust" "$RP" /transactions/send/basic "$(jq -cn --arg d "$B" '{destination: $d, amount: 1000000}')"; then
    R1=$WL_TX
fi
if cpp_send_tx before 1000000; then
    C1=$WL_TX
fi
fx_confirm ${R1:-} ${C1:-}
[ -n "${R1:-}" ] && mined_in before "Rust send before F14" "$R1" 1 "$F14" 1
[ -n "${C1:-}" ] && mined_in before "C++ send before F14" "$C1" 1 "$F14" 1

# ------------------------------------------------------------------ grace window

fx_mine "$FX_DUMMY_ADDR" $((F14 + 1))
tv=$(tx_versions)
[ "$tv" = "[1,2]" ] && pass grace "X at $((F14 + 1)): tx_versions $tv" || fail grace "X at $((F14 + 1)): tx_versions $tv, want [1,2]"
wl_api_wait_or_die "$RP" "xkr-wallet-api"
if wl_api_wait "$CP" "C++ wallet-api"; then
    pass grace "the C++ wallet-api synced past F14 ($(num "$(fx_api_q GET /status "" "$CP")" walletBlockCount))"
else
    fail grace "the C++ wallet-api did not sync past F14"
fi
R2="" C2=""
if wl_api_send grace "Rust" "$RP" /transactions/send/basic "$(jq -cn --arg d "$B" '{destination: $d, amount: 500000}')"; then
    R2=$WL_TX
    RUST_TO_B+=("$R2")
fi
if cpp_send_tx grace 500000; then
    C2=$WL_TX
fi
fx_wait_pool $R2 $C2 || note "mining anyway"
fx_mine_count "$FX_DUMMY_ADDR" 1
# any version the node lists is right; the engine takes the highest, so 2 is expected here
[ -n "$R2" ] && mined_in grace "Rust send in the grace window (tx_versions $tv)" "$R2" $((F14 + 1)) $((F14 + GRACE)) "$(tr -d '[]' <<< "$tv" | tr ',' ' ')"
[ -n "$C2" ] && mined_in grace "C++ send in the grace window" "$C2" $((F14 + 1)) $((F14 + GRACE)) "1"

# ------------------------------------------------------------------ v2 coinbase to the Rust wallet

CBH=$(($(height_of "$FX_RPC") + 1))
[ "$CBH" -le $((F14 + GRACE)) ] || fail coinbase "the coinbase block $CBH is past the grace window (raise GRACE)"
fx_mine "$A" "$CBH"
CB=$(coinbase_at "$CBH")
cbj=$(tx_json "$FX_RPC" "$CB")
if [ -n "$CB" ] && [ "$(num "$cbj" version)" = 2 ] && [ "$(count "$cbj" '"type":"03"')" -gt 0 ]; then
    pass coinbase "block $CBH pays A with a version-2 coinbase (tagged outputs): $CB"
else
    fail coinbase "coinbase of $CBH [$CB]: version $(num "$cbj" version), $(count "$cbj" '"type":"03"') tagged outputs"
fi
wl_api_wait_or_die "$RP" "xkr-wallet-api"
rec=$(api_tx "$RP" "$CB")
if [ -n "$CB" ] && [ "$(jq -r '.isCoinbaseTransaction' <<< "$rec" 2> /dev/null || true)" = true ] \
    && [ "$(jq -r '[.transfers[].amount] | add > 0' <<< "$rec" 2> /dev/null || true)" = true ]; then
    pass coinbase "the Rust wallet lists the version-2 coinbase: $(jq -c '{blockHeight, transfers}' <<< "$rec")"
else
    fail coinbase "the Rust wallet's record of $CB: [$rec]"
fi

# ------------------------------------------------------------------ after the grace window

fx_mine "$FX_DUMMY_ADDR" $((F14 + GRACE + 1))
tv=$(tx_versions)
[ "$tv" = "[2]" ] && pass after "X at $((F14 + GRACE + 1)): tx_versions $tv" || fail after "X at $((F14 + GRACE + 1)): tx_versions $tv, want [2]"
wl_api_wait_or_die "$RP" "xkr-wallet-api"
wl_api_wait "$CP" "C++ wallet-api" || note "the C++ wallet-api is behind; its send below is tried anyway"
R3=""
if wl_api_send after "Rust" "$RP" /transactions/send/basic "$(jq -cn --arg d "$B" '{destination: $d, amount: 400000}')"; then
    R3=$WL_TX
    RUST_TO_B+=("$R3")
fi
pool0=$(num "$(get "$FX_RPC" /getinfo)" tx_pool_size)
log0=$(log_count "$SD/xkrd.log" "transaction version not allowed")
cpp_send 300000
c_code=$WL_CODE
c_body=$WL_BODY
sleep 2
pool1=$(num "$(get "$FX_RPC" /getinfo)" tx_pool_size)
log1=$(log_count "$SD/xkrd.log" "transaction version not allowed")
c_hash=$(jq -r '.transactionHash // empty' <<< "$c_body" 2> /dev/null || true)
if [ -z "$c_hash" ] && [ "${pool1:-x}" = "${pool0:-y}" ] \
    && { grep -qF "transaction version not allowed" <<< "$c_body" || [ "${log1:-0}" -gt "${log0:-0}" ]; }; then
    pass after "the C++ wallet-api send after F14 + GRACE is refused (HTTP $c_code): \"transaction version not allowed at this height\"; deadline for C++ wallets on xkrd: block $((F14 + GRACE))"
else
    fail after "C++ send after the grace window: HTTP $c_code $(head -c 300 <<< "$c_body"); pool $pool0 -> $pool1; refusals in X's log $log0 -> $log1"
fi
fx_confirm $R3
[ -n "$R3" ] && mined_in after "Rust send after F14 + GRACE" "$R3" $((F14 + GRACE + 1)) $((F14 + GRACE + 100)) "2"

# ------------------------------------------------------------------ the C++ wallet still receives

if wl_api_wait "$CP" "C++ wallet-api"; then
    pass receive "the C++ wallet-api synced to $(height_of "$FX_RPC") (block version $(header_at "$FX_RPC" "$(height_of "$FX_RPC")" | cut -d' ' -f2))"
else
    fail receive "the C++ wallet-api did not sync after the grace window"
fi
for h in ${RUST_TO_B[@]+"${RUST_TO_B[@]}"}; do
    rec=$(api_tx "$CP" "$h")
    v=$(wl_tx_version "$h")
    if [ -n "$rec" ] && [ "$(jq -r '[.transfers[].amount] | add' <<< "$rec" 2> /dev/null || true)" -gt 0 ] 2> /dev/null; then
        pass receive "the C++ wallet-api lists the version-$v transfer $h: $(jq -c '{blockHeight, transfers}' <<< "$rec")"
    else
        fail receive "the C++ wallet-api does not list the version-$v transfer $h (record [$rec])"
    fi
done
wl_api_close "$RP"
wl_api_close "$CP"
fx_stop_tool "$C_API"
stop_proc w63-rapi
finish
