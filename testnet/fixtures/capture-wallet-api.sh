#!/usr/bin/env bash
# capture-wallet-api: fixtures of the C++ wallet-api (REST) from the published testnet image.
#
#   xkrd X     fixture chain, P2P 24898 / RPC 24899 (own genesis, `xkr-testwallet mine` only)
#   wallet-api FX_IMAGE, port 24070 (plain) and 24071 (--enable-cors https://fixture.example),
#              X-API-KEY fixture-api-key, wallet files in <wd>/fx-wallet-api/w (= /w)
#
# Steps: image report and follow check of the image's kryptokronad (fx_verify_image); auth, OPTIONS
# and CORS; every route with no wallet open (403); wallet open/import/create and their errors;
# every route that needs a closed wallet while one is open (403); blocks 1..30 pay wallet A
# (seed "skulls woozy ..."), filler to 130; status, keys, addresses, integrated addresses,
# subwallets; sends and every send error the testnet can show; fusion; transactions by height,
# range, address and hash; balances; node, save, reset; a non-deterministic wallet (K), a view
# wallet (A's view key) with every view-banned route, wallet B with the empty password; 24 created
# wallets for the mnemonic vectors.
#
# Output:
#   crates/xkr-wallet-api/tests/fixtures/cpp/<group>.jsonl  (normalised, lib-wallet.sh fx_normalise)
#   crates/xkr-wallet-api/tests/fixtures/cpp/_meta.json, _image-follow.json
#   crates/xkr-wallet-core/tests/data/mnemonic.json          (raw values, nothing random replaced)
#
# Env: FX_IMAGE, MNEMONIC_COUNT (24), FX_OUT_ROOT, plus lib-wallet.sh knobs.
# Never builds anything. Needs: xkrd, xkr-testwallet, docker, the image, curl, jq, python3.
# suite: expected=480 timeout=1800
set -euo pipefail
: "${TIMEOUT:=300}"
. "$(dirname "$0")/lib-wallet.sh"
fx_init "${1:-}" fx-wallet-api

MNEMONIC_COUNT=${MNEMONIC_COUNT:-24}
OUT=$FX_OUT_ROOT/crates/xkr-wallet-api/tests/fixtures/cpp
MN_OUT=$FX_OUT_ROOT/crates/xkr-wallet-core/tests/data/mnemonic.json
fx_require_image "$FX_IMAGE"

jget() { jq -r "$1" <<< "$FX_BODY" 2> /dev/null || true; }
# the transaction hash of the last answer, if it was a successful send
txof() { if [ "$FX_STATUS" = 200 ] || [ "$FX_STATUS" = 201 ]; then jget '.transactionHash // empty'; fi; }

# ------------------------------------------------------------------ image

fx_image_report "$FX_IMAGE" "$SD/image.json"
if [ "$(jq -r '.files | index("wallet-api") != null' "$SD/image.json")" != true ]; then
    missing_exit image "$FX_IMAGE has no wallet-api in $FX_BIN" "$PULL_FX_IMAGE"
fi
fx_verify_image "$FX_IMAGE"

# ------------------------------------------------------------------ chain and wallet-api

fx_fresh_chain
fx_api_start "$C_FX_API" "$PORT_FX_API"
fx_api_start "$C_FX_API_CORS" "$PORT_FX_API_CORS" --enable-cors "$FX_CORS"
printf 'this is not a wallet file\n' > "$SD/w/not-a-wallet.wallet"
mkdir -p "$SD/vec"
: > "$SD/vec/valid.jsonl"
: > "$SD/vec/invalid.jsonl"

# mn_vector <name>: seed and keys of the open wallet's primary address (unrecorded calls)
mn_vector() {
    local a
    a=$(fx_api_q GET /addresses/primary | jq -r '.address // empty' 2> /dev/null || true)
    [ -n "$a" ] || return 0
    jq -cn --arg name "$1" --arg a "$a" --arg v "$(fx_api_q GET /keys)" --arg k "$(fx_api_q GET "/keys/$a")" \
        --arg m "$(fx_api_q GET "/keys/mnemonic/$a")" '
        def j: try fromjson catch {};
        ($m | j) as $mm | {name: $name, address: $a,
         privateViewKey: ($v | j | .privateViewKey), privateSpendKey: ($k | j | .privateSpendKey),
         publicSpendKey: ($k | j | .publicSpendKey)}
        + (if $mm.mnemonicSeed then {mnemonicSeed: $mm.mnemonicSeed}
           else {mnemonicError: {errorCode: $mm.errorCode, errorMessage: $mm.errorMessage}} end)' \
        >> "$SD/vec/valid.jsonl"
}
# mn_invalid <name> <seed>: the last answer was an import of an invalid seed
mn_invalid() {
    jq -cn --arg name "$1" --arg seed "$2" --argjson status "$FX_STATUS" --arg body "$FX_BODY" \
        '{name: $name, mnemonicSeed: $seed, status: $status, response: ($body | try fromjson catch $body)}' \
        >> "$SD/vec/invalid.jsonl"
}

# ------------------------------------------------------------------ auth, OPTIONS, CORS

fx_group auth
P_OPEN=$(fx_wparams api-a.wallet pass-a)
fx_record "auth/missing-key" GET "127.0.0.1:$PORT_FX_API" /status -
fx_record "auth/wrong-key" GET "127.0.0.1:$PORT_FX_API" /status - "X-API-KEY: wrong"
fx_record "auth/wrong-key-post" POST "127.0.0.1:$PORT_FX_API" /wallet/open "$P_OPEN" \
    "X-API-KEY: wrong" "Content-Type: application/json"
fx_record "options/no-cors/preflight" OPTIONS "127.0.0.1:$PORT_FX_API" /wallet/open - \
    "Origin: $FX_CORS" "Access-Control-Request-Method: POST"
fx_record "options/no-cors/plain" OPTIONS "127.0.0.1:$PORT_FX_API" /balance -
fx_record "options/cors/preflight" OPTIONS "127.0.0.1:$PORT_FX_API_CORS" /wallet/open - \
    "Origin: $FX_CORS" "Access-Control-Request-Method: POST" "Access-Control-Request-Headers: X-API-KEY, Content-Type"
fx_record "options/cors/plain" OPTIONS "127.0.0.1:$PORT_FX_API_CORS" /balance - "Origin: $FX_CORS"
fx_record "cors/wrong-key" GET "127.0.0.1:$PORT_FX_API_CORS" /status - "Origin: $FX_CORS" "X-API-KEY: wrong"
fx_record "cors/no-wallet" GET "127.0.0.1:$PORT_FX_API_CORS" /status - "Origin: $FX_CORS" "X-API-KEY: $FX_API_KEY"
fx_record "cors/open-missing-file" POST "127.0.0.1:$PORT_FX_API_CORS" /wallet/open \
    "$(fx_wparams missing.wallet x)" "Origin: $FX_CORS" "X-API-KEY: $FX_API_KEY" "Content-Type: application/json"
fx_api "route/unknown" GET /no/such/route
fx_api "route/bad-height" GET /transactions/12x

# ------------------------------------------------------------------ every route without a wallet

fx_group state
D=$FX_DUMMY_ADDR
ROUTES=(
    "DELETE /wallet" "POST /addresses/create" "POST /addresses/import" "POST /addresses/import/view"
    "DELETE /addresses/$D" "PUT /save" "PUT /reset" "PUT /node" "GET /node" "GET /keys"
    "GET /keys/$D" "GET /keys/mnemonic/$D" "GET /status" "GET /addresses" "GET /addresses/primary"
    "GET /addresses/$D/$FX_PID1" "GET /transactions" "GET /transactions/unconfirmed"
    "GET /transactions/unconfirmed/$D" "GET /transactions/0" "GET /transactions/0/10"
    "GET /transactions/address/$D/0" "GET /transactions/address/$D/0/10"
    "GET /transactions/privatekey/$FX_UNKNOWN_HASH" "GET /transactions/hash/$FX_UNKNOWN_HASH"
    "GET /balance" "GET /balance/$D" "GET /balances" "POST /transactions/send/basic"
    "POST /transactions/send/advanced" "POST /transactions/send/fusion/basic"
    "POST /transactions/send/fusion/advanced"
)
for r in "${ROUTES[@]}"; do
    m=${r%% *}
    p=${r#* }
    case $m in POST | PUT) fx_api "no-wallet/$m $p" "$m" "$p" '{}' ;; *) fx_api "no-wallet/$m $p" "$m" "$p" ;; esac
done
n403=$(jq -s '[.[] | select(.response.status == 403)] | length' "$FX_JSONL")
note "no wallet open: $n403 of ${#ROUTES[@]} routes answered 403"

# ------------------------------------------------------------------ open / import / create

fx_group wallet
fx_api "open/missing-file" POST /wallet/open "$(fx_wparams missing.wallet pass)"
fx_api "open/missing-password-field" POST /wallet/open '{"filename":"/w/api-a.wallet"}'
fx_api "open/non-json-body" POST /wallet/open 'this is not json'
fx_api "open/not-a-wallet" POST /wallet/open "$(fx_wparams not-a-wallet.wallet pass)"
fx_api "create/empty-filename" POST /wallet/create "$(jq -cn --argjson port "$PORT_FX_RPC" '{filename: "", password: "x", daemonHost: "127.0.0.1", daemonPort: $port}')"
fx_api "import/seed/bad-checksum" POST /wallet/import/seed "$(fx_wparams bad-1.wallet x "$(jq -cn --arg s "$FX_SEED_BAD_CHECKSUM" '{mnemonicSeed: $s}')")"
mn_invalid bad-checksum "$FX_SEED_BAD_CHECKSUM"
fx_api "import/seed/24-words" POST /wallet/import/seed "$(fx_wparams bad-2.wallet x "$(jq -cn --arg s "$FX_SEED_24_WORDS" '{mnemonicSeed: $s}')")"
mn_invalid 24-words "$FX_SEED_24_WORDS"
fx_api "import/seed/unknown-word" POST /wallet/import/seed "$(fx_wparams bad-3.wallet x "$(jq -cn --arg s "$FX_SEED_UNKNOWN_WORD" '{mnemonicSeed: $s}')")"
mn_invalid unknown-word "$FX_SEED_UNKNOWN_WORD"
fx_api "import/seed/empty" POST /wallet/import/seed "$(fx_wparams bad-4.wallet x '{"mnemonicSeed":""}')"
mn_invalid empty ""
fx_api "import/seed/missing-field" POST /wallet/import/seed "$(fx_wparams bad-5.wallet x)"
fx_api "import/key/bad-key-format" POST /wallet/import/key "$(fx_wparams bad-6.wallet x "$(jq -cn --arg v "$FX_K_VIEW" '{privateViewKey: $v, privateSpendKey: "zz"}')")"
fx_api "import/key/not-reduced" POST /wallet/import/key "$(fx_wparams bad-7.wallet x "$(jq -cn --arg v "$FX_K_VIEW" '{privateViewKey: $v, privateSpendKey: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"}')")"
fx_api_q DELETE /wallet > /dev/null  # in case an import above was accepted after all
fx_api "import/view/bad-address" POST /wallet/import/view "$(fx_wparams bad-8.wallet x "$(jq -cn --arg v "$FX_A_VIEW" '{privateViewKey: $v, address: "SEKRnotanaddress"}')")"

fx_api_q DELETE /wallet > /dev/null
# wallet A: seed "skulls woozy ...", password pass-a
fx_api "import/seed/a" POST /wallet/import/seed "$(fx_wparams api-a.wallet pass-a "$(jq -cn --arg s "$FX_SEED_A" '{mnemonicSeed: $s, scanHeight: 0}')")"
[ "$FX_STATUS" = 200 ] || die setup "wallet-api did not import seed A: $FX_STATUS $FX_BODY"
fx_api "addresses/primary/a" GET /addresses/primary
A=$(jget .address)
[ -n "$A" ] || die setup "no primary address for A: $FX_BODY"
XKR_A=$(fx_alt_address "$A")
fx_keep "$A" "Xkr${A:4}" "$XKR_A"
fx_api "keys/a" GET "/keys/$A"
A_PUB=$(jget .publicSpendKey)
fx_keep "$A_PUB"
fx_api "status/right-after-import~shape" GET /status
mn_vector js-seed-skulls
note "A = $A"

fx_group state-open
for r in "POST /wallet/open" "POST /wallet/import/key" "POST /wallet/import/seed" "POST /wallet/import/view" "POST /wallet/create"; do
    fx_api "wallet-open/$r" POST "${r#* }" "$(fx_wparams other.wallet x)"
done

fx_group wallet
fx_api "close/a" DELETE /wallet
fx_api "close/again" DELETE /wallet
head -c -16 "$SD/w/api-a.wallet" > "$SD/w/api-a-truncated.wallet"
python3 -I -c 'import sys; b = bytearray(open(sys.argv[1], "rb").read()); b[len(b) // 2] ^= 0xff; open(sys.argv[2], "wb").write(b)' \
    "$SD/w/api-a.wallet" "$SD/w/api-a-corrupted.wallet"
fx_api "open/wrong-password" POST /wallet/open "$(fx_wparams api-a.wallet nope)"
fx_api "open/corrupted-middle-byte" POST /wallet/open "$(fx_wparams api-a-corrupted.wallet pass-a)"
fx_api "open/truncated-16-bytes" POST /wallet/open "$(fx_wparams api-a-truncated.wallet pass-a)"

# wallet B: seed "nugget lazy ...", empty password
fx_api "import/seed/b-empty-password" POST /wallet/import/seed "$(fx_wparams api-b.wallet "" "$(jq -cn --arg s "$FX_SEED_B" '{mnemonicSeed: $s}')")"
fx_api "addresses/primary/b" GET /addresses/primary
B=$(jget .address)
fx_keep "$B"
fx_api "keys/b" GET "/keys/$B"
fx_keep "$(jget .publicSpendKey)"
mn_vector js-seed-nugget
fx_api "close/b" DELETE /wallet

# wallet K: keys whose view key is not derived from the spend key
fx_api "import/key/k" POST /wallet/import/key "$(fx_wparams api-k.wallet pass-k "$(jq -cn --arg v "$FX_K_VIEW" --arg s "$FX_K_SPEND" '{privateViewKey: $v, privateSpendKey: $s, scanHeight: 0}')")"
fx_api "addresses/primary/k" GET /addresses/primary
K=$(jget .address)
fx_keep "$K"
fx_api "keys/mnemonic/k-not-deterministic" GET "/keys/mnemonic/$K"
mn_vector js-keys-non-deterministic
fx_api "close/k" DELETE /wallet
fx_api "import/key/already-exists" POST /wallet/import/key "$(fx_wparams api-k.wallet pass-k "$(jq -cn --arg v "$FX_K_VIEW" --arg s "$FX_K_SPEND" '{privateViewKey: $v, privateSpendKey: $s}')")"

# view wallet of A
fx_api "import/view/a" POST /wallet/import/view "$(fx_wparams api-view.wallet pass-view "$(jq -cn --arg v "$FX_A_VIEW" --arg a "$A" '{privateViewKey: $v, address: $a, scanHeight: 0}')")"
fx_api "close/view" DELETE /wallet

# a random wallet
fx_api "create/random" POST /wallet/create "$(fx_wparams api-created.wallet pass-created)"
fx_api "addresses/primary/created" GET /addresses/primary
fx_api "keys/mnemonic/created" GET "/keys/mnemonic/$(jget .address)"
fx_api "close/created" DELETE /wallet
fx_api "create/already-exists" POST /wallet/create "$(fx_wparams api-created.wallet pass-created)"
[ -n "$B" ] && [ -n "$K" ] || die setup "wallets B [$B] or K [$K] missing"

# ------------------------------------------------------------------ fund A

fx_mine "$A" "$FX_FUND_TO"
fx_mine "$FX_DUMMY_ADDR" "$FX_FILLER_TO"
pass chain "X at $(top_of "$FX_RPC"): blocks 1..$FX_FUND_TO pay A, filler to $FX_FILLER_TO"

fx_group wallet
fx_api "open/a" POST /wallet/open "$P_OPEN"
fx_api "status/during-sync~shape" GET /status
fx_api_wait_synced

fx_group status
fx_api "status/synced" GET /status
fx_api "node/get" GET /node
fx_api "balance/a" GET /balance
fx_api "balances/a-only" GET /balances
fx_api "keys/view" GET /keys
fx_api "keys/spend/a" GET "/keys/$A"
fx_api "keys/mnemonic/a" GET "/keys/mnemonic/$A"
fx_api "keys/spend/foreign-b" GET "/keys/$B"

# ------------------------------------------------------------------ addresses

fx_group addresses
fx_api "addresses/list" GET /addresses
fx_api "integrated/a-pid1" GET "/addresses/$A/$FX_PID1"
fx_keep "$(jget .integratedAddress)"
fx_api "integrated/foreign-b-pid2" GET "/addresses/$B/$FX_PID2"
INT_B=$(jget .integratedAddress)
fx_keep "$INT_B"
fx_api "integrated/pid-63-chars" GET "/addresses/$A/${FX_PID1:1}"
fx_api "integrated/pid-not-hex" GET "/addresses/$A/zz${FX_PID1:2}"
fx_api "integrated/xkr-prefix-syntactic" GET "/addresses/Xkr${A:4}/$FX_PID1"
fx_api "integrated/xkr-prefix-a" GET "/addresses/$XKR_A/$FX_PID1"
fx_api "balance/xkr-prefix-a" GET "/balance/$XKR_A"
fx_api "keys/xkr-prefix-a" GET "/keys/$XKR_A"
fx_api "integrated/of-integrated" GET "/addresses/$INT_B/$FX_PID1"
fx_api "create/random" POST /addresses/create
R1=$(jget .address)
fx_api "import/sub1" POST /addresses/import "$(jq -cn --arg k "$FX_SUB1" '{privateSpendKey: $k, scanHeight: 0}')"
SUB1=$(jget .address)
fx_keep "$SUB1"
fx_api "import/sub1-again" POST /addresses/import "$(jq -cn --arg k "$FX_SUB1" '{privateSpendKey: $k}')"
fx_api "import/bad-key" POST /addresses/import '{"privateSpendKey":"zz"}'
fx_api "import/missing-key" POST /addresses/import '{}'
fx_api "import/view-in-spend-wallet" POST /addresses/import/view "$(jq -cn --arg k "$FX_K_SPEND_PUB" '{publicSpendKey: $k, scanHeight: 0}')"
fx_api "list/with-subwallets" GET /addresses
fx_api "balances/with-subwallets" GET /balances
fx_api "keys/sub1" GET "/keys/$SUB1"
SUB1_PUB=$(jget .publicSpendKey)
fx_keep "$SUB1_PUB"
fx_api "keys/mnemonic/sub1-not-deterministic" GET "/keys/mnemonic/$SUB1"
fx_api "delete/random" DELETE "/addresses/$R1"
fx_api "delete/primary" DELETE "/addresses/$A"
fx_api "delete/not-in-wallet" DELETE "/addresses/$B"
fx_api "list/after-delete" GET /addresses
fx_api "primary" GET /addresses/primary

# ------------------------------------------------------------------ sends

fx_group send
TXS=()
LAST_TX=""
adv() {
    fx_api "$1" POST /transactions/send/advanced "$2"
    LAST_TX=$(txof)
    [ -z "$LAST_TX" ] || TXS+=("$LAST_TX")
}
fx_api "basic/k" POST /transactions/send/basic "$(jq -cn --arg d "$K" '{destination: $d, amount: 100000}')"
TX1=$(txof)
TXS+=("$TX1")
fx_api "basic/payment-id" POST /transactions/send/basic "$(jq -cn --arg d "$B" --arg p "$FX_PID1" '{destination: $d, amount: 200000, paymentID: $p}')"
TX2=$(txof)
TXS+=("$TX2")
adv "advanced/multi" "$(jq -cn --arg k "$K" --arg b "$B" '{destinations: [{address: $k, amount: 300000}, {address: $b, amount: 400000}], mixin: 3, fee: 10}')"
TX3=$LAST_TX
adv "advanced/integrated" "$(jq -cn --arg i "$INT_B" '{destinations: [{address: $i, amount: 500000}]}')"
adv "advanced/integrated-conflicting-payment-id" "$(jq -cn --arg i "$INT_B" --arg p "$FX_PID1" '{destinations: [{address: $i, amount: 500000}], paymentID: $p}')"
adv "advanced/integrated-same-payment-id" "$(jq -cn --arg i "$INT_B" --arg p "$FX_PID2" '{destinations: [{address: $i, amount: 510000}], paymentID: $p}')"
adv "advanced/source-a" "$(jq -cn --arg k "$K" --arg a "$A" '{destinations: [{address: $k, amount: 600000}], sourceAddresses: [$a]}')"
adv "advanced/source-empty-subwallet" "$(jq -cn --arg k "$K" --arg s "$SUB1" '{destinations: [{address: $k, amount: 600000}], sourceAddresses: [$s]}')"
adv "advanced/source-not-in-wallet" "$(jq -cn --arg k "$K" --arg b "$B" '{destinations: [{address: $k, amount: 600000}], sourceAddresses: [$b]}')"
adv "advanced/change-not-in-wallet" "$(jq -cn --arg k "$K" --arg b "$B" '{destinations: [{address: $k, amount: 600000}], changeAddress: $b}')"
UNLOCK_AT=$(($(height_of "$FX_RPC") + 40))
adv "advanced/unlock-time-height" "$(jq -cn --arg k "$K" --argjson u "$UNLOCK_AT" '{destinations: [{address: $k, amount: 700000}], unlockTime: $u}')"
adv "advanced/mixin-0" "$(jq -cn --arg k "$K" '{destinations: [{address: $k, amount: 800000}], mixin: 0}')"
adv "advanced/mixin-500" "$(jq -cn --arg k "$K" '{destinations: [{address: $k, amount: 800000}], mixin: 500}')"
adv "advanced/fee-below-minimum" "$(jq -cn --arg k "$K" '{destinations: [{address: $k, amount: 800000}], fee: 1}')"
adv "advanced/insufficient-funds" "$(jq -cn --arg k "$K" '{destinations: [{address: $k, amount: 9000000000000000}]}')"
adv "advanced/amount-zero" "$(jq -cn --arg k "$K" '{destinations: [{address: $k, amount: 0}]}')"
adv "advanced/amount-negative" '{"destinations":[{"address":"'"$K"'","amount":-5}]}'
adv "advanced/amount-fraction" '{"destinations":[{"address":"'"$K"'","amount":1.5}]}'
adv "advanced/amount-overflow" '{"destinations":[{"address":"'"$K"'","amount":9000000000000000000},{"address":"'"$K"'","amount":9000000000000000000},{"address":"'"$K"'","amount":9000000000000000000}]}'
adv "advanced/no-destinations" '{"destinations":[]}'
adv "advanced/many-outputs-120" "$(jq -cn --arg k "$K" '{destinations: [range(120) | {address: $k, amount: 10000}]}')"
adv "advanced/address-wrong-length" '{"destinations":[{"address":"SEKRshort","amount":1000}]}'
adv "advanced/address-wrong-prefix" '{"destinations":[{"address":"TRTL'"${A:4}"'","amount":1000}]}'
adv "advanced/address-bad-checksum" '{"destinations":[{"address":"'"${A:0:98}$([ "${A:98:1}" = a ] && echo b || echo a)"'","amount":1000}]}'
adv "advanced/address-xkr-prefix-a" "$(jq -cn --arg x "$XKR_A" '{destinations: [{address: $x, amount: 1000}]}')"
adv "advanced/address-xkr-prefix-syntactic" '{"destinations":[{"address":"Xkr'"${A:4}"'","amount":1000}]}'
adv "advanced/payment-id-wrong-length" "$(jq -cn --arg k "$K" --arg p "${FX_PID1:1}" '{destinations: [{address: $k, amount: 1000}], paymentID: $p}')"
adv "advanced/payment-id-not-hex" "$(jq -cn --arg k "$K" --arg p "zz${FX_PID1:2}" '{destinations: [{address: $k, amount: 1000}], paymentID: $p}')"
adv "advanced/missing-amount" "$(jq -cn --arg k "$K" '{destinations: [{address: $k}]}')"
adv "advanced/missing-destinations" '{}'
fx_api "basic/missing-destination" POST /transactions/send/basic '{"amount":1000}'
fx_api "basic/non-json-body" POST /transactions/send/basic 'amount=1000'
fx_api "fusion/basic" POST /transactions/send/fusion/basic '{}'
t=$(txof); [ -z "$t" ] || TXS+=("$t")
fx_api "fusion/advanced" POST /transactions/send/fusion/advanced "$(jq -cn --arg a "$A" '{destination: $a, mixin: 3, sourceAddresses: [$a]}')"
t=$(txof); [ -z "$t" ] || TXS+=("$t")
fx_api "fusion/advanced-foreign-destination" POST /transactions/send/fusion/advanced "$(jq -cn --arg b "$B" '{destination: $b}')"
fx_api "fusion/advanced-mixin-500" POST /transactions/send/fusion/advanced "$(jq -cn --arg a "$A" '{destination: $a, mixin: 500}')"
note "sent ${#TXS[@]} transaction(s)"
[ -n "$TX1" ] || fail send "send/basic to K failed: $(head -c 300 <<< "$FX_BODY")"

fx_group transactions-pending
fx_api "unconfirmed~shape" GET /transactions/unconfirmed
fx_api "unconfirmed/for-address-a" GET "/transactions/unconfirmed/$A"
fx_api "hash/tx1-pending" GET "/transactions/hash/$TX1"
fx_api "privatekey/tx1" GET "/transactions/privatekey/$TX1"
fx_api "privatekey/unknown" GET "/transactions/privatekey/$FX_UNKNOWN_HASH"
fx_api "balance/after-sends~shape" GET /balance
fx_api "balances/after-sends~shape" GET /balances

fx_confirm "${TXS[@]}"
fx_api_wait_synced
TXH=$(num "$(tx_json "$FX_RPC" "$TX1")" height)
TOP=$(height_of "$FX_RPC")
note "first send mined at ${TXH:-?}, X at $TOP"
TXH=${TXH:-$((FX_FILLER_TO + 1))}

fx_group transactions
fx_api "all" GET /transactions
fx_api "from-0" GET /transactions/0
fx_api "from-fund-end" GET "/transactions/$FX_FUND_TO"
fx_api "from-tx-height" GET "/transactions/$TXH"
fx_api "range/tx-block" GET "/transactions/$TXH/$((TXH + 1))"
fx_api "range/whole-chain" GET "/transactions/0/$((TOP + 1))"
fx_api "range/start-after-end" GET "/transactions/$((TXH + 1))/$TXH"
fx_api "range/start-equals-end" GET "/transactions/$TXH/$TXH"
fx_api "address/a-from-0" GET "/transactions/address/$A/0"
fx_api "address/a-tx-block" GET "/transactions/address/$A/$TXH/$((TXH + 1))"
fx_api "address/sub1-from-0" GET "/transactions/address/$SUB1/0"
fx_api "address/foreign-k" GET "/transactions/address/$K/0"
fx_api "hash/tx1-confirmed" GET "/transactions/hash/$TX1"
fx_api "hash/tx3-confirmed" GET "/transactions/hash/${TX3:-$FX_UNKNOWN_HASH}"
fx_api "hash/unknown" GET "/transactions/hash/$FX_UNKNOWN_HASH"
fx_api "privatekey/tx3" GET "/transactions/privatekey/${TX3:-$FX_UNKNOWN_HASH}"
fx_api "unconfirmed/after-mining" GET /transactions/unconfirmed
fx_api "balance" GET /balance
fx_api "balance/a" GET "/balance/$A"
fx_api "balance/sub1" GET "/balance/$SUB1"
fx_api "balance/not-in-wallet" GET "/balance/$B"
fx_api "balances" GET /balances
fx_api "status" GET /status

# change to a subwallet last: its amounts depend on the random input choice
fx_group send-change
fx_api "advanced/change-to-sub1" POST /transactions/send/advanced "$(jq -cn --arg k "$K" --arg s "$SUB1" '{destinations: [{address: $k, amount: 900000}], changeAddress: $s}')"
TXC=$(txof)
fx_api "hash/change-to-sub1~shape" GET "/transactions/hash/${TXC:-$FX_UNKNOWN_HASH}"

# ------------------------------------------------------------------ node, save, reset

fx_group node
fx_api "node/put" PUT /node "$(jq -cn --argjson p "$PORT_FX_RPC" '{daemonHost: "127.0.0.1", daemonPort: $p}')"
fx_api "node/get-after-put" GET /node
fx_api "node/put-missing-port" PUT /node '{"daemonHost":"127.0.0.1"}'
fx_api "save" PUT /save
fx_api "reset" PUT /reset '{}'
fx_api "status/after-reset~shape" GET /status
fx_api_wait_synced
fx_api "balance/after-reset" GET /balance
fx_api "reset/scan-height" PUT /reset "$(jq -cn --argjson h "$FX_FUND_TO" '{scanHeight: $h}')"
fx_api_wait_synced
fx_api "balances/after-reset-from-fund-end" GET /balances
fx_api "transactions/after-reset-from-fund-end~shape" GET /transactions
fx_confirm "${TXC:-}"
fx_api_wait_synced
fx_api "close/a-final" DELETE /wallet

# ------------------------------------------------------------------ wallet K

fx_group wallet-k
fx_api "open/k" POST /wallet/open "$(fx_wparams api-k.wallet pass-k)"
fx_api_wait_synced
fx_api "balance/unlock-time-pending" GET /balance
fx_api "transactions" GET /transactions
fx_api "keys/mnemonic/not-deterministic" GET "/keys/mnemonic/$K"
fx_api "fusion/basic-not-enough-inputs" POST /transactions/send/fusion/basic '{}'
fx_api "fusion/advanced-not-enough-inputs" POST /transactions/send/fusion/advanced "$(jq -cn --arg k "$K" '{destination: $k}')"
fx_api "send/basic-to-a" POST /transactions/send/basic "$(jq -cn --arg a "$A" '{destination: $a, amount: 50000}')"
TXK=$(txof)
fx_confirm "${TXK:-}"
fx_api_wait_synced
fx_api "transactions/after-send" GET /transactions
fx_api "balance/after-send" GET /balance
fx_api "close" DELETE /wallet

# ------------------------------------------------------------------ view wallet

fx_group view-wallet
fx_api "open" POST /wallet/open "$(fx_wparams api-view.wallet pass-view)"
fx_api_wait_synced
fx_api "status" GET /status
fx_api "banned/addresses-create" POST /addresses/create '{}'
fx_api "banned/addresses-import" POST /addresses/import "$(jq -cn --arg k "$FX_SUB2" '{privateSpendKey: $k}')"
fx_api "banned/send-basic" POST /transactions/send/basic "$(jq -cn --arg k "$K" '{destination: $k, amount: 1000}')"
fx_api "banned/send-advanced" POST /transactions/send/advanced "$(jq -cn --arg k "$K" '{destinations: [{address: $k, amount: 1000}]}')"
fx_api "banned/fusion-basic" POST /transactions/send/fusion/basic '{}'
fx_api "banned/fusion-advanced" POST /transactions/send/fusion/advanced "$(jq -cn --arg a "$A" '{destination: $a}')"
fx_api "banned/keys-spend" GET "/keys/$A"
fx_api "banned/keys-mnemonic" GET "/keys/mnemonic/$A"
fx_api "banned/transaction-private-key" GET "/transactions/privatekey/$TX1"
fx_api "allowed/keys" GET /keys
fx_api "allowed/balance" GET /balance
fx_api "allowed/balances" GET /balances
fx_api "allowed/transactions" GET /transactions
fx_api "allowed/addresses-import-view" POST /addresses/import/view "$(jq -cn --arg k "$SUB1_PUB" '{publicSpendKey: $k, scanHeight: 0}')"
fx_keep "$(jget .address)"
fx_api "allowed/addresses-import-view-again" POST /addresses/import/view "$(jq -cn --arg k "$SUB1_PUB" '{publicSpendKey: $k}')"
fx_api "allowed/addresses" GET /addresses
fx_api "allowed/delete-view-subwallet" DELETE "/addresses/$(jq -r '.addresses[-1] // empty' <<< "$FX_BODY" 2> /dev/null || true)"
fx_api "close" DELETE /wallet

# ------------------------------------------------------------------ wallet B (empty password)

fx_group wallet-b
fx_api "open/empty-password" POST /wallet/open "$(fx_wparams api-b.wallet "")"
fx_api_wait_synced
fx_api "balances/one-subwallet" GET /balances
fx_api "balance" GET /balance
fx_api "transactions" GET /transactions
fx_api "keys/mnemonic" GET "/keys/mnemonic/$B"
fx_api "close" DELETE /wallet

# ------------------------------------------------------------------ mnemonic vectors

step "mnemonic vectors: $MNEMONIC_COUNT created wallets"
for ((i = 1; i <= MNEMONIC_COUNT; i++)); do
    r=$(fx_api_q POST /wallet/create "$(fx_wparams "mn-$i.wallet" "mn")")
    mn_vector "created-$i"
    fx_api_q DELETE /wallet > /dev/null
done
nv=$(wc -l < "$SD/vec/valid.jsonl")
ns=$(jq -s '[.[] | select(.mnemonicSeed)] | length' "$SD/vec/valid.jsonl")
if [ "$ns" -ge 20 ]; then
    pass mnemonic "$ns seed/key vectors (of $nv wallets) and $(wc -l < "$SD/vec/invalid.jsonl") invalid seeds"
else
    fail mnemonic "only $ns seed/key vectors (want >= 20)"
fi

# ------------------------------------------------------------------ publish

fx_stop_tool "$C_FX_API"
fx_stop_tool "$C_FX_API_CORS"
fx_publish_jsonl "$OUT"
cp "$SD/follow.json" "$OUT/_image-follow.json"
fx_meta "$OUT/_meta.json" wallet-api "$SD/image.json" \
    "wallet-api runs against xkrd X directly (Nigel routes)" \
    "$(fx_image_note "$FX_IMAGE")" \
    "testnet heights are below MIXIN_LIMITS_V1_HEIGHT: no mixin limits apply, so mixin-0 / mixin-500 show the daemon's and the decoy answers, not MIXIN_TOO_SMALL/BIG" \
    "fixed material: lib-wallet.sh (seeds A and B, keys K, sub keys, payment ids); wallet files: /w/api-*.wallet with passwords pass-a, empty (B), pass-k, pass-view, pass-created" \
    "UNLOCK_AT=$UNLOCK_AT (unlock-time send), first send mined at height $TXH"
mkdir -p "$(dirname "$MN_OUT")"
jq -n --slurpfile valid "$SD/vec/valid.jsonl" --slurpfile invalid "$SD/vec/invalid.jsonl" \
    --slurpfile img "$SD/image.json" \
    '{source: "C++ wallet-api: /wallet/create or /wallet/import/seed|key, then /addresses/primary, /keys, /keys/{address}, /keys/mnemonic/{address}",
      image: {name: $img[0].image, id: $img[0].id, created: $img[0].created},
      wordList: "src/mnemonics/wordlist.h (1626 words), checksum word = CRC32 over the 3-letter prefixes",
      vectors: [$valid[] | select(.mnemonicSeed)],
      nonDeterministic: [$valid[] | select(.mnemonicSeed | not)],
      invalid: $invalid}' > "$MN_OUT"
pass publish "mnemonic vectors in ${MN_OUT#"$FX_OUT_ROOT"/}"
finish
