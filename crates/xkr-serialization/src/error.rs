//! Error types. Every C++ `throw std::runtime_error(...)` on the deserialization path maps to a
//! [`DecodeError`] variant; serialization-side throws map to [`EncodeError`].

/// Malformed binary input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// Ran out of bytes.
    #[error("unexpected end of input at offset {offset} while reading {what}")]
    UnexpectedEof {
        /// Byte offset where the read started.
        offset: usize,
        /// Field being read.
        what: &'static str,
    },
    /// `readVarint, value overflow`.
    #[error("varint overflow at offset {offset} while reading {what}")]
    VarintOverflow {
        /// Byte offset of the varint.
        offset: usize,
        /// Field being read.
        what: &'static str,
    },
    /// `readVarint, invalid value representation` (trailing zero group).
    #[error("non-canonical varint at offset {offset} while reading {what}")]
    VarintNonCanonical {
        /// Byte offset of the varint.
        offset: usize,
        /// Field being read.
        what: &'static str,
    },
    /// A length prefix larger than the remaining input (C++ would fail later at EOF).
    #[error("{what}: length {len} at offset {offset} exceeds the {remaining} remaining bytes")]
    LengthTooLarge {
        /// Byte offset of the length prefix.
        offset: usize,
        /// Field being read.
        what: &'static str,
        /// Declared length / element count.
        len: u64,
        /// Bytes left in the input.
        remaining: usize,
    },
    /// `Wrong transaction version` (version > [`crate::MAX_TRANSACTION_VERSION`]), or a
    /// version-2 transaction with a version-1 (`0x02`) output.
    #[error("unsupported transaction version {0}")]
    WrongTransactionVersion(u8),
    /// A coinbase input (at this index) in a transaction with more than one input. A coinbase
    /// input is only legal as the single input of a miner transaction (C++ `validateBlock` and
    /// `validateTransactionInputs`), so such a transaction is invalid in every context; refusing
    /// it while decoding bounds the decoder's memory.
    #[error("coinbase input {0} in a transaction with more than one input")]
    CoinbaseInputInSignedTransaction(usize),
    /// `Unknown variant tag` for an input.
    #[error("unknown input type tag {tag:#04x} at offset {offset}")]
    UnknownInputTag {
        /// The tag byte.
        tag: u8,
        /// Its offset.
        offset: usize,
    },
    /// `Unknown variant tag` for an output target.
    #[error("unknown output type tag {tag:#04x} at offset {offset}")]
    UnknownOutputTag {
        /// The tag byte.
        tag: u8,
        /// Its offset.
        offset: usize,
    },
    /// `Wrong major version` (0 or above [`crate::MAX_BLOCK_MAJOR_VERSION`]).
    #[error("unsupported block major version {0}")]
    WrongBlockMajorVersion(u8),
    /// `Wrong transactions number` in the parent block.
    #[error("parent block transaction count is zero")]
    ZeroParentTransactionCount,
    /// `Can't get extra merge mining tag`.
    #[error("parent block miner transaction extra has no merge-mining tag")]
    MissingMergeMiningTag,
    /// `Wrong merge mining tag depth`.
    #[error("merge-mining tag depth {0} exceeds 256")]
    MergeMiningDepthTooLarge(u64),
    /// `Serialization error: signatures are not expected` (unreachable for decoded inputs but
    /// kept for completeness).
    #[error("signatures not expected for input {0}")]
    SignaturesNotExpected(usize),
    /// `failed to unpack type`: bytes left after the object.
    #[error("{trailing} trailing bytes after {what}")]
    TrailingBytes {
        /// Object that was decoded.
        what: &'static str,
        /// Number of unread bytes.
        trailing: usize,
    },
}

/// An object the C++ serializer refuses to write.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// `Serialization error: unexpected signatures size` (outer vector).
    #[error("transaction has {inputs} inputs but {signatures} signature vectors")]
    SignatureVectorCount {
        /// Number of inputs.
        inputs: usize,
        /// Number of signature vectors.
        signatures: usize,
    },
    /// `Serialization error: unexpected signatures size` (per input).
    #[error("input {input} needs {expected} signatures, has {got}")]
    SignatureCount {
        /// Input index.
        input: usize,
        /// Ring size.
        expected: usize,
        /// Signatures present.
        got: usize,
    },
    /// `Serialization error: signatures are not expected`.
    #[error("signatures missing for key input {0}")]
    SignaturesNotExpected(usize),
    /// Legacy signatures on a version-2 transaction or CLSAG signatures on a version 0/1 one.
    #[error("signature format does not match transaction version {version}")]
    SignatureFormat {
        /// The transaction version.
        version: u8,
    },
    /// A version-2 transaction with key inputs also has a coinbase input (at this index).
    #[error("coinbase input {0} in a signed version-2 transaction")]
    CoinbaseInputInSignedTransaction(usize),
    /// `Wrong major version`.
    #[error("unsupported block major version {0}")]
    WrongBlockMajorVersion(u8),
    /// Major version >= 2 without a parent block.
    #[error("block major version {0} requires a parent block")]
    MissingParentBlock(u8),
    /// `Wrong transactions number`.
    #[error("parent block transaction count is zero")]
    ZeroParentTransactionCount,
    /// `Wrong miner transaction branch size`.
    #[error("parent miner transaction branch has {got} hashes, expected {expected}")]
    BaseTransactionBranchLength {
        /// `tree_depth(transaction_count)`.
        expected: usize,
        /// Actual length.
        got: usize,
    },
    /// `Can't get extra merge mining tag`.
    #[error("parent block miner transaction extra has no merge-mining tag")]
    MissingMergeMiningTag,
    /// `Wrong merge mining tag depth`.
    #[error("merge-mining tag depth {0} exceeds 256")]
    MergeMiningDepthTooLarge(u64),
    /// `Blockchain branch size must be equal to merge mining tag depth`.
    #[error("blockchain branch has {got} hashes, merge-mining tag depth is {expected}")]
    BlockchainBranchLength {
        /// Merge-mining tag depth.
        expected: u64,
        /// Actual length.
        got: usize,
    },
}

/// Failure computing the proof-of-work hash.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PowError {
    /// The block could not be serialized.
    #[error(transparent)]
    Encode(#[from] EncodeError),
    /// `Unknown block major version.`
    #[error("no proof-of-work algorithm for block major version {0}")]
    UnknownMajorVersion(u8),
    /// CryptoNight variant 1 input shorter than 43 bytes.
    #[error(transparent)]
    DataTooShort(#[from] xkr_crypto::cryptonight::DataCanNotBeHashed),
}
