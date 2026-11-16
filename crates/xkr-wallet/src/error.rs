//! Errors of the wallet engine with the numbers and texts of the C++ wallets.
//!
//! [`ErrorCode`] is `src/errors/errors.h` `ErrorCode` (numbers 0 to 50) with the default texts of
//! `src/errors/errors.cpp` `Error::getErrorMessage`. [`WalletError`] is C++ `Error`: a code and an
//! optional custom message (C++ `Error(code, customMessage)`), plus two things C++ does not
//! carry: a [`WalletError::detail`] text for logs (for example the node's reason for refusing a
//! transaction, which C++ drops) and, for wallet file errors, the `WalletGreen` code that
//! walletd reports ([`WalletError::walletd_code`]).

use std::fmt;

use xkr_wallet_core::{MnemonicError, PaymentIdError, PolicyError, SelectError};
use xkr_wallet_files::FileError;

/// `src/errors/errors.h` `ErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum ErrorCode {
    /// `SUCCESS`.
    Success = 0,
    /// `FILENAME_NON_EXISTENT`.
    FilenameNonExistent = 1,
    /// `INVALID_WALLET_FILENAME`.
    InvalidWalletFilename = 2,
    /// `NOT_A_WALLET_FILE`.
    NotAWalletFile = 3,
    /// `WALLET_FILE_CORRUPTED`.
    WalletFileCorrupted = 4,
    /// `WRONG_PASSWORD`.
    WrongPassword = 5,
    /// `UNSUPPORTED_WALLET_FILE_FORMAT_VERSION`.
    UnsupportedWalletFileFormatVersion = 6,
    /// `INVALID_MNEMONIC`.
    InvalidMnemonic = 7,
    /// `WALLET_FILE_ALREADY_EXISTS`.
    WalletFileAlreadyExists = 8,
    /// `WILL_OVERFLOW`.
    WillOverflow = 9,
    /// `ADDRESS_NOT_IN_WALLET`.
    AddressNotInWallet = 10,
    /// `NOT_ENOUGH_BALANCE`.
    NotEnoughBalance = 11,
    /// `ADDRESS_WRONG_LENGTH`.
    AddressWrongLength = 12,
    /// `ADDRESS_WRONG_PREFIX`.
    AddressWrongPrefix = 13,
    /// `ADDRESS_NOT_BASE58`.
    AddressNotBase58 = 14,
    /// `ADDRESS_NOT_VALID`.
    AddressNotValid = 15,
    /// `INTEGRATED_ADDRESS_PAYMENT_ID_INVALID`.
    IntegratedAddressPaymentIdInvalid = 16,
    /// `FEE_TOO_SMALL`.
    FeeTooSmall = 17,
    /// `NO_DESTINATIONS_GIVEN`.
    NoDestinationsGiven = 18,
    /// `AMOUNT_IS_ZERO`.
    AmountIsZero = 19,
    /// `FAILED_TO_CREATE_RING_SIGNATURE`.
    FailedToCreateRingSignature = 20,
    /// `MIXIN_TOO_SMALL`.
    MixinTooSmall = 21,
    /// `MIXIN_TOO_BIG`.
    MixinTooBig = 22,
    /// `PAYMENT_ID_WRONG_LENGTH`.
    PaymentIdWrongLength = 23,
    /// `PAYMENT_ID_INVALID`.
    PaymentIdInvalid = 24,
    /// `ADDRESS_IS_INTEGRATED`.
    AddressIsIntegrated = 25,
    /// `CONFLICTING_PAYMENT_IDS`.
    ConflictingPaymentIds = 26,
    /// `CANT_GET_FAKE_OUTPUTS`.
    CantGetFakeOutputs = 27,
    /// `NOT_ENOUGH_FAKE_OUTPUTS`.
    NotEnoughFakeOutputs = 28,
    /// `INVALID_GENERATED_KEYIMAGE`.
    InvalidGeneratedKeyimage = 29,
    /// `DAEMON_OFFLINE`.
    DaemonOffline = 30,
    /// `DAEMON_ERROR`.
    DaemonError = 31,
    /// `TOO_MANY_INPUTS_TO_FIT_IN_BLOCK`.
    TooManyInputsToFitInBlock = 32,
    /// `MNEMONIC_INVALID_WORD`.
    MnemonicInvalidWord = 33,
    /// `MNEMONIC_WRONG_LENGTH`.
    MnemonicWrongLength = 34,
    /// `MNEMONIC_INVALID_CHECKSUM`.
    MnemonicInvalidChecksum = 35,
    /// `FULLY_OPTIMIZED`.
    FullyOptimized = 36,
    /// `FUSION_MIXIN_TOO_LARGE`.
    FusionMixinTooLarge = 37,
    /// `SUBWALLET_ALREADY_EXISTS`.
    SubwalletAlreadyExists = 38,
    /// `ILLEGAL_VIEW_WALLET_OPERATION`.
    IllegalViewWalletOperation = 39,
    /// `ILLEGAL_NON_VIEW_WALLET_OPERATION`.
    IllegalNonViewWalletOperation = 40,
    /// `KEYS_NOT_DETERMINISTIC`.
    KeysNotDeterministic = 41,
    /// `CANNOT_DELETE_PRIMARY_ADDRESS`.
    CannotDeletePrimaryAddress = 42,
    /// `TX_PRIVATE_KEY_NOT_FOUND`.
    TxPrivateKeyNotFound = 43,
    /// `AMOUNTS_NOT_PRETTY`.
    AmountsNotPretty = 44,
    /// `UNEXPECTED_FEE`.
    UnexpectedFee = 45,
    /// `NEGATIVE_VALUE_GIVEN`.
    NegativeValueGiven = 46,
    /// `INVALID_KEY_FORMAT`.
    InvalidKeyFormat = 47,
    /// `HASH_WRONG_LENGTH`.
    HashWrongLength = 48,
    /// `HASH_INVALID`.
    HashInvalid = 49,
    /// `NON_INTEGER_GIVEN`.
    NonIntegerGiven = 50,
}

impl ErrorCode {
    /// Every code, indexed by its number.
    pub const ALL: [Self; 51] = [
        Self::Success,
        Self::FilenameNonExistent,
        Self::InvalidWalletFilename,
        Self::NotAWalletFile,
        Self::WalletFileCorrupted,
        Self::WrongPassword,
        Self::UnsupportedWalletFileFormatVersion,
        Self::InvalidMnemonic,
        Self::WalletFileAlreadyExists,
        Self::WillOverflow,
        Self::AddressNotInWallet,
        Self::NotEnoughBalance,
        Self::AddressWrongLength,
        Self::AddressWrongPrefix,
        Self::AddressNotBase58,
        Self::AddressNotValid,
        Self::IntegratedAddressPaymentIdInvalid,
        Self::FeeTooSmall,
        Self::NoDestinationsGiven,
        Self::AmountIsZero,
        Self::FailedToCreateRingSignature,
        Self::MixinTooSmall,
        Self::MixinTooBig,
        Self::PaymentIdWrongLength,
        Self::PaymentIdInvalid,
        Self::AddressIsIntegrated,
        Self::ConflictingPaymentIds,
        Self::CantGetFakeOutputs,
        Self::NotEnoughFakeOutputs,
        Self::InvalidGeneratedKeyimage,
        Self::DaemonOffline,
        Self::DaemonError,
        Self::TooManyInputsToFitInBlock,
        Self::MnemonicInvalidWord,
        Self::MnemonicWrongLength,
        Self::MnemonicInvalidChecksum,
        Self::FullyOptimized,
        Self::FusionMixinTooLarge,
        Self::SubwalletAlreadyExists,
        Self::IllegalViewWalletOperation,
        Self::IllegalNonViewWalletOperation,
        Self::KeysNotDeterministic,
        Self::CannotDeletePrimaryAddress,
        Self::TxPrivateKeyNotFound,
        Self::AmountsNotPretty,
        Self::UnexpectedFee,
        Self::NegativeValueGiven,
        Self::InvalidKeyFormat,
        Self::HashWrongLength,
        Self::HashInvalid,
        Self::NonIntegerGiven,
    ];

    /// The C++ number (`errorCode` in wallet-api answers).
    #[must_use]
    pub const fn number(self) -> u16 {
        self as u16
    }

    /// The code with C++ number `number`.
    #[must_use]
    pub fn from_number(number: u32) -> Option<Self> {
        usize::try_from(number)
            .ok()
            .and_then(|n| Self::ALL.get(n))
            .copied()
    }

    /// `Error::getErrorMessage` without a custom message.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Success => "The operation completed successfully.",
            Self::FilenameNonExistent => {
                "The filename you are attempting to open does not exist, or the wallet does not \
                 have permission to open it."
            }
            Self::InvalidWalletFilename => {
                "We could not open/save to the filename given. Possibly invalid characters, or \
                 permission issues."
            }
            Self::NotAWalletFile => {
                "This file is not a wallet file, or is not a wallet file type supported by this \
                 wallet version."
            }
            Self::WalletFileCorrupted => "This wallet file appears to have gotten corrupted.",
            Self::WrongPassword => "The password given for this wallet is incorrect.",
            Self::UnsupportedWalletFileFormatVersion => {
                "This wallet file appears to be from a newer or older version of the software, \
                 that we do not support."
            }
            Self::InvalidMnemonic => "The mnemonic seed given is invalid.",
            Self::WalletFileAlreadyExists => {
                "The wallet file you are attempting to create already exists. Please delete it \
                 first."
            }
            Self::AddressNotInWallet => {
                "The address given does not exist in the wallet container, but is required to \
                 exist for this operation."
            }
            Self::NotEnoughBalance => {
                "Not enough unlocked funds were found to cover this transaction in the subwallets \
                 specified (or all wallets, if not specified. (Sum of amounts + fee + node fee)"
            }
            Self::AddressWrongLength => "The address given is too short or too long.",
            Self::AddressWrongPrefix => {
                "The address does not have the correct prefix corresponding to this coin - it \
                 appears to be an address for another cryptocurrency."
            }
            Self::AddressNotBase58 => {
                "The address contains invalid characters, that are not in the base58 set."
            }
            Self::AddressNotValid => {
                "The address given is not valid. Possibly invalid checksum. Most likely a typo."
            }
            Self::IntegratedAddressPaymentIdInvalid => {
                "The payment ID stored in the integrated address supplied is not valid."
            }
            Self::FeeTooSmall => {
                "The fee given for this transaction is below the minimum allowed network fee."
            }
            Self::NoDestinationsGiven => "The destinations array (amounts/addresses) is empty.",
            Self::AmountIsZero => "One of the destination parameters has an amount given of zero.",
            Self::FailedToCreateRingSignature => {
                "Failed to create ring signature - probably a programmer error, or a corrupted \
                 wallet."
            }
            Self::MixinTooSmall => {
                "The mixin value given is too low to be accepted by the network (based on the \
                 current height known by the wallet)"
            }
            Self::MixinTooBig => {
                "The mixin value given is too high to be accepted by the network (based on the \
                 current height known by the wallet)"
            }
            Self::PaymentIdWrongLength => "The payment ID given is not 64 characters long.",
            Self::PaymentIdInvalid => "The payment ID given is not a hex string (A-Za-z0-9)",
            Self::AddressIsIntegrated => {
                "The address given is an integrated address, but integrated addresses aren't \
                 valid for this parameter, for example, change address."
            }
            Self::ConflictingPaymentIds => {
                "Conflicting payment IDs were given. This could mean an integrated address + \
                 payment ID were given, where they are not the same, or that multiple integrated \
                 addresses with different payment IDs were given."
            }
            Self::CantGetFakeOutputs => {
                "Failed to get fake outputs from the daemon to obscure our transaction, and mixin \
                 is not zero."
            }
            Self::NotEnoughFakeOutputs => {
                "We could not get enough fake outputs for this transaction to complete. If \
                 possible, try lowering the mixin value used, or decrease the amount you are \
                 sending."
            }
            Self::InvalidGeneratedKeyimage => {
                "The key image we generated is invalid - probably a programmer error, or a \
                 corrupted wallet."
            }
            Self::DaemonOffline => {
                "We were not able to submit our request to the daemon. Ensure it is online and \
                 not frozen."
            }
            Self::DaemonError => {
                "An error occured whilst the daemon processed the request. Possibly our software \
                 is outdated, the daemon is faulty, or there is a programmer error. Check your \
                 daemon logs for more info. (set_log 4)"
            }
            Self::TooManyInputsToFitInBlock => {
                "The transaction is too large (in BYTES, not AMOUNT) to fit in a block. Either \
                 decrease the amount you are sending, perform fusion transactions, or decrease \
                 mixin (if possible)."
            }
            Self::MnemonicInvalidWord => {
                "The mnemonic seed given has a word that is not present in the english word list."
            }
            Self::MnemonicWrongLength => "The mnemonic seed given is the wrong length.",
            Self::MnemonicInvalidChecksum => {
                "The mnemonic seed given has an invalid checksum word."
            }
            Self::FullyOptimized => {
                "Cannot send fusion transaction - wallet is already fully optimized."
            }
            Self::FusionMixinTooLarge => {
                "Cannot send fusion transacton - mixin is too large to meet input/output ratio \
                 requirements whilst remaining in size constraints."
            }
            Self::SubwalletAlreadyExists => "A subwallet with the given key already exists.",
            Self::IllegalViewWalletOperation => {
                "This function cannot be called when using a view wallet."
            }
            Self::IllegalNonViewWalletOperation => {
                "This function can only be used when using a view wallet."
            }
            Self::WillOverflow => {
                "This operation will cause integer overflow. Please decrease the amounts you are \
                 sending."
            }
            Self::KeysNotDeterministic => {
                "You cannot get a mnemonic seed for this address, as the view key is derived in \
                 terms of the spend key."
            }
            Self::CannotDeletePrimaryAddress => {
                "Each wallet has a primary address when created, this address cannot be removed."
            }
            Self::TxPrivateKeyNotFound => {
                "Couldn't find the private key for this transaction. The transaction must exist, \
                 and have been sent by this program. Transaction private keys cannot be found \
                 upon rescanning/reimporting."
            }
            Self::AmountsNotPretty => {
                "The created transaction isn't comprised of only 'Pretty' amounts. This will \
                 cause the outputs to be unmixable. Almost certainly a programmer error. \
                 Cancelling transaction."
            }
            Self::UnexpectedFee => {
                "The fee of the created transaction is not the same as that which was specified \
                 (0 for fusion transactions). Almost certainly a programmer error. Cancelling \
                 transaction."
            }
            Self::NegativeValueGiven => {
                "The input for this operation must be greater than or equal to zero, but a \
                 negative number was given."
            }
            Self::InvalidKeyFormat => {
                "The public/private key or hash given is not a 64 char hex string."
            }
            Self::HashWrongLength => "The hash given is not 64 characters long.",
            Self::HashInvalid => "The hash given is not a hex string (A-Za-z0-9)",
            // C++ has no text for this code; wallet-backend-js `WalletError.ts` has this one.
            Self::NonIntegerGiven => "The number given was a float, not an integer.",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// `WalletGreen` (`src/wallet/wallet_errors.h`) numbers the engine sets where walletd answers
/// with them and the `WalletBackend` codes have no match.
pub mod walletd_codes {
    /// `WRONG_PASSWORD`.
    pub const WRONG_PASSWORD: u32 = 4;
    /// `BAD_ADDRESS`.
    pub const BAD_ADDRESS: u32 = 7;
    /// `WRONG_VERSION`.
    pub const WRONG_VERSION: u32 = 16;
    /// `OBJECT_NOT_FOUND`.
    pub const OBJECT_NOT_FOUND: u32 = 23;
    /// `EXTRA_TOO_LARGE`.
    pub const EXTRA_TOO_LARGE: u32 = 34;
}

/// A failed wallet operation: C++ `Error` (code and optional custom message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletError {
    code: ErrorCode,
    message: Option<String>,
    detail: Option<String>,
    walletd_code: Option<u32>,
}

impl WalletError {
    /// `Error(code)`: the default message.
    #[must_use]
    pub const fn new(code: ErrorCode) -> Self {
        Self {
            code,
            message: None,
            detail: None,
            walletd_code: None,
        }
    }

    /// `Error(code, customMessage)`.
    #[must_use]
    pub fn with_message(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            ..Self::new(code)
        }
    }

    /// The same error with a detail text for logs (not part of the C++ message).
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// The same error with the `WalletGreen` code walletd reports for it.
    #[must_use]
    pub const fn with_walletd_code(mut self, code: u32) -> Self {
        self.walletd_code = Some(code);
        self
    }

    /// The code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    /// The C++ number of the code.
    #[must_use]
    pub const fn number(&self) -> u16 {
        self.code.number()
    }

    /// The message: the custom one if there is one, else the code's (`Error::getErrorMessage`).
    #[must_use]
    pub fn message(&self) -> &str {
        self.message.as_deref().unwrap_or(self.code.message())
    }

    /// Extra information for logs, if any (for example the node's reason for refusing a
    /// transaction).
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// The `WalletGreen` application code walletd answers with, when the engine knows one (wallet
    /// file errors and the few walletd-only conditions in [`walletd_codes`]). xkr-walletd maps
    /// every other error from [`WalletError::code`].
    #[must_use]
    pub const fn walletd_code(&self) -> Option<u32> {
        self.walletd_code
    }
}

impl fmt::Display for WalletError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for WalletError {}

impl From<ErrorCode> for WalletError {
    fn from(code: ErrorCode) -> Self {
        Self::new(code)
    }
}

impl From<FileError> for WalletError {
    /// The C++ code of the file error (`FileError::code`), the `WalletGreen` code for walletd,
    /// and what did not parse as the detail.
    fn from(e: FileError) -> Self {
        let code = ErrorCode::from_number(e.code()).unwrap_or(ErrorCode::WalletFileCorrupted);
        let mut out = Self::new(code);
        match &e {
            FileError::WalletFileCorrupted(what) => out.detail = Some(what.clone()),
            FileError::InvalidWalletFilename(io) => out.detail = Some(io.to_string()),
            _ => {}
        }
        out.walletd_code = e.green_code();
        out
    }
}

impl From<MnemonicError> for WalletError {
    /// The C++ code with the message of `Mnemonics::MnemonicToPrivateKey`.
    fn from(e: MnemonicError) -> Self {
        let code =
            ErrorCode::from_number(u32::from(e.code())).unwrap_or(ErrorCode::InvalidMnemonic);
        Self::with_message(code, e.to_string())
    }
}

impl From<PaymentIdError> for WalletError {
    fn from(e: PaymentIdError) -> Self {
        Self::new(
            ErrorCode::from_number(u32::from(e.code())).unwrap_or(ErrorCode::PaymentIdInvalid),
        )
    }
}

impl From<PolicyError> for WalletError {
    /// The C++ code; the mixin and size errors carry the C++ custom messages.
    fn from(e: PolicyError) -> Self {
        let code = ErrorCode::from_number(u32::from(e.code())).unwrap_or(ErrorCode::DaemonError);
        match e {
            PolicyError::FeeTooSmall => Self::new(code),
            _ => Self::with_message(code, e.to_string()),
        }
    }
}

impl From<SelectError> for WalletError {
    fn from(e: SelectError) -> Self {
        Self::new(
            ErrorCode::from_number(u32::from(e.code())).unwrap_or(ErrorCode::NotEnoughBalance),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_messages() {
        for (n, code) in ErrorCode::ALL.iter().enumerate() {
            assert_eq!(usize::from(code.number()), n);
            assert_eq!(ErrorCode::from_number(n as u32), Some(*code));
        }
        assert_eq!(ErrorCode::from_number(51), None);
        assert_eq!(ErrorCode::NotEnoughBalance.number(), 11);
        assert_eq!(ErrorCode::KeysNotDeterministic.number(), 41);
        assert_eq!(
            WalletError::new(ErrorCode::WrongPassword).to_string(),
            "The password given for this wallet is incorrect."
        );
        let custom = WalletError::with_message(ErrorCode::MixinTooBig, "x");
        assert_eq!((custom.number(), custom.message()), (22, "x"));
    }

    #[test]
    fn conversions_keep_the_cpp_codes() {
        let e = WalletError::from(FileError::WrongPassword);
        assert_eq!((e.number(), e.walletd_code()), (5, Some(4)));
        let e = WalletError::from(FileError::NotAWalletFile);
        assert_eq!((e.number(), e.walletd_code()), (3, None));
        let e = WalletError::from(MnemonicError::InvalidChecksum);
        assert_eq!(e.number(), 35);
        let e = WalletError::from(PolicyError::MixinTooSmall { mixin: 1, min: 3 });
        assert_eq!(e.number(), 21);
        assert_eq!(
            e.message(),
            "The mixin value given (1) is lower than the minimum mixin allowed (3)"
        );
        let e = WalletError::from(SelectError::FullyOptimized);
        assert_eq!(e.number(), 36);
        assert_eq!(WalletError::from(PaymentIdError::WrongLength).number(), 23);
    }
}
