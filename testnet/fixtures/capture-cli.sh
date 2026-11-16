#!/usr/bin/env bash
# capture-cli: transcripts and files of the C++ terminal wallets, driven through stdin.
#
#   xkrd X         fixture chain, P2P 24898 / RPC 24899 (own genesis, `xkr-testwallet mine` only)
#   xkrwallet      (zedwallet, format B) from CLI_IMAGE, -r 127.0.0.1:24899 (xkrd serves
#                  /queryblockslite; FX_WALLETD_DAEMON=cpp: a C++ node following X, 24889)
#   xkrwallet-beta (zedwallet++, format A) from the first of BETA_IMAGES (CLI_IMAGE,
#                  xkr-cpp-wallets:master, CPP_OLD_IMAGE) that has it (the published image does
#                  not ship it; none: SKIP), -r 127.0.0.1:24899
#
# `backup` (and transfer) ask "Confirm your current password:" (zedwallet++ command_implementations.cpp
# backup / utilities.cpp confirmPassword, zedwallet tools.cpp confirmPassword), so the password
# line follows them; a wallet that keeps prompting after the input ends is cut at FX_CLI_MAX_BYTES
# (1 MiB) and the session FAILs.
# Non-interactive use works: with stdin not a tty, linenoise reads plain lines and the password
# prompt reads one line without asking twice. The empty password is typed (an empty first line), not
# given as `-p ""`: the terminal wallets did not accept `-p ""` as the empty password in the first
# run ("Incorrect password!", then the next input lines were taken as passwords). Each session is one container fed line by line
# (FX_CLI_DELAY s apart, "@sleep N" pauses) with a hard limit (FX_CLI_TIMEOUT).
# Sessions: create, seed_restore (seed A, empty password), key_restore (keys K), view_wallet
# (A's view key), open with -w/-p (balance, address, backup, status, transfers lists, address
# book, integrated address, save, save_csv, help, advanced), transfer, after-transfer, wrong
# password then the right one.
#
# Output: crates/xkr-wallet-cli/tests/fixtures/cpp/<tool>-<session>.txt (normalised), exit codes,
#         <tool>-transactions.csv; crates/xkr-wallet-files/tests/data/green/xkrwallet-*.wallet and
#         backend/beta-*.wallet with sidecars; addressbook/<tool>-addressBook.json.
# Env: CLI_IMAGE, FX_CLI_DELAY, FX_CLI_TIMEOUT, FX_WALLETD_DAEMON, lib-wallet.sh knobs.
# Never builds anything.
# suite: expected=600 timeout=2400
set -euo pipefail
: "${TIMEOUT:=300}"
. "$(dirname "$0")/lib-wallet.sh"
fx_init "${1:-}" fx-cli

OUT_T=$FX_OUT_ROOT/crates/xkr-wallet-cli/tests/fixtures/cpp
OUT_A=$FX_OUT_ROOT/crates/xkr-wallet-files/tests/data/backend
OUT_B=$FX_OUT_ROOT/crates/xkr-wallet-files/tests/data/green
OUT_AB=$FX_OUT_ROOT/crates/xkr-wallet-files/tests/data/addressbook
fx_require_image "$CLI_IMAGE"
fx_image_report "$CLI_IMAGE" "$SD/image.json"
[ "$(jq -r '.files | index("xkrwallet") != null' "$SD/image.json")" = true ] \
    || missing_exit image "$CLI_IMAGE has no xkrwallet" "set CLI_IMAGE"
# xkrwallet-beta: from the first of BETA_IMAGES that has it
BETA_IMAGES=${BETA_IMAGES:-"$CLI_IMAGE $CPP_WALLETS_IMAGE $CPP_OLD_IMAGE"}
BETA_IMAGE=""
i=0
for img in $BETA_IMAGES; do
    i=$((i + 1))
    docker image inspect "$img" > /dev/null 2>&1 || continue
    if [ "$img" = "$CLI_IMAGE" ]; then rep=$SD/image.json; else rep=$SD/image-beta-$i.json; fx_image_report "$img" "$rep"; fi
    if [ "$(jq -r '.xkrwalletBeta' "$rep")" = true ]; then
        BETA_IMAGE=$img
        break
    fi
done
note "xkrwallet-beta: ${BETA_IMAGE:-not found in $BETA_IMAGES}"
if [ "$FX_WALLETD_DAEMON" = cpp ]; then fx_choose_follower; fi
fx_fresh_chain
if [ "$FX_WALLETD_DAEMON" = cpp ]; then fx_start_follower "$FX_FOLLOWER"; fi
mkdir -p "$SD/in" "$SD/tx" "$SD/side"
declare -A RC=()

# session <tool> <session> <binary> <image> <daemon port> [args...]; input in $SD/in/<tool>-<session>.in
session() {
    local tool=$1 s=$2 bin=$3 img=$4 port=$5
    shift 5
    RC[$tool-$s]=$(fx_cli_session "$SD/tx/$tool-$s.txt" "$img" "$bin" "$SD/in/$tool-$s.in" -r "127.0.0.1:$port" "$@")
    if [ "${RC[$tool-$s]}" = truncated ]; then
        fail "$tool-$s" "transcript cut at $FX_CLI_MAX_BYTES bytes: the wallet kept prompting after the input ended ($SD/tx/$tool-$s.txt)"
    fi
    note "$tool $s: exit ${RC[$tool-$s]}, $(wc -l < "$SD/tx/$tool-$s.txt") lines"
}
addr_in() { # first standard address in a transcript (colour codes removed)
    sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$1" | { grep -aoE 'SEKR[1-9A-HJ-NP-Za-km-z]{95}' || true; } | grep -v "^$FX_DUMMY_ADDR\$" | head -n 1
}

# run_tool <tool> <binary> <image> <daemon port>
run_tool() {
    local t=$1 bin=$2 img=$3 port=$4 a
    step "$t ($bin from $img, daemon port $port)"
    printf '%s\n' create "$t-created" cli-pass "@sleep 8" address backup cli-pass balance status exit > "$SD/in/$t-create.in"
    session "$t" create "$bin" "$img" "$port"
    printf '%s\n' seed_restore "$FX_SEED_A" "$t-seed" "" "" "@sleep 10" address exit > "$SD/in/$t-seed-restore.in"
    session "$t" seed-restore "$bin" "$img" "$port"
    a=$(addr_in "$SD/tx/$t-seed-restore.txt")
    [ -n "$a" ] || die "$t" "no address in the seed_restore transcript $SD/tx/$t-seed-restore.txt"
    if [ -n "${A:-}" ] && [ "$a" != "$A" ]; then fail "$t" "seed_restore gave $a, expected $A"; fi
    A=$a
    fx_keep "$A" "$(fx_alt_address "$A")"
    printf '%s\n' key_restore "$FX_K_SPEND" "$FX_K_VIEW" "$t-keys" cli-pass-2 "" "@sleep 8" address backup cli-pass-2 exit > "$SD/in/$t-key-restore.in"
    session "$t" key-restore "$bin" "$img" "$port"
    K=$(addr_in "$SD/tx/$t-key-restore.txt")
    [ -z "$K" ] || fx_keep "$K" "$(fx_alt_address "$K")"
    if [ "$(height_of "$FX_RPC")" -lt "$FX_FILLER_TO" ]; then
        fx_mine "$A" "$FX_FUND_TO"
        fx_mine "$FX_DUMMY_ADDR" "$FX_FILLER_TO"
        fx_wait_follower
    fi
    printf '%s\n' view_wallet y "$FX_A_VIEW" "$A" "$t-view" view-pass "" "@sleep 15" balance exit > "$SD/in/$t-view.in"
    session "$t" view "$bin" "$img" "$port"
    printf '%s\n' "" "@sleep 15" balance address backup "" status list_transfers incoming_transfers outgoing_transfers \
        ab_add friend-one "${K:-$FX_DUMMY_ADDR}" "" ab_add friend-pid "${K:-$FX_DUMMY_ADDR}" "$FX_PID1" ab_list \
        make_integrated_address "$A" "$FX_PID1" save save_csv help advanced exit > "$SD/in/$t-open.in"
    session "$t" open "$bin" "$img" "$port" -w "$t-seed.wallet"
    # transfer prompts: xkrwallet (zedwallet transfer.cpp): address, payment id, amount, fee,
    # "Is this correct?", password; xkrwallet-beta (zedwallet++ transfer.cpp): no fee prompt
    if [ "$bin" = xkrwallet-beta ]; then
        printf '%s\n' "" "@sleep 15" transfer "${K:-$FX_DUMMY_ADDR}" "" 1 y "" "@sleep 15" balance exit > "$SD/in/$t-transfer.in"
    else
        printf '%s\n' "" "@sleep 15" transfer "${K:-$FX_DUMMY_ADDR}" "" 1 "" y "" "@sleep 15" balance exit > "$SD/in/$t-transfer.in"
    fi
    session "$t" transfer "$bin" "$img" "$port" -w "$t-seed.wallet"
    fx_confirm
    printf '%s\n' "" "@sleep 15" balance outgoing_transfers list_transfers exit > "$SD/in/$t-after-transfer.in"
    session "$t" after-transfer "$bin" "$img" "$port" -w "$t-seed.wallet"
    printf '%s\n' "" "@sleep 15" address exit > "$SD/in/$t-wrong-password.in"
    session "$t" wrong-password "$bin" "$img" "$port" -w "$t-seed.wallet" -p wrong
    if grep -aq "Incorrect password" "$SD/tx/$t-wrong-password.txt"; then
        pass "$t" "wrong password reported, then the empty password opened the wallet"
    else
        note "NOTE: no \"Incorrect password\" line in $t-wrong-password.txt"
    fi
    # keep this tool's address book and CSV apart from the next tool's
    [ -f "$SD/w/.addressBook.json" ] && mv "$SD/w/.addressBook.json" "$SD/side/$t-addressBook.json"
    [ -f "$SD/w/transactions.csv" ] && mv "$SD/w/transactions.csv" "$SD/side/$t-transactions.csv"
    return 0
}

# sidecar <dir> <file> <format> <tool> <image> <password> <description> <expected json>
sidecar() {
    [ -s "$SD/w/$2" ] || { fail files "$2 was not written"; return 0; }
    cp "$SD/w/$2" "$1/$2"
    jq -n --arg file "$2" --arg fmt "$3" --arg tool "$4" --arg image "$5" \
        --arg id "$(docker image inspect -f '{{.Id}}' "$5" 2> /dev/null || true)" --arg pass "$6" \
        --arg desc "$7" --argjson exp "$8" \
        '{file: $file, format: $fmt, tool: $tool, image: $image, imageId: $id, password: $pass,
          description: $desc, expected: $exp}' > "$1/$2.json"
}

A=""
run_tool xkrwallet xkrwallet "$CLI_IMAGE" "$(fx_walletd_daemon_port)"
if [ -n "$BETA_IMAGE" ]; then
    run_tool beta xkrwallet-beta "$BETA_IMAGE" "$PORT_FX_RPC"
else
    skip beta "xkrwallet-beta is in none of $BETA_IMAGES (format A files come from capture-wallet-files.sh via wallet-api, the same WalletBackend code)"
fi

# ------------------------------------------------------------------ publish

mkdir -p "$OUT_T" "$OUT_A" "$OUT_B" "$OUT_AB"
rm -f "$OUT_T"/xkrwallet-* "$OUT_T"/beta-* "$OUT_B"/xkrwallet-* "$OUT_A"/beta-* "$OUT_AB"/xkrwallet-* "$OUT_AB"/beta-*
for f in "$SD"/tx/*.txt; do
    if [ "$(wc -c < "$f")" -gt $((FX_CLI_MAX_BYTES + 512)) ]; then
        head -c "$FX_CLI_MAX_BYTES" "$f" > "$f.cut"
        printf '\n%s\n' "$FX_CLI_TRUNCATED" >> "$f.cut"
        mv "$f.cut" "$f"
        fail publish "$(basename "$f") was over $FX_CLI_MAX_BYTES bytes; truncated"
    fi
    fx_normalise text "$f" "$OUT_T/$(basename "$f")"
done
for f in "$SD"/in/*.in; do
    fx_normalise text "$f" "$OUT_T/$(basename "$f")"
done
for k in "${!RC[@]}"; do printf '%s\t%s\n' "$k" "${RC[$k]}"; done | sort \
    | jq -Rn '[inputs | split("\t") | {key: .[0], value: (.[1] | tonumber? // .)}] | from_entries' > "$OUT_T/exit-codes.json"
for f in "$SD"/side/*; do
    [ -e "$f" ] || continue
    case $f in
        *.csv) fx_normalise text "$f" "$OUT_T/$(basename "$f")" ;;
        *) cp "$f" "$OUT_AB/$(basename "$f")" ;;
    esac
done
KEYS_A=$(jq -cn --arg a "$A" --arg s "$FX_A_SPEND" --arg v "$FX_A_VIEW" --arg m "$FX_SEED_A" \
    '{address: $a, privateSpendKey: $s, privateViewKey: $v, mnemonicSeed: $m}')
KEYS_K=$(jq -cn --arg a "${K:-}" --arg s "$FX_K_SPEND" --arg v "$FX_K_VIEW" --arg p "$FX_K_SPEND_PUB" \
    '{address: $a, privateSpendKey: $s, privateViewKey: $v, publicSpendKey: $p}')
KEYS_V=$(jq -cn --arg a "$A" --arg v "$FX_A_VIEW" '{address: $a, privateViewKey: $v, viewOnly: true}')
for t in xkrwallet beta; do
    if [ "$t" = xkrwallet ]; then dir=$OUT_B fmt=B img=$CLI_IMAGE; else dir=$OUT_A fmt=A img=$BETA_IMAGE; fi
    [ -n "$img" ] || continue
    [ -s "$SD/w/$t-seed.wallet" ] || [ -s "$SD/w/$t-keys.wallet" ] || continue
    sidecar "$dir" "$t-seed.wallet" "$fmt" "$t" "$img" "" "seed_restore of seed A, empty password; coinbase, one transfer of 1 XKR to K" "$KEYS_A"
    sidecar "$dir" "$t-keys.wallet" "$fmt" "$t" "$img" cli-pass-2 "key_restore of keys K (not deterministic)" "$KEYS_K"
    sidecar "$dir" "$t-view.wallet" "$fmt" "$t" "$img" view-pass "view_wallet of A" "$KEYS_V"
    sidecar "$dir" "$t-created.wallet" "$fmt" "$t" "$img" cli-pass "create (random keys; see $t-create.txt for the printed keys, normalised)" \
        "$(jq -cn --arg a "$(addr_in "$SD/tx/$t-create.txt")" '{address: $a}')"
done
pass publish "$(ls "$OUT_T" | wc -l) transcript files, wallet files with sidecars, address books: $(ls "$OUT_AB" 2> /dev/null | tr '\n' ' ')"
finish
