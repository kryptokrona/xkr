# Mainnet fixtures

Fetched 2026-11-12 from the public node `http://node.myxkr.com:11898` (v1.2.2, height ~2 617 470).

| File | Content | Used by |
|---|---|---|
| `blocks.json` | 39 blocks: 0-10, 1000, 5052, 5053, 187000/1/14, 440000/1, 620000/1, 620106, 700001/3, 800001/6, 1250000/13, 1300000/1, 1750000/3, 2200000/1/6/11, 2617000/19/400. Per block: raw blob (`/getblocks`), transaction blobs (`/gettransactions`), header fields (`getblockheaderbyheight`) and `f_block_json` statistics (size median, cumulative size, already generated coins of the block and its parent, fees). | `xkr-serialization`, `xkr-consensus` |
| `../../../xkr-consensus/tests/fixtures/headers.json` | ~1400 headers (timestamp, difficulty, version, ...) in ranges around every difficulty switch | `xkr-consensus` |
| `../../../xkr-consensus/tests/fixtures/rings.json` | ring-member output keys for all 8 transactions of blocks 5052/5053 (`/queryblocksdetailed` over blocks 0-5060) and for the amount-7 000 000 000 input of tx `26c73013...` in block 2 617 400 (`/getrandom_outs` with `outs_count = 65535` returns every output of a rare amount) | `xkr-consensus` |

`tools/` holds the scripts that produced and independently cross-checked these files (run with
`python3 -I`): `fetch.py` (blocks + headers), `outs_early.py` (output keys of the early chain),
`ref.py` (Python reference of the C++ serialization, tree hash and block hash: all 39 block hashes,
all tx hashes and cumulative sizes match the node), `ed.py` + `verify_rings.py` (Python ref10
`ge_frombytes_vartime` / `ge_fromfe_frombytes_vartime` / `checkRingSignature`: all 564 fixture rings
verify), `diff.py` (all difficulty algorithms: 1406 of 1406 headers reproduced).

Note: `/getblocks` serializes `RawBlock` with repeated JSON keys, so only the last transaction blob
of a block survives in its JSON; transaction blobs therefore come from `/gettransactions`.
