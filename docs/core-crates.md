# Core crates: types, serialization, crypto, consensus

The crates that reproduce the consensus of the C++ node (github.com/kryptokrona/kryptokrona,
rev `cf5a63e1`, v1.2.x). Every consensus function names the C++ file/function it mirrors.

## Layout

```
xkr-types          keys.rs        Hash, PublicKey, SecretKey, KeyImage, KeyDerivation, Signature (Pod, hex)
                   transaction.rs TransactionPrefix, Transaction, TxInput, TxOutput(Target)      cryptonote.h
                   block.rs       BlockHeader, ParentBlock, Block, RawBlock                       cryptonote.h
                   extra.rs       parse_extra(_partial), tx_public_key, merge_mining_tag,         transaction_extra.cpp
                                  payment_id; parse_utils_extra / hugin_data (kryptokrona-utils layout, not consensus)
xkr-serialization  varint.rs      write_varint / read_varint (C++ readVarint overflow + canonical rules)
                   tx.rs          (de|en)code_transaction(_prefix), encode_base_transaction       cryptonote_serialization.cpp
                   block.rs       (de|en)code_block, encode_parent_block(mode), raw blocks         idem (ParentBlockSerializer)
                   hashing.rs     block_hashing_blob, block_hash, block_pow_blob, block_long_hash, cached_block.cpp,
                                  transaction_hash(_from_blob), transaction_prefix_hash,           cached_transaction.cpp,
                                  base_transaction_hash, aux_block_header_hash                     cryptonote_tools.h
xkr-crypto         keccak.rs      keccakf, keccak1600, cn_fast_hash                                keccak.c, hash.c
                   tree_hash.rs   tree_hash, tree_depth, tree_branch, tree_hash_from_branch        tree-hash.c
                   field.rs       GF(2^255-19), only for hash_to_ec                                crypto-ops.c
                   ops.rs         decompress (= ge_frombytes_vartime), check_key, key derivation,   crypto.cpp
                                  derive/underive keys, hash_to_ec (port of ge_fromfe_frombytes_vartime),
                                  generate_key_image, key_image_in_prime_subgroup, Schnorr signatures
                   ring.rs        generate_ring_signature, check_ring_signature(s)                 crypto.cpp
                   pow.rs         cn_slow_hash_v0, cn_lite_slow_hash_v1, cn_turtle_lite_slow_hash_v2, check_hash
                   cryptonight/   vendored Cuprate CryptoNight, parametrised (see crates/xkr-crypto/VENDORED.md)
xkr-consensus      params.rs      every cryptonote_config.h constant; Network {Mainnet, Testnet} -> NetworkParams
                   version.rs     block_major_version_for_height, pow_variant_for                 UpgradeManager
                   difficulty.rs  next_difficulty (original/Zawy, LWMA-2 V3/V4/V5, LWMA-3 V6),    currency.cpp, difficulty.cpp
                                  difficulty_window, difficulty_for_next_block
                   reward.rs      get_block_reward, get_penalized_amount, max_block_cumulative_size, currency.cpp, mixins.h
                                  median, dust thresholds, decompose_amount, fusion, mixin limits, unlock time
                   validation.rs  validate_semantic, validate_transaction, validate_block_header_and_coinbase,
                                  check_block_transaction_hashes, check_block_mixins, check_proof_of_work,
                                  validate_block (= Core::addBlock checks), pool/template policy   core.cpp
                   genesis.rs     genesis_block, genesis_hash                                      Currency::generateGenesisBlock
```

Storage is not touched: `validate_block` takes a `ChainContext` (recent timestamps, difficulty
window, recent block sizes, already generated coins, wall clock, genesis hash, checkpoint) and one
`ResolvedInput` per transaction input (ring member keys or `InvalidGlobalIndex`/`OutputLocked`, and
whether the key image is already spent). `reward::is_spend_time_unlocked` is the unlock rule the
storage layer applies when resolving rings.

## Verified against mainnet (fixtures, see `crates/xkr-serialization/tests/fixtures/README.md`)

* 39 real blocks (heights 0-10, 1000, 5052-5053, around 187 000, 440 000, 620 000, 700 000,
  800 000, 1 250 000, 1 300 000, 1 750 000, 2 200 000, 2 617 000-2 617 400, all major versions
  1-5): decode -> encode is byte-identical, block hash == node hash, coinbase/tx hashes, prefix
  hash = hash of blob minus signatures, cumulative size == node `transactionsCumulativeSize`,
  raw-block round trip, truncation never panics; genesis built from `GENESIS_COINBASE_TX_HEX`
  hashes to `7fb97df8...6f4c`.
* PoW: every v5 fixture block (CN-Turtle-Lite v2) meets its difficulty (~3e9 at the tip) and fails
  `u64::MAX`; blocks 1-3 (CN v0, difficulty 1/1/90) and block 4 (CN-Lite v1, difficulty 8190, the only
  v4 block on mainnet); merge-mining aux proof (`checkProofOfWorkV2`) for every v2+ block.
* CryptoNight: all 18 vectors of `src/crypto_test/main.cpp` (= `docs/test-vectors/cryptonight.txt`)
  plus Cuprate's Monero v0/v1/v2 vectors; `cn_fast_hash` vector.
* Difficulty: 1406 headers recomputed from their windows, covering the original/Zawy algorithm,
  LWMA-2 V3 (620k), V4 (700k), V5 (800k) and LWMA-3 (2.2M).
* Reward: every fixture block's reward and emission (`alreadyGeneratedCoins` delta).
* Ring signatures: 564 real inputs (all 8 transactions of blocks 5052/5053, 4-member rings, and
  one input of a 2026 transaction), plus negative checks; full `validate_block` of blocks 5052 and
  5053 with every ring member resolved (difficulty, reward, size, fees, PoW all equal the node's).
* Semantics: every fixture transaction passes `validate_semantic`, fees add up to the node's
  `totalFeeAmount`, every zero-fee transaction is a valid fusion transaction, mixin rules hold.

All of the above were also cross-checked by an independent Python reference
(`tests/fixtures/tools/`) before writing the Rust tests, so a failing Rust test means a Rust bug.

## C++ behaviour worth knowing (all mirrored)

* **V2 activates at height 2, not 1.** `UpgradeManager` picks the highest version whose upgrade
  height is *strictly below* the block height: mainnet heights 0-1 are v1, 2 v2, 3 v3, 4 v4, >= 5 v5.
  The difficulty algorithm is selected by the *previous* block index (LWMA-3 from height 2 200 001).
* **`nextDifficultyV4` never clamps from above**: `clamp(-6 * T, ST, 6 * T)` with TurtleCoin's
  `clamp(n, lower, upper)` is `max(ST, -6T)`. Without this quirk 91 headers above 700 000 mismatch.
* For major >= 3 the pre-LWMA algorithm returns `work * T / span` right away (Zawy by version),
  so the `ZAWY_DIFFICULTY_BLOCK_INDEX` branch is dead on mainnet.
* **Block hash** = `cn_fast_hash(varint(len) || header_blob || tree_root || varint(tx_count+1)
  [|| parent_block_hashing_blob])` (`getObjectHash` of a `BinaryArray` length-prefixes it).
  The PoW blob is the header-only parent blob *without* a length prefix (v1: the hashing blob).
* `BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT` (1 750 000): `tx_hashes` unique, supplied transactions unique,
  each supplied hash in `tx_hashes`, and both lists identical in order (`TRANSACTION_DUPLICATES` /
  `TRANSACTION_INCONSISTENCY`).
* `TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT` (1 750 000): coinbase must have no signatures
  (compared with the block's own height) and every ring size must equal its signature count
  (compared with the *previous* block index).
* **`MAX_EXTRA_SIZE_BLOCK` (128), `MAX_EXTRA_SIZE_V2` and `MAX_EXTRA_SIZE_V2_HEIGHT` are not checked
  on received blocks.** 128 is only applied when building block templates
  (`validateBlockTemplateTransaction`) and by the pool cleaner; `MAX_EXTRA_SIZE_POOL` (2200) only
  on pool admission; `MAX_EXTRA_SIZE_V2` only in the wallet. They live in `validation.rs` as policy
  functions (`check_pool_policy`, `is_valid_for_block_template`).
* Key images: `validateSemantic` requires `l * KI == identity` (`key_image_in_prime_subgroup`);
  `checkRingSignature` repeats it with `ge_check_subgroup_precomp_vartime`. ref10 point decoding
  rejects non-canonical `y` and `x = 0` with the sign bit set, which dalek accepts; `ops::decompress`
  adds a re-encoding check to match.
* Parent block: transaction count is read as `u64` and truncated to `u16`; a parent miner
  transaction of version >= 2 has an extra ignored varint (re-encoded as 0, as C++ does) and a
  different id (`getBaseTransactionHash`).
* Extra parsing: unknown tags are skipped byte by byte, duplicates stop parsing successfully, the
  nonce length is a raw byte. Hugin/kryptokrona-utils write a *varint* nonce length with sub-tag
  `0x7f`; `parse_utils_extra`/`hugin_data` decode that layout (non-consensus).
* **Testnet genesis = mainnet genesis.** `generateGenesisBlock` bumps the nonce only if
  `Currency::m_testnet`, which no code ever sets; a testnet daemon built from master
  reports the mainnet genesis hash. `TESTNET.genesis_nonce` is therefore 70.
* CN-Turtle-Lite v2 initialises 256 KiB but only addresses the first 128 KiB (light flag halves the
  mask). The portable C fallback in `slow-hash.c` differs from the x86-64 path; we mirror x86-64.
  The Rust port is tested against the canonical vectors and mainnet block 4.

## Known gaps

* No `unsafe` (`bytemuck` derives in `xkr-types/keys.rs` aside).
* Upgrade voting (`BasicUpgradeDetector` for versions without a fixed height) is not implemented;
  all mainnet/testnet versions have fixed heights. Fork 13 is in: the fork table
  `xkr-consensus::forks` (`ForkSchedule`), `MAX_BLOCK_MAJOR_VERSION = 6`, version 6 = version 5
  rules; see `docs/forks.md`.
* Checkpoints: `ChainContext::{in_checkpoint_zone, checkpoint}` are inputs; the C++ list is empty.
* `ge_scalarmult` with a secret key `>= 2^255` (invalid keys only) reduces the key mod `l`. Not
  reachable from consensus.
* Degenerate inputs (LWMA-2 when all solve times cancel, a signature list shorter than the ring,
  a point that does not decode) give 0 / `false` / an error.
* `hash_to_ec` does two field exponentiations (~500 squarings) per ring member (no batched
  inversion).
* The PoW tests for CN v0 (blocks 1-3, 2 MiB, 2^19 rounds) are slow in debug builds.
