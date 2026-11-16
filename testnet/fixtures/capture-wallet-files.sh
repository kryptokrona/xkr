#!/usr/bin/env bash
# capture-wallet-files: wallet files made by the C++ tools, with their passwords and what a reader
# must find in them ("expected" dumps taken from the tool right before it saved the file).
#
#   xkrd X     fixture chain, P2P 24898 / RPC 24899 (own genesis, `xkr-testwallet mine` only)
#   wallet-api FX_IMAGE, port 24070, against X            -> format A (WalletBackend JSON, AES)
#   walletd    WALLETD_IMAGE, port 24080, against X       -> format B (WalletGreen container)
#              (FX_WALLETD_DAEMON=cpp: against a C++ node following X, 24888 / 24889)
#
# Format A (crates/xkr-wallet-files/tests/data/backend/):
#   api-seed-subwallets.wallet  seed A, empty password; coinbase, one random + one imported
#                               subwallet, sends (to K, to its own subwallet, with a payment id)
#   api-keys.wallet             keys K (not deterministic), password "fixture"; received only
#   api-view.wallet             view wallet of A, password "pässwörd ✓"
#   api-created.wallet          /wallet/create, a 100-character password; nothing received
#   api-fresh-import.wallet     seed A imported at the top, never synced past it, password "fresh"
# Format B (crates/xkr-wallet-files/tests/data/green/):
#   walletd-one.wallet          keys K, empty password, as `-g` wrote it (never opened)
#   walletd-one-synced.wallet   the same after one walletd run (synced cache)
#   walletd-many.wallet         seed A, password walletd-pass: 2 random + 3 fixed-key addresses,
#                               sends; walletd-exported.wallet = its `export`
#   walletd-tracking.wallet     password "pässwörd ✓", only a tracking address (spend public key
#                               of K) after deleting the generated one
# Every file has a sidecar <file>.json {file, format, tool, image, imageId, password, description,
# expected}; expected values are raw (the files hold the real hashes and keys).
# Env: FX_IMAGE, WALLETD_IMAGE, FX_WALLETD_DAEMON, lib-wallet.sh knobs. Never builds anything.
# suite: expected=420 timeout=1800
set -euo pipefail
: "${TIMEOUT:=300}"
. "$(dirname "$0")/lib-wallet.sh"
fx_init "${1:-}" fx-wallet-files

OUT_A=$FX_OUT_ROOT/crates/xkr-wallet-files/tests/data/backend
OUT_B=$FX_OUT_ROOT/crates/xkr-wallet-files/tests/data/green
fx_require_image "$FX_IMAGE"
fx_require_image "$WALLETD_IMAGE"
jget() { jq -r "$1" <<< "$FX_BODY" 2> /dev/null || true; }

fx_image_report "$FX_IMAGE" "$SD/image-api.json"
if [ "$WALLETD_IMAGE" = "$FX_IMAGE" ]; then
    cp "$SD/image-api.json" "$SD/image-walletd.json"
else
    fx_image_report "$WALLETD_IMAGE" "$SD/image-walletd.json"
fi
if [ "$FX_WALLETD_DAEMON" = cpp ]; then fx_choose_follower; fi
fx_fresh_chain
if [ "$FX_WALLETD_DAEMON" = cpp ]; then fx_start_follower "$FX_FOLLOWER"; fi
mkdir -p "$SD/out-a" "$SD/out-b"

# sidecar <dir> <file> <format A|B> <tool> <image json> <password> <description> <expected json file>
sidecar() {
    local dir=$1 file=$2 fmt=$3 tool=$4 img=$5 pass=$6 desc=$7 exp=$8
    cp "$SD/w/$file" "$dir/$file"
    jq -n --arg file "$file" --arg fmt "$fmt" --arg tool "$tool" --slurpfile img "$img" \
        --arg pass "$pass" --arg desc "$desc" --slurpfile exp "$exp" \
        --arg size "$(wc -c < "$SD/w/$file")" \
        '{file: $file, format: $fmt, tool: $tool, image: $img[0].image, imageId: $img[0].id,
          imageCreated: $img[0].created, password: $pass, sizeBytes: ($size | tonumber),
          description: $desc, expected: $exp[0]}' > "$dir/$file.json"
    note "$file ($fmt, $(wc -c < "$SD/w/$file") bytes)"
}

# ------------------------------------------------------------------ format A: wallet-api

FX_JSONL=$SD/raw/setup.jsonl
fx_api_start "$C_FX_API" "$PORT_FX_API"
PW_LONG=$(printf 'fixture-password-%.0s' 1 2 3 4 5 6)
PW_LONG=${PW_LONG:0:100}
PW_UNI="pässwörd ✓"

fx_api "import/a" POST /wallet/import/seed "$(fx_wparams api-seed-subwallets.wallet "" "$(jq -cn --arg s "$FX_SEED_A" '{mnemonicSeed: $s, scanHeight: 0}')")"
[ "$FX_STATUS" = 200 ] || die setup "import of seed A failed: $FX_STATUS $FX_BODY"
A=$(fx_api_q GET /addresses/primary | jq -r '.address')
fx_api_q DELETE /wallet > /dev/null
fx_api "import/k" POST /wallet/import/key "$(fx_wparams api-keys.wallet fixture "$(jq -cn --arg v "$FX_K_VIEW" --arg s "$FX_K_SPEND" '{privateViewKey: $v, privateSpendKey: $s, scanHeight: 0}')")"
K=$(fx_api_q GET /addresses/primary | jq -r '.address')
fx_api_q DELETE /wallet > /dev/null
fx_api "import/view" POST /wallet/import/view "$(fx_wparams api-view.wallet "$PW_UNI" "$(jq -cn --arg v "$FX_A_VIEW" --arg a "$A" '{privateViewKey: $v, address: $a, scanHeight: 0}')")"
fx_api_q DELETE /wallet > /dev/null
[ -n "$A" ] && [ -n "$K" ] && [ "$A" != null ] && [ "$K" != null ] || die setup "addresses A [$A] K [$K]"

fx_mine "$A" "$FX_FUND_TO"
fx_mine "$FX_DUMMY_ADDR" "$FX_FILLER_TO"

fx_api "create" POST /wallet/create "$(fx_wparams api-created.wallet "$PW_LONG")"
fx_api_wait_synced
fx_api_dump "$SD/out-a/created.json"
fx_api_q DELETE /wallet > /dev/null
sidecar "$SD/out-a" api-created.wallet A wallet-api "$SD/image-api.json" "$PW_LONG" \
    "/wallet/create (random keys, deterministic view key), 100-character password, nothing received" "$SD/out-a/created.json"

fx_api "open/a" POST /wallet/open "$(fx_wparams api-seed-subwallets.wallet "")"
fx_api_wait_synced
fx_api "addresses/create" POST /addresses/create
fx_api "addresses/import-sub1" POST /addresses/import "$(jq -cn --arg k "$FX_SUB1" '{privateSpendKey: $k, scanHeight: 0}')"
SUB1=$(jget .address)
TXS=()
fx_api "send/k" POST /transactions/send/basic "$(jq -cn --arg d "$K" '{destination: $d, amount: 100000}')"
TXS+=("$(jget '.transactionHash // empty')")
fx_api "send/sub1" POST /transactions/send/advanced "$(jq -cn --arg d "$SUB1" '{destinations: [{address: $d, amount: 250000}]}')"
TXS+=("$(jget '.transactionHash // empty')")
fx_api "send/k-payment-id" POST /transactions/send/basic "$(jq -cn --arg d "$K" --arg p "$FX_PID1" '{destination: $d, amount: 300000, paymentID: $p}')"
TXS+=("$(jget '.transactionHash // empty')")
fx_confirm "${TXS[@]}"
fx_api_wait_synced
fx_api_dump "$SD/out-a/seed.json"
fx_api_q DELETE /wallet > /dev/null
sidecar "$SD/out-a" api-seed-subwallets.wallet A wallet-api "$SD/image-api.json" "" \
    "seed A (skulls woozy ...), empty password; blocks 1..$FX_FUND_TO coinbase, a random and an imported (fixed key sub1) subwallet, sends to K, to its own subwallet and with a payment id" \
    "$SD/out-a/seed.json"

for f in "api-keys.wallet|fixture|keys K (view key not derived from the spend key), password fixture; received from A|keys" \
    "api-view.wallet|$PW_UNI|view wallet of A (private spend key all zeros), UTF-8 password|view"; do
    IFS='|' read -r file pw desc tag <<< "$f"
    fx_api "open/$tag" POST /wallet/open "$(fx_wparams "$file" "$pw")"
    fx_api_wait_synced
    fx_api_dump "$SD/out-a/$tag.json"
    fx_api_q DELETE /wallet > /dev/null
    sidecar "$SD/out-a" "$file" A wallet-api "$SD/image-api.json" "$pw" "$desc" "$SD/out-a/$tag.json"
done

H=$(height_of "$FX_RPC")
fx_api "import/fresh" POST /wallet/import/seed "$(fx_wparams api-fresh-import.wallet fresh "$(jq -cn --arg s "$FX_SEED_A" --argjson h "$H" '{mnemonicSeed: $s, scanHeight: $h}')")"
fx_api_wait_synced
fx_api_dump "$SD/out-a/fresh.json"
fx_api_q DELETE /wallet > /dev/null
sidecar "$SD/out-a" api-fresh-import.wallet A wallet-api "$SD/image-api.json" fresh \
    "seed A imported with scanHeight $H (the top), password fresh: no transactions, sync state only" "$SD/out-a/fresh.json"
fx_stop_tool "$C_FX_API"

# ------------------------------------------------------------------ format B: kryptokrona-service

step "format B"
rc=$(fx_wd_generate "$SD/gen-one.txt" walletd-one.wallet "" --spend-key "$FX_K_SPEND" --view-key "$FX_K_VIEW")
[ -s "$SD/w/walletd-one.wallet" ] || die generate "walletd -g with keys K failed (exit $rc): $(tail -n 5 "$SD/gen-one.txt" | tr '\n' ' ')"
jq -n --arg a "$K" --arg s "$FX_K_SPEND" --arg v "$FX_K_VIEW" --arg p "$FX_K_SPEND_PUB" \
    '{addresses: [$a], viewSecretKey: $v, spendKeys: [{address: $a, keys: {spendSecretKey: $s, spendPublicKey: $p}}],
      note: "as written by kryptokrona-service -g: no transactions, empty cache"}' > "$SD/out-b/one.json"
sidecar "$SD/out-b" walletd-one.wallet B kryptokrona-service "$SD/image-walletd.json" "" \
    "keys K, empty password, generated with -g --spend-key --view-key and never opened" "$SD/out-b/one.json"

cp "$SD/w/walletd-one.wallet" "$SD/w/walletd-one-synced.wallet"
fx_wd_start "$C_FX_WD" "$PORT_FX_WALLETD" walletd-one-synced.wallet ""
fx_wd_wait_synced
fx_wd_dump "$SD/out-b/one-synced.json"
fx_stop_tool "$C_FX_WD"
sidecar "$SD/out-b" walletd-one-synced.wallet B kryptokrona-service "$SD/image-walletd.json" "" \
    "walletd-one.wallet after one walletd run (synced; received from A)" "$SD/out-b/one-synced.json"

rc=$(fx_wd_generate "$SD/gen-many.txt" walletd-many.wallet walletd-pass --mnemonic-seed "$FX_SEED_A")
[ -s "$SD/w/walletd-many.wallet" ] || die generate "walletd -g with seed A failed (exit $rc)"
fx_wd_start "$C_FX_WD" "$PORT_FX_WALLETD" walletd-many.wallet walletd-pass
fx_wd_wait_synced
fx_wd_q createAddress > /dev/null
fx_wd_q createAddress > /dev/null
fx_wd_q createAddress "$(jq -cn --arg k "$FX_SUB1" '{spendSecretKey: $k}')" > /dev/null
fx_wd_q createAddressList "$(jq -cn --arg a "$FX_SUB2" --arg b "$FX_SUB3" '{spendSecretKeys: [$a, $b], scanHeight: 0}')" > /dev/null
WSUB1=$(fx_wd_q getAddresses | jq -r '.addresses[3] // empty')
TXS=()
t=$(fx_wd_q sendTransaction "$(jq -cn --arg d "$K" --arg a "$A" '{transfers: [{address: $d, amount: 110000}], fee: 10, changeAddress: $a, anonymity: 3}')" | jq -r '.transactionHash // empty')
TXS+=("$t")
t=$(fx_wd_q sendTransaction "$(jq -cn --arg d "${WSUB1:-$K}" --arg a "$A" --arg p "$FX_PID2" '{transfers: [{address: $d, amount: 120000}], fee: 10, changeAddress: $a, paymentId: $p}')" | jq -r '.transactionHash // empty')
TXS+=("$t")
note "walletd sends: ${TXS[*]}"
fx_confirm "${TXS[@]}"
fx_wd_wait_synced
fx_wd_dump "$SD/out-b/many.json"
fx_wd_q export '{"fileName":"walletd-exported.wallet"}' > /dev/null
fx_stop_tool "$C_FX_WD"
sidecar "$SD/out-b" walletd-many.wallet B kryptokrona-service "$SD/image-walletd.json" walletd-pass \
    "seed A, password walletd-pass: primary, 2 random addresses, sub1, sub2, sub3 (fixed keys); coinbase and two sends (change to A)" \
    "$SD/out-b/many.json"
if [ -s "$SD/w/walletd-exported.wallet" ]; then
    sidecar "$SD/out-b" walletd-exported.wallet B kryptokrona-service "$SD/image-walletd.json" walletd-pass \
        "export of walletd-many.wallet (same keys and history)" "$SD/out-b/many.json"
else
    fail export "walletd export did not write walletd-exported.wallet"
fi

rc=$(fx_wd_generate "$SD/gen-tracking.txt" walletd-tracking.wallet "$PW_UNI")
[ -s "$SD/w/walletd-tracking.wallet" ] || die generate "walletd -g (random) failed (exit $rc)"
fx_wd_start "$C_FX_WD" "$PORT_FX_WALLETD" walletd-tracking.wallet "$PW_UNI"
R=$(fx_wd_q getAddresses | jq -r '.addresses[0] // empty')
d=$(fx_wd_q deleteAddress "$(jq -cn --arg a "$R" '{address: $a}')")
c=$(fx_wd_q createAddress "$(jq -cn --arg k "$FX_K_SPEND_PUB" '{spendPublicKey: $k, scanHeight: 0}')")
note "tracking: delete [$d] create [$c]"
fx_wd_wait_synced
fx_wd_dump "$SD/out-b/tracking.json"
fx_stop_tool "$C_FX_WD"
if jq -e '.address' <<< "$c" > /dev/null 2>&1; then
    sidecar "$SD/out-b" walletd-tracking.wallet B kryptokrona-service "$SD/image-walletd.json" "$PW_UNI" \
        "random container, its address deleted, then a tracking address (spend public key of K, random view key); UTF-8 password" \
        "$SD/out-b/tracking.json"
else
    fail tracking "walletd did not make a tracking-only container (delete: $d; create: $c)"
fi

# ------------------------------------------------------------------ publish

for d in "$OUT_A:api-" "$OUT_B:walletd-"; do
    dir=${d%%:*}
    mkdir -p "$dir"
    rm -f "$dir/${d#*:}"*
done
cp "$SD"/out-a/api-* "$OUT_A/"
cp "$SD"/out-b/walletd-* "$OUT_B/"
pass publish "$(ls "$OUT_A"/api-*.wallet | wc -l) format A and $(ls "$OUT_B"/walletd-*.wallet | wc -l) format B files with sidecars"
finish
