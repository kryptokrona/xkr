# xkr-storage: chain database (schema v2)

Status: schema v2 (size redesign). Code: `crates/xkr-storage`. Engine: redb 4.5 (pure Rust,
copy-on-write B-trees, ACID, MVCC readers, one writer). Schema v1 (the first layout) is refused on open
(`schema_version` 1 → resync); `xkrd db stats` still reads v1 files for before/after numbers.

## Why v1 was 2.4× the raw chain

The first mainnet sync measured an 11–13 GB `chain.redb` at height ~815 000, 2.39× the raw
chain (sum of `block_size`, ~5.4 GB). Raw XKR blocks of the busy eras are dominated by
many-input transactions (fusion and consolidation, 10–94 inputs with rings of 4 or 8): in the
39 mainnet fixture blocks, 389 KB of 482 KB are ring signatures. The causes, checked against
the redb 4.5 source:

| cause | where in redb | v1 effect |
|---|---|---|
| **Power-of-two page runs.** A leaf holding one value larger than a page is allocated as an order-k buddy block (8, 16, 32 KB …). | `LeafBuilder::build` → `allocate(required_bytes)` | Signatures (`tx_prunable`, 2–27 KB per tx) and big prefixes lose 30–40 % to rounding. |
| **Half-full leaves.** A full leaf splits evenly; only an insert past the global right edge starts a fresh leaf (`rightmost` fast path). | `btree_mutator::insert_helper`, `build_split` | `outputs` grows at one point *per amount* (~150 append points in the middle of the tree): each split strands a half-full page. Hash-keyed tables fill ~69 %. |
| **Duplicate and wide rows.** | — | Every output key stored twice (prefix blob + `outputs`), `OutputInfo` 88 B with the 32-byte `tx_hash`, 16-byte keys, 104-byte `BlockInfo`, `tx_output_indexes` per tx. At ~50 % fill an output cost ~240 B against 38 B on the wire. |
| **Per-blob zstd** without a dictionary on 0.3–4 KB blobs whose content is mostly keys. | — | Little gain; the frame header eats part of it. |
| Free pages under `Durability::None` | `non_durable_commit`, `process_freed_pages_nondurable` | **Not a main cause.** Pages allocated and freed between durable commits are reused at once; pages of the durable state freed by a non-durable commit become reusable at the next durable commit (at most 5 s later). The file never shrinks by itself, which `compact` fixes. |

## Principles (v2)

- **Fixed-width keys and values** wherever possible: a table whose key and value are both
  fixed-width has no per-entry length fields in redb's leaves. Heights are `u32` in keys and
  rows, global output indexes `u32` (as on the wire).
- **Every byte once.** Output keys live only in `outputs` (ring members need them there);
  blobs are stored *key-stripped* and the keys are put back on read. No `tx_hash` per output;
  payment-id rows point at `(height, tx_index)` and the hash is read from the block blob.
- **Big values become full pages.** Blocks are stored as *records*; 64 consecutive records are
  zstd-compressed together (one *chunk*) and the compressed bytes are split into 4 064-byte
  *parts*, so every part fills exactly one 4 KB page (`4 + 4 + 8 + 4064 = 4080 ≤ 4096`).
  Signatures get their own parts (same split), so pruning deletes whole parts.
- **Appends at the right edge.** All height-keyed and chunk-keyed tables only grow at their
  end (redb packs those leaves full). `outputs` is written with one cursor run per amount per
  batch (redb rebuilds the leaf at the gap packed greedily), and maintenance repacks the
  middle-insert tables in key order after the initial sync.
- **One write transaction per verified batch**, readers on MVCC snapshots (unchanged).
- **No decoding or hashing in storage.** The node passes `VerifiedBlock`s with the decoded
  block and transactions. The only "parsing" is the key-stripping walk over the outputs section
  (`varint amount | 0x02 | key[32]`), which is verified byte for byte while stripping.

## Tables

| table | key | value | v1 equivalent |
|---|---|---|---|
| `block_infos` | `height u32` | `BlockInfoRow` 88 B {hash, timestamp, difficulty, cumulative difficulty u64, coins, cumulative tx count, cumulative size, block size u32, tx count u32} | 104-B `BlockInfo` (128-bit cumulative difficulty, flags) |
| `block_hash_to_height` | `[u8;32]` | `u32` | value `u64` |
| `hot_blocks` | `height u32` | block record, uncompressed | `block_blobs` + `tx_blobs` + `tx_output_indexes` |
| `hot_sigs` | `height u32` | the block's signatures, concatenated | `tx_prunable` (per tx) |
| `body_parts` | `chunk << 16 \| part u64` | ≤ 4 064 B of a framed zstd chunk of 64 records | — |
| `sig_parts` | `chunk << 16 \| part u64` | ≤ 4 064 B of the chunk's signature stream (raw) | — |
| `tx_hash_to_loc` | `[u8;32]` | `TxLocRow` 8 B {height u32, tx_index u32} | 16-B `TxLoc` (prefix length) |
| `key_images` | `[u8;32]` | `()` | value `u64` height |
| `outputs` | `(amount u64, global_index u32)` | `OutputRow` 40 B {key, height u32, unlock u32} | key 16 B, 88-B `OutputInfo` (tx hash, output index, tx index, u64s) |
| `unlock_overflow` | `(amount u64, global_index u32)` | `u64` | — (rare `unlock_time ≥ u32::MAX`; `OutputRow.unlock = u32::MAX` points here) |
| `output_counts` | `amount u64` | `u64` | same |
| `payment_ids` | `([u8;32], height u32, tx_index u32)` | `()` | value = tx hash (32 B) |
| `timestamps` | `(timestamp u64, height u32)` | `()` | `(u64, u64)` |
| `alt_blocks`, `alt_block_heights`, `meta`, `meta_blobs` | | unchanged | |

`meta`: `schema_version` = 2, `top_height`, `sealed_below` (a multiple of 64), `pruned_below`
(a multiple of 4 096 with stripe pruning, of 64 otherwise), `pruning_seed` (absent = 0, added
to schema 2 without a version bump), `seal_lag` (default 1 024). `meta_blobs`:
`zstd_dictionary`, `zstd_dictionary_prev` (only while a re-compression runs), `state_hashes`
(64 B per stable span from span 0: the state hash, then the hash of the span's last block; see
"State hashes"; added to schema 2 without a version bump, absent = none computed).

### Records and chunks (`codec.rs`)

A **block record** is `version | blob | miner outputs offset | miner global indexes | per tx:
prefix | outputs offset | global indexes | signature length` (LEB128 varints). The block blob
and each prefix are **key-stripped**: the 32 key bytes of every output are cut out and the
record keeps `1 + offset of the first output` (0 = stored as is; never happens for valid
transactions, see `real_blobs_are_strippable`). Reading walks the outputs section, reads each
amount and puts `outputs[(amount, global_index)].key` back, so `/getrawblocks`, P2P
`GET_OBJECTS` and `getwalletsyncdata` get byte-identical blobs. Global indexes, which v1 kept
in `tx_output_indexes`, are part of the record (and compress with it).

A **chunk** is the 64 records of heights `64c .. 64c+63`: `version | n | lengths | records`,
framed and compressed by `Compressor::compress_chunk` (zstd level 9, with the database's
dictionary if any), split into parts. Its **signature stream** is the concatenation of the
blocks' signatures in height and transaction order; a transaction's offset follows from the
records' signature lengths, so one transaction's signatures are read part-exactly.

**Sealing.** New blocks go to `hot_blocks`/`hot_sigs`. When a push leaves chunk `c` at least
`seal_lag` blocks below the tip, its 64 hot records are compressed into `body_parts`, the
signatures moved to `sig_parts` and the hot rows deleted, in the same write transaction.
`pop_blocks` below `sealed_below` unseals the last chunk first (the default seal lag 1 024 is
above the node's 1 000-block reorg limit, `--max-reorg-depth`, so this does not happen in
practice; a reorg is refused below a checkpoint the chain has reached and when it would pop a
pruned block with transactions). Readers cache
the last decoded chunk in their `Compressor`, tagged with the chunk's last block hash, so
walking a chunk block by block decompresses it once and a re-sealed chunk never hits a stale
cache.

### Blob framing (`compress.rs`)

`tag u8 | uncompressed_len u32 LE | [dict_id u32 LE] | payload`; tag 0 = raw, 1 = zstd,
2 = zstd with dictionary. A `Compressor` writes with one dictionary and can decode several
(`add_decoder_dictionary`), which keeps a database readable while it is re-compressed.

### Dictionary

`xkrd db train-dict` trains over records spread evenly over the chain (the bytes chunks are
made of), stores the dictionary, re-compresses every chunk and compacts. A chunk already gives
zstd 64 blocks of context, so expect a few percent on `body_parts`, not the 10–25 % a dictionary
gives single small blobs. For deterministic fresh syncs the dictionary must ship with the
binary: `crates/xkr-storage/dicts/mainnet.zdict` is compiled in (`builtin_dictionary`), and a
fresh database installs it before its first chunk is sealed (`dicts/README.md`). The file is
empty until a dictionary has been trained on a full mainnet sync.

## Stripe pruning

Ring signatures (64 B per ring member per input) are only needed to verify a transaction once,
but a syncing node needs *someone* to serve them. Like Monero, a pruning node keeps the
signatures of one *stripe* of the old chain and drops the others; nodes with different stripes
together hold everything, and full nodes hold it all. Code: `crates/xkr-storage/src/pruning.rs`
(stripe math), `ops::prune_stripe` / `ops::prune_all_below` (deletion), `Store::prune`
(stepped), `xkr_chain::Checkpoints::prune_bound` (safety bound).

**Stripes.** The chain is cut into spans of `STRIPE_SPAN` = 4 096 blocks, exactly 64 sealed
chunks, so a chunk never straddles two stripes and pruning deletes whole signature parts (no
partial pages). Height `h` belongs to stripe `(h / 4096) % stripes + 1`.

**Seed.** `PruningSeed` = `stripes << 8 | stripe` with `stripes` in 2..=8 and `stripe` in
1..=stripes (0x0402 = stripe 2 of 4); 0 = full node; `0x0100` (stripe count 1, stripe 0) =
`PruningSeed::RECENT_ONLY`, a database pruned completely that keeps no stripe (`keeps(h)` is
false for every height, `stripes()` and `stripe()` are 0). Any other value is rejected on decode
(`PruningSeed::decode`); the raw value is private, so a `PruningSeed` is always one of the three
kinds (`is_full`, `is_stripe`, `is_recent_only`). The stripe count travels in the seed, so 4- and 6-stripe nodes can
coexist without a protocol change. All counts share the 4 096-block spans, so `holds_range`
and `keeps` are correct for any mix; only the stripe *number* of a height differs per count
(span 4 is stripe 1 of 4 but stripe 5 of 6). `PruningSeed::holding(n, h)` names the seed of
`n` stripes that keeps `h`; nodes look for holders under every count in `LOOKUP_STRIPES` = [4,
6] (`docs/node-architecture.md`, "Pruned peers"), so `--prune-stripes` other than 4 or 6 works
but such a node is not found by stripe-topic lookups. `xkrd --prune` picks a random stripe of
`DEFAULT_STRIPES` = 4 (`--prune-stripes N` for another count) and stores the seed in
`meta.pruning_seed`.

**What is kept.** Everything except the dropped signatures: records (block blobs, prefixes,
global indexes), outputs, key images and all indexes. Wallets (`/getwalletsyncdata`), RPC
headers and consensus for new blocks are unaffected. For a block below `pruned_below` whose
stripe the seed does not keep, `Reader::raw_block` and `Reader::transaction_blob` return
`Error::Pruned(height)` (blocks without transactions still read, they have no signatures),
`BlockInfo` gets `BLOCK_FLAG_PRUNED` and `Reader::has_full_block(height)` is false. The rule is
computed from `(pruning_seed, pruned_below)` (`PruneState::dropped`), never from a per-block
flag scan; missing signatures of a kept block are reported as corruption.

**Bound.** A node never prunes at or above `min(last checkpoint, tip − 2 × STRIPE_SPAN)`
rounded down to a multiple of 4 096 (`Checkpoints::prune_bound`; storage takes the bound as a
parameter and knows nothing about checkpoints). The newest 8 192–12 287 blocks therefore stay
whole: reorgs (at most `--max-reorg-depth` 1 000 deep and never below a checkpoint the chain
has reached) can always pop them, and a peer syncing the tip finds every block on every node. With the bound tied
to the last checkpoint, blocks after it are only pruned once a release adds a newer checkpoint.
`xkrd db prune --below N` refuses a height above the bound (the error names the bound) unless
`--force-below` is given too: above it a reorg into the pruned blocks is refused (the node can
fork off for good) and peers that assume the newest blocks whole get misses. Testnet has only the
genesis checkpoint (bound 0), so it never prunes by itself; the testnet scenario
`33-pruned-peers` prunes a copy with `xkrd db prune --stripes 4 --stripe 1 --below 8192
--force-below` (`docs/testnet-suite.md`). On a pruning or pruned database `--max-reorg-depth` is
clamped to 7 168 (2 × 4 096 − 1 024) at start.

**As the chain grows.** The chain thread calls `Chain::prune` at start and after every batch
and every block that changes the main chain; it is a snapshot check unless the bound passed the
next multiple of 4 096, and then drops the newly covered dropped-stripe chunks (the hot rows of
blocks not sealed yet too), one step of 32 768 blocks per writer lock (the lock is released
and the bound read again between steps). A chunk sealed below `pruned_below` in a dropped stripe (only after a
pop and re-push there) gets no signature parts; a chunk sealed above it keeps its parts until the
bound passes it. `Store::prune` advances in steps of 8 spans (32 768 blocks) per durable write
transaction, so the first prune of a full mainnet database does not build one huge
transaction. A reorg that would pop a pruned block with transactions is refused
(`Switch::Refused`, logged; the alternative branch is kept and the node stays on its chain);
`load_verified` answers `ChainError::Pruned`. P2P block serving
answers pruned blocks as missed.

**Modes.**

| state | `pruning_seed` | `pruned_below` | dropped below `pruned_below` |
|---|---|---|---|
| full node | 0 | 0 | nothing |
| stripe pruning (`--prune`, `db prune --stripes`) | stripe seed | multiple of 4 096 | every stripe except the seed's |
| pruned completely (`db prune --all`) | `0x0100` (`RECENT_ONLY`) | multiple of 64 | everything (the old `prune_below`) |

A stripe-pruned database only accepts its own seed; `--all` on it also drops the kept stripe
(the prune height never decreases); a completely pruned database cannot go back to keeping a
stripe without a resync. A completely pruned node stores and announces `0x0100`, never 0, so it
never poses as a full source (and `has_full_block` is false below `pruned_below`). A database
pruned completely before this seed existed has no stored seed but a prune height; it reads back
as `RECENT_ONLY` (`pruning::state_from_meta`). `db prune --all` on a chain shorter than one
chunk prunes nothing and leaves the database full.

**Sizes** (mainnet 2026-11-13, 2 618 387 blocks; `sig_parts` 12 216.5 MB of the 19 092.5 MB
file):

| node | signatures kept | file (after `db compact`) |
|---|---|---|
| full | 12.2 GB | 19.1 GB |
| stripe-pruned, 4 stripes | ≈ 3.1 GB (a quarter, plus the newest ≤ 12 k blocks) | ≈ 10 GB |
| stripe-pruned, 6 stripes | ≈ 2.0 GB | ≈ 8.9 GB |
| pruned completely | the newest ≤ 12 k blocks | ≈ 6.9 GB |

Deleted parts become free pages; `xkrd db compact` (or the automatic maintenance after a long
sync) shrinks the file.

## State hashes

One hash per stripe span over the data every node keeps, also a stripe-pruned or completely
pruned one (pruning drops only ring signatures), so nodes and wallets can check that a node
holds the same chain without downloading it whole. Code: `crates/xkr-chain/src/state.rs`
(`span_digest`, `chain_state`, `Chain::update_state_hashes`), `Reader::block_prefixes`,
`Store::state_hashes`.

**Definition.** Span `k` covers heights `4096k ..= 4096k + 4095` (`STRIPE_SPAN`). `H` is
`cn_fast_hash` (Keccak-256 with the original `0x01` padding). `u64` is 8 bytes little-endian,
hashes and keys are their 32 raw bytes.

```
S_{-1} = 32 zero bytes
S_k    = H("xkr state v1" || S_{k-1} || D_k)        "xkr state v1" = 12 ASCII bytes, no terminator
D_k    = H(block(4096k) || block(4096k + 1) || ... || block(4096k + 4095))

block(h) = u64 height h
           [32] block hash                     /getwalletsyncdata blockHash
           u64  timestamp                      blockTimestamp
           u64  transaction count n            1 + transactions.length (the coinbase counts)
           tx(coinbase) || tx(t_1) || ... || tx(t_{n-1})   coinbaseTX, then transactions in order

tx       = [32] transaction hash               hash
           [32] transaction public key         txPublicKey (32 zero bytes when none)
           u64  unlock time                    unlockTime
           u64  output count m                 outputs.length
           output_1 || ... || output_m         in output order
           u64  input count                    inputs.length; coinbase: 1
           input_1 || ...                      in input order

output   = [32] output key                     outputs[i].key
           u64  amount                         outputs[i].amount
           u8   1 if the output has a view tag, else 0
           u8   the view tag, 0 when there is none        outputs[i].view_tag
           u64  global output index            /get_global_indexes_for_range value[i]

input    = [32] key image                      inputs[j].k_image
coinbase input = u64 its height                (the block height; coinbaseTX has no inputs field)
```

Field sources, exactly as the RPC returns them:

* *transaction hash*: the coinbase's hash recomputed from the block (`transaction_hash`), the
  others from the block's `tx_hashes`; `/get_global_indexes_for_range` keys its entries by it.
* *transaction public key*: `xkr_types::wallet_tx_public_key` (C++ `Core::getPubKeyFromExtra`):
  the first public-key field of `extra` read in order, else the 32 bytes after the first `0x01`
  byte, else 32 zeros. Exactly the `txPublicKey` hex.
* *amount*: the output's `amount` field as stored and returned, also for version-2 (fork 14)
  outputs.
* *view tag*: version-2 tagged outputs (target `0x03`) carry `view_tag` (two hex digits);
  version 0/1 outputs (target `0x02`) have none and hash as `00 00`.
* *global output index*: per amount (the n-th output of amount `a` ever created has index `n`),
  the `value` list of `/get_global_indexes_for_range` for the transaction, one per output.
* *inputs*: every input of a non-coinbase transaction is a key input (consensus), so the count
  equals the `inputs` array of `/getwalletsyncdata`; the coinbase has exactly one coinbase input
  whose height equals the block height (consensus), which a wallet writes as count 1 and
  `u64 blockHeight`.

A wallet that synced the 4 096 blocks of a span through `/getwalletsyncdata` (all fields above)
and `/get_global_indexes_for_range` (the global indexes) recomputes `D_k`, chains `S_k` from
`S_{k-1}` and compares it with the built-in list or with `getstatehash` of other nodes.

**In the node.** A span is hashed once its last block is at least `max_reorg_depth` below the
tip (`Chain::stable_spans`), by the state thread `xkr-state` next to the chain thread
(`Chain::update_state_hashes`, 8 spans per call so a first start on a long chain hashes the
backlog without holding up blocks). The hashes go to `meta_blobs.state_hashes` with the hash of
each span's last block; on every call the stored list is cut back to the last span whose last
block is still on the main chain (only a reorg deeper than `max_reorg_depth` can change one).
Each new hash is compared with the compiled-in list (`MAINNET_STATE_HASHES` /
`TESTNET_STATE_HASHES` in `state.rs`, `(span, "hash")` pairs, generated with
`xkrd db state-hashes --to <height>` on a fully verified node), and so is every stored hash
when the chain opens. On a difference the node logs an error, `/status.state.mismatch` names
the span, `Chain::add_blocks` / `add_block` answer `ChainError::StateMismatch`, and the
downloader stops requesting blocks. RPC: `getstatehash { span | height }` (`docs/node-architecture.md`).
`xkrd db verify-state` recomputes every complete span and compares it with the stored hashes
and the built-in list (exit code 1 on a difference).

## Durability and maintenance

- During sync batches commit with `Durability::None` and one durable commit happens at least
  every `--db-durable-secs` (default 5) seconds, always for live blocks and reorgs. With redb's
  free-page rules (above) the file overhead of this is bounded by one interval of churn.
- **Maintenance** (`Store::maintain`, `xkrd db compact`): repacks every repackable table
  (`outputs`, `key_images`, `tx_hash_to_loc`, `block_hash_to_height`, `payment_ids`,
  `timestamps`, `unlock_overflow`) whose pages are less than 85 % full and that is at least
  32 MB, by moving 1 M rows per durable transaction into `<name>.repack` in key order (right-edge
  appends, packed leaves) and deleting them from the source as it goes (no doubled disk space);
  `Store::open` finishes an interrupted repack. Then `Database::compact` shrinks the file when
  ≥ 10 % is free or anything was repacked. Exclusive: readers get `Error::Busy` meanwhile.
- **Automatically**: when the node has synced ≥ 50 000 blocks in one run and reaches the
  network height, the P2P manager queues `ChainJob::Maintain` once; the chain thread runs it
  (minutes on mainnet; no blocks are applied meanwhile, RPC answers "busy").
  `--no-db-maintenance` disables it.

## Size estimate for the full chain (2.62 M blocks)

Assumptions, to be replaced by `xkrd db stats` numbers from the finished sync (the tool prints
every input of this table): raw chain R ≈ 15 GB (the extrapolation of the 819 k measurement;
the band averages grow to 10–18 KB per block after 400 k); signatures ≈ 78 % of R (fixture
mix), rings of 4 → I ≈ 46 M inputs (key images); N ≈ 31 M outputs (2.62 M × 6 coinbase
outputs + ~15 M transaction outputs); T ≈ 5.2 M transactions including miner transactions.

| item | v1 per unit | v2 per unit | v1 total | v2 total (after maintenance) |
|---|---|---|---|---|
| signatures | 256 B/input × ~1.35 (page runs) | × 1.008 (parts) | 15.8 GB | 11.8 GB |
| prefixes (inputs: key image + offsets) | ~47 B × 1.3 | ~40 B (keys incompressible, rest zstd) | 2.9 GB | 1.85 GB |
| outputs in blobs | 38 B | ~4 B (amount, tag, global index; key stripped) | 1.2 GB | 0.12 GB |
| block blobs (header, parent block, extra, tx hashes) | ~400 B × 1.2 | ~180 B compressed | 1.3 GB | 0.5 GB |
| `outputs` index | (16 + 88) B / ~0.5 fill | (12 + 40) B / 0.98 | 6.4 GB | 1.65 GB |
| `key_images` | 40 B / 0.69 | 32 B / 0.98 | 2.7 GB | 1.5 GB |
| tx index (`tx_hash_to_loc`, `tx_output_indexes`, payment ids) | ~100 B / tx | 40 B / tx | 0.6 GB | 0.22 GB |
| block index (`block_infos`, hash → height, timestamps) | 176 B / block | 136 B / block | 0.45 GB | 0.36 GB |
| **total, unpruned** | | | **≈ 31 GB (2.1 R)** | **≈ 18 GB (1.2 R)** |
| **total, pruned below the last checkpoint** | | | ≈ 15 GB | **≈ 6.2 GB (0.41 R)** |
| **total, stripe-pruned (4 stripes)** | | | | ≈ 9 GB (0.6 R; measured ≈ 10 GB below) |

Observations:

- Unpruned, the floor is what cannot be compressed: signatures (~0.78 R), key images in the
  prefixes (~0.1 R) and output keys. v2 removes the redb and duplication overhead (from
  +110–140 % to +20 %); getting the unpruned file below R would need dropping data, not
  layout. **The ≤ 60 % target is reached with pruning** (≈ 0.4 R), which is also where wallet
  and RPC behaviour is unchanged.
- Remaining v3 options, each measurable with `xkrd db stats`: per-amount output arrays
  (32 outputs per row: −12 B/output, ~0.37 GB); moving `height`/`unlock` from `OutputRow` to a
  per-transaction table (−8 B/output, one more lookup per ring member); a 16-byte key-image
  fingerprint index confirmed against the record (−12 B/input, ~0.5 GB); storing signatures
  directly in the chunk stream instead of hot rows (halves signature writes during sync).

## Measured on mainnet (2026-11-13, height 2 618 387)

The full chain was synced and verified with the v1-schema node, converted with `xkrd db convert`
(2 618 388 blocks in 38 min, 1 150–1 900 blocks/s) and re-verified with `xkrd db verify`
(657 s, 3 960 blocks/s, 0 problems). Raw chain (sum of `block_size`) 15 438.7 MB;
4 667 642 transactions, 43 355 401 outputs, 40 075 580 key images.

| | file | / raw |
|---|---|---|
| v1 | 36 532.8 MB | 2.37 |
| v2 after `convert` (dense, `compact` found nothing to do) | 19 092.5 MB | 1.24 |
| v2 pruned below the tip (computed: file minus `sig_parts`) | ≈ 6 870 MB | ≈ 0.45 |

v2 by table: `sig_parts` 12 216.5 MB (64.0 %, 4 160 B per 4 064-byte part, 97 % fill),
`outputs` 2 531.5 MB (58.4 B per output, 89 % fill), `body_parts` 2 453.5 MB (compressed
blocks and prefixes, ≈ 3.2 GB raw), `key_images` 1 313.0 MB (32.8 B each), block index
375.5 MB, transaction index 193.0 MB; fragmentation incl. free pages 605 MB. The estimates
in the table above were within 10 % on every row except the outputs index (43 M outputs instead
of the estimated 31 M: more small transaction outputs than the fixture mix suggested).

**Clean v2 sync from genesis (2026-11-14, `--fast-sync`):** 2 618 911 blocks in 8 h 39 min of
running time (9 h 11 min wall-clock: a heat-induced hardware reset at 2 569 700 cost 33 min, and
the run was restarted three times to change `--verify-threads` between 4 and 8). It shared the
machine with two follower nodes and, for the first hours, a `db convert`/`compact` of a 19 GB
file, so treat the time as an upper bound. CPU stayed at ~2–4 cores. The file was 21 475 MB at
the tip without a `compact` (redb keeps freed pages; the same data compacted is 19 092 MB, see
above). Signature verification in the region 300 000–750 000 took 5 of the 8.5 hours
(40–60 blocks/s); the rest of the chain ran at 300–500 blocks/s.

**Stripe pruning measured (2026-11-14, copy of the same database, new binary):** `xkrd db prune
--stripes 4 --stripe 1` dropped the signatures of 1 542 030 transactions (29 666 chunks,
2 232 915 parts, 9 146 MB of pages) below 2 609 152 in 299 s; `db compact --no-repack` then
shrank the file from 19 112 MB to 9 809 MB in 72 min (redb rewrites every live page). Result:
`sig_parts` 2 928 MB (29.9 %), `outputs` 2 532 MB, `body_parts` 2 436 MB, `key_images` 1 334 MB;
**file / raw 0.635, 3 747 B per block**. A full prune (`--all`) would land at ≈ 6.9 GB (0.45 R). `db verify` on the pruned copy: 2 618 566 blocks,
4 667 928 transactions, 0 problems (43 min alongside other jobs).

**Dictionary training on real data is not worth shipping.** `db train-dict --samples 20000
--max-dict-bytes 110000` over the whole chain shrank `body_parts` by 0.7 % (2 453.5 MB →
2 435.6 MB, 18 MB) and rewriting all 40 896 chunks left 1.2 GB of free pages until the next
`compact`. 64-block chunks already give zstd the context a dictionary would add. The shipped
`crates/xkr-storage/dicts/mainnet.zdict` therefore stays empty; the mechanism remains for a
future format where blobs are small enough to benefit (see `dicts/README.md`).

## Measuring

Run with the node stopped (or on a copy of the file).

```sh
xkrd db stats   --network testnet --data-dir DIR     # per-table bytes, fill, file vs raw ratio
xkrd db verify  --network testnet --data-dir DIR     # full cross-check walk
xkrd db compact --network testnet --data-dir DIR [--force]
xkrd db train-dict --data-dir DIR --samples 20000 --max-dict-bytes 110000 [--out FILE]
xkrd db prune   --data-dir DIR --stripes 4 [--stripe K] # keep one stripe (random K), then db compact
xkrd db prune   --data-dir DIR                       # continue with the stored seed (or --all)
xkrd db prune   --data-dir DIR --all                 # keep no old signatures (seed 0x0100)
xkrd db prune   ... --below HEIGHT                   # prune below HEIGHT, at most the safety bound
xkrd db prune   ... --below HEIGHT --force-below     # above the bound too (tests; reorgs break)
xkrd db convert --from OLD/chain.redb --db NEW/chain.redb [--network testnet]
```

`db convert` copies a v1 database into a new v2 file without re-verifying (the v1 node did),
runs maintenance and prints the v2 stats: the before/after comparison on the same chain
without a second sync. `db verify` on the result recomputes every block and transaction hash.

`db stats` works on v1 and v2 files (it lists tables generically), so the same command gives
the before and after numbers. The fixture size test (`fixture_size_regression`, 5 scrambled
replicas of the 39 mainnet fixture blocks) prints its per-table numbers with `--nocapture`.

## Comparison with the C++ RocksDB layout (`src/cryptonote_core/dbutils.h`)

The C++ node uses one RocksDB column family. Every key is the *KV-binary (epee portable
storage)* serialization of `std::pair(prefix_string, key)`; values are KV-binary too; RocksDB
compression is disabled (`rocksdb_wrapper.cpp`). The C++ node stores each transaction in effect
twice (raw block under `"4"` with all tx blobs, plus `ExtendedTransactionInfo` with outputs and
indexes under `"a"`), cannot prune without rewriting every `"4"` value, and keeps per-block
spent-key-image lists (`"0"`) for undo; we require the caller to supply the decoded blocks to
`pop_blocks` instead. Its prefixes map as in v1: `"1"`/`"4"` → records and chunks,
`"5"` → `block_hash_to_height`, `"6"` → `block_infos`, `"7"` → `key_images`, `"a"` →
`tx_hash_to_loc` + record global indexes, `"b"`/`"j"` → `outputs` + `output_counts`,
`"e"`/`"g"` → `timestamps`, `"f"` → `payment_ids`.
