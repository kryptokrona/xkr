#!/usr/bin/env bash
# 50-storage: the chain database tools on the suite's chain (docs/storage.md, "Measuring").
#
#   <wd>/chain/xkrd/chain.redb  xkrd S's database from 10-14 (set up here if missing); S is
#   stopped for the tools. The round trip works on a copy in <wd>/50-storage/copy/.
#
# Checks: `db stats` on S's database (prints the file / raw and tables / raw ratios); a copy
# gets `db compact --force` (repack + compact), `db verify` (0 problems) and `db stats` again
# (same top, ratios before -> after); `db verify` on the original. With XKR_V1_DB=<schema v1
# chain.redb> (XKR_V1_NETWORK, default testnet) also `db convert` into a new v2 file + `db verify`
# + `db stats`; without it that check is a SKIP (no v1 database exists on a fresh testnet).
# suite: expected=30 timeout=900
set -euo pipefail
. "$(dirname "$0")/../lib.sh"
suite_init "${1:-}"

require_xkrd
DATA=$WD/chain/xkrd
DB=$DATA/chain.redb

if [ ! -f "$DB" ]; then
    note "no suite chain yet: setting it up (seed + S)"
    require_testwallet
    require_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE"
    ensure_suite_chain "$SD/xkrd-S.log"
    unwatch
fi
reclaim S

# stats_line <stats output>: "top T, file/raw X, tables/raw Y, B/block Z, file MB"
stats_line() {
    local f=$1 top ratio file
    top=$({ grep -o 'top Some([0-9]*)' "$f" || true; } | head -n 1 | tr -dc '0-9')
    ratio=$({ grep '^file / raw' "$f" || true; } | head -n 1 | tr -s ' ')
    file=$({ grep '^file size' "$f" || true; } | head -n 1 | awk '{print $3}')
    echo "top ${top:-?}, ${ratio:-no ratio line}, file ${file:-?} MB"
}
stats_top() { { grep -o 'top Some([0-9]*)' "$1" || true; } | head -n 1 | tr -dc '0-9'; }

# ------------------------------------------------------------------ 1. stats on S's database

if xkrd_db "$SD/stats-original.out" stats --network testnet --data-dir "$DATA" \
    && grep -q '^file / raw' "$SD/stats-original.out"; then
    pass 1 "db stats: $(stats_line "$SD/stats-original.out")"
else
    fail 1 "db stats failed: $(tail -n 3 "$SD/stats-original.out" | tr '\n' ' ')"
fi
TOP0=$(stats_top "$SD/stats-original.out")

# ------------------------------------------------------------------ 2. compact / verify round trip

mkdir -p "$SD/copy"
cp "$DB" "$SD/copy/chain.redb"
if xkrd_db "$SD/compact.out" compact --network testnet --db "$SD/copy/chain.redb" --force \
    && grep -q '^compacted:' "$SD/compact.out"; then
    pass 2 "db compact --force: $(grep '^compacted:' "$SD/compact.out" | tail -n 1); $(grep -c '^repacked' "$SD/compact.out" || true) tables repacked"
else
    fail 2 "db compact failed: $(tail -n 3 "$SD/compact.out" | tr '\n' ' ')"
fi
if xkrd_db "$SD/verify-copy.out" verify --network testnet --db "$SD/copy/chain.redb" \
    && grep -q ': 0 problems$' "$SD/verify-copy.out"; then
    pass 2 "db verify after compact: $(grep '^verified blocks' "$SD/verify-copy.out" | tail -n 1)"
else
    fail 2 "db verify after compact failed: $(tail -n 3 "$SD/verify-copy.out" | tr '\n' ' ')"
fi
if xkrd_db "$SD/stats-compacted.out" stats --network testnet --db "$SD/copy/chain.redb" \
    && [ -n "$TOP0" ] && [ "$(stats_top "$SD/stats-compacted.out")" = "$TOP0" ]; then
    pass 2 "db stats after compact: $(stats_line "$SD/stats-compacted.out") (before: $(stats_line "$SD/stats-original.out"))"
else
    fail 2 "db stats after compact: [$(stats_line "$SD/stats-compacted.out")], top before $TOP0"
fi
if xkrd_db "$SD/verify-original.out" verify --network testnet --data-dir "$DATA" \
    && grep -q ': 0 problems$' "$SD/verify-original.out"; then
    pass 2 "db verify on S's database: $(grep '^verified blocks' "$SD/verify-original.out" | tail -n 1)"
else
    fail 2 "db verify on S's database failed: $(tail -n 3 "$SD/verify-original.out" | tr '\n' ' ')"
fi

# ------------------------------------------------------------------ 3. convert v1 -> v2

if [ -z "${XKR_V1_DB:-}" ]; then
    skip 3 "db convert (set XKR_V1_DB to a schema v1 chain.redb to run it)"
elif [ ! -f "$XKR_V1_DB" ]; then
    fail 3 "XKR_V1_DB=$XKR_V1_DB does not exist"
else
    net=${XKR_V1_NETWORK:-testnet}
    mkdir -p "$SD/converted"
    if xkrd_db "$SD/convert.out" convert --from "$XKR_V1_DB" --network "$net" --db "$SD/converted/chain.redb" \
        && grep -q '^done in' "$SD/convert.out"; then
        pass 3 "db convert ($net): $(grep '^done in' "$SD/convert.out" | tail -n 1); $(stats_line "$SD/convert.out")"
    else
        fail 3 "db convert failed: $(tail -n 3 "$SD/convert.out" | tr '\n' ' ')"
    fi
    if [ -f "$SD/converted/chain.redb" ] \
        && xkrd_db "$SD/verify-converted.out" verify --network "$net" --db "$SD/converted/chain.redb" \
        && grep -q ': 0 problems$' "$SD/verify-converted.out"; then
        pass 3 "db verify on the converted database: $(grep '^verified blocks' "$SD/verify-converted.out" | tail -n 1)"
    else
        fail 3 "db verify on the converted database failed: $(tail -n 3 "$SD/verify-converted.out" 2>/dev/null | tr '\n' ' ')"
    fi
fi

echo "ratios:"
for f in stats-original stats-compacted; do
    printf '  %-16s %s\n' "$f" "$(stats_line "$SD/$f.out")"
done
finish
