//! xkr-serialization: byte-exact CryptoNote binary format of the Kryptokrona C++ node.
//!
//! Mirrors `BinaryOutputStreamSerializer` / `BinaryInputStreamSerializer`
//! (`src/serialization/`), `cryptonote_serialization.cpp` and the hashing helpers of
//! `cached_block.cpp`, `cached_transaction.cpp` and `cryptonote_tools.h`:
//!
//! * [`varint`]: `writeVarint` / `readVarint<T>` with the C++ overflow and canonical-form rules;
//! * transaction version 2 (fork 14, `docs/tx-v2.md`): tagged outputs and CLSAG signatures;
//! * [`decode_transaction`], [`encode_transaction`], [`decode_block`], [`encode_block`],
//!   [`decode_raw_block`], [`encode_raw_block`];
//! * hashing: [`transaction_hash`], [`transaction_prefix_hash`], [`block_hashing_blob`],
//!   [`block_pow_blob`], [`block_hash`], [`block_long_hash`] (the PoW hash).
//!
//! Decoding never panics: malformed input yields a [`DecodeError`] naming the field and byte
//! offset. Encoding fails only for objects the C++ serializer would also refuse
//! ([`EncodeError`]).
#![forbid(unsafe_code)]

mod block;
mod error;
mod hashing;
mod reader;
mod tx;
pub mod varint;

pub use block::{
    MAX_BLOCK_MAJOR_VERSION, ParentBlockMode, decode_block, decode_raw_block, encode_block,
    encode_block_header, encode_parent_block, encode_raw_block,
};
pub use error::{DecodeError, EncodeError, PowError};
pub use hashing::{
    aux_block_header_hash, base_transaction_hash, block_hash, block_hashing_blob, block_long_hash,
    block_pow_blob, block_tx_tree_hash, get_object_hash_of_blob, parent_block_merkle_root,
    transaction_hash, transaction_hash_from_blob, transaction_prefix_hash,
};
pub use tx::{
    CURRENT_TRANSACTION_VERSION, MAX_PARENT_BLOCK_SIZE, MAX_TRANSACTION_VERSION, TAG_TAGGED_KEY,
    TRANSACTION_VERSION_2, decode_transaction, decode_transaction_prefix, encode_base_transaction,
    encode_transaction, encode_transaction_prefix,
};
