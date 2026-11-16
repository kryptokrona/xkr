#!/usr/bin/env bash
# 60-wallet-api: the Rust xkr-wallet-api next to the C++ wallet-api against one xkrd, same keys, same answers, each opens the other's file.
#
#   xkrd X       own testnet chain from genesis, P2P 26098 / RPC 26099, mined only with
#                `xkr-testwallet mine` (exact heights)
#   C++          wallet-api from WL_IMAGE (xkr-cpp-wallets:master, -O2 reference build), container
#                xkr-wl-60-api, --network host, port 26070, files in <sd>/w (= /w)
#   Rust         target/release/xkr-wallet-api, port 26071, the same flags plus --network testnet
#
# Both import seed A (FX_SEED_A) and the subwallets S1, S2 (fixed spend keys), so both hold the
# same keys and must see the same chain the same way. Sends alternate between the two programs;
# after each batch X mines FX_CONFIRM blocks and both must have synced.
# Checks:
#   import       both import the seed and the subwallets; same primary address, same address list
#   funded       after the coinbase (blocks 6..35 pay A, filler to 140): /balance, /balances,
#                /transactions (transfers and list sorted), /status (block counts, view flag,
#                subwallet count) and /addresses equal
#   send-cpp     C++ /transactions/send/basic to S1's Xkr form
#   send-rust    Rust /send/basic to S1 (SEKR) with a payment ID; Rust /send/advanced to S1's Xkr
#                form and a foreign address with a payment ID, source and change address, plus
#                15 small outputs to S1
#   send-cpp2    C++ /send/basic to an integrated address of S1 (made by C++); C++ /send/advanced
#                with 15 small outputs to S2's Xkr form
#   sends        every send mined; both programs report the same five parts again
#   fusion       Rust /send/fusion/advanced on S1, C++ on S2: both mined with fee 0; same parts
#   files        both save and close; C++ opens a copy of the Rust file, Rust a copy of the C++
#                file; after sync each reports what the other reported before closing
#
# Env: WL_IMAGE, XKR_WALLET_API, FX_SYNC_WAIT (240), FX_CONFIRM (12), WALLET_LABEL, TIMEOUT,
# KEEP=1.
# suite: expected=420 timeout=1800
set -euo pipefail
: "${TIMEOUT:=300}"
. "$(dirname "$0")/../fixtures/lib-wallet.sh"
wl_init "${1:-}" "$PORT_W60_P2P" "$PORT_W60_RPC"
require_wallet_api
require_cpp_wallets wallet-api

C_API=xkr-wl-60-api
CP=$PORT_W60_CPP_API
RP=$PORT_W60_RUST_API
SNAP=$SD/snap
rm_container "$C_API"
reclaim w60-rapi

# ------------------------------------------------------------------ chain and both wallet-api

fx_fresh_chain
fx_mine "$FX_DUMMY_ADDR" 5
fx_api_start "$C_API" "$CP"
rapi_start w60-rapi "$RP"
watch w60-rapi

SEED_BODY=$(jq -cn --arg s "$FX_SEED_A" '{mnemonicSeed: $s, scanHeight: 0}')
wl_api "$CP" POST /wallet/import/seed "$(wl_wparams cpp cpp-a.wallet "$WL_PASS" "$SEED_BODY")"
c_code=$WL_CODE c_body=$WL_BODY
wl_api "$RP" POST /wallet/import/seed "$(wl_wparams rust rust-a.wallet "$WL_PASS" "$SEED_BODY")"
if [ "$c_code" = 200 ] && [ "$WL_CODE" = 200 ]; then
    pass import "both imported seed A (HTTP 200)"
else
    die import "seed import: C++ HTTP $c_code $(head -c 200 <<< "$c_body"), Rust HTTP $WL_CODE $(head -c 200 <<< "$WL_BODY")"
fi
A=$(fx_api_q GET /addresses/primary "" "$CP" | jq -r '.address' 2> /dev/null || true)
RA=$(fx_api_q GET /addresses/primary "" "$RP" | jq -r '.address' 2> /dev/null || true)
[ "${#A}" = 99 ] && [ "$A" = "$RA" ] && pass import "same primary address $A" \
    || die import "primary address: C++ [$A] Rust [$RA]"
for k in "$FX_SUB1" "$FX_SUB2"; do
    body=$(jq -cn --arg k "$k" '{privateSpendKey: $k, scanHeight: 0}')
    wl_api "$CP" POST /addresses/import "$body"
    c_body=$WL_BODY
    wl_api "$RP" POST /addresses/import "$body"
    ca=$(jq -r '.address // "none"' <<< "$c_body" 2> /dev/null || echo none)
    ra=$(jq -r '.address // "none"' <<< "$WL_BODY" 2> /dev/null || echo none)
    [ "${#ca}" = 99 ] && [ "$ca" = "$ra" ] && pass import "subwallet $ca imported in both" \
        || die import "subwallet import: C++ $(head -c 200 <<< "$c_body") Rust $(head -c 200 <<< "$WL_BODY")"
    if [ "$k" = "$FX_SUB1" ]; then S1=$ca; else S2=$ca; fi
done
A_XKR=$(fx_alt_address "$A")
S1_XKR=$(fx_alt_address "$S1")
S2_XKR=$(fx_alt_address "$S2")
[ -n "$S1_XKR" ] && [ -n "$S2_XKR" ] || die import "no Xkr form of $S1 / $S2"
note "A $A (Xkr $A_XKR), S1 $S1, S2 $S2"

both_synced() { # <check>
    wl_api_wait "$CP" "C++ wallet-api" || die "$1" "C++ wallet-api did not sync"
    wl_api_wait "$RP" "xkr-wallet-api" || die "$1" "xkr-wallet-api did not sync"
}
compare_now() { # <check>
    wl_api_snapshot "$1" "$CP" "$SNAP/$1-cpp"
    wl_api_snapshot "$1" "$RP" "$SNAP/$1-rust"
    wl_api_compare "$1" "$SNAP/$1-cpp" "$SNAP/$1-rust" "C++" "Rust"
}

# ------------------------------------------------------------------ funded

fx_mine "$A" 35
fx_mine "$FX_DUMMY_ADDR" 140
both_synced funded
compare_now funded
note "balance after the coinbase: $(jq -c . "$SNAP/funded-cpp.balance.json" 2> /dev/null || cat "$SNAP/funded-cpp.balance.json")"

# ------------------------------------------------------------------ sends

TXS=()   # every send
BATCH=() # the sends since the last confirmation
sent() { # <check> <text>: after a successful wl_api_send
    TXS+=("$WL_TX")
    BATCH+=("$WL_TX")
    pass "$1" "$2: $WL_TX"
}
confirm_batch() { # <check>
    fx_confirm ${BATCH[@]+"${BATCH[@]}"}
    BATCH=()
    both_synced "$1"
}

if wl_api_send send-cpp "C++" "$CP" /transactions/send/basic \
    "$(jq -cn --arg d "$S1_XKR" '{destination: $d, amount: 1000000}')"; then
    sent send-cpp "C++ basic send of 10 XKR to S1's Xkr form"
fi
confirm_batch send-cpp

if wl_api_send send-rust "Rust" "$RP" /transactions/send/basic \
    "$(jq -cn --arg d "$S1" --arg p "$FX_PID1" '{destination: $d, amount: 2000000, paymentID: $p}')"; then
    sent send-rust "Rust basic send of 20 XKR to S1 with a payment ID"
fi
ADV=$(jq -cn --arg s1x "$S1_XKR" --arg s1 "$S1" --arg d "$FX_DUMMY_ADDR" --arg a "$A" --arg p "$FX_PID2" '
    {destinations: ([{address: $s1x, amount: 300000}, {address: $d, amount: 400000}]
                    + [range(15) | {address: $s1, amount: 10000}]),
     paymentID: $p, sourceAddresses: [$a], changeAddress: $a}')
if wl_api_send send-rust "Rust" "$RP" /transactions/send/advanced "$ADV"; then
    sent send-rust "Rust advanced send (S1 Xkr form, a foreign address, 15 x 0.1 XKR to S1, payment ID, source and change A)"
fi
confirm_batch send-rust

INT_S1=$(fx_api_q GET "/addresses/$S1/$FX_PID1" "" "$CP" | jq -r '.integratedAddress // empty' 2> /dev/null || true)
if [ -n "$INT_S1" ]; then
    if wl_api_send send-cpp2 "C++" "$CP" /transactions/send/basic \
        "$(jq -cn --arg d "$INT_S1" '{destination: $d, amount: 500000}')"; then
        sent send-cpp2 "C++ basic send of 5 XKR to an integrated address of S1"
    fi
else
    fail send-cpp2 "C++ made no integrated address for S1: $(fx_api_q GET "/addresses/$S1/$FX_PID1" "" "$CP" | head -c 200)"
fi
ADV2=$(jq -cn --arg s2x "$S2_XKR" --arg a "$A" '
    {destinations: [range(15) | {address: $s2x, amount: 10000}], sourceAddresses: [$a], changeAddress: $a}')
if wl_api_send send-cpp2 "C++" "$CP" /transactions/send/advanced "$ADV2"; then
    sent send-cpp2 "C++ advanced send of 15 x 0.1 XKR to S2's Xkr form"
fi
confirm_batch sends

unmined=""
for t in ${TXS[@]+"${TXS[@]}"}; do
    [ -n "$(wl_tx_height "$t")" ] || unmined="$unmined $t"
done
if [ "${#TXS[@]}" = 5 ] && [ -z "$unmined" ]; then
    pass sends "all 5 sends mined on X"
else
    fail sends "${#TXS[@]} of 5 sends made; not mined:${unmined:- none}"
fi
compare_now sends

# ------------------------------------------------------------------ fusion

FUS=()
if wl_api_send fusion "Rust" "$RP" /transactions/send/fusion/advanced \
    "$(jq -cn --arg s "$S1" '{destination: $s, sourceAddresses: [$s]}')"; then
    sent fusion "Rust fusion of S1's small outputs"
    FUS+=("$WL_TX")
fi
if wl_api_send fusion "C++" "$CP" /transactions/send/fusion/advanced \
    "$(jq -cn --arg s "$S2" '{destination: $s, sourceAddresses: [$s]}')"; then
    sent fusion "C++ fusion of S2's small outputs"
    FUS+=("$WL_TX")
fi
confirm_batch fusion
for t in ${FUS[@]+"${FUS[@]}"}; do
    ht=$(wl_tx_height "$t")
    fee=$(fx_api_q GET "/transactions/hash/$t" "" "$RP" | jq -r '.transaction.fee' 2> /dev/null || echo "no answer")
    if [ -n "$ht" ] && [ "$fee" = 0 ]; then
        pass fusion "fusion $t mined at $ht with fee 0"
    else
        fail fusion "fusion $t: height [$ht], fee [$fee]"
    fi
done
compare_now fusion

# ------------------------------------------------------------------ files

fx_api_q PUT /save "" "$CP" > /dev/null
fx_api_q PUT /save "" "$RP" > /dev/null
wl_api_close "$CP"
wl_api_close "$RP"
sleep 2
cp "$SD/w/rust-a.wallet" "$SD/w/cpp-opens-rust.wallet"
cp "$SD/w/cpp-a.wallet" "$SD/w/rust-opens-cpp.wallet"
pass files "both saved and closed; files $(wl_sha "$SD/w/cpp-a.wallet" | cut -c1-12) (C++) and $(wl_sha "$SD/w/rust-a.wallet" | cut -c1-12) (Rust)"
if wl_api_open files "$CP" cpp cpp-opens-rust.wallet "$WL_PASS" "C++ wallet-api" \
    && wl_api_open files "$RP" rust rust-opens-cpp.wallet "$WL_PASS" "xkr-wallet-api"; then
    pass files "C++ opened the Rust file, Rust opened the C++ file"
    both_synced files
    wl_api_snapshot files "$CP" "$SNAP/files-cpp"
    wl_api_snapshot files "$RP" "$SNAP/files-rust"
    wl_api_compare files-cpp-opens-rust "$SNAP/fusion-rust" "$SNAP/files-cpp" "Rust before closing" "C++ on the Rust file"
    wl_api_compare files-rust-opens-cpp "$SNAP/fusion-cpp" "$SNAP/files-rust" "C++ before closing" "Rust on the C++ file"
    wl_api_close "$CP"
    wl_api_close "$RP"
fi
fx_stop_tool "$C_API"
stop_proc w60-rapi
finish
