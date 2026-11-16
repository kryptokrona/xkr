//! Parameter checks of the C++ wallets (`src/errors/validate_parameters.cpp`), on the strings
//! the APIs receive, with the C++ error codes and custom messages.

use xkr_types::{Hash, PublicKey};
use xkr_wallet_core::address::ADDRESS_PREFIX;
use xkr_wallet_core::{Address, AddressError, base58, parse_payment_id};

use crate::error::{ErrorCode, WalletError};

/// `wallet_config::addressPrefix`.
pub const ADDRESS_PREFIX_TEXT: &str = "SEKR";
/// `wallet_config::addressPrefixAlt`.
pub const ADDRESS_PREFIX_ALT_TEXT: &str = "Xkr";
/// `wallet_config::standardAddressLength`.
pub const STANDARD_ADDRESS_LENGTH: usize = 99;
/// `wallet_config::standardAddressLengthAlt` (`Xkr…`).
pub const STANDARD_ADDRESS_LENGTH_ALT: usize = 98;
/// `wallet_config::integratedAddressLength`: the standard length plus 64 hex characters of
/// payment id in base58 (`64 * 11 / 8`).
pub const INTEGRATED_ADDRESS_LENGTH: usize = STANDARD_ADDRESS_LENGTH + 64 * 11 / 8;
/// `wallet_config::integratedAddressLengthAlt`.
pub const INTEGRATED_ADDRESS_LENGTH_ALT: usize = STANDARD_ADDRESS_LENGTH_ALT + 64 * 11 / 8;

/// `validateAddresses({address}, integratedAddressesAllowed)` followed by the parse: length,
/// prefix, integrated address rules, base58, payment id and keys.
pub fn validate_address(text: &str, integrated_allowed: bool) -> Result<Address, WalletError> {
    let len = text.len();
    if ![
        STANDARD_ADDRESS_LENGTH,
        INTEGRATED_ADDRESS_LENGTH,
        STANDARD_ADDRESS_LENGTH_ALT,
        INTEGRATED_ADDRESS_LENGTH_ALT,
    ]
    .contains(&len)
    {
        return Err(WalletError::with_message(
            ErrorCode::AddressWrongLength,
            format!(
                "The address given is the wrong length. It should be {STANDARD_ADDRESS_LENGTH} \
                 or {STANDARD_ADDRESS_LENGTH_ALT} chars (standard), or \
                 {INTEGRATED_ADDRESS_LENGTH} or {INTEGRATED_ADDRESS_LENGTH_ALT} chars \
                 (integrated), but it is {len} chars."
            ),
        ));
    }
    if !text.starts_with(ADDRESS_PREFIX_TEXT) && !text.starts_with(ADDRESS_PREFIX_ALT_TEXT) {
        return Err(ErrorCode::AddressWrongPrefix.into());
    }
    let integrated = len == INTEGRATED_ADDRESS_LENGTH || len == INTEGRATED_ADDRESS_LENGTH_ALT;
    if integrated && !integrated_allowed {
        return Err(WalletError::with_message(
            ErrorCode::AddressIsIntegrated,
            format!(
                "The address given ({text}) is an integrated address, but integrated addresses \
                 aren't valid for this parameter."
            ),
        ));
    }
    match Address::parse(text) {
        Ok(address) if address.payment_id.is_some() == integrated => Ok(address),
        Err(AddressError::Encoding) if integrated => Err(ErrorCode::AddressNotBase58.into()),
        Err(AddressError::PaymentId) => Err(ErrorCode::IntegratedAddressPaymentIdInvalid.into()),
        Ok(_) | Err(_) => Err(ErrorCode::AddressNotValid.into()),
    }
}

/// `validateHash`: 64 characters, hex.
pub fn validate_hash(text: &str) -> Result<Hash, WalletError> {
    if text.len() != 64 {
        return Err(ErrorCode::HashWrongLength.into());
    }
    text.parse()
        .map_err(|_| WalletError::new(ErrorCode::HashInvalid))
}

/// `validatePaymentID` and the hex decoding: `None` for the empty string.
pub fn validate_payment_id(text: &str) -> Result<Option<Hash>, WalletError> {
    parse_payment_id(text).map_err(WalletError::from)
}

/// `WalletBackend::createIntegratedAddress`: the payment id (64 hex characters, kept as given)
/// and the keys of a standard address, encoded with the `SEKR` prefix.
pub fn create_integrated_address(address: &str, payment_id: &str) -> Result<String, WalletError> {
    if payment_id.is_empty() {
        return Err(ErrorCode::PaymentIdWrongLength.into());
    }
    validate_payment_id(payment_id)?;
    let address = validate_address(address, false)?;
    let mut data = Vec::with_capacity(128);
    data.extend_from_slice(payment_id.as_bytes());
    data.extend_from_slice(&address.spend.0);
    data.extend_from_slice(&address.view.0);
    Ok(base58::encode_addr(ADDRESS_PREFIX, &data))
}

/// The public spend key of a valid standard or integrated address.
#[must_use]
pub fn spend_key_of(text: &str) -> Option<PublicKey> {
    Address::parse(text).ok().map(|a| a.spend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xkr_wallet_core::WalletKeys;

    #[test]
    fn addresses_are_checked_like_cpp() {
        let keys = WalletKeys::generate(&mut xkr_crypto::os_rng());
        let text = keys.address().to_string();
        assert_eq!(text.len(), STANDARD_ADDRESS_LENGTH);
        assert_eq!(
            validate_address(&text, false).unwrap().spend,
            keys.spend_public
        );
        let e = validate_address("SEKRshort", false).unwrap_err();
        assert_eq!(e.code(), ErrorCode::AddressWrongLength);
        assert!(e.message().contains("but it is 9 chars"));
        let wrong_prefix = format!("TRTL{}", &text[4..]);
        assert_eq!(
            validate_address(&wrong_prefix, false).unwrap_err().code(),
            ErrorCode::AddressWrongPrefix
        );
        let mut typo = text.clone().into_bytes();
        typo[50] = if typo[50] == b'a' { b'b' } else { b'a' };
        let typo = String::from_utf8(typo).unwrap();
        assert_eq!(
            validate_address(&typo, false).unwrap_err().code(),
            ErrorCode::AddressNotValid
        );

        let pid = "ab".repeat(32);
        let integrated = create_integrated_address(&text, &pid).unwrap();
        assert_eq!(integrated.len(), INTEGRATED_ADDRESS_LENGTH);
        let parsed = validate_address(&integrated, true).unwrap();
        assert_eq!(parsed.payment_id, Some(Hash([0xab; 32])));
        assert_eq!(
            validate_address(&integrated, false).unwrap_err().code(),
            ErrorCode::AddressIsIntegrated
        );
        assert_eq!(
            create_integrated_address(&integrated, &pid)
                .unwrap_err()
                .code(),
            ErrorCode::AddressIsIntegrated
        );
        assert_eq!(
            create_integrated_address(&text, "abc").unwrap_err().code(),
            ErrorCode::PaymentIdWrongLength
        );
    }

    #[test]
    fn hashes() {
        assert!(validate_hash(&"00".repeat(32)).is_ok());
        assert_eq!(
            validate_hash("00").unwrap_err().code(),
            ErrorCode::HashWrongLength
        );
        assert_eq!(
            validate_hash(&"zz".repeat(32)).unwrap_err().code(),
            ErrorCode::HashInvalid
        );
    }
}
