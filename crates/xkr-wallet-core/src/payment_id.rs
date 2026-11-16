//! Payment ids given as text (`src/errors/validate_parameters.cpp` `validatePaymentID`,
//! `src/wallet_service/wallet_service.cpp` `checkPaymentId`).
//!
//! A payment id is 64 hex characters (32 bytes). The empty string means "no payment id". The
//! errors carry the C++ error codes and messages of wallet-api; walletd reports any failure as
//! `WRONG_PAYMENT_ID_FORMAT`.

use xkr_types::Hash;

/// Why a payment id does not parse ([`PaymentIdError::code`] is the C++ `WalletErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PaymentIdError {
    /// `PAYMENT_ID_WRONG_LENGTH` (23).
    #[error("The payment ID given is not 64 characters long.")]
    WrongLength,
    /// `PAYMENT_ID_INVALID` (24).
    #[error("The payment ID given is not a hex string (A-Za-z0-9)")]
    Invalid,
}

impl PaymentIdError {
    /// The C++ error code (`src/errors/errors.h` `WalletErrorCode`).
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::WrongLength => 23,
            Self::Invalid => 24,
        }
    }
}

/// `validatePaymentID` followed by the hex decoding: `Ok(None)` for the empty string, the
/// 32-byte id for 64 hex characters (either case), otherwise the C++ error. The length is
/// checked first (in bytes, as `std::string::length`).
pub fn parse_payment_id(text: &str) -> Result<Option<Hash>, PaymentIdError> {
    if text.is_empty() {
        return Ok(None);
    }
    if text.len() != 64 {
        return Err(PaymentIdError::WrongLength);
    }
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(text, &mut bytes).map_err(|_| PaymentIdError::Invalid)?;
    Ok(Some(Hash(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_wallet_api() {
        assert_eq!(parse_payment_id(""), Ok(None));
        let lower = "ab".repeat(32);
        assert_eq!(parse_payment_id(&lower), Ok(Some(Hash([0xab; 32]))));
        assert_eq!(
            parse_payment_id(&"AB".repeat(32)),
            Ok(Some(Hash([0xab; 32])))
        );
        let e = parse_payment_id(&"a".repeat(63)).unwrap_err();
        assert_eq!((e, e.code()), (PaymentIdError::WrongLength, 23));
        assert_eq!(
            e.to_string(),
            "The payment ID given is not 64 characters long."
        );
        assert_eq!(
            parse_payment_id(&"a".repeat(65)),
            Err(PaymentIdError::WrongLength)
        );
        // 64 bytes but not hex
        let e = parse_payment_id(&"g".repeat(64)).unwrap_err();
        assert_eq!((e, e.code()), (PaymentIdError::Invalid, 24));
        assert_eq!(
            e.to_string(),
            "The payment ID given is not a hex string (A-Za-z0-9)"
        );
        assert_eq!(
            parse_payment_id(&format!("{} ", "a".repeat(63))),
            Err(PaymentIdError::Invalid)
        );
        // multi-byte characters count in bytes: 32 two-byte characters are 64 bytes
        assert_eq!(
            parse_payment_id(&"é".repeat(32)),
            Err(PaymentIdError::Invalid)
        );
        assert_eq!(
            parse_payment_id(&"é".repeat(64)),
            Err(PaymentIdError::WrongLength)
        );
    }
}
