# Transaction version 2 (fork 14): view tags and single-key CLSAG

Status: implemented, scheduled, with RPC, test wallet and testnet scenario; written 2026-11-13.
**Scheduled on
mainnet at height 3 000 000** (`FORK_14_HEIGHT_MAINNET`, provisional height, set on
2026-11-13): block 3 000 001 is the first v7 block, version 1 is valid until 3 028 800. Below
3 000 000 every rule is exactly that of fork 13. Testnet: not scheduled by default,
`xkrd --network testnet --fork14-height F [--fork14-grace N]` (`docs/forks.md` §5,
`testnet/fork14-scenario.sh`). Fork table: `docs/forks.md`; RPC for wallets and explorers: §9
below.

## 1. Summary

| | version 1 (today) | version 2 (fork 14) |
|---|---|---|
| outputs | tag `0x02`, key | tag `0x03`, key, 1-byte **view tag** |
| inputs | `TxInput::Key` | unchanged (amount, relative indexes, key image) |
| signature per input | `(c_i, r_i)` per ring member, 64·n B | **CLSAG**: `s_0..s_{n-1}`, `c1`, 32·(n+1) B |
| key image | `x·H_p(P)` | the same `x·H_p(P)`; one spent set for both versions |
| amounts | in the clear | in the clear (no RingCT, no range proofs) |
| tx hash | `cn_fast_hash(blob)` | the same |
| ring-signature message | `cn_fast_hash(prefix)` | the same |
| block | major 5/6 | major **7** |
| coinbase | version 1 | version 2 (tagged outputs) |

## 2. Wire format

Everything is the CryptoNote binary format (`varint` = LEB128 as in `xkr-serialization::varint`,
keys and scalars 32 raw bytes, little endian scalars).

```
transaction_v2 :=
  varint version = 2
  varint unlock_time
  varint n_in,  input*      ; unchanged:
                            ;   0xff varint height                         (coinbase)
                            ;   0x02 varint amount varint n (varint idx)*n key_image[32]
  varint n_out, output*     ; output := varint amount 0x03 key[32] view_tag[1]
  varint n_extra, extra     ; unchanged
  signatures                ; see below
```

* **Output tag `0x03`** (`TAG_TAGGED_KEY`), the value Monero uses for `txout_to_tagged_key`;
  it clashes with neither `0x02` (key) nor `0xff` (coinbase input). Layout like Monero:
  key, then the view-tag byte.
* **Version / layout binding** (decoder, `read_prefix`): versions 0 and 1 accept only `0x02`
  outputs (a `0x03` output is still `UnknownOutputTag`, so version-1 decoding is unchanged);
  version 2 accepts only `0x03` outputs (a `0x02` output under version byte 2 is
  `WrongTransactionVersion(2)`); versions above 2 are `WrongTransactionVersion`. The miner
  transaction of the merge-mining *parent* block is not an XKR transaction and keeps the old
  parser whatever its version.
* **Signatures**: none if the transaction has no inputs or exactly one coinbase input
  (coinbase transactions). Otherwise every input must be a key input
  (`CoinbaseInputInSignedTransaction` otherwise, at decode and encode) and is followed, in
  input order, by its CLSAG signature:

  ```
  clsag := s_0[32] s_1[32] .. s_{n-1}[32] c1[32]      ; n = ring size of the input
  ```

  The order (`s` vector, then `c1`) is Monero's. Monero's third field `D` (the commitment key
  image `z·H_p(P)/8`) belongs to the commitment column, which XKR does not have: omitted.
  Scalars are stored as raw bytes; canonical form is checked by the verifier.
* In Rust: `TxOutputTarget::TaggedKey { key, view_tag }`, `Transaction::clsag_signatures:
  Vec<ClsagSignature>` (`ClsagSignature { s: Vec<EcScalar>, c1: EcScalar }`) next to the legacy
  `signatures`; version 2 has `signatures` empty, versions 0/1 have `clsag_signatures` empty
  (`EncodeError::SignatureFormat` otherwise). `Transaction::signature_section_len()` gives the
  byte length after the prefix for both (storage splits blobs there).

**Hashes.** The C++ node hashes a transaction as `getObjectHash(tx)` = `cn_fast_hash` of the
whole serialized blob (`CachedTransaction::getTransactionHash`, no length prefix). Version 2
keeps that: `tx_hash = cn_fast_hash(blob)`, `prefix_hash = cn_fast_hash(prefix bytes)`, and the
CLSAG message is `prefix_hash`. Monero's v2 hash (`H(prefix_hash || H(rct_base) ||
H(prunable))`) exists so pruned nodes can recompute ids without signatures; xkrd stores ids and
prunes signatures separately, so there is no need. The coinbase hash inside
the block's transaction tree is the blob hash as for version 1 (`getBaseTransactionHash`'s
version-2 rule only applies to the merge-mining parent).

## 3. View tags

`view_tag(D, i) = cn_fast_hash("view_tag" || D || varint(i))[0]`, with `"view_tag"` the 8 ASCII
bytes without terminator, `D = 8·r·A = 8·a·R` the CryptoNote derivation
(`generate_key_derivation`) and `i` the output index. This is Monero's `derive_view_tag`
byte for byte; `xkr_crypto::view_tag` passes all 70 vectors of Monero's
`tests/crypto/tests.txt` (unit test `monero_view_tag_vectors`, also re-checked by the Python
reference, §7).

**Scanning** (`xkr_wallet_core::scan_tagged_outputs`, `scan_transaction`): per transaction the
wallet computes `D` once (one variable-base scalar multiplication, as today); per output it
first compares `view_tag(D, i)` with the output's tag (one Keccak) and only for a match runs
`derive_public_key` (hash to scalar, fixed-base multiplication, point decompression, addition,
compression) and the key comparison. A foreign output matches with probability 1/256. Untagged
(version-1) outputs always take the full check, so mixed histories scan correctly. The result
is identical to the untagged scan.

The node cannot check a view tag (it needs the recipient's view key); a wrong tag only hides
the output from its own recipient, and the tag is in the signed prefix, so nobody else can
change it.

## 4. Single-key CLSAG

Construction: Monero's CLSAG (`CLSAG_Gen`, `verRctCLSAGSimple` in `src/ringct/rctSigs.cpp`) with
only the `P` column: no `C_i`, no `C_offset`, no `D`, no `mu_C`. That is the one-key case of the
CLSAG paper (a compact LSAG with an aggregation coefficient). Code: `xkr_crypto::clsag`.

**Transcripts** (`H_s(x) = sc_reduce32(keccak256(x))` = `hash_to_scalar`, `H_p` = `hash_to_ec`
= `8·ge_fromfe_frombytes_vartime(keccak256(P))`, `pad32(s)` = ASCII `s` + zero bytes to 32;
points are 32-byte compressed encodings, scalars 32-byte little endian, ring in ring order,
i.e. ascending global index, see "Ring order" below):

```
mu      = H_s( pad32("XKR_CLSAG_agg_0") || P_0 || ... || P_{n-1} || I )
c_{i+1} = H_s( pad32("XKR_CLSAG_round") || P_0 || ... || P_{n-1} || m || L_i || R_i )
L_i     = s_i·G       + (c_i·mu)·P_i
R_i     = s_i·H_p(P_i) + (c_i·mu)·I
```

`m` is the 32-byte prefix hash. The strings are XKR's own, so XKR and Monero CLSAG
transcripts never coincide; the structure (domain block, ring, then the
round-specific data) is Monero's.

**Signing** (`clsag_sign(msg, ring, secret, index, rng)`): `I = x·H_p(P_l)`; random `α`;
`c_{l+1} = H(.., α·G, α·H_p(P_l))`; for `i = l+1, ..., l-1 (mod n)` random `s_i` and the next
challenge; `c1 = c_0`; `s_l = α − c_l·mu·x`. Returns `None` for an index out of range, an
undecodable ring member, or a secret with `x·G != P_l` (checked with `subtle`). The nonce and
the decoys' responses come from the caller's `CryptoRng` (wallets: `os_rng()`); `α` and `x` are
zeroized. Secret-dependent work is constant time (dalek scalar arithmetic, constant-time fixed-
and variable-base multiplication for `α`); every ring position costs the same.

**Verification** (`clsag_verify(msg, ring, key_image, sig)`), in this order, `false` on the first
failure:

1. `n = ring.len() >= 1` and `s.len() == n`;
2. `c1` and every `s_i` canonical (`< l`, `sc_check`);
3. `I` decodes (ref10 rules), is not the identity and `l·I == identity`
   (`point_in_prime_subgroup`, the same predicate as `key_image_in_prime_subgroup`);
4. every ring member decodes;
5. `c = c1`; for `i = 0..n`: `c = H_s(round transcript with L_i, R_i)`, reject `c == 0` (as
   Monero); accept iff the final `c == c1`.

`ClsagCheck` / `clsag_verify_all` are the batch shape used by the node; CLSAG's hash chain has
no cheaper combined equation, so batches are parallelised per input (rayon in `xkr-chain`).

**Key images and double spends.** The key image is the CryptoNote `x·H_p(P)` and lives in the
unchanged input, so the storage spent set, the block-wide set in `validate_transaction` /
`check_inputs` and the pool's key-image index are shared: an output spent by a version-1
transaction cannot be spent by a version-2 one and vice versa (tests in `xkr-consensus`,
`xkr-txpool`, `xkr-chain`).

## 5. Consensus rules (fork 14)

Fork table entry (`ForkSchedule::fork14_entry`, index 14): height `F` (mainnet 3 000 000,
testnet `--fork14-height`; `None` = not scheduled),
block major version **7**, minor signal 14, `tx_versions` 1..=2, rules `MAJOR_VERSION |
TX_VERSION_2 | VIEW_TAGS | CLSAG`. It appears in `ForkSchedule::forks()` (and so in
`/getinfo.upgrade_heights`, `fork_at`, `next_fork`, the peer policy) only once scheduled.
Parameters: `Fork14Params { grace_blocks, v2_ring_size }` in `NetworkParams::fork14`
(`FORK_14_PARAMS_MAINNET`: 28 800 blocks of grace, mixin limits; testnet: 20).
`NetworkParams::with_fork14_height` / `with_fork14_params` and `ChainConfig::{fork14_height,
fork14}` schedule it at run time, mirroring fork 13 (`xkrd --fork14-height`,
`--fork14-grace`, testnet only). A schedule with fork 14 supports and signals index 14
(`ForkSchedule::supported_index`, template minor version 14).

Activation convention as in `docs/forks.md`: the fork governs blocks `F + 1, ...`.

| block height `h` | non-coinbase versions | coinbase | block major |
|---|---|---|---|
| `h <= F` (or unscheduled) | 0, 1 | 0, 1 | 5/6 as before |
| `F < h <= F + grace` | 0, 1, 2 | exactly 2 | 7 |
| `h > F + grace` | 2 | exactly 2 | 7 |

Version 0 is treated as 1 (the C++ deserializer accepts it wherever version 1 is valid).

* `validate_semantic`: outputs of version 2 must be tagged and outputs of 0/1 untagged
  (`OUTPUT_UNKNOWN_TYPE`; decoded transactions always comply, so version 1 is unchanged); all
  other rules (zero amounts, valid keys, unique key images in the subgroup, index deltas,
  amounts) apply to both versions.
* `check_transaction_version(forks, height, tx)` (new, `WRONG_TRANSACTION_VERSION`): the table
  above, plus the optional `RingSizeRule::Range { min, max }` for version-2 key inputs
  (`INVALID_MIXIN`). Called by `validate_block`, the `xkr-chain` pipeline (`check_header`), the
  pool (as if for the next block) and template selection.
* `validate_transaction`: version 2 resolves rings exactly like version 1 (same ring order,
  same unlock rule, `OutputLocked`, spent check), requires one CLSAG per input with
  `s.len() == ring size` (always, not only above `TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT`)
  and `clsag_verify` against the resolved ring. Mixin limits (`check_block_mixins`, pool policy)
  are the same for both versions.
* **Ring order** (both versions): the ring of a key input is resolved from its packed
  `output_indexes` as
  1. absolute indexes by wrapping `u32` addition of the offsets (`relativeOutputOffsetsToAbsolute`);
  2. sorted ascending, each index once;
  3. the outputs of the input's amount at those indexes, in that order; an index without an
     output is left out.

  The signature (ring signature or CLSAG) is verified over the keys in that order, and the
  signature count is compared with the number of members left (always for version 2, from
  `TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT` for version 1; below it extra version-1
  signatures are ignored). This is how the C++ node resolves rings for version 1:
  `DatabaseBlockchainCache::extractKeyOutputs` (`database_blockchain_cache.cpp`) reads the
  outputs in one batch and walks them through a `std::map` keyed by `(amount, global index)`.
  Version 2 has no C++ implementation yet; it uses the same rule so that both versions share
  one resolver (`xkr_txpool::ring_global_indexes`, `xkr_storage::Reader::ring_keys`). Wallets
  build rings with strictly increasing absolute indexes (offsets after the first are
  positive and do not wrap), so their offsets order and the ring order are the same.
* Coinbase (`validate_block_header_and_coinbase`): version per the table, tagged outputs for
  version 2, no signatures. The view tags of the coinbase are not checkable (see §3).
* Blocks: `MAX_BLOCK_MAJOR_VERSION = 7` in the decoder, `check_proof_of_work` accepts 1..=7
  (CN-Turtle-Lite v2, LWMA-3, reward, sizes: version 5 rules).
* Pool: admission applies `check_transaction_version` for `top + 1` (`PoolChainState::
  fork_schedule`, default mainnet table); after each stored block, transactions the next block
  no longer allows (version 1 after the grace) are evicted (`TxPool::remove_disallowed`), and
  `fill_block_template_with` skips them.
* Templates: from `F + 1` major 7 and a version-2 coinbase with
  `view_tag(8·r·A_miner, i)` per output (`MinerTxParams::tx_version`).
* `xkr-chain` stage 3 verifies CLSAG jobs in the same rayon pass as ring signatures (one
  `SigJob` per input, dispatched on the version).

Not changed: unlock times, fees, fusion rules, extra limits, P2P messages (a version-2 blob is
just a blob), storage layout. Storage note: the output-key stripping of `xkr-storage` only
recognises `0x02` outputs, so version-2 prefixes are stored unstripped (no loss, ~32 B per
output less compression); `output_key` accepts tagged keys (one added match arm).

## 6. Size and CPU

Per input with ring size `n` (key image and indexes unchanged):

| n | v1 signatures | v2 CLSAG | saving |
|---|---|---|---|
| 2 | 128 B | 96 B | 25 % |
| 4 | 256 B | 160 B | 37.5 % |
| 6 (max today) | 384 B | 224 B | 42 % |
| 16 (Monero) | 1024 B | 544 B | 47 % |

Outputs grow by 1 byte (view tag). A typical 2-input, 2-output transaction with ring size 4
(~ 2·(4·~3 + 32 + 4) + 2·(~5 + 33) + 70 B prefix ≈ 240 B) shrinks from ≈ 750 B to ≈ 560 B.

Verification per ring member: v1 and v2 both need one decompression, one `H_p` (two field
exponentiations, the dominant cost), one double-base `s·G + c·P` and one two-term multiscalar
`s·H_p + c·I`, and two point compressions. v1 hashes everything once; v2 hashes one
`32·(n+3)`-byte transcript per member (Keccak, negligible next to `H_p`). Expect equal speed
within a few percent; signing likewise. Scanning: per output one Keccak instead of a fixed-base
multiplication plus decompression/compression, so for blocks full of foreign outputs the wallet
work is about one variable-base multiplication per transaction instead of one per transaction
plus one fixed-base multiplication per output (Monero measured ~40 % faster wallet sync).

## 7. Test vectors and tests

* View tags: the 70 Monero vectors (`xkr-crypto` `ops::tests::monero_view_tag_vectors`).
* CLSAG fixed vectors (`xkr-crypto` `clsag::tests::fixed_vectors`): ring secrets
  `x_i = H_s("xkr clsag vector key" || u8 i)`, message `cn_fast_hash("xkr clsag vector
  message")`, nonce stream block `k` = `cn_fast_hash("xkr clsag test rng" || seed_le64 ||
  k_le64)` (64 bytes per `random_scalar`, reduced mod `l`), ring sizes 1, 2, 4, 11 with signer
  index 0, 1, 2, 7 and seeds 11-14. Expected key image, `mu`, `c1` and all `s_i` were computed
  by an **independent Python implementation** of this document,
  `crates/xkr-serialization/tests/fixtures/tools/clsag_ref.py` (on top of the Python
  ed25519/Keccak/`hash_to_ec` reference that already verified 564 mainnet ring signatures);
  `python3 -I clsag_ref.py <monero>/tests/crypto/tests.txt` reprints them and re-checks the view
  tags. Example (n = 1, index 0, seed 11):

  ```
  P_0 = x_0·G, I   = 7890cf3872bcf89f8caab0a5c81e52054f7ef3354d4db7d7df235b781f8f561a
  mu               = 99fef1b6556cf3fc57947bbae2b9873123a79f428b285b2047776eceeed7060d
  c1               = 0cf4cf2cf511c3964d923819e8f4d023c5f88805674ffdfa7528f6748785460b
  s_0              = d945d58cea7b0354a9b7d224d140461860f73fbef388f60250910f72e67a690e
  ```

* Cross-check against Monero's CLSAG vectors: **not feasible**. Monero's `TEST(ringct, CLSAG)`
  (`tests/unit_tests/ringct.cpp`) has no fixed vectors (random keys every run), and the
  commitment-free special case (`C_i = C_offset = identity`, `z = 0`) is exactly what Monero's
  verifier rejects (`D·8 == identity` is "Bad auxiliary key image"), besides the different
  domain strings and the `C` terms in both transcripts. Instead the Rust tests repeat that unit
  test's negative cases with one key column (`monero_negative_cases`): zero/other message, wrong
  index, wrong secret, wrong/identity/torsioned key image, every altered `s_i`, altered `c1`,
  wrong lengths, reordered and replaced rings; plus non-canonical scalars, all ring sizes 1-8
  at every index, unlinkability with a shared key image, and the batch API.
* Serialization: v2 round trip and exact layout, every truncation fails, coinbase v2, format
  rules (tag `0x03` in v1, `0x02` in v2, version 3, mixed signature kinds, wrong `s` counts,
  coinbase next to key inputs); all mainnet fixtures still round-trip byte for byte.
* Consensus: CLSAG through `validate_transaction` (altered signatures, counts, changed prefix, spent),
  v1/v2 double spend of one output inside a block in both orders, the version table with
  grace and fixed ring size, coinbase versions around `F`.
* Pool: v2 refused before the fork, both versions during the grace, v1 refused/evicted/kept out
  of templates after it, v1/v2 key-image conflict.
* Chain (`testnet_fork14_transaction_version_2`): testnet chain with fork 13 at 2 and fork 14
  at 4 (grace 3): v2 refused at `F`, v1 spend at `F`, v6 block and v1 coinbase refused at
  `F + 1`, v2 spend through the pool at `F + 1`, cross-version double spends refused both ways,
  v1 refused in pool and block at `F + 4`, a mined v7 template with a v2 coinbase whose view
  tags match the miner's derivation, `/status.fork.active_index = 14`.
* Wallet: v2 build (decode, semantic, CLSAG verify, size `32·(n+1)`), view-tag scan equals full
  scan, wrong tag hides only that output.

**Vector files** for other implementations (the JavaScript wallet libraries), in
`docs/test-vectors/`. Each file has a generator test and a verification test in the same test
target:

| vector | file | regenerate with |
|---|---|---|
| A | `tx-v2-view-tags.json` | `XKR_WRITE_VECTORS=1 cargo test -p xkr-wallet-core --test tx_v2_view_tags` |
| B | `tx-v2-layout.json` | `XKR_WRITE_VECTORS=1 cargo test -p xkr-wallet-core --test tx_v2_layout` |
| C | `tx-v2-clsag.json` | `XKR_WRITE_VECTORS=1 cargo test -p xkr-wallet-core --test tx_v2_clsag` |
| D | `tx-v2-transactions.json` | `XKR_WRITE_VECTORS=1 cargo test -p xkr-wallet-core --test tx_v2_transactions` |
| E | `tx-v2-walletsync.json` | `XKR_WRITE_VECTORS=1 cargo test -p xkr-rpc --test tx_v2_walletsync` |

With `XKR_WRITE_VECTORS=1` the generator test writes the file and checks what it wrote; the
verification test does nothing. Without it (every normal `cargo test`) the generator does
nothing and the verification test reads the file, requires it to be byte-identical to a fresh
generation (so a stale or hand-edited file fails) and checks it against the Rust code: view tags
recomputed (A), valid blobs decode and re-encode, invalid blobs give the named error (B), CLSAG
signatures verify and re-signing with the given randomness gives the same `c1` and `s` (C),
transactions are rebuilt from the file alone and their blobs, hashes, signatures, view tags and
scans match (D), the recorded RPC answers scan to the expected outputs (E). Each file's
`description` field says what it holds. Conventions: keys of every object sorted, two-space
indentation, trailing newline; bytes as lowercase hex; amounts, heights and indexes as JSON
numbers; `null` where a field does not apply. Every secret and random scalar is
`H_s("xkr tx-v2 vectors " || label)`; the files contain every derived value. Sources:
`crates/xkr-wallet-core/tests/tx_v2_*` (shared helpers in `tests/vectors/mod.rs`) and
`crates/xkr-rpc/tests/tx_v2_walletsync/`.

## 8. Parameters

1. **Fork-14 height**: 3 000 000 on mainnet (provisional). Signalling follows the schedule
   (`ForkSchedule::supported_index` = 14 wherever fork 14 is scheduled, so mainnet xkrd
   templates write minor 14). Wallets (§9), p2pool (version-2 coinbase with tagged outputs,
   block v7) and explorers need a release that knows version 2 before that height.
2. **Ring size**: `RingSizeRule::MixinLimits`, i.e. the existing limits (mixin 1..=5, default 5,
   ring size 2..=6, default 6, since height 1 250 000). Rings are per amount. Decoy selection
   is wallet policy.
3. **Grace period** for version 1: `grace_blocks` = 28 800 on mainnet (30 days at 90 s); 0 means
   a hard cut at `F + 1`.
4. Fusion transactions follow the same version rules as all other transactions.
5. Transaction id: `cn_fast_hash(blob)` for both versions (§2).
6. View tags are exposed in `/getwalletsyncdata`, `/get_pool_changes_lite` and the explorer
   JSON (§9) and used by `xkr-testwallet`.

## 9. RPC and wallet-library changes

Everything below is additive: version-1 data is byte-for-byte what it was, so a client that
never sees a version-2 transaction notices nothing. A client that does not know the new fields
keeps working until the grace period ends (it scans tagged outputs the slow way, since `key`
is still there), but it can only *send* version 1, which the pool refuses after `F + grace`.

**`/getwalletsyncdata`** (`items[].coinbaseTX.outputs[]`, `items[].transactions[].outputs[]`)
and the same output lists wherever `wallet::outputs_json` is used: C++ writes `{ "key",
"amount" }` per output. A tagged output adds one field:

```json
{ "key": "9c1f…", "amount": 200000 }                      // version 1, unchanged
{ "key": "9c1f…", "amount": 200000, "view_tag": "5e" }    // version 2 (tag 0x03)
```

`view_tag` is the one-byte tag as two lowercase hex digits. Output order and indexes are
unchanged (the index is part of both the key derivation and the view tag).

**Explorer JSON** (`f_transaction_json` and `/get_pool_changes_lite` `txPrefix`, i.e. everything
built by `explorer::prefix_json`; `f_block_json` lists transactions only as hash, fee, size and
amount and needs no change beyond `major_version` 7 and minor 14): the C++
serializer writes output targets as `{ "type": "02", "data": { "key" } }`. A tagged output is

```json
{ "amount": 200000, "target": { "type": "03", "data": { "key": "9c1f…", "view_tag": "5e" } } }
```

(`"03"` is the wire tag, like `"02"` and the coinbase input's `"ff"`). Inputs keep `{ "type":
"02", "value": { "amount", "key_offsets", "k_image" } }`. A version-2 transaction has
`"version": 2`, an empty `"signatures": []` and its CLSAG signatures in

```json
"clsag_signatures": [ { "s": ["<64 hex>", …], "c1": "<64 hex>" } ]   // one per input, ring order
```

`txDetails.mixin` is the ring size as before. `/gettransactions` returns blobs (`txs_as_hex`)
and needs no change; `/sendrawtransaction` accepts version-2 blobs once the next block allows
them (`"error": "invalid transaction: transaction version not allowed at this height"`
otherwise, the same text for version 1 after the grace period).

**`/getrandom_outs`** returns `{ "global_amount_index", "out_key" }` for every output of the
amount, tagged or not (storage keeps the plain key of both kinds); decoys for a version-2 ring
may be version-1 outputs and vice versa, rings stay per amount.

**`/getinfo`** adds `"tx_versions"`: the transaction versions the **next** block accepts,
`[1]` before the fork, `[1, 2]` during the grace period, `[2]` after it (also in
`/status.fork.tx_versions`). Wallets should build the highest listed version. Also changed by
the schedule: `upgrade_heights` ends with the fork-14 height, `supported_height` is the fork-14
height, `major_version` of the top block becomes 7 after the fork (a fallback signal for
daemons without `tx_versions`: major >= 7 means version 2 is allowed).

**What the wallet libraries must change** (the transaction chain is kryptokrona-crypto →
kryptokrona-utils → wallet-backend-js → hugin-desktop and hugin-native, which also has its own
native scanning code; hugin-crypto is message encryption and is not involved):

1. Read `view_tag` from `/getwalletsyncdata` outputs and, per transaction, compute the
   derivation once, then for each tagged output compare `view_tag(D, i)` = first byte of
   `cn_fast_hash("view_tag" || D || varint(i))` before `derive_public_key`; skip on mismatch.
   Untagged outputs take the full check as today (§3).
2. Parse the tag `0x03` output layout (`amount, 0x03, key[32], view_tag[1]`) and transaction
   version 2 wherever raw transactions are decoded (pool changes, `/gettransactions`).
3. Build version 2 when `/getinfo.tx_versions` contains 2: tagged outputs with
   `view_tag(8·r·A, i)`, the CLSAG of §4 per input (`s_0..s_{n-1}`, `c1`) instead of the
   per-member `(c, r)` pairs; key images, rings, amounts, extra and fees are unchanged. Until
   `F + grace` version 1 still works.
4. Explorers: render `"type": "03"` targets and `clsag_signatures`.

**`xkr-testwallet`** (the reference client in this repository): `send --tx-version 1|2`
(default: the highest of `/getinfo.tx_versions`; without the field 2 only if the top block is
major >= 7), the default mixin is the minimum of the mixin limits, kept inside the fork-14
`RingSizeRule` for version 2 (testnet and mainnet: `MixinLimits`); scanning uses
`scan_tagged_outputs_with_stats` with the RPC's `view_tag`, and `balance` prints a line like

```
scan     1843 outputs in 12 ms: 1203 with view tags, 1198 skipped by view tag, 645 full derivations
```

(`ScanStats` in `xkr-wallet-core`: outputs, tagged, skipped by tag, full `derive_public_key`
checks).
