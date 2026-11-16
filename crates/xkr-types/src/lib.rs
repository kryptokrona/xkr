//! xkr-types: plain data types shared by every xkr crate.
//!
//! * [`Hash`], [`PublicKey`], [`SecretKey`], [`KeyImage`], [`KeyDerivation`], [`Signature`]:
//!   fixed-size byte newtypes (`bytemuck::Pod`, hex `Display`/`FromStr`).
//! * [`Block`], [`BlockHeader`], [`ParentBlock`], [`Transaction`], [`TransactionPrefix`],
//!   [`TxInput`], [`TxOutput`], [`TxOutputTarget`], [`RawBlock`]: the CryptoNote object model,
//!   field-for-field the same as the C++ `cryptonote.h` structs; [`ClsagSignature`],
//!   [`TxOutputTarget::TaggedKey`] and [`EcScalar`] for transaction version 2 (fork 14,
//!   `docs/tx-v2.md`).
//! * [`extra`]: transaction `extra` parsing (`parse_extra`, `tx_public_key`, ...), mirroring
//!   `transaction_extra.cpp`, plus the kryptokrona-utils / Hugin nonce sub-tags.
//!
//! No (de)serialization lives here; see `xkr-serialization`.
#![deny(unsafe_code)]

mod block;
pub mod extra;
mod keys;
mod transaction;

pub use block::{Block, BlockHeader, ParentBlock, RawBlock};
pub use extra::{
    ExtraError, ExtraField, MergeMiningTag, merge_mining_tag, parse_extra, parse_extra_partial,
    payment_id, tx_public_key,
};
pub use keys::{
    EcScalar, Hash, HexError, KeyDerivation, KeyImage, PublicKey, SecretKey, Signature,
};
pub use transaction::{
    ClsagSignature, TX_VERSION_2, Transaction, TransactionPrefix, TxInput, TxOutput,
    TxOutputTarget, absolute_to_relative, relative_to_absolute,
};
