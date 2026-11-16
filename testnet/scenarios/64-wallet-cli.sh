#!/usr/bin/env bash
# 64-wallet-cli: the Rust terminal wallet xkrwallet in script mode (no terminal) against xkrd: restore, send to an Xkr address, txs, export, back, exit; it opens C++ xkrwallet and xkrwallet-beta files.
#
#   xkrd X       own testnet chain from genesis, P2P 26498 / RPC 26499, mined only with
#                `xkr-testwallet mine`
#   Rust         target/release/xkrwallet -r 127.0.0.1:26499 --network testnet, stdin a script
#                fed line by line (FX_CLI_DELAY s apart), working directory <sd>/w
#   C++          xkrwallet and xkrwallet-beta from WL_IMAGE (xkr-cpp-wallets:master, -O2
#                reference build), one-off containers; without the image the Rust part still runs
#                and the C++ part is reported MISSING
#
# Checks:
#   cpp-files    (C++) xkrwallet and xkrwallet-beta restore seed A into xkrwallet-seed.wallet
#                (container) and beta-seed.wallet (WalletBackend file) while the chain is short
#   restore      `restore` of seed A into a.wallet: "✔ Restored a.wallet · <A short>", synced
#   send         `send` to the Xkr form of a foreign address, no payment ID, 1 XKR, confirmed with
#                the password: "✔ Sent · hash <64 hex>", the transaction reaches X's pool
#   txs          `txs` lists the transactions ("N transactions")
#   export       `export` writes transactions.csv with the C++ header and the incoming lines
#   back         `back` closes ("Closing the wallet", "Wallet saved · goodbye") and the start menu
#                comes back; `open a` reopens it; `exit` ends with status 0 and "Thanks for
#                stopping by..."
#   reopen       after the send is mined: -w a.wallet -p <password>; `txs out` shows -1.0001 XKR,
#                `export` has the OUT line of the send, `balance` is read
#   open-cpp-*   (C++) -w xkrwallet-seed.wallet -p "" and -w beta-seed.wallet -p "": opened for A
#                (the container keeps its bytes and gets .xkrstate next to it; the WalletBackend
#                file is reported as a wallet file), `address` prints A and its Xkr form, the
#                balance equals a.wallet's, exit 0
#
# Env: WL_IMAGE, XKR_WALLET_CLI, FX_CLI_DELAY (1.5), FX_CLI_TIMEOUT (240), FX_CLI_MAX_BYTES (1 MiB),
# FX_CONFIRM (12), WALLET_LABEL, TIMEOUT, KEEP=1.
# suite: expected=300 timeout=1500
set -euo pipefail
: "${TIMEOUT:=300}"
. "$(dirname "$0")/../fixtures/lib-wallet.sh"
wl_init "${1:-}" "$PORT_W64_P2P" "$PORT_W64_RPC"
require_wallet_cli
CPP=1
if ! cpp_wallets_have xkrwallet xkrwallet-beta; then
    CPP=0
    missing cpp "$WL_IMAGE with xkrwallet and xkrwallet-beta not found; the C++ files part does not run" "$BUILD_CPP_WALLETS"
fi

PW=$WL_PASS
A_SHORT=""
D_XKR=$(fx_alt_address "$FX_DUMMY_ADDR")
[ -n "$D_XKR" ] || die setup "no Xkr form of $FX_DUMMY_ADDR"
# has <check> <transcript> <fixed text> <what>: PASS when the transcript has the text
has() {
    if grep -aqF -- "$3" "$2"; then pass "$1" "$4"; else fail "$1" "$4: \"$3\" not in $2"; fi
}
# session <name> <args...>: runs $SD/in/<name>.in, transcript $SD/tx/<name>.txt; sets RC
session() {
    local n=$1
    shift
    RC=$(rcli_session "$SD/tx/$n.txt" "$SD/in/$n.in" "$@")
    [ "$RC" = truncated ] && fail "$n" "transcript cut at $FX_CLI_MAX_BYTES bytes: the wallet kept prompting ($SD/tx/$n.txt)"
    note "xkrwallet $n: exit $RC, $(wc -l < "$SD/tx/$n.txt") lines"
}

fx_fresh_chain
fx_mine "$FX_DUMMY_ADDR" 5

# ------------------------------------------------------------------ C++ files while the chain is short

declare -A CFILE=([xw]=xkrwallet-seed.wallet [beta]=beta-seed.wallet)
if [ "$CPP" = 1 ]; then
    for k in xw beta; do
        bin=xkrwallet
        [ "$k" = beta ] && bin=xkrwallet-beta
        wl_script "$SD/in/cpp-$k.in" seed_restore "$FX_SEED_A" "${CFILE[$k]%.wallet}" "" "" "@sleep 10" address exit
        rc=$(cpp_cli_session "$SD/tx/cpp-$k.txt" "$bin" "$SD/in/cpp-$k.in")
        if [ -s "$SD/w/${CFILE[$k]}" ]; then
            pass cpp-files "$bin seed_restore wrote ${CFILE[$k]} (exit $rc)"
        else
            fail cpp-files "$bin seed_restore (exit $rc) wrote no ${CFILE[$k]} ($SD/tx/cpp-$k.txt)"
        fi
    done
fi

# ------------------------------------------------------------------ restore, send, txs, export, back, exit

# the address of seed A, from the restore below; the chain pays it once known
wl_script "$SD/in/probe.in" restore "$FX_SEED_A" probe "$PW" "" address exit
session probe
A=$(sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$SD/tx/probe.txt" | { grep -aoE '^  SEKR[1-9A-HJ-NP-Za-km-z]{95}$' || true; } | head -n 1 | tr -d ' ')
[ "${#A}" = 99 ] || die restore "no address of seed A in $SD/tx/probe.txt (exit $RC)"
A_SHORT=$(wl_short "$A")
note "A = $A"
fx_mine "$A" 35
fx_mine "$FX_DUMMY_ADDR" 140

wl_script "$SD/in/main.in" \
    restore "$FX_SEED_A" a "$PW" "" \
    balance \
    send "$D_XKR" "" 1 y "$PW" \
    "@sleep 2" \
    txs \
    export \
    back \
    open a "$PW" \
    exit
session main
T=$SD/tx/main.txt
has restore "$T" "  ✔ Restored a.wallet · $A_SHORT" "restore of seed A into a.wallet"
has restore "$T" "Sync … done " "the restored wallet synced"
HASH=$(sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$T" | { grep -aoE '✔ Sent · hash [0-9a-f]{64}' || true; } | head -n 1 | awk '{print $NF}')
if [ -n "$HASH" ]; then
    pass send "sent 1 XKR to the Xkr form $(wl_short "$D_XKR"): $HASH"
    if tx_in_pool "$FX_RPC" "$HASH" || tx_in_block "$FX_RPC" "$HASH"; then
        pass send "the transaction reached X"
    else
        fail send "$HASH is not known to X: $(flat "$(tx_status "$FX_RPC" "$HASH")" | cut -c1-200)"
    fi
else
    fail send "no \"✔ Sent · hash\" line in $T: $(grep -a -m 3 -E '✗|Not sent|Cancelled' "$T" | tr '\n' ' ')"
fi
if grep -aqE '^  [0-9]+ transactions$' "$T"; then
    pass txs "txs: $(grep -aE '^  [0-9]+ transactions$' "$T" | head -n 1 | sed 's/^ *//')"
else
    fail txs "no \"N transactions\" line after txs in $T"
fi
CSV=$SD/w/transactions.csv
if [ -s "$CSV" ] && head -n 1 "$CSV" | grep -qx 'Timestamp,Block Height,Hash,Amount,In/Out' && grep -q ',IN$' "$CSV"; then
    pass export "transactions.csv: the C++ header and $(grep -c ',IN$' "$CSV") IN lines"
else
    fail export "transactions.csv: $(head -n 2 "$CSV" 2> /dev/null | tr '\n' ' ')"
fi
has back "$T" "Closing the wallet" "back: closing steps"
has back "$T" "Wallet saved · goodbye" "back: the wallet was saved"
if [ "$(grep -acF 'Welcome to your XKR wallet' "$T")" -ge 2 ]; then
    pass back "the start menu came back after back"
else
    fail back "the start menu was shown $(grep -acF 'Welcome to your XKR wallet' "$T") time(s)"
fi
has back "$T" "  ✔ Opened a.wallet · $A_SHORT" "open a from the start menu"
if [ "$RC" = 0 ] && grep -aqF "Thanks for stopping by..." "$T"; then
    pass exit "exit: status 0, \"Thanks for stopping by...\""
else
    fail exit "exit status [$RC], last lines: $(tail -n 3 "$T" | tr '\n' ' ')"
fi

# ------------------------------------------------------------------ reopen after the send is mined

[ -n "$HASH" ] && fx_confirm "$HASH"
rm -f "$CSV"
wl_script "$SD/in/reopen.in" "txs out" export balance exit
session reopen -w a.wallet -p "$PW"
T=$SD/tx/reopen.txt
has reopen "$T" "  ✔ Opened a.wallet · $A_SHORT" "-w a.wallet -p <password> opened it"
has reopen "$T" " · -1.0001 XKR · " "txs out lists the send with its fee"
if [ -n "$HASH" ] && grep -qF ",$HASH,1.00010,OUT" "$CSV" 2> /dev/null; then
    pass reopen "export has the OUT line of $HASH"
else
    fail reopen "transactions.csv has no OUT line for [$HASH]: $(grep ',OUT$' "$CSV" 2> /dev/null | head -n 2 | tr '\n' ' ')"
fi
TOTAL=$(rcli_total "$T")
[[ $TOTAL =~ ^[0-9]+$ ]] && pass reopen "balance $TOTAL" || fail reopen "balance: $TOTAL"
[ "$RC" = 0 ] || fail reopen "exit status $RC"

# ------------------------------------------------------------------ the C++ files

if [ "$CPP" = 1 ]; then
    for k in xw beta; do
        f=${CFILE[$k]}
        [ -s "$SD/w/$f" ] || continue
        sha0=$(wl_sha "$SD/w/$f")
        wl_script "$SD/in/open-$k.in" address balance exit
        session "open-$k" -w "$f" -p ""
        T=$SD/tx/open-$k.txt
        has "open-cpp-$k" "$T" "  ✔ Opened $f · $A_SHORT" "xkrwallet opened the C++ $f for A"
        has "open-cpp-$k" "$T" "  $A" "address prints A"
        has "open-cpp-$k" "$T" "  $(fx_alt_address "$A")" "address prints A's Xkr form"
        if [ "$k" = xw ]; then
            has "open-cpp-$k" "$T" "($f, container; state in $f.xkrstate)" "reported as a container with its state file"
            if [ "$(wl_sha "$SD/w/$f")" = "$sha0" ] && [ -s "$SD/w/$f.xkrstate" ]; then
                pass "open-cpp-$k" "the container kept its bytes; $f.xkrstate written"
            else
                fail "open-cpp-$k" "container sha $(wl_sha "$SD/w/$f" | cut -c1-12) (before ${sha0:0:12}), state file $([ -s "$SD/w/$f.xkrstate" ] && echo present || echo missing)"
            fi
        else
            has "open-cpp-$k" "$T" "($f, wallet file)" "reported as a wallet file"
        fi
        t=$(rcli_total "$T")
        if [ "$RC" = 0 ] && [ "$t" = "$TOTAL" ]; then
            pass "open-cpp-$k" "balance $t, the same as a.wallet; exit 0"
        else
            fail "open-cpp-$k" "exit [$RC], balance [$t], a.wallet $TOTAL"
        fi
    done
fi
finish
