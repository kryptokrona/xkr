//! The application codes walletd puts in `error.data.application_code`, with their messages.
//!
//! C++ walletd answers a failed method with `makeErrorResponse(ec)`
//! (`src/json_rpc_server/json_rpc_server.cpp` `JsonRpcServer::makeErrorResponse`):
//! `{"code": -32700, "message": ec.message(), "data": {"application_code": ec.value()}}`. The
//! `std::error_code` comes from one of several categories whose numbers overlap, so the number
//! alone does not tell the category; the message does:
//!
//! | category | C++ |
//! |---|---|
//! | [`WalletCode`] | `src/wallet/wallet_errors.h` `WalletErrorCategory` (`WalletGreen`) |
//! | [`ServiceCode`] | `src/wallet_service/wallet_service_error_category.h` `WalletServiceErrorCategory` |
//! | [`NodeCode`] | `src/node_rpc_proxy/node_errors.h` `NodeErrorCategory` |
//! | [`AppError::system`] | `std::generic_category()` (`errno`, e.g. `export` to an existing file) |
//!
//! [`from_wallet_error`] maps the engine's `WalletError` (the `WalletBackend` codes of
//! `src/errors/errors.h`, plus the `WalletGreen` code the engine sets for file errors) to the
//! code walletd would have answered.

use xkr_wallet::{ErrorCode, WalletError};

/// `src/wallet/wallet_errors.h` `WalletErrorCodes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum WalletCode {
    /// `NOT_INITIALIZED`.
    NotInitialized = 1,
    /// `ALREADY_INITIALIZED`.
    AlreadyInitialized = 2,
    /// `WRONG_STATE`.
    WrongState = 3,
    /// `WRONG_PASSWORD`.
    WrongPassword = 4,
    /// `INTERNAL_WALLET_ERROR`.
    InternalWalletError = 5,
    /// `MIXIN_COUNT_TOO_BIG`.
    MixinCountTooBig = 6,
    /// `BAD_ADDRESS`.
    BadAddress = 7,
    /// `TRANSACTION_SIZE_TOO_BIG`.
    TransactionSizeTooBig = 8,
    /// `WRONG_AMOUNT`.
    WrongAmount = 9,
    /// `SUM_OVERFLOW`.
    SumOverflow = 10,
    /// `ZERO_DESTINATION`.
    ZeroDestination = 11,
    /// `TX_CANCEL_IMPOSSIBLE`.
    TxCancelImpossible = 12,
    /// `TX_CANCELLED`.
    TxCancelled = 13,
    /// `OPERATION_CANCELLED`.
    OperationCancelled = 14,
    /// `TX_TRANSFER_IMPOSSIBLE`.
    TxTransferImpossible = 15,
    /// `WRONG_VERSION`.
    WrongVersion = 16,
    /// `FEE_TOO_SMALL`.
    FeeTooSmall = 17,
    /// `KEY_GENERATION_ERROR`.
    KeyGenerationError = 18,
    /// `INDEX_OUT_OF_RANGE`.
    IndexOutOfRange = 19,
    /// `ADDRESS_ALREADY_EXISTS`.
    AddressAlreadyExists = 20,
    /// `TRACKING_MODE`.
    TrackingMode = 21,
    /// `WRONG_PARAMETERS`.
    WrongParameters = 22,
    /// `OBJECT_NOT_FOUND`.
    ObjectNotFound = 23,
    /// `WALLET_NOT_FOUND`.
    WalletNotFound = 24,
    /// `CHANGE_ADDRESS_REQUIRED`.
    ChangeAddressRequired = 25,
    /// `CHANGE_ADDRESS_NOT_FOUND`.
    ChangeAddressNotFound = 26,
    /// `DESTINATION_ADDRESS_REQUIRED`.
    DestinationAddressRequired = 27,
    /// `DESTINATION_ADDRESS_NOT_FOUND`.
    DestinationAddressNotFound = 28,
    /// `BAD_PAYMENT_ID`.
    BadPaymentId = 29,
    /// `BAD_TRANSACTION_EXTRA`.
    BadTransactionExtra = 30,
    /// `MIXIN_BELOW_THRESHOLD`.
    MixinBelowThreshold = 31,
    /// `MIXIN_ABOVE_THRESHOLD`.
    MixinAboveThreshold = 32,
    /// `CONFLICTING_PAYMENT_IDS`.
    ConflictingPaymentIds = 33,
    /// `EXTRA_TOO_LARGE`.
    ExtraTooLarge = 34,
}

impl WalletCode {
    /// Every code, in number order.
    pub const ALL: [Self; 34] = [
        Self::NotInitialized,
        Self::AlreadyInitialized,
        Self::WrongState,
        Self::WrongPassword,
        Self::InternalWalletError,
        Self::MixinCountTooBig,
        Self::BadAddress,
        Self::TransactionSizeTooBig,
        Self::WrongAmount,
        Self::SumOverflow,
        Self::ZeroDestination,
        Self::TxCancelImpossible,
        Self::TxCancelled,
        Self::OperationCancelled,
        Self::TxTransferImpossible,
        Self::WrongVersion,
        Self::FeeTooSmall,
        Self::KeyGenerationError,
        Self::IndexOutOfRange,
        Self::AddressAlreadyExists,
        Self::TrackingMode,
        Self::WrongParameters,
        Self::ObjectNotFound,
        Self::WalletNotFound,
        Self::ChangeAddressRequired,
        Self::ChangeAddressNotFound,
        Self::DestinationAddressRequired,
        Self::DestinationAddressNotFound,
        Self::BadPaymentId,
        Self::BadTransactionExtra,
        Self::MixinBelowThreshold,
        Self::MixinAboveThreshold,
        Self::ConflictingPaymentIds,
        Self::ExtraTooLarge,
    ];

    /// The code with C++ number `n`.
    #[must_use]
    pub fn from_number(n: u32) -> Option<Self> {
        let i = usize::try_from(n).ok()?.checked_sub(1)?;
        Self::ALL.get(i).copied()
    }

    /// `WalletErrorCategory::message`.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotInitialized => "Object was not initialized",
            Self::WrongPassword => "The password is wrong",
            Self::AlreadyInitialized => "The object is already initialized",
            Self::InternalWalletError => "Internal error occurred",
            Self::MixinCountTooBig => "MixIn count is too big",
            Self::BadAddress => "Bad address",
            Self::TransactionSizeTooBig => "Transaction size is too big",
            Self::WrongAmount => "Wrong amount",
            Self::SumOverflow => "Sum overflow",
            Self::ZeroDestination => "The destination is empty",
            Self::TxCancelImpossible => "Impossible to cancel transaction",
            Self::WrongState => {
                "The wallet is in wrong state (maybe loading or saving), try again later"
            }
            Self::OperationCancelled => "The operation you've requested has been cancelled",
            Self::TxTransferImpossible => "Transaction transfer impossible",
            Self::WrongVersion => "Wrong version",
            Self::FeeTooSmall => "Transaction fee is too small",
            Self::KeyGenerationError => "Cannot generate new key",
            Self::IndexOutOfRange => "Index is out of range",
            Self::AddressAlreadyExists => "Address already exists",
            Self::TrackingMode => "The wallet is in tracking mode",
            Self::WrongParameters => "Wrong parameters passed",
            Self::ObjectNotFound => "Object not found",
            Self::WalletNotFound => "Requested wallet not found",
            Self::ChangeAddressRequired => "Change address required",
            Self::ChangeAddressNotFound => "Change address not found",
            Self::DestinationAddressRequired => "Destination address required",
            Self::DestinationAddressNotFound => "Destination address not found",
            Self::BadPaymentId => "Wrong payment id format",
            Self::BadTransactionExtra => "Wrong transaction extra format",
            Self::MixinBelowThreshold => "Mixin below minimum allowed threshold",
            Self::MixinAboveThreshold => "Mixin above maximum allowed threshold",
            Self::ConflictingPaymentIds => {
                "Multiple conflicting payment ID's were specified via the use of integrated \
                 addresses"
            }
            Self::ExtraTooLarge => "Transaction extra too large",
            // the category message for code 13
            Self::TxCancelled => "Unknown error",
        }
    }
}

/// `src/wallet_service/wallet_service_error_category.h` `WalletServiceErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ServiceCode {
    /// `WRONG_KEY_FORMAT`.
    WrongKeyFormat = 1,
    /// `WRONG_PAYMENT_ID_FORMAT`.
    WrongPaymentIdFormat = 2,
    /// `WRONG_HASH_FORMAT`.
    WrongHashFormat = 3,
    /// `OBJECT_NOT_FOUND`.
    ObjectNotFound = 4,
    /// `DUPLICATE_KEY`.
    DuplicateKey = 5,
    /// `KEYS_NOT_DETERMINISTIC`.
    KeysNotDeterministic = 6,
}

impl ServiceCode {
    /// `WalletServiceErrorCategory::message`.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::WrongKeyFormat => "Wrong key format",
            Self::WrongPaymentIdFormat => "Wrong payment id format",
            Self::WrongHashFormat => "Wrong block id format",
            Self::ObjectNotFound => "Requested object not found",
            Self::DuplicateKey => "Duplicate key",
            Self::KeysNotDeterministic => "Keys not deterministic",
        }
    }
}

/// `src/node_rpc_proxy/node_errors.h` `NodeErrorCodes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum NodeCode {
    /// `NOT_INITIALIZED`.
    NotInitialized = 1,
    /// `ALREADY_INITIALIZED`.
    AlreadyInitialized = 2,
    /// `NETWORK_ERROR`.
    NetworkError = 3,
    /// `NODE_BUSY`.
    NodeBusy = 4,
    /// `INTERNAL_NODE_ERROR`.
    InternalNodeError = 5,
    /// `REQUEST_ERROR`.
    RequestError = 6,
    /// `CONNECT_ERROR`.
    ConnectError = 7,
    /// `TIMEOUT`.
    Timeout = 8,
}

impl NodeCode {
    /// `NodeErrorCategory::message`.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotInitialized => "Object was not initialized",
            Self::AlreadyInitialized => "Object has been already initialized",
            Self::NetworkError => "Network error",
            Self::NodeBusy => "Node is busy",
            Self::InternalNodeError => "Internal node error",
            Self::RequestError => "Error in request parameters",
            Self::ConnectError => "Can't connect to daemon",
            Self::Timeout => "Operation timed out",
        }
    }
}

/// A failed walletd operation: `ec.value()` and `ec.message()` of the C++ `std::error_code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppError {
    /// `application_code`.
    pub code: i32,
    /// `message`.
    pub message: String,
}

impl AppError {
    /// A `WalletErrorCategory` code.
    #[must_use]
    pub fn wallet(code: WalletCode) -> Self {
        Self {
            code: code as i32,
            message: code.message().to_owned(),
        }
    }

    /// A `WalletServiceErrorCategory` code.
    #[must_use]
    pub fn service(code: ServiceCode) -> Self {
        Self {
            code: code as i32,
            message: code.message().to_owned(),
        }
    }

    /// A `NodeErrorCategory` code.
    #[must_use]
    pub fn node(code: NodeCode) -> Self {
        Self {
            code: code as i32,
            message: code.message().to_owned(),
        }
    }

    /// An `errno` of `std::generic_category()`, with the system's text for it (C++
    /// `strerror`).
    #[must_use]
    pub fn system(errno: i32) -> Self {
        let text = std::io::Error::from_raw_os_error(errno).to_string();
        // Rust appends " (os error N)" to strerror's text.
        let message = text
            .rsplit_once(" (os error ")
            .map_or(text.as_str(), |(m, _)| m)
            .to_owned();
        Self {
            code: errno,
            message,
        }
    }

    /// The error of a failed file operation (`std::system_error` from the file layer).
    #[must_use]
    pub fn io(e: &std::io::Error) -> Self {
        e.raw_os_error().map_or_else(
            || Self::wallet(WalletCode::InternalWalletError),
            Self::system,
        )
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl From<WalletCode> for AppError {
    fn from(code: WalletCode) -> Self {
        Self::wallet(code)
    }
}

impl From<ServiceCode> for AppError {
    fn from(code: ServiceCode) -> Self {
        Self::service(code)
    }
}

impl From<NodeCode> for AppError {
    fn from(code: NodeCode) -> Self {
        Self::node(code)
    }
}

impl From<&WalletError> for AppError {
    fn from(e: &WalletError) -> Self {
        from_wallet_error(e)
    }
}

impl From<WalletError> for AppError {
    fn from(e: WalletError) -> Self {
        from_wallet_error(&e)
    }
}

/// The walletd answer for an engine error: the `WalletGreen` code the engine set
/// ([`WalletError::walletd_code`]) if any, otherwise the code `WalletGreen`, `WalletService` or
/// `NodeRpcProxy` raises for the same condition. Anything without a counterpart is
/// `INTERNAL_WALLET_ERROR`, which is what `WalletService` answers for any other exception.
#[must_use]
pub fn from_wallet_error(e: &WalletError) -> AppError {
    if let Some(code) = e.walletd_code().and_then(WalletCode::from_number) {
        return AppError::wallet(code);
    }
    match e.code() {
        ErrorCode::WrongPassword => WalletCode::WrongPassword.into(),
        ErrorCode::UnsupportedWalletFileFormatVersion => WalletCode::WrongVersion.into(),
        ErrorCode::NotEnoughBalance => WalletCode::WrongAmount.into(),
        ErrorCode::AmountIsZero | ErrorCode::NoDestinationsGiven => {
            WalletCode::ZeroDestination.into()
        }
        ErrorCode::FeeTooSmall => WalletCode::FeeTooSmall.into(),
        ErrorCode::WillOverflow => WalletCode::SumOverflow.into(),
        ErrorCode::MixinTooSmall => WalletCode::MixinBelowThreshold.into(),
        ErrorCode::MixinTooBig => WalletCode::MixinAboveThreshold.into(),
        ErrorCode::CantGetFakeOutputs
        | ErrorCode::NotEnoughFakeOutputs
        | ErrorCode::FusionMixinTooLarge => WalletCode::MixinCountTooBig.into(),
        ErrorCode::TooManyInputsToFitInBlock => WalletCode::TransactionSizeTooBig.into(),
        ErrorCode::AddressWrongLength
        | ErrorCode::AddressWrongPrefix
        | ErrorCode::AddressNotBase58
        | ErrorCode::AddressNotValid
        | ErrorCode::AddressIsIntegrated
        | ErrorCode::IntegratedAddressPaymentIdInvalid => WalletCode::BadAddress.into(),
        ErrorCode::AddressNotInWallet => WalletCode::ObjectNotFound.into(),
        ErrorCode::PaymentIdWrongLength | ErrorCode::PaymentIdInvalid => {
            WalletCode::BadPaymentId.into()
        }
        ErrorCode::ConflictingPaymentIds => WalletCode::ConflictingPaymentIds.into(),
        ErrorCode::IllegalViewWalletOperation => WalletCode::TrackingMode.into(),
        ErrorCode::IllegalNonViewWalletOperation | ErrorCode::CannotDeletePrimaryAddress => {
            WalletCode::WrongParameters.into()
        }
        ErrorCode::SubwalletAlreadyExists => WalletCode::AddressAlreadyExists.into(),
        // `WalletGreen::createFusionTransaction` returns no transaction when there is nothing
        // to optimize; walletd then asks for transaction `WALLET_INVALID_TRANSACTION_ID`.
        ErrorCode::FullyOptimized => WalletCode::IndexOutOfRange.into(),
        ErrorCode::KeysNotDeterministic => ServiceCode::KeysNotDeterministic.into(),
        ErrorCode::InvalidKeyFormat => ServiceCode::WrongKeyFormat.into(),
        ErrorCode::HashWrongLength | ErrorCode::HashInvalid => ServiceCode::WrongHashFormat.into(),
        ErrorCode::DaemonOffline => NodeCode::ConnectError.into(),
        ErrorCode::DaemonError => NodeCode::InternalNodeError.into(),
        _ => WalletCode::InternalWalletError.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_messages_follow_the_cpp_categories() {
        for (i, code) in WalletCode::ALL.iter().enumerate() {
            assert_eq!(*code as usize, i + 1);
            assert_eq!(WalletCode::from_number(i as u32 + 1), Some(*code));
        }
        assert_eq!(WalletCode::from_number(0), None);
        assert_eq!(WalletCode::from_number(35), None);
        // the codes the captured C++ answers carry (application-codes.json)
        let wallet = [
            (5, "Internal error occurred"),
            (6, "MixIn count is too big"),
            (7, "Bad address"),
            (9, "Wrong amount"),
            (11, "The destination is empty"),
            (17, "Transaction fee is too small"),
            (20, "Address already exists"),
            (21, "The wallet is in tracking mode"),
            (22, "Wrong parameters passed"),
            (23, "Object not found"),
            (24, "Requested wallet not found"),
            (25, "Change address required"),
            (26, "Change address not found"),
            (28, "Destination address not found"),
            (29, "Wrong payment id format"),
        ];
        for (n, text) in wallet {
            let e = AppError::wallet(WalletCode::from_number(n).unwrap());
            assert_eq!((e.code, e.message.as_str()), (n as i32, text));
        }
        let service = [
            (ServiceCode::WrongKeyFormat, 1, "Wrong key format"),
            (
                ServiceCode::WrongPaymentIdFormat,
                2,
                "Wrong payment id format",
            ),
            (ServiceCode::ObjectNotFound, 4, "Requested object not found"),
            (
                ServiceCode::KeysNotDeterministic,
                6,
                "Keys not deterministic",
            ),
        ];
        for (code, n, text) in service {
            let e = AppError::service(code);
            assert_eq!((e.code, e.message.as_str()), (n, text));
        }
        assert_eq!(AppError::node(NodeCode::ConnectError).code, 7);
    }

    #[test]
    fn errno_codes_use_the_system_text() {
        let e = AppError::system(17);
        assert_eq!(e.code, 17);
        assert!(!e.message.contains("os error"), "{}", e.message);
        assert!(!e.message.is_empty());
    }

    #[test]
    fn engine_errors_map_to_walletd_codes() {
        let cases = [
            (ErrorCode::NotEnoughBalance, 9),
            (ErrorCode::AmountIsZero, 11),
            (ErrorCode::FeeTooSmall, 17),
            (ErrorCode::NotEnoughFakeOutputs, 6),
            (ErrorCode::AddressNotValid, 7),
            (ErrorCode::IllegalViewWalletOperation, 21),
            (ErrorCode::SubwalletAlreadyExists, 20),
            (ErrorCode::FullyOptimized, 19),
            (ErrorCode::KeysNotDeterministic, 6),
            (ErrorCode::WalletFileCorrupted, 5),
        ];
        for (code, want) in cases {
            assert_eq!(
                from_wallet_error(&WalletError::new(code)).code,
                want,
                "{code:?}"
            );
        }
        let with_green = WalletError::new(ErrorCode::HashInvalid).with_walletd_code(23);
        let e = from_wallet_error(&with_green);
        assert_eq!((e.code, e.message.as_str()), (23, "Object not found"));
    }
}
