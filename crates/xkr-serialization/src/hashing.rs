//! Object hashes and hashing blobs: C++ `CachedBlock` (`cached_block.cpp`),
//! `CachedTransaction` (`cached_transaction.cpp`), `getObjectHash` / `getBinaryArrayHash` /
//! `getBaseTransactionHash` (`cryptonote_tools.h/.cpp`).

use xkr_crypto::keccak::cn_fast_hash;
use xkr_crypto::tree_hash::{tree_hash, tree_hash_from_branch};
use xkr_crypto::{cn_lite_slow_hash_v1, cn_slow_hash_v0, cn_turtle_lite_slow_hash_v2_reused};
use xkr_types::{Block, Hash, ParentBlock, Transaction, TransactionPrefix};

use crate::block::{ParentBlockMode, encode_parent_block, write_block_header};
use crate::error::{EncodeError, PowError};
use crate::tx::{encode_base_transaction, encode_transaction, encode_transaction_prefix};
use crate::varint::write_varint;

/// C++ `getObjectHash(const BinaryArray&)`: the `BinaryArray` specialization of `toBinaryArray`
/// serializes the blob **as a string**, i.e. `varint(len) || blob`, and that is what gets hashed.
#[must_use]
pub fn get_object_hash_of_blob(blob: &[u8]) -> Hash {
    let mut buf = Vec::with_capacity(blob.len() + 10);
    write_varint(&mut buf, blob.len() as u64);
    buf.extend_from_slice(blob);
    cn_fast_hash(&buf)
}

/// C++ `CachedTransaction::getTransactionHash` for an already-serialized transaction:
/// `getBinaryArrayHash(blob)` = `cn_fast_hash(blob)` (no length prefix).
#[must_use]
pub fn transaction_hash_from_blob(blob: &[u8]) -> Hash {
    cn_fast_hash(blob)
}

/// C++ `getObjectHash(Transaction)`: hash of the re-serialized transaction.
pub fn transaction_hash(tx: &Transaction) -> Result<Hash, EncodeError> {
    Ok(cn_fast_hash(&encode_transaction(tx)?))
}

/// C++ `CachedTransaction::getTransactionPrefixHash`: the ring-signature message.
#[must_use]
pub fn transaction_prefix_hash(prefix: &TransactionPrefix) -> Hash {
    cn_fast_hash(&encode_transaction_prefix(prefix))
}

/// C++ `getBaseTransactionHash(const BaseTransaction&)`: for version < 2 the hash of the
/// serialized `BaseTransaction`; for version >= 2 the hash of the 96-byte array
/// `prefix_hash || bc36789e...c98a || 0^32` (Monero v2-style miner tx id).
#[must_use]
pub fn base_transaction_hash(prefix: &TransactionPrefix) -> Hash {
    const MIDDLE: [u8; 32] = [
        0xbc, 0x36, 0x78, 0x9e, 0x7a, 0x1e, 0x28, 0x14, 0x36, 0x46, 0x42, 0x29, 0x82, 0x8f, 0x81,
        0x7d, 0x66, 0x12, 0xf7, 0xb4, 0x77, 0xd6, 0x65, 0x91, 0xff, 0x96, 0xa9, 0xe0, 0x64, 0xbc,
        0xc9, 0x8a,
    ];
    if prefix.version < 2 {
        return cn_fast_hash(&encode_base_transaction(prefix));
    }
    let mut data = [0u8; 96];
    data[..32].copy_from_slice(&transaction_prefix_hash(prefix).0);
    data[32..64].copy_from_slice(&MIDDLE);
    cn_fast_hash(&data)
}

/// Merkle root committed by a hashing-mode parent block:
/// `tree_hash_from_branch(baseTransactionBranch, getBaseTransactionHash(minerTx), path = 0)`.
#[must_use]
pub fn parent_block_merkle_root(pb: &ParentBlock) -> Hash {
    tree_hash_from_branch(
        &pb.base_transaction_branch,
        &base_transaction_hash(&pb.base_transaction.prefix),
        None,
    )
}

/// C++ `CachedBlock::getTransactionTreeHash`: tree hash of `[hash(miner_tx), tx_hashes...]`.
pub fn block_tx_tree_hash(block: &Block) -> Result<Hash, EncodeError> {
    let mut hashes = Vec::with_capacity(block.tx_hashes.len() + 1);
    hashes.push(transaction_hash(&block.miner_tx)?);
    hashes.extend_from_slice(&block.tx_hashes);
    Ok(tree_hash(&hashes))
}

/// C++ `CachedBlock::getBlockHashingBinaryArray`:
/// `serializeBlockHeader(block) || tree_hash || varint(tx_hashes.len() + 1)`.
pub fn block_hashing_blob(block: &Block) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(80);
    write_block_header(&mut out, &block.header)?;
    out.extend_from_slice(&block_tx_tree_hash(block)?.0);
    write_varint(&mut out, block.tx_hashes.len() as u64 + 1);
    Ok(out)
}

/// C++ `CachedBlock::getBlockHash`: `getObjectHash(hashing_blob [|| parent_hashing_blob])`
/// where the parent block part (major >= 2) is `getParentBlockHashingBinaryArray(false)`, and
/// the whole concatenation is length-prefixed before hashing (see [`get_object_hash_of_blob`]).
pub fn block_hash(block: &Block) -> Result<Hash, EncodeError> {
    let mut blob = block_hashing_blob(block)?;
    if block.header.major_version >= 2 {
        blob.extend_from_slice(&encode_parent_block(block, ParentBlockMode::Hashing)?);
    }
    Ok(get_object_hash_of_blob(&blob))
}

/// C++ `CachedBlock::getAuxiliaryBlockHeaderHash`: `getObjectHash(getBlockHashingBinaryArray())`,
/// the leaf proven by the merge-mining aux branch in `checkProofOfWorkV2`.
pub fn aux_block_header_hash(block: &Block) -> Result<Hash, EncodeError> {
    Ok(get_object_hash_of_blob(&block_hashing_blob(block)?))
}

/// The bytes fed to the slow hash by C++ `CachedBlock::getBlockLongHash`: the hashing blob for
/// major version 1, `getParentBlockHashingBinaryArray(true)` (header-only parent block with
/// merkle root) for major >= 2. No length prefix.
pub fn block_pow_blob(block: &Block) -> Result<Vec<u8>, EncodeError> {
    if block.header.major_version == 1 {
        block_hashing_blob(block)
    } else {
        encode_parent_block(block, ParentBlockMode::HashingHeaderOnly)
    }
}

/// C++ `CachedBlock::getBlockLongHash`: the proof-of-work hash.
///
/// | major | algorithm |
/// |---|---|
/// | 1, 2, 3 | `cn_slow_hash_v0` (CryptoNight) |
/// | 4 | `cn_lite_slow_hash_v1` (CryptoNight-Lite variant 1) |
/// | >= 5 | `cn_turtle_lite_slow_hash_v2` (CryptoNight-Turtle-Lite variant 2) |
///
/// Major 5 and later hash on the calling thread's reused hasher
/// ([`xkr_crypto::cn_turtle_lite_slow_hash_v2_reused`]).
pub fn block_long_hash(block: &Block) -> Result<Hash, PowError> {
    let major = block.header.major_version;
    let blob = block_pow_blob(block)?;
    match major {
        1..=3 => Ok(cn_slow_hash_v0(&blob)),
        4 => Ok(cn_lite_slow_hash_v1(&blob)?),
        5.. => Ok(cn_turtle_lite_slow_hash_v2_reused(&blob)),
        0 => Err(PowError::UnknownMajorVersion(major)),
    }
}
