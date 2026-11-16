#!/usr/bin/env bash
# 61-walletd: the Rust xkr-walletd next to the C++ kryptokrona-service against one xkrd: same keys, same JSON-RPC answers, each opens the other's export.
#
#   xkrd X       own testnet chain from genesis, P2P 26198 / RPC 26199, mined only with
#                `xkr-testwallet mine`; it serves /queryblockslite, so the C++ walletd syncs from
#                it directly
#   C++          kryptokrona-service from WL_IMAGE (xkr-cpp-wallets:master, -O2 reference build),
#                container xkr-wl-61-walletd, port 26180, files in <sd>/w (= /w)
#   Rust         target/release/xkr-walletd, port 26181, the same flags plus --network testnet
#
# Both generate a container from seed A (-g --mnemonic-seed) and add S1 (createAddress with a
# spend key). Checks:
#   generate     both write a container; same first address
#   addresses    createAddress S1 gives the same address; a random createAddress and its
#                deleteAddress work in both; getAddresses equal
#   funded       after the coinbase: getBalance (wallet and per address), getAddresses,
#                getTransactions (firstBlockIndex 1 over the whole chain, transfers sorted),
#                getStatus (block counts, last block hash), getDelayedTransactionHashes equal
#   validate     validateAddress of the Xkr forms of A, S1 and a foreign address: valid, and the
#                same answer in both
#   send-cpp     C++ sendTransaction to S1's Xkr form with a payment ID and changeAddress
#   send-rust    Rust sendTransaction to S1 and a foreign address with source and change address
#   delayed-*    createDelayedTransaction, getDelayedTransactionHashes, sendDelayedTransaction of
#                one, deleteDelayedTransaction of another, in Rust and then in C++
#   sends        every send mined; the same parts equal again, the transaction lists with each
#                program's own sends in observer form (foreign address "", change type 0)
#   export       both export; C++ opens the Rust export, Rust the C++ export; each then reports
#                what the other reported before
#
# Env: WL_IMAGE, XKR_WALLETD, FX_SYNC_WAIT (240), FX_CONFIRM (12), WALLET_LABEL, TIMEOUT, KEEP=1.
# suite: expected=420 timeout=1800
set -euo pipefail
: "${TIMEOUT:=300}"
. "$(dirname "$0")/../fixtures/lib-wallet.sh"
wl_init "${1:-}" "$PORT_W61_P2P" "$PORT_W61_RPC"
require_walletd
require_cpp_wallets kryptokrona-service

C_WD=xkr-wl-61-walletd
CP=$PORT_W61_CPP_WD
RP=$PORT_W61_RUST_WD
SNAP=$SD/snap
rm_container "$C_WD"
reclaim w61-rwd

# ------------------------------------------------------------------ chain and containers

fx_fresh_chain
fx_mine "$FX_DUMMY_ADDR" 5
rc_c=$(fx_wd_generate "$SD/generate-cpp.log" cpp-wd.wallet "$WL_PASS" --mnemonic-seed "$FX_SEED_A")
rc_r=$(rwd_generate "$SD/generate-rust.log" rust-wd.wallet "$WL_PASS" --mnemonic-seed "$FX_SEED_A")
if [ -s "$SD/w/cpp-wd.wallet" ] && [ -s "$SD/w/rust-wd.wallet" ]; then
    pass generate "both wrote a container from seed A (exit C++ $rc_c, Rust $rc_r)"
else
    die generate "C++ exit $rc_c $(tail -n 3 "$SD/generate-cpp.log" | tr '\n' ' '); Rust exit $rc_r $(tail -n 3 "$SD/generate-rust.log" | tr '\n' ' ')"
fi
fx_wd_start "$C_WD" "$CP" cpp-wd.wallet "$WL_PASS"
rwd_start w61-rwd "$RP" rust-wd.wallet "$WL_PASS"
watch w61-rwd
A=$(fx_wd_q getAddresses "" "$CP" | jq -r '.addresses[0]' 2> /dev/null || true)
RA=$(fx_wd_q getAddresses "" "$RP" | jq -r '.addresses[0]' 2> /dev/null || true)
[ "${#A}" = 99 ] && [ "$A" = "$RA" ] && pass generate "same first address $A" \
    || die generate "first address: C++ [$A] Rust [$RA]"

both_synced() { # <check>
    wl_wd_wait "$CP" "C++ kryptokrona-service" || die "$1" "C++ walletd did not sync"
    wl_wd_wait "$RP" "xkr-walletd" || die "$1" "xkr-walletd did not sync"
}
compare_now() { # <check>
    wl_wd_snapshot "$1" "$CP" "$SNAP/$1-cpp"
    wl_wd_snapshot "$1" "$RP" "$SNAP/$1-rust"
    wl_wd_compare "$1" "$SNAP/$1-cpp" "$SNAP/$1-rust" "C++" "Rust"
}

# ------------------------------------------------------------------ addresses

P_S1=$(jq -cn --arg k "$FX_SUB1" '{spendSecretKey: $k}')
S1=""
if wl_wd_ok addresses "C++" "$CP" createAddress "$P_S1"; then
    S1=$(jq -r '.result.address' <<< "$WL_BODY")
fi
RS1=""
if wl_wd_ok addresses "Rust" "$RP" createAddress "$P_S1"; then
    RS1=$(jq -r '.result.address' <<< "$WL_BODY")
fi
[ "${#S1}" = 99 ] && [ "$S1" = "$RS1" ] && pass addresses "createAddress with S1's spend key: $S1 in both" \
    || die addresses "createAddress S1: C++ [$S1] Rust [$RS1]"
for side in "C++:$CP" "Rust:$RP"; do
    name=${side%%:*}
    port=${side#*:}
    if wl_wd_ok addresses "$name" "$port" createAddress; then
        r=$(jq -r '.result.address' <<< "$WL_BODY")
        if [ "${#r}" = 99 ] && wl_wd_ok addresses "$name" "$port" deleteAddress "$(jq -cn --arg a "$r" '{address: $a}')"; then
            pass addresses "$name: createAddress (random) $r and deleteAddress"
        fi
    fi
done
S1_XKR=$(fx_alt_address "$S1")
A_XKR=$(fx_alt_address "$A")
D_XKR=$(fx_alt_address "$FX_DUMMY_ADDR")
[ -n "$S1_XKR" ] && [ -n "$A_XKR" ] || die addresses "no Xkr form of $A / $S1"

# ------------------------------------------------------------------ funded

fx_mine "$A" 35
fx_mine "$FX_DUMMY_ADDR" 140
both_synced funded
compare_now funded

for addr in "$A_XKR" "$S1_XKR" "$D_XKR"; do
    p=$(jq -cn --arg a "$addr" '{address: $a}')
    c=$(fx_wd_q validateAddress "$p" "$CP" | jq -S -c . 2> /dev/null || echo "no answer")
    r=$(fx_wd_q validateAddress "$p" "$RP" | jq -S -c . 2> /dev/null || echo "no answer")
    if [ "$(jq -r '.isValid' <<< "$r" 2> /dev/null || true)" = true ] && [ "$c" = "$r" ]; then
        pass validate "validateAddress ${addr:0:12}…: valid, same answer in both ($r)"
    else
        fail validate "validateAddress $addr: C++ $c / Rust $r"
    fi
done

# ------------------------------------------------------------------ sends

TXS=()
BATCH=()
CPP_SENT=()
RUST_SENT=()
sent() { # <check> <text> <C++|Rust>: after a successful wl_wd_hash
    TXS+=("$WL_TX")
    BATCH+=("$WL_TX")
    if [ "$3" = Rust ]; then RUST_SENT+=("$WL_TX"); else CPP_SENT+=("$WL_TX"); fi
    pass "$1" "$2: $WL_TX"
}
confirm_batch() { # <check>
    fx_confirm ${BATCH[@]+"${BATCH[@]}"}
    BATCH=()
    both_synced "$1"
}

if wl_wd_hash send-cpp "C++" "$CP" sendTransaction "$(jq -cn --arg d "$S1_XKR" --arg p "$FX_PID1" --arg a "$A" \
    '{transfers: [{address: $d, amount: 1000000}], fee: 10, anonymity: 3, paymentId: $p, changeAddress: $a}')"; then
    sent send-cpp "C++ sendTransaction of 10 XKR to S1's Xkr form with a payment ID" "C++"
fi
confirm_batch send-cpp

if wl_wd_hash send-rust "Rust" "$RP" sendTransaction "$(jq -cn --arg s "$S1" --arg d "$FX_DUMMY_ADDR" --arg a "$A" \
    '{transfers: [{address: $s, amount: 2000000}, {address: $d, amount: 300000}], fee: 10, addresses: [$a], changeAddress: $a}')"; then
    sent send-rust "Rust sendTransaction of 20 XKR to S1 and 3 XKR to a foreign address, source and change A" Rust
fi
confirm_batch send-rust

# delayed: create two, list, send one, delete the other (Rust, then C++ after a confirmation)
delayed() { # <check> <name> <port>
    local id=$1 name=$2 port=$3 d1 d2 list
    local body
    body=$(jq -cn --arg d "$D_XKR" --arg a "$A" '{transfers: [{address: $d, amount: 150000}], fee: 10, addresses: [$a], changeAddress: $a}')
    wl_wd_hash "$id" "$name" "$port" createDelayedTransaction "$body" || return 0
    d1=$WL_TX
    body=$(jq -cn --arg d "$D_XKR" --arg a "$A" '{transfers: [{address: $d, amount: 160000}], fee: 10, addresses: [$a], changeAddress: $a}')
    wl_wd_hash "$id" "$name" "$port" createDelayedTransaction "$body" || return 0
    d2=$WL_TX
    list=$(fx_wd_q getDelayedTransactionHashes "" "$port" | jq -c '.transactionHashes | sort' 2> /dev/null || echo "no list")
    if [ "$list" = "$(jq -cn --arg a "$d1" --arg b "$d2" '[$a, $b] | sort')" ]; then
        pass "$id" "$name: two delayed transactions listed"
    else
        fail "$id" "$name: delayed list $list, expected $d1 and $d2"
    fi
    wl_wd_ok "$id" "$name" "$port" sendDelayedTransaction "$(jq -cn --arg t "$d1" '{transactionHash: $t}')" || return 0
    wl_wd_ok "$id" "$name" "$port" deleteDelayedTransaction "$(jq -cn --arg t "$d2" '{transactionHash: $t}')" || return 0
    list=$(fx_wd_q getDelayedTransactionHashes "" "$port" | jq -c '.transactionHashes' 2> /dev/null || echo "no list")
    if [ "$list" = "[]" ] && wait_for 60 tx_in_pool "$FX_RPC" "$d1"; then
        pass "$id" "$name: sendDelayedTransaction put $d1 in X's pool, deleteDelayedTransaction removed $d2, list empty"
        TXS+=("$d1")
        BATCH+=("$d1")
        if [ "$name" = Rust ]; then RUST_SENT+=("$d1"); else CPP_SENT+=("$d1"); fi
    else
        fail "$id" "$name: after send and delete the list is $list; $d1 in the pool: $(tx_in_pool "$FX_RPC" "$d1" && echo yes || echo no)"
    fi
}
delayed delayed-rust Rust "$RP"
confirm_batch delayed-rust
delayed delayed-cpp "C++" "$CP"
confirm_batch sends

unmined=""
for t in ${TXS[@]+"${TXS[@]}"}; do
    [ -n "$(wl_tx_height "$t")" ] || unmined="$unmined $t"
done
if [ "${#TXS[@]}" = 4 ] && [ -z "$unmined" ]; then
    pass sends "all 4 transactions (2 sends, 2 delayed) mined on X"
else
    fail sends "${#TXS[@]} of 4 transactions made; not mined:${unmined:- none}"
fi
wl_wd_snapshot sends "$CP" "$SNAP/sends-cpp"
wl_wd_snapshot sends "$RP" "$SNAP/sends-rust"
wl_wd_compare sends "$SNAP/sends-cpp" "$SNAP/sends-rust" "C++" "Rust" "balance addresses balances status delayed"
# Each program sent two of the four transactions and only observed the other two. For a
# transaction it observed, walletd lists outputs to addresses outside the wallet with address ""
# and change as an ordinary transfer (type 0); for its own send it lists the full address and
# change as type 2. as_observer rewrites the sender's view of its own sends into that observer
# form, so every transaction is compared in the same form: the Rust observer view must equal the
# C++ sender view rewritten, and the C++ observer view the Rust sender view rewritten.
as_observer() { # <transactions.json> <addresses.json> <out> <sent hash...>
    local txs=$1 own=$2 out=$3
    shift 3
    jq -S -e --slurpfile own "$own" '
        ($ARGS.positional) as $sent | $own[0] as $mine
        | map(.transactions |= map(
            if (.transactionHash as $h | $sent | index($h)) != null then
                .transfers |= (map(
                    (if .type == 2 then .type = 0 else . end)
                    | (if (.address as $a | $mine | index($a)) == null then .address = "" else . end))
                    | sort_by(.address, .type, .amount))
            else . end))' --args "$@" < "$txs" > "$out" 2> "$out.err"
}
if as_observer "$SNAP/sends-cpp.transactions.json" "$SNAP/sends-cpp.addresses.json" \
        "$SNAP/sends-cpp.observer.json" ${CPP_SENT[@]+"${CPP_SENT[@]}"} \
    && as_observer "$SNAP/sends-rust.transactions.json" "$SNAP/sends-rust.addresses.json" \
        "$SNAP/sends-rust.observer.json" ${RUST_SENT[@]+"${RUST_SENT[@]}"}; then
    wl_same sends-transactions "transactions (own sends in observer form; C++ sent ${#CPP_SENT[@]}, Rust ${#RUST_SENT[@]})" \
        "$SNAP/sends-cpp.observer.json" "$SNAP/sends-rust.observer.json" "C++" "Rust"
else
    fail sends-transactions "could not read the transaction lists: $(head -c 200 "$SNAP/sends-cpp.observer.json.err" "$SNAP/sends-rust.observer.json.err" 2> /dev/null | tr '\n' ' ')"
fi

# ------------------------------------------------------------------ export, each opens the other's

wl_wd_ok export "C++" "$CP" export '{"fileName":"cpp-export.wallet"}' || true
wl_wd_ok export "Rust" "$RP" export '{"fileName":"rust-export.wallet"}' || true
if [ -s "$SD/w/cpp-export.wallet" ] && [ -s "$SD/w/rust-export.wallet" ]; then
    pass export "both wrote their export next to the container"
else
    fail export "export files: C++ $(wl_sha "$SD/w/cpp-export.wallet" | cut -c1-12), Rust $(wl_sha "$SD/w/rust-export.wallet" | cut -c1-12)"
fi
wl_wd_stop cpp "$C_WD"
unwatch
watch "$FX_XNAME"
wl_wd_stop rust w61-rwd
if [ -s "$SD/w/cpp-export.wallet" ] && [ -s "$SD/w/rust-export.wallet" ]; then
    fx_wd_start "$C_WD" "$CP" rust-export.wallet "$WL_PASS"
    rwd_start w61-rwd "$RP" cpp-export.wallet "$WL_PASS"
    watch w61-rwd
    both_synced export
    wl_wd_snapshot export "$CP" "$SNAP/export-cpp"
    wl_wd_snapshot export "$RP" "$SNAP/export-rust"
    parts="balance addresses balances transactions"
    wl_wd_compare export-cpp-opens-rust "$SNAP/sends-rust" "$SNAP/export-cpp" "Rust before export" "C++ on the Rust export" "$parts"
    wl_wd_compare export-rust-opens-cpp "$SNAP/sends-cpp" "$SNAP/export-rust" "C++ before export" "Rust on the C++ export" "$parts"
    wl_wd_stop cpp "$C_WD"
    wl_wd_stop rust w61-rwd
fi
finish
