//! The error type of the core API and its mapping to the status codes of the C ABI.

/// Why a core function refused its input or could not produce a result.
///
/// The `&'static str` in each variant names the argument (in the JavaScript spelling, e.g.
/// `"publicKey"`), so a rejected promise says which argument was wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    /// Not a hexadecimal string (a non-hex character or an odd number of characters).
    #[error("{0}: not a hexadecimal string")]
    InvalidHex(&'static str),
    /// A fixed-size value with the wrong number of hex characters.
    #[error("{what}: expected {expected} hex characters, got {got}")]
    InvalidLength {
        /// The argument.
        what: &'static str,
        /// Required number of hex characters.
        expected: usize,
        /// Number of characters given.
        got: usize,
    },
    /// The bytes are not the encoding of a curve point.
    #[error("{0}: not a valid public key")]
    InvalidPublicKey(&'static str),
    /// The bytes are not a canonical scalar (`< l`).
    #[error("{0}: not a valid private key or scalar")]
    InvalidScalar(&'static str),
    /// An index outside its array, or a JavaScript number that is not a safe non-negative
    /// integer.
    #[error("{0}: invalid index")]
    InvalidIndex(&'static str),
    /// Any other argument that does not fit the function (wrong array size, ...).
    #[error("{0}: invalid argument")]
    InvalidArgument(&'static str),
    /// The JSON input of `processBlockOutputs` does not have the expected shape.
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    /// Signing refused the inputs (a ring member or key image that does not decode, or a
    /// secret that does not belong to the real ring member).
    #[error("could not sign with the given keys")]
    SigningFailed,
    /// The data cannot be hashed with the requested CryptoNight variant (variant 1 needs at
    /// least 43 bytes).
    #[error("data can not be hashed with this CryptoNight variant")]
    DataCanNotBeHashed,
}

/// Result of the core API.
pub type CryptoResult<T> = Result<T, CryptoError>;

/// Status codes of the C ABI (`include/xkr_crypto_ffi.h`). Zero is success, every error is
/// negative. The values are part of the ABI and never change meaning.
pub mod status {
    /// Success.
    pub const OK: i32 = 0;
    /// A required pointer argument was null.
    pub const NULL_POINTER: i32 = -1;
    /// A string argument is not hexadecimal (or not UTF-8).
    pub const INVALID_HEX: i32 = -2;
    /// A fixed-size hex argument has the wrong length.
    pub const INVALID_LENGTH: i32 = -3;
    /// A public key (point) does not decode.
    pub const INVALID_PUBLIC_KEY: i32 = -4;
    /// A private key or scalar is not canonical.
    pub const INVALID_SCALAR: i32 = -5;
    /// An index or count is out of range, or a number is not a safe non-negative integer.
    pub const INVALID_INDEX: i32 = -6;
    /// The caller's output buffer is too small.
    pub const BUFFER_TOO_SMALL: i32 = -7;
    /// A JSON argument does not parse or has the wrong shape.
    pub const INVALID_JSON: i32 = -8;
    /// Signing refused the keys.
    pub const SIGNING_FAILED: i32 = -9;
    /// Any other unusable argument.
    pub const INVALID_ARGUMENT: i32 = -10;
    /// The data can not be hashed with the requested variant.
    pub const DATA_CAN_NOT_BE_HASHED: i32 = -11;
    /// A Rust panic was caught at the ABI boundary (not expected in normal operation).
    pub const INTERNAL: i32 = -100;
}

impl CryptoError {
    /// The C ABI status code of this error.
    #[must_use]
    pub const fn status(&self) -> i32 {
        match self {
            Self::InvalidHex(_) => status::INVALID_HEX,
            Self::InvalidLength { .. } => status::INVALID_LENGTH,
            Self::InvalidPublicKey(_) => status::INVALID_PUBLIC_KEY,
            Self::InvalidScalar(_) => status::INVALID_SCALAR,
            Self::InvalidIndex(_) => status::INVALID_INDEX,
            Self::InvalidArgument(_) => status::INVALID_ARGUMENT,
            Self::InvalidJson(_) => status::INVALID_JSON,
            Self::SigningFailed => status::SIGNING_FAILED,
            Self::DataCanNotBeHashed => status::DATA_CAN_NOT_BE_HASHED,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_are_distinct_and_negative() {
        let errors = [
            CryptoError::InvalidHex("a"),
            CryptoError::InvalidLength {
                what: "a",
                expected: 64,
                got: 2,
            },
            CryptoError::InvalidPublicKey("a"),
            CryptoError::InvalidScalar("a"),
            CryptoError::InvalidIndex("a"),
            CryptoError::InvalidArgument("a"),
            CryptoError::InvalidJson(String::new()),
            CryptoError::SigningFailed,
            CryptoError::DataCanNotBeHashed,
        ];
        let mut codes: Vec<i32> = errors.iter().map(CryptoError::status).collect();
        assert!(codes.iter().all(|c| *c < 0));
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), errors.len());
    }
}
