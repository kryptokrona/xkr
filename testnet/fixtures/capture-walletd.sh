#!/usr/bin/env bash
# capture-walletd: fixtures of the C++ kryptokrona-service (walletd JSON-RPC), plus a comparison of
# xkrd's /queryblockslite and /get_blocks_hashes_by_timestamps with a C++ node on the same chain.
#
#   xkrd X       fixture chain, P2P 24898 / RPC 24899 (own genesis, `xkr-testwallet mine` only)
#   C++ node     follows X (24888 / 24889; image: FX_FOLLOWER_IMAGE, auto = the wallet image if its
#                kryptokronad follows X, else CPP_OLD_IMAGE); the reference for the route comparison
#   walletd      WALLETD_IMAGE, port 24080, --rpc-password fixture-rpc-pass, daemon = xkrd X
#                (FX_WALLETD_DAEMON=cpp: the C++ node); 24081 = the same wallet with
#                --rpc-legacy-security --enable-cors https://fixture.example
#
# Steps: `-g` container generation (seed, keys, random, and its errors) and `--address`; transport
# errors (path, parse error, invalid request, unknown method, password); every method's success
# shape and required-field errors (payment_service_json_rpc_messages.cpp); sends (extra, payment
# id, unlock time, fee, anonymity, change address with several addresses); delayed transactions;
# getTransactions / getTransactionHashes by block hash, by index, by payment id, with the
# empty-items case; fusion; addresses (create, list, tracking, delete); save, export, reset;
# legacy security and CORS; a tracking-only container; the two node routes on xkrd and on C++.
#
# Output: crates/xkr-walletd/tests/fixtures/cpp/<group>.jsonl, application-codes.json,
#         node-routes-compare.json, generate/*.txt, _meta.json
# Env: WALLETD_IMAGE, FX_WALLETD_DAEMON (xkrd|cpp), FX_FOLLOWER_IMAGE, lib-wallet.sh knobs.
# Never builds anything.
# suite: expected=480 timeout=1800
set -euo pipefail
: "${TIMEOUT:=300}"
. "$(dirname "$0")/lib-wallet.sh"
fx_init "${1:-}" fx-walletd

OUT=$FX_OUT_ROOT/crates/xkr-walletd/tests/fixtures/cpp
fx_require_image "$WALLETD_IMAGE"
jget() { jq -r "$1" <<< "$FX_BODY" 2> /dev/null || true; }
wtx() { jget '.result.transactionHash // empty'; }

fx_image_report "$WALLETD_IMAGE" "$SD/image.json"
[ "$(jq -r '.files | index("kryptokrona-service") != null' "$SD/image.json")" = true ] \
    || missing_exit image "$WALLETD_IMAGE has no kryptokrona-service" "set WALLETD_IMAGE"
fx_choose_follower
fx_fresh_chain
fx_start_follower "$FX_FOLLOWER"
note "walletd's daemon: $FX_WALLETD_DAEMON (127.0.0.1:$(fx_walletd_daemon_port))"

# ------------------------------------------------------------------ container generation

step "generate containers"
mkdir -p "$SD/gen"
declare -A GEN_RC=()
gen() { # <case> <file> <password> [args...]
    local c=$1
    shift
    GEN_RC[$c]=$(fx_wd_generate "$SD/gen/$c.txt" "$@")
}
gen seed-a walletd-main.wallet walletd-pass --mnemonic-seed "$FX_SEED_A"
gen keys-k-empty-password walletd-k.wallet "" --spend-key "$FX_K_SPEND" --view-key "$FX_K_VIEW"
gen random-unicode-password walletd-random.wallet "pässwörd ✓"
gen view-key-only walletd-bad1.wallet x --view-key "$FX_K_VIEW"
gen bad-spend-key walletd-bad2.wallet x --spend-key zz --view-key "$FX_K_VIEW"
gen bad-seed-checksum walletd-bad3.wallet x --mnemonic-seed "$FX_SEED_BAD_CHECKSUM"
gen file-exists walletd-main.wallet walletd-pass --mnemonic-seed "$FX_SEED_A"
[ -s "$SD/w/walletd-main.wallet" ] || die generate "kryptokrona-service -g did not write walletd-main.wallet ($(tail -n 5 "$SD/gen/seed-a.txt" | tr '\n' ' '))"
GEN_RC[address-k]=$(fx_tool_fg "$SD/gen/address-k.txt" "$WALLETD_IMAGE" 120 kryptokrona-service --address \
    -w /w/walletd-k.wallet -p "" --rpc-password "$FX_WD_RPC_PASS" --daemon-address 127.0.0.1 \
    --daemon-port "$(fx_walletd_daemon_port)" -l /w/walletd-k.address.log)
for c in "${!GEN_RC[@]}"; do printf '%s\t%s\n' "$c" "${GEN_RC[$c]}"; done | sort \
    | jq -Rn '[inputs | split("\t") | {key: .[0], value: (.[1] | tonumber? // .)}] | from_entries' > "$SD/gen/exit-codes.json"
note "generate exit codes: $(jq -c . "$SD/gen/exit-codes.json")"
K=$({ grep -aoE 'SEKR[1-9A-HJ-NP-Za-km-z]{95}' "$SD/gen/address-k.txt" "$SD/gen/keys-k-empty-password.txt" || true; } | head -n 1 | cut -d: -f2)
[ -n "$K" ] && fx_keep "$K"

# ------------------------------------------------------------------ main walletd, funding

fx_wd_start "$C_FX_WD" "$PORT_FX_WALLETD" walletd-main.wallet walletd-pass
A=$(fx_wd_q getAddresses | jq -r '.addresses[0] // empty')
[ -n "$A" ] || die setup "walletd has no address"
XKR_A=$(fx_alt_address "$A")
fx_keep "$A" "Xkr${A:4}" "$XKR_A"
note "A = $A, K = ${K:-?}"
fx_mine "$A" "$FX_FUND_TO"
fx_mine "$FX_DUMMY_ADDR" "$FX_FILLER_TO"
fx_wd_wait_synced
pass chain "X at $(top_of "$FX_RPC"): blocks 1..$FX_FUND_TO pay A, filler to $FX_FILLER_TO; walletd synced"
FOREIGN=${K:-$FX_DUMMY_ADDR}

# ------------------------------------------------------------------ transport

fx_group transport
fx_wd_raw "post-other-path" '{"jsonrpc":"2.0","id":1,"method":"getStatus","params":{}}' "$PORT_FX_WALLETD" /other
fx_wd_raw "get-json-rpc" - "$PORT_FX_WALLETD" /json_rpc GET
fx_wd_raw "parse-error" '{"jsonrpc":"2.0", "id": 1, "method": '
fx_wd_raw "no-method" '{"jsonrpc":"2.0","id":1,"password":"'"$FX_WD_RPC_PASS"'","params":{}}'
fx_wd_raw "method-not-string" '{"jsonrpc":"2.0","id":1,"password":"'"$FX_WD_RPC_PASS"'","method":5,"params":{}}'
fx_wd_raw "id-string" '{"jsonrpc":"2.0","id":"abc","password":"'"$FX_WD_RPC_PASS"'","method":"getStatus","params":{}}'
fx_wd_raw "no-params" '{"jsonrpc":"2.0","id":1,"password":"'"$FX_WD_RPC_PASS"'","method":"getStatus"}'
fx_wd_raw "params-array" '{"jsonrpc":"2.0","id":1,"password":"'"$FX_WD_RPC_PASS"'","method":"getStatus","params":[]}'
fx_wd "unknown-method" noSuchMethod
fx_wd "missing-password" getStatus '{}' "$PORT_FX_WALLETD" -none
fx_wd "wrong-password" getStatus '{}' "$PORT_FX_WALLETD" wrong
fx_record "options" OPTIONS "127.0.0.1:$PORT_FX_WALLETD" /json_rpc - "Origin: $FX_CORS"

# ------------------------------------------------------------------ read-only methods

fx_group methods
fx_wd "getStatus/synced" getStatus
fx_wd "getAddresses" getAddresses
fx_wd "getViewKey" getViewKey
fx_wd "getMnemonicSeed/a" getMnemonicSeed "$(jq -cn --arg a "$A" '{address: $a}')"
fx_wd "getMnemonicSeed/missing-address" getMnemonicSeed
fx_wd "getMnemonicSeed/not-in-wallet" getMnemonicSeed "$(jq -cn --arg a "$FOREIGN" '{address: $a}')"
fx_wd "getSpendKeys/a" getSpendKeys "$(jq -cn --arg a "$A" '{address: $a}')"
fx_keep "$(jget '.result.spendPublicKey // empty')"
fx_wd "getSpendKeys/missing-address" getSpendKeys
fx_wd "getSpendKeys/not-in-wallet" getSpendKeys "$(jq -cn --arg a "$FOREIGN" '{address: $a}')"
fx_wd "getSpendKeys/bad-address" getSpendKeys '{"address":"SEKRnope"}'
fx_wd "getBalance/all" getBalance
fx_wd "getBalance/a" getBalance "$(jq -cn --arg a "$A" '{address: $a}')"
fx_wd "getBalance/bad-address" getBalance '{"address":"SEKRnope"}'
fx_wd "getBalance/not-in-wallet" getBalance "$(jq -cn --arg a "$FOREIGN" '{address: $a}')"
fx_wd "getFeeInfo" getFeeInfo
fx_wd "getNodeFeeInfo" getNodeFeeInfo
fx_wd "validateAddress/a" validateAddress "$(jq -cn --arg a "$A" '{address: $a}')"
fx_wd "validateAddress/foreign" validateAddress "$(jq -cn --arg a "$FX_DUMMY_ADDR" '{address: $a}')"
fx_wd "validateAddress/garbage" validateAddress '{"address":"SEKRnope"}'
fx_wd "validateAddress/xkr-prefix-a" validateAddress "$(jq -cn --arg a "$XKR_A" '{address: $a}')"
fx_wd "getBalance/xkr-prefix-a" getBalance "$(jq -cn --arg a "$XKR_A" '{address: $a}')"
fx_wd "validateAddress/xkr-prefix-syntactic" validateAddress "$(jq -cn --arg a "Xkr${A:4}" '{address: $a}')"
fx_wd "validateAddress/missing" validateAddress
fx_wd "createIntegratedAddress/a-pid1" createIntegratedAddress "$(jq -cn --arg a "$A" --arg p "$FX_PID1" '{address: $a, paymentId: $p}')"
INT_A=$(jget '.result.integratedAddress // empty')
fx_keep "$INT_A"
fx_wd "validateAddress/integrated" validateAddress "$(jq -cn --arg a "$INT_A" '{address: $a}')"
fx_wd "createIntegratedAddress/foreign-pid2" createIntegratedAddress "$(jq -cn --arg a "$FX_DUMMY_ADDR" --arg p "$FX_PID2" '{address: $a, paymentId: $p}')"
INT_D=$(jget '.result.integratedAddress // empty')
fx_keep "$INT_D"
fx_wd "createIntegratedAddress/missing-payment-id" createIntegratedAddress "$(jq -cn --arg a "$A" '{address: $a}')"
fx_wd "createIntegratedAddress/missing-address" createIntegratedAddress "$(jq -cn --arg p "$FX_PID1" '{paymentId: $p}')"
fx_wd "createIntegratedAddress/bad-payment-id" createIntegratedAddress "$(jq -cn --arg a "$A" --arg p "${FX_PID1:1}" '{address: $a, paymentId: $p}')"
fx_wd "createIntegratedAddress/xkr-prefix-a" createIntegratedAddress "$(jq -cn --arg a "$XKR_A" --arg p "$FX_PID1" '{address: $a, paymentId: $p}')"
fx_wd "createIntegratedAddress/xkr-prefix-syntactic" createIntegratedAddress "$(jq -cn --arg a "Xkr${A:4}" --arg p "$FX_PID1" '{address: $a, paymentId: $p}')"
fx_wd "getBlockHashes/0-5" getBlockHashes '{"firstBlockIndex":0,"blockCount":5}'
fx_wd "getBlockHashes/missing-block-count" getBlockHashes '{"firstBlockIndex":0}'
fx_wd "getBlockHashes/missing-first-index" getBlockHashes '{"blockCount":5}'
fx_wd "getBlockHashes/beyond-top" getBlockHashes '{"firstBlockIndex":1000000,"blockCount":1}'

# ------------------------------------------------------------------ sends (one address: change to A)

fx_group send
TXS=()
send() { # <name> <params>
    fx_wd "$1" sendTransaction "$2"
    LAST_TX=$(wtx)
    [ -z "$LAST_TX" ] || TXS+=("$LAST_TX")
}
tr1() { jq -cn --arg d "$FOREIGN" --argjson n "$1" '{transfers: [{address: $d, amount: $n}], fee: 10}'; }
send "basic" "$(tr1 100000 | jq -c '. + {anonymity: 3}')"
TX1=$LAST_TX
send "payment-id" "$(tr1 200000 | jq -c --arg p "$FX_PID1" '. + {paymentId: $p}')"
TXP=$LAST_TX
send "extra" "$(tr1 300000 | jq -c '. + {extra: "020568656c6c6f"}')"
send "integrated-destination" "$(jq -cn --arg d "$INT_D" '{transfers: [{address: $d, amount: 310000}], fee: 10}')"
send "extra-and-payment-id" "$(tr1 300000 | jq -c --arg p "$FX_PID1" '. + {extra: "020568656c6c6f", paymentId: $p}')"
send "missing-fee" "$(jq -cn --arg d "$FOREIGN" '{transfers: [{address: $d, amount: 1000}]}')"
send "missing-transfers" '{"fee":10}'
send "transfer-missing-amount" "$(jq -cn --arg d "$FOREIGN" '{transfers: [{address: $d}], fee: 10}')"
send "transfer-missing-address" '{"transfers":[{"amount":1000}],"fee":10}'
send "fee-1" "$(tr1 1000 | jq -c '.fee = 1')"
send "amount-0" "$(tr1 0)"
send "insufficient-funds" "$(tr1 9000000000000000)"
send "anonymity-0" "$(tr1 400000 | jq -c '. + {anonymity: 0}')"
send "anonymity-500" "$(tr1 400000 | jq -c '. + {anonymity: 500}')"
UNLOCK_AT=$(($(height_of "$FX_RPC") + 40))
send "unlock-time-height" "$(tr1 500000 | jq -c --argjson u "$UNLOCK_AT" '. + {unlockTime: $u}')"
send "bad-address" '{"transfers":[{"address":"SEKRnope","amount":1000}],"fee":10}'
send "xkr-prefix-a" "$(jq -cn --arg d "$XKR_A" '{transfers: [{address: $d, amount: 1000}], fee: 10}')"
send "xkr-prefix-syntactic" "$(jq -cn --arg d "Xkr${A:4}" '{transfers: [{address: $d, amount: 1000}], fee: 10}')"
send "bad-payment-id" "$(tr1 1000 | jq -c --arg p "${FX_PID1:1}" '. + {paymentId: $p}')"
send "source-addresses-a" "$(tr1 600000 | jq -c --arg a "$A" '. + {addresses: [$a]}')"
send "source-not-in-wallet" "$(tr1 600000 | jq -c --arg a "$FX_DUMMY_ADDR" '. + {addresses: [$a]}')"
send "change-not-in-wallet" "$(tr1 600000 | jq -c --arg a "$FX_DUMMY_ADDR" '. + {changeAddress: $a}')"
fx_wd "getUnconfirmedTransactionHashes/all~shape" getUnconfirmedTransactionHashes
fx_wd "getUnconfirmedTransactionHashes/a~shape" getUnconfirmedTransactionHashes "$(jq -cn --arg a "$A" '{addresses: [$a]}')"
fx_wd "getTransaction/pending" getTransaction "$(jq -cn --arg t "$TX1" '{transactionHash: $t}')"
fx_wd "getTransaction/missing-hash" getTransaction
fx_wd "getTransaction/unknown" getTransaction "$(jq -cn --arg t "$FX_UNKNOWN_HASH" '{transactionHash: $t}')"
fx_wd "getBalance/after-sends~shape" getBalance
[ -n "$TX1" ] || fail send "sendTransaction basic failed: $(head -c 300 <<< "$FX_BODY")"

fx_group delayed
fx_wd "create" createDelayedTransaction "$(tr1 150000)"
D1=$(wtx)
fx_wd "list-one" getDelayedTransactionHashes
fx_wd "send" sendDelayedTransaction "$(jq -cn --arg t "$D1" '{transactionHash: $t}')"
[ -z "$D1" ] || TXS+=("$D1")
fx_wd "create-second" createDelayedTransaction "$(tr1 160000)"
D2=$(wtx)
fx_wd "delete" deleteDelayedTransaction "$(jq -cn --arg t "$D2" '{transactionHash: $t}')"
fx_wd "delete-unknown" deleteDelayedTransaction "$(jq -cn --arg t "$FX_UNKNOWN_HASH" '{transactionHash: $t}')"
fx_wd "delete-missing-hash" deleteDelayedTransaction
fx_wd "send-unknown" sendDelayedTransaction "$(jq -cn --arg t "$FX_UNKNOWN_HASH" '{transactionHash: $t}')"
fx_wd "send-missing-hash" sendDelayedTransaction
fx_wd "create-extra-and-payment-id" createDelayedTransaction "$(tr1 1000 | jq -c --arg p "$FX_PID1" '. + {extra: "0201ff", paymentId: $p}')"
fx_wd "create-missing-fee" createDelayedTransaction "$(jq -cn --arg d "$FOREIGN" '{transfers: [{address: $d, amount: 1000}]}')"
fx_wd "list-empty" getDelayedTransactionHashes

fx_confirm "${TXS[@]}"
fx_wd_wait_synced
TXH=$(num "$(tx_json "$FX_RPC" "$TX1")" height)
TXH=${TXH:-$((FX_FILLER_TO + 1))}
TXPH=$(num "$(tx_json "$FX_RPC" "${TXP:-$TX1}")" height)
TXPH=${TXPH:-$TXH}
TOP=$(height_of "$FX_RPC")
BH=$(header_hash_at "$FX_RPC" "$TXH")
note "first send mined at $TXH (block $BH), X at $TOP"

# ------------------------------------------------------------------ transactions

fx_group transactions
q() { jq -cn "$@"; }
fx_wd "getTransactions/by-index-whole-chain" getTransactions "$(q --argjson n $((TOP + 1)) '{firstBlockIndex: 1, blockCount: $n}')"
fx_wd "getTransactions/first-index-0" getTransactions "$(q --argjson n $((TOP + 1)) '{firstBlockIndex: 0, blockCount: $n}')"
fx_wd "getTransactions/block-count-0" getTransactions '{"firstBlockIndex":1,"blockCount":0}'
fx_wd "getTransactions/beyond-top" getTransactions "$(q --argjson h $((TOP + 5)) '{firstBlockIndex: $h, blockCount: 3}')"
fx_wd "getTransactions/by-index-tx-block" getTransactions "$(q --argjson h "$TXH" '{firstBlockIndex: $h, blockCount: 1}')"
fx_wd "getTransactions/by-block-hash" getTransactions "$(q --arg b "$BH" '{blockHash: $b, blockCount: 3}')"
fx_wd "getTransactions/payment-id" getTransactions "$(q --argjson h "$TXPH" --arg p "$FX_PID1" '{firstBlockIndex: $h, blockCount: 2, paymentId: $p}')"
fx_wd "getTransactions/addresses-a" getTransactions "$(q --argjson h "$TXH" --arg a "$A" '{firstBlockIndex: $h, blockCount: 2, addresses: [$a]}')"
fx_wd "getTransactions/both-hash-and-index" getTransactions "$(q --arg b "$BH" '{blockHash: $b, firstBlockIndex: 0, blockCount: 1}')"
fx_wd "getTransactions/neither-hash-nor-index" getTransactions '{"blockCount":1}'
fx_wd "getTransactions/missing-block-count" getTransactions '{"firstBlockIndex":0}'
fx_wd "getTransactions/unknown-block-hash" getTransactions "$(q --arg b "$FX_UNKNOWN_HASH" '{blockHash: $b, blockCount: 1}')"
fx_wd "getTransactions/bad-payment-id" getTransactions "$(q '{firstBlockIndex: 0, blockCount: 1, paymentId: "zz"}')"
fx_wd "getTransactionHashes/by-index-whole-chain" getTransactionHashes "$(q --argjson n $((TOP + 1)) '{firstBlockIndex: 1, blockCount: $n}')"
fx_wd "getTransactionHashes/first-index-0" getTransactionHashes "$(q --argjson n $((TOP + 1)) '{firstBlockIndex: 0, blockCount: $n}')"
fx_wd "getTransactionHashes/by-block-hash" getTransactionHashes "$(q --arg b "$BH" '{blockHash: $b, blockCount: 3}')"
fx_wd "getTransactionHashes/payment-id" getTransactionHashes "$(q --argjson h "$TXPH" --arg p "$FX_PID1" '{firstBlockIndex: $h, blockCount: 2, paymentId: $p}')"
fx_wd "getTransactionHashes/both-hash-and-index" getTransactionHashes "$(q --arg b "$BH" '{blockHash: $b, firstBlockIndex: 0, blockCount: 1}')"
fx_wd "getTransactionHashes/missing-block-count" getTransactionHashes '{"firstBlockIndex":0}'
fx_wd "getTransaction/confirmed" getTransaction "$(q --arg t "$TX1" '{transactionHash: $t}')"
fx_wd "getUnconfirmedTransactionHashes/after-mining" getUnconfirmedTransactionHashes
fx_wd "getBalance/after-mining" getBalance
fx_wd "getStatus/after-mining" getStatus

fx_group fusion
fx_wd "estimateFusion~shape" estimateFusion '{"threshold":100000000}'
fx_wd "estimateFusion/missing-threshold" estimateFusion
fx_wd "estimateFusion/addresses-a~shape" estimateFusion "$(q --arg a "$A" '{threshold: 100000000, addresses: [$a]}')"
fx_wd "sendFusionTransaction" sendFusionTransaction '{"threshold":100000000,"anonymity":3}'
TXF=$(wtx)
fx_wd "sendFusionTransaction/missing-threshold" sendFusionTransaction
fx_wd "sendFusionTransaction/threshold-1" sendFusionTransaction '{"threshold":1}'
fx_wd "sendFusionTransaction/foreign-destination" sendFusionTransaction "$(q --arg d "$FX_DUMMY_ADDR" '{threshold: 100000000, destinationAddress: $d}')"

# ------------------------------------------------------------------ several addresses

fx_group addresses
fx_wd "createAddress/random" createAddress
R1=$(jget '.result.address // empty')
fx_wd "createAddress/spend-secret-sub1" createAddress "$(q --arg k "$FX_SUB1" '{spendSecretKey: $k}')"
SUB1=$(jget '.result.address // empty')
fx_keep "$SUB1"
fx_wd "createAddress/spend-public-in-spend-wallet" createAddress "$(q --arg k "$FX_K_SPEND_PUB" '{spendPublicKey: $k}')"
fx_wd "createAddress/both-keys" createAddress "$(q --arg s "$FX_SUB2" --arg p "$FX_K_SPEND_PUB" '{spendSecretKey: $s, spendPublicKey: $p}')"
fx_wd "createAddress/new-address-and-scan-height" createAddress '{"newAddress":true,"scanHeight":5}'
fx_wd "createAddress/bad-key" createAddress '{"spendSecretKey":"zz"}'
fx_wd "createAddress/sub1-again" createAddress "$(q --arg k "$FX_SUB1" '{spendSecretKey: $k}')"
fx_wd "createAddressList/sub2-sub3" createAddressList "$(q --arg a "$FX_SUB2" --arg b "$FX_SUB3" '{spendSecretKeys: [$a, $b], scanHeight: 0}')"
fx_keep $(jget '.result.addresses[]? // empty')
fx_wd "createAddressList/missing-keys" createAddressList
fx_wd "createAddressList/new-address-and-scan-height" createAddressList "$(q --arg a "$FX_SUB2" '{spendSecretKeys: [$a], newAddress: true, scanHeight: 5}')"
fx_wd "getAddresses/several" getAddresses
fx_wd "getBalance/sub1" getBalance "$(q --arg a "$SUB1" '{address: $a}')"
fx_wd "getSpendKeys/sub1" getSpendKeys "$(q --arg a "$SUB1" '{address: $a}')"
fx_keep "$(jget '.result.spendPublicKey // empty')"
fx_wd "getMnemonicSeed/sub1-not-deterministic" getMnemonicSeed "$(q --arg a "$SUB1" '{address: $a}')"
fx_wd "sendTransaction/several-addresses-no-change-address" sendTransaction "$(tr1 100000)"
fx_wd "sendTransaction/several-addresses-change-a" sendTransaction "$(tr1 110000 | jq -c --arg a "$A" '. + {changeAddress: $a}')"
TXC1=$(wtx)
fx_wd "sendTransaction/source-a-change-a" sendTransaction "$(tr1 120000 | jq -c --arg a "$A" '. + {addresses: [$a], changeAddress: $a}')"
TXC2=$(wtx)
fx_wd "sendTransaction/to-own-sub1" sendTransaction "$(jq -cn --arg s "$SUB1" --arg a "$A" '{transfers: [{address: $s, amount: 130000}], fee: 10, changeAddress: $a}')"
TXC3=$(wtx)
fx_wd "deleteAddress/random" deleteAddress "$(q --arg a "$R1" '{address: $a}')"
fx_wd "deleteAddress/missing" deleteAddress
fx_wd "deleteAddress/not-in-wallet" deleteAddress "$(q --arg a "$FX_DUMMY_ADDR" '{address: $a}')"
fx_wd "getAddresses/after-delete" getAddresses

fx_confirm "${TXF:-}" "${TXC1:-}" "${TXC2:-}" "${TXC3:-}"
fx_wd_wait_synced
TOP=$(height_of "$FX_RPC")
CH=$(num "$(tx_json "$FX_RPC" "${TXC1:-$TX1}")" height)
CH=${CH:-$TXH}
fx_wd "getTransactions/empty-items-quirk" getTransactions "$(q --argjson h "$CH" --arg s "$SUB1" '{firstBlockIndex: $h, blockCount: 1, addresses: [$s]}')"
fx_wd "getTransactionHashes/empty-items-quirk" getTransactionHashes "$(q --argjson h "$CH" --arg s "$SUB1" '{firstBlockIndex: $h, blockCount: 1, addresses: [$s]}')"
fx_wd "getTransactions/sub1-whole-chain" getTransactions "$(q --argjson n $((TOP + 1)) --arg s "$SUB1" '{firstBlockIndex: 1, blockCount: $n, addresses: [$s]}')"
fx_wd "getBalance/sub1-after" getBalance "$(q --arg a "$SUB1" '{address: $a}')"
fx_wd "getBalance/after-several" getBalance

fx_group save-export-reset
fx_wd "save" save
fx_wd "export" export '{"fileName":"walletd-export.wallet"}'
fx_wd "export/missing-file-name" export
fx_wd "export/existing-file" export '{"fileName":"walletd-export.wallet"}'
fx_wd "reset" reset
fx_wd "getStatus/after-reset~shape" getStatus
fx_wd_wait_synced
fx_wd "getBalance/after-reset" getBalance
fx_wd "reset/scan-height" reset "$(q --argjson h "$FX_FUND_TO" '{scanHeight: $h}')"
fx_wd_wait_synced
fx_wd "getBalance/after-reset-scan-height" getBalance
fx_wd "getStatus/final" getStatus
fx_stop_tool "$C_FX_WD"

# ------------------------------------------------------------------ legacy security + CORS

fx_group legacy-cors
cp "$SD/w/walletd-main.wallet" "$SD/w/walletd-alt.wallet"
fx_wd_start "$C_FX_WD_ALT" "$PORT_FX_WALLETD_ALT" walletd-alt.wallet walletd-pass --rpc-legacy-security --enable-cors "$FX_CORS"
fx_wd "legacy/no-password" getStatus '{}' "$PORT_FX_WALLETD_ALT" -none
fx_wd "legacy/wrong-password" getStatus '{}' "$PORT_FX_WALLETD_ALT" wrong
fx_wd "legacy/right-password" getAddresses '{}' "$PORT_FX_WALLETD_ALT"
fx_record "cors/post" POST "127.0.0.1:$PORT_FX_WALLETD_ALT" /json_rpc '{"jsonrpc":"2.0","id":1,"method":"getStatus","params":{}}' \
    "Origin: $FX_CORS" "Content-Type: application/json"
fx_record "cors/options" OPTIONS "127.0.0.1:$PORT_FX_WALLETD_ALT" /json_rpc - "Origin: $FX_CORS" "Access-Control-Request-Method: POST"
fx_stop_tool "$C_FX_WD_ALT"

# ------------------------------------------------------------------ tracking-only container

fx_group tracking
fx_wd_start "$C_FX_WD" "$PORT_FX_WALLETD" walletd-random.wallet "pässwörd ✓"
R=$(fx_wd_q getAddresses | jq -r '.addresses[0] // empty')
fx_wd "getAddresses/random" getAddresses
fx_wd "createAddress/tracking-in-spend-wallet" createAddress "$(q --arg k "$FX_K_SPEND_PUB" '{spendPublicKey: $k}')"
fx_wd "deleteAddress/only-address" deleteAddress "$(q --arg a "$R" '{address: $a}')"
fx_wd "getAddresses/after-delete" getAddresses
fx_wd "createAddress/tracking" createAddress "$(q --arg k "$FX_K_SPEND_PUB" '{spendPublicKey: $k, scanHeight: 0}')"
T=$(jget '.result.address // empty')
fx_wd "createAddress/spend-secret-in-tracking-wallet" createAddress "$(q --arg k "$FX_SUB1" '{spendSecretKey: $k}')"
fx_wd "getSpendKeys/tracking" getSpendKeys "$(q --arg a "${T:-$R}" '{address: $a}')"
fx_wd "getMnemonicSeed/tracking" getMnemonicSeed "$(q --arg a "${T:-$R}" '{address: $a}')"
fx_wd_wait_synced
fx_wd "getBalance/tracking" getBalance
fx_wd "sendTransaction/from-tracking" sendTransaction "$(tr1 1000)"
fx_wd "estimateFusion/tracking" estimateFusion '{"threshold":100000000}'
fx_stop_tool "$C_FX_WD"

# ------------------------------------------------------------------ node routes: xkrd vs C++

fx_group node-routes
fx_wait_follower
G=$(header_hash_at "$FX_RPC" 0)
TOPH=$(top_hash_of "$FX_RPC")
MID=$(header_hash_at "$FX_RPC" $((TXH - 1)))
TS1=$(num "$(rpc "$FX_RPC" getblockheaderbyheight '{"height":1}')" timestamp)
TS50=$(num "$(rpc "$FX_RPC" getblockheaderbyheight '{"height":50}')" timestamp)
TSTOP=$(num "$(rpc "$FX_RPC" getlastblockheader)" timestamp)
# Ranges stay short: C++ DatabaseBlockchainCache::getBlockHashesByTimestamps
# (src/cryptonote_core/database_blockchain_cache.cpp) reads one database key per second of the
# range, so a range of years keeps the C++ node busy far beyond any timeout.
CASES=(
    "queryblockslite|genesis|$(q --arg g "$G" '{blockIds: [$g], timestamp: 0}')"
    "queryblockslite|top-and-genesis|$(q --arg t "$TOPH" --arg g "$G" '{blockIds: [$t, $g], timestamp: 0}')"
    "queryblockslite|before-first-send|$(q --arg m "$MID" --arg g "$G" '{blockIds: [$m, $g], timestamp: 0}')"
    "queryblockslite|timestamp-block-50|$(q --arg g "$G" --argjson ts "${TS50:-0}" '{blockIds: [$g], timestamp: $ts}')"
    "queryblockslite|unknown-only|$(q --arg u "$FX_UNKNOWN_HASH" '{blockIds: [$u], timestamp: 0}')"
    "queryblockslite|empty-ids|{\"blockIds\":[],\"timestamp\":0}"
    "get_blocks_hashes_by_timestamps|from-block-1-hour|$(q --argjson ts "${TS1:-0}" '{timestampBegin: $ts, secondsCount: 3600}')"
    "get_blocks_hashes_by_timestamps|from-0-one-second|{\"timestampBegin\":0,\"secondsCount\":1}"
    "get_blocks_hashes_by_timestamps|from-0-ten-seconds|{\"timestampBegin\":0,\"secondsCount\":10}"
    "get_blocks_hashes_by_timestamps|zero-seconds|$(q --argjson ts "${TS1:-0}" '{timestampBegin: $ts, secondsCount: 0}')"
    "get_blocks_hashes_by_timestamps|whole-chain|$(q --argjson ts "${TS1:-0}" --argjson te "${TSTOP:-0}" '{timestampBegin: ($ts - 10), secondsCount: ($te - $ts + 30)}')"
)
: > "$SD/node-routes-compare.jsonl"
for c in "${CASES[@]}"; do
    route=${c%%|*}
    rest=${c#*|}
    case_name=${rest%%|*}
    body=${rest#*|}
    fx_record "$route/$case_name/xkrd" POST "$FX_RPC" "/$route" "$body" "Content-Type: application/json"
    xs=$FX_STATUS
    xb=$FX_BODY
    fx_record "$route/$case_name/cpp" POST "$FX_CPP_RPC" "/$route" "$body" "Content-Type: application/json"
    cs=$FX_STATUS
    cb=$FX_BODY
    # bodies go through files: a large /queryblockslite answer does not fit in an argument
    printf '%s' "$xb" > "$SD/.cmp-x.json"
    printf '%s' "$cb" > "$SD/.cmp-c.json"
    same=false
    if [ "$xs" = "$cs" ] && cmp -s <(jq -S . "$SD/.cmp-x.json" 2> /dev/null || cat "$SD/.cmp-x.json") \
        <(jq -S . "$SD/.cmp-c.json" 2> /dev/null || cat "$SD/.cmp-c.json"); then
        same=true
    fi
    diffs=$(jq -cn --slurpfile x "$SD/.cmp-x.json" --slurpfile c "$SD/.cmp-c.json" '
        ($x[0] // null) as $x | ($c[0] // null) as $c
        | ([$x | paths(scalars)] + [$c | paths(scalars)]) | unique
        | map(select(. as $p | try (($x | getpath($p)) != ($c | getpath($p))) catch true))
        | map(map(tostring) | join(".")) | .[0:20]' 2> /dev/null || echo '["(not JSON)"]')
    jq -cn --arg route "$route" --arg case "$case_name" --argjson xs "$xs" --argjson cs "$cs" \
        --argjson same "$same" --argjson diffs "$diffs" \
        '{route: $route, case: $case, xkrdStatus: $xs, cppStatus: $cs, equal: $same, differingPaths: $diffs}' \
        >> "$SD/node-routes-compare.jsonl"
    if [ "$same" = true ]; then
        pass routes "/$route $case_name: xkrd and the C++ node answer the same (HTTP $xs)"
    else
        fail routes "/$route $case_name: xkrd HTTP $xs vs C++ HTTP $cs; differing paths $(jq -c . <<< "$diffs")"
    fi
done

# ------------------------------------------------------------------ publish

fx_publish_jsonl "$OUT"
jq -s . "$SD/node-routes-compare.jsonl" > "$OUT/node-routes-compare.json"
n=$(fx_app_codes "$OUT" "$OUT/application-codes.json")
pass publish "$n distinct JSON-RPC errors in application-codes.json"
rm -rf "$OUT/generate"
mkdir -p "$OUT/generate"
for f in "$SD"/gen/*.txt; do
    fx_normalise text "$f" "$OUT/generate/$(basename "$f")"
done
cp "$SD/gen/exit-codes.json" "$OUT/generate/exit-codes.json"
cp "$SD/image.json" "$OUT/_image.json"
[ -f "$WD/fx-follow.json" ] && cp "$WD/fx-follow.json" "$OUT/_image-follow.json"
fx_meta "$OUT/_meta.json" kryptokrona-service "$SD/image.json" \
    "walletd daemon: $FX_WALLETD_DAEMON; C++ node image (route comparison): $FX_FOLLOWER" \
    "containers: walletd-main.wallet (seed A, password walletd-pass), walletd-k.wallet (keys K, empty password), walletd-random.wallet (password pässwörd ✓, made tracking-only)" \
    "$(fx_image_note "$WALLETD_IMAGE")" \
    "UNLOCK_AT=$UNLOCK_AT; first send mined at height $TXH; node-routes-compare.json: xkrd vs C++ answers, compared after sorting keys"
finish
