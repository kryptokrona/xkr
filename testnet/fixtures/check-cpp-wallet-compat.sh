#!/usr/bin/env bash
# check-cpp-wallet-compat: wallets made by the current C++ wallet tools open in the Rust wallet
# code with the same keys and addresses, under both address prefixes (SEKR and Xkr).
#
#   testnet/fixtures/check-cpp-wallet-compat.sh <work dir>
#
# Needs target/release/{xkrd,xkr-testwallet}, target/release/examples/wallet_check
# (cargo build --release -p xkr-wallet-files --example wallet_check) and an image with the C++
# master wallet tools (default xkr-cpp-wallets:master, made by
# XKR_CPP_IMAGE=xkr-cpp-wallets:master XKR_CPP_PATCHES=none testnet/build-cpp.sh).
#
# 1. wallet-api creates a wallet (WalletBackend file) on a fresh xkrd chain; the chain pays it;
#    keys, seed, address, integrated addresses and balance come from wallet-api.
# 2. kryptokrona-service generates a container (WalletGreen); keys, seed and balance from walletd;
#    walletd's validateAddress is asked about the Xkr form of every address.
# 3. wallet-api sends to the Xkr form of the walletd address; walletd sees the deposit.
# 4. The Rust reader opens both files: same public keys, the stored address is the SEKR encoding
#    of the keys, the Xkr encoding parses back to the same keys, the C++ seed restores the same
#    keys and address.
# Ports 25xxx and container names xkr-suite-compat-* so it can run next to the fixture capture.
set -euo pipefail

FX_IMAGE=${COMPAT_IMAGE:-xkr-cpp-wallets:master}
WALLETD_IMAGE=$FX_IMAGE
CLI_IMAGE=$FX_IMAGE
. "$(dirname "$0")/lib-wallet.sh"

PORT_FX_P2P=25898; PORT_FX_RPC=25899
PORT_FX_API=25070; PORT_FX_WALLETD=25080
FX_RPC=127.0.0.1:$PORT_FX_RPC
# its own container label, so a suite or fixture run's cleanup leaves these containers alone
SUITE_LABEL=xkr-compat
C_FX_API=xkr-suite-compat-api
C_FX_WD=xkr-suite-compat-walletd
C_FX_ONCE=xkr-suite-compat-once
FX_WALLETD_DAEMON=xkrd
CHECK_BIN=${CHECK_BIN:-$ROOT/target/release/examples/wallet_check}
SYNC_BIN=${SYNC_BIN:-$ROOT/target/release/examples/wallet_sync}
PW_API=compat-api-pass
PW_WD=compat-walletd-pass

# synced: the wallet has scanned as far as the node reports (C++ master counts blocks like xkrd's
# /height; the 2025 build reported one more)
api_synced() {
    local s top
    s=$(fx_api_q GET /status)
    top=$(height_of "$FX_RPC")
    [ -n "$top" ] && [ "$(num "$s" walletBlockCount)" -ge "$top" ] 2> /dev/null \
        && [ "$(num "$s" localDaemonBlockCount)" -ge "$top" ] 2> /dev/null
}
wd_synced() {
    local s top
    s=$(fx_wd_q getStatus)
    top=$(height_of "$FX_RPC")
    [ -n "$top" ] && [ "$(num "$s" blockCount)" -ge "$top" ] 2> /dev/null \
        && [ "$(num "$s" knownBlockCount)" -ge "$top" ] 2> /dev/null
}
wait_wd() {
    wait_for "$FX_SYNC_WAIT" wd_synced || die sync "walletd did not sync: $(fx_wd_q getStatus) top $(height_of "$FX_RPC")"
    sleep 2
}

fx_init "$1" compat-cpp-wallets
fx_require_image "$FX_IMAGE" "XKR_CPP_IMAGE=$FX_IMAGE XKR_CPP_PATCHES=none testnet/build-cpp.sh"
[ -x "$CHECK_BIN" ] || missing_exit check "no $CHECK_BIN" "cargo build --release -p xkr-wallet-files --example wallet_check"

fx_fresh_chain
fx_mine "$FX_DUMMY_ADDR" 5

# ---------------------------------------------------------------- wallet-api
fx_api_start "$C_FX_API" "$PORT_FX_API"
r=$(fx_api_q POST /wallet/create "$(fx_wparams api.wallet "$PW_API")")
API_ADDR=$(fx_api_q GET /addresses/primary | jq -r .address)
[ "${#API_ADDR}" = 99 ] && pass api-create "wallet-api created $API_ADDR" || die api-create "create: $r / $API_ADDR"
API_VIEW=$(fx_api_q GET /keys | jq -r .privateViewKey)
API_KEYS=$(fx_api_q GET "/keys/$API_ADDR")
API_SPEND_PUB=$(jq -r .publicSpendKey <<< "$API_KEYS")
API_SEED=$(fx_api_q GET "/keys/mnemonic/$API_ADDR" | jq -r .mnemonicSeed)
[ "$(wc -w <<< "$API_SEED")" = 25 ] && pass api-seed "wallet-api gave a 25-word seed" || fail api-seed "seed: $API_SEED"

fx_mine "$API_ADDR" 40
fx_mine "$FX_DUMMY_ADDR" 80
if ! wait_for "$FX_SYNC_WAIT" api_synced; then
    note "status: $(fx_api_q GET /status) top $(height_of "$FX_RPC")"
    docker logs --tail 40 "$C_FX_API" 2>&1 | sed 's/^/        api| /'
    die sync "wallet-api did not sync"
fi
API_BAL=$(fx_api_q GET /balance)
note "wallet-api balance $API_BAL"
fx_api_q PUT /save > /dev/null

# ---------------------------------------------------------------- walletd
gen=$(fx_wd_generate "$SD/wd-generate.log" wd.wallet "$PW_WD")
[ -s "$SD/w/wd.wallet" ] && pass wd-generate "kryptokrona-service -g wrote a container" \
    || die wd-generate "exit $gen: $(tail -n 5 "$SD/wd-generate.log" | tr '\n' ' ')"
fx_wd_start "$C_FX_WD" "$PORT_FX_WALLETD" wd.wallet "$PW_WD"
WD_ADDR=$(fx_wd_q getAddresses | jq -r '.addresses[0]')
WD_VIEW=$(fx_wd_q getViewKey | jq -r .viewSecretKey)
WD_SPEND_PUB=$(fx_wd_q getSpendKeys "{\"address\":\"$WD_ADDR\"}" | jq -r .spendPublicKey)
WD_SEED=$(fx_wd_q getMnemonicSeed "{\"address\":\"$WD_ADDR\"}" | jq -r .mnemonicSeed)
note "walletd address $WD_ADDR"
wait_wd

# ---------------------------------------------------------------- Rust reader
rust_check() { # <file> <password> <seed> <out>
    nice -n 19 "$CHECK_BIN" "$SD/w/$1" "$2" --mnemonic "$3" > "$4" 2> "$4.err" || true
}
rust_check api.wallet "$PW_API" "$API_SEED" "$SD/rust-api.json"
API_XKR=$(jq -r '.addresses[0].xkr' "$SD/rust-api.json")

# Xkr forms in the C++ tools: integrated addresses agree, validateAddress accepts, send works
i_sekr=$(fx_api_q GET "/addresses/$API_ADDR/$FX_PID1" | jq -r .integratedAddress)
i_xkr=$(fx_api_q GET "/addresses/$API_XKR/$FX_PID1" | jq -r .integratedAddress)
[ -n "$i_sekr" ] && [ "$i_sekr" != null ] && [ -n "$i_xkr" ] && [ "$i_xkr" != null ] \
    && pass api-xkr-route "wallet-api takes the Xkr form ($API_XKR) in its address route" \
    || fail api-xkr-route "SEKR: $i_sekr Xkr: $i_xkr"
note "integrated addresses for pid 0123…: from SEKR $i_sekr, from Xkr $i_xkr"
WD_XKR=$("$CHECK_BIN" "$SD/w/wd.wallet" "$PW_WD" 2> /dev/null | jq -r '.addresses[0].xkr' || true)
v=$(fx_wd_q validateAddress "{\"address\":\"$WD_XKR\"}" | jq -r .isValid)
[ "$v" = true ] && pass wd-validate-xkr "walletd validateAddress accepts $WD_XKR" || fail wd-validate-xkr "validateAddress($WD_XKR) = $v"
v=$(fx_wd_q validateAddress "{\"address\":\"$API_XKR\"}" | jq -r .isValid)
[ "$v" = true ] && pass wd-validate-xkr2 "walletd validateAddress accepts $API_XKR" || fail wd-validate-xkr2 "validateAddress($API_XKR) = $v"

send=$(fx_api_q POST /transactions/send/basic "{\"destination\":\"$WD_XKR\",\"amount\":12345}")
tx=$(jq -r '.transactionHash // empty' <<< "$send")
if [ -n "$tx" ]; then
    pass api-send-xkr "wallet-api sent 12345 to the Xkr form of the walletd address: $tx"
    fx_confirm "$tx"
    wait_wd
    bal=$(fx_wd_q getBalance | jq -r '(.availableBalance + .lockedAmount)')
    [ "$bal" = 12345 ] && pass wd-received-xkr "walletd received 12345 sent to its Xkr form" || fail wd-received-xkr "walletd balance $bal"
else
    fail api-send-xkr "send: $send"
fi

WD_BAL_FINAL=$(fx_wd_q getBalance | jq -r '(.availableBalance + .lockedAmount)')
API_BAL_FINAL=$(fx_api_q GET /balance | jq -r '(.unlocked + .locked)')
fx_stop_tool "$C_FX_WD"
fx_api_q DELETE /wallet > /dev/null
fx_stop_tool "$C_FX_API"
rust_check api.wallet "$PW_API" "$API_SEED" "$SD/rust-api.json"
rust_check wd.wallet "$PW_WD" "$WD_SEED" "$SD/rust-wd.json"

compare() { # <name> <rust json> <C++ address> <C++ public spend key>
    local j=$2
    [ -s "$j" ] && [ "$(jq -r '.error // empty' "$j")" = "" ] || { fail "$1-open" "$(cat "$j" "$j.err" 2> /dev/null | head -c 300)"; return; }
    pass "$1-open" "Rust opened the file ($(jq -r .kind "$j"))"
    [ "$(jq -r '.addresses[0].spendPublicKey' "$j")" = "$4" ] && pass "$1-spendkey" "same public spend key" || fail "$1-spendkey" "$(jq -c '.addresses[0].spendPublicKey' "$j") vs $4"
    [ "$(jq -r '.addresses[0].sekr' "$j")" = "$3" ] && pass "$1-address" "the keys encode to the C++ address $3" || fail "$1-address" "$(jq -r '.addresses[0].sekr' "$j") vs $3"
    [ "$(jq -r '.addresses[0].xkrParsesToSameKeys' "$j")" = true ] && pass "$1-xkr" "Xkr form $(jq -r '.addresses[0].xkr' "$j") parses to the same keys" || fail "$1-xkr" "$(jq -c '.addresses[0]' "$j")"
    [ "$(jq -r '.seed.address' "$j")" = "$3" ] && [ "$(jq -r '.seed.viewKeyMatchesFile' "$j")" = true ] && [ "$(jq -r '.seed.spendKeyInFile' "$j")" = true ] \
        && pass "$1-seed" "the C++ seed restores the same keys and address in Rust" || fail "$1-seed" "$(jq -c .seed "$j")"
    [ "$(jq -r '.addresses[0].spendKeyMatches' "$j")" = true ] && [ "$(jq -r .viewKeyValid "$j")" = true ] \
        && pass "$1-secrets" "private keys in the file match the public keys" || fail "$1-secrets" "$(jq -c '{v: .viewKeyValid, s: .addresses[0].spendKeyMatches}' "$j")"
}
compare rust-api "$SD/rust-api.json" "$API_ADDR" "$API_SPEND_PUB"
[ "$(jq -r '.addresses[0].stored' "$SD/rust-api.json")" = "$API_ADDR" ] && pass rust-api-stored "stored address equals wallet-api's" || fail rust-api-stored "$(jq -c '.addresses[0].stored' "$SD/rust-api.json")"
note "wallet-api file: $(jq -c '{tx: .transactions, sync: .syncHeight, unspent: .addresses[0].unspent}' "$SD/rust-api.json") C++ balance $API_BAL"
compare rust-wd "$SD/rust-wd.json" "$WD_ADDR" "$WD_SPEND_PUB"
note "container: $(jq -c '{tx: .transactions}' "$SD/rust-wd.json")"
# keep the private view keys out of the logs: only compare
[ -n "$API_VIEW" ] && [ -n "$WD_VIEW" ] && note "view keys read from both tools (not printed)"

# ---------------------------------------------------------------- Rust wallet engine
# 5. The Rust wallet opens copies of both C++ files, syncs from xkrd and reports the same
#    balances; it sends to the Xkr form of the walletd address; the C++ walletd sees the deposit;
#    the C++ wallet-api opens the file the Rust wallet saved.
if [ ! -x "$SYNC_BIN" ]; then
    skip rust-sync "no $SYNC_BIN (cargo build --release -p xkr-wallet --example wallet_sync)"
    exit 0
fi
cp "$SD/w/api.wallet" "$SD/w/rust-api.wallet"
cp "$SD/w/wd.wallet" "$SD/w/rust-wd.wallet"
rust_sync() { # <file> <password> <out> [args...]
    local f=$1 p=$2 o=$3
    shift 3
    nice -n 19 "$SYNC_BIN" "$SD/w/$f" "$p" --daemon "$FX_RPC" --network testnet --timeout-secs 300 "$@" > "$o" 2> "$o.err"
}
total() { jq -r '(.balance.unlocked + .balance.locked)' "$1"; }
if rust_sync rust-api.wallet "$PW_API" "$SD/sync-api.json"; then
    pass rust-sync-api "the Rust wallet synced the wallet-api file ($(jq -r .format "$SD/sync-api.json"), $(jq -r .transactionCount "$SD/sync-api.json") transactions)"
    [ "$(total "$SD/sync-api.json")" = "$API_BAL_FINAL" ] && pass rust-balance-api "same balance as C++ wallet-api: $API_BAL_FINAL" \
        || fail rust-balance-api "Rust $(total "$SD/sync-api.json") vs C++ $API_BAL_FINAL"
    [ "$(jq -r .primaryAddress "$SD/sync-api.json")" = "$API_ADDR" ] && pass rust-primary-api "same primary address" || fail rust-primary-api "$(jq -r .primaryAddress "$SD/sync-api.json")"
else
    fail rust-sync-api "$(tail -n 5 "$SD/sync-api.json.err" | tr '\n' ' ')"
fi
if rust_sync rust-wd.wallet "$PW_WD" "$SD/sync-wd.json"; then
    pass rust-sync-wd "the Rust wallet synced the walletd container ($(jq -r .format "$SD/sync-wd.json"))"
    [ "$(total "$SD/sync-wd.json")" = "$WD_BAL_FINAL" ] && pass rust-balance-wd "same balance as C++ walletd: $WD_BAL_FINAL" \
        || fail rust-balance-wd "Rust $(total "$SD/sync-wd.json") vs C++ $WD_BAL_FINAL"
else
    fail rust-sync-wd "$(tail -n 5 "$SD/sync-wd.json.err" | tr '\n' ' ')"
fi

# Rust sends 5000 to the Xkr form of the walletd address (mined while it waits)
rust_sync rust-api.wallet "$PW_API" "$SD/send-api.json" --send "$WD_XKR" 5000 --save &
send_pid=$!
for _ in $(seq 1 60); do
    [ -n "$(jq -r '.sent.hash // empty' "$SD/send-api.json" 2> /dev/null)" ] && break
    [ "$(fx_wd_q getStatus 2> /dev/null)" ] || true
    pool=$(curl -s "http://$FX_RPC/getinfo" | jq -r '.tx_pool_size // 0')
    [ "${pool:-0}" -gt 0 ] && break
    sleep 2
done
fx_mine_count "$FX_DUMMY_ADDR" 12
wait "$send_pid" || true
RHASH=$(jq -r '.sent.hash // empty' "$SD/send-api.json")
[ -n "$RHASH" ] && [ "$(jq -r .sent.confirmed "$SD/send-api.json")" = true ] \
    && pass rust-send-xkr "the Rust wallet sent 5000 to $WD_XKR and saw it confirmed: $RHASH" \
    || fail rust-send-xkr "$(cat "$SD/send-api.json" "$SD/send-api.json.err" 2> /dev/null | tail -c 400)"
fx_mine_count "$FX_DUMMY_ADDR" 2

# the C++ walletd (original container) sees the Rust deposit
fx_wd_start "$C_FX_WD" "$PORT_FX_WALLETD" wd.wallet "$PW_WD"
wait_wd
b=$(fx_wd_q getBalance | jq -r '(.availableBalance + .lockedAmount)')
[ "$b" = $((WD_BAL_FINAL + 5000)) ] && pass cpp-sees-rust-send "C++ walletd balance $b includes the 5000 the Rust wallet sent" || fail cpp-sees-rust-send "walletd balance $b, expected $((WD_BAL_FINAL + 5000))"
fx_stop_tool "$C_FX_WD"

# the Rust wallet (its walletd container copy) sees it too
if rust_sync rust-wd.wallet "$PW_WD" "$SD/sync-wd2.json" --save; then
    [ "$(total "$SD/sync-wd2.json")" = $((WD_BAL_FINAL + 5000)) ] && pass rust-sees-rust-send "Rust walletd copy balance $(total "$SD/sync-wd2.json")" || fail rust-sees-rust-send "$(total "$SD/sync-wd2.json")"
else
    fail rust-sees-rust-send "$(tail -n 5 "$SD/sync-wd2.json.err" | tr '\n' ' ')"
fi

# the C++ wallet-api opens the file the Rust wallet saved, with the same balance
fx_api_start "$C_FX_API" "$PORT_FX_API"
r=$(fx_api_q POST /wallet/open "$(fx_wparams rust-api.wallet "$PW_API")")
if [ "$(fx_api_q GET /addresses/primary | jq -r .address)" = "$API_ADDR" ]; then
    wait_for "$FX_SYNC_WAIT" api_synced || true
    after=$(jq -r '(.balance.unlocked + .balance.locked)' "$SD/send-api.json")
    cb=$(fx_api_q GET /balance | jq -r '(.unlocked + .locked)')
    pass cpp-opens-rust-file "C++ wallet-api opened the file the Rust wallet saved"
    [ "$cb" -le "$API_BAL_FINAL" ] 2> /dev/null && [ "$cb" = $((API_BAL_FINAL - 5000 - $(jq -r '.transactions[] | select(.hash == "'"$RHASH"'") | .fee' "$SD/send-api.json" 2> /dev/null || echo 10))) ] \
        && pass cpp-balance-rust-file "C++ balance $cb after the Rust send" || fail cpp-balance-rust-file "C++ $cb, before $API_BAL_FINAL, Rust report $after"
    fx_api_q DELETE /wallet > /dev/null
else
    fail cpp-opens-rust-file "open: $r"
fi
fx_stop_tool "$C_FX_API"
