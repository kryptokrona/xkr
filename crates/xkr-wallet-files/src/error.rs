//! Errors of the wallet file readers and writers, with the numbers and texts of the C++ tools.
//!
//! Two C++ error families meet here:
//! * `src/errors/errors.h` / `errors.cpp` (`Error`, `ErrorCode`): used by wallet-api, zedwallet++
//!   and wallet-backend-js (`lib/WalletError.ts`) for the `WalletBackend` file. [`FileError::code`]
//!   and the `Display` text follow these.
//! * `src/wallet/wallet_errors.h` (`cryptonote::error::WalletErrorCodes`): used by `WalletGreen`
//!   (walletd, zedwallet) for the container. [`FileError::green_code`] and
//!   [`FileError::green_message`] follow these.

use std::io;

/// Why a wallet file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// C++ `FILENAME_NON_EXISTENT` (1).
    #[error(
        "The filename you are attempting to open does not exist, or the wallet does not have \
         permission to open it."
    )]
    FilenameNonExistent,

    /// C++ `INVALID_WALLET_FILENAME` (2): the file could not be opened or written.
    #[error(
        "We could not open/save to the filename given. Possibly invalid characters, or \
         permission issues."
    )]
    InvalidWalletFilename(#[source] io::Error),

    /// C++ `NOT_A_WALLET_FILE` (3): the plain `WalletBackend` identifier is missing.
    #[error(
        "This file is not a wallet file, or is not a wallet file type supported by this wallet \
         version."
    )]
    NotAWalletFile,

    /// C++ `WALLET_FILE_CORRUPTED` (4). The string says what did not parse; it is not part of
    /// the C++ message.
    #[error("This wallet file appears to have gotten corrupted.")]
    WalletFileCorrupted(String),

    /// C++ `WRONG_PASSWORD` (5); `WalletGreen` `WRONG_PASSWORD` (4).
    #[error("The password given for this wallet is incorrect.")]
    WrongPassword,

    /// C++ `UNSUPPORTED_WALLET_FILE_FORMAT_VERSION` (6): `walletFileFormatVersion` is not 0.
    #[error(
        "This wallet file appears to be from a newer or older version of the software, that we \
         do not support."
    )]
    UnsupportedWalletFileFormatVersion,

    /// C++ `WALLET_FILE_ALREADY_EXISTS` (8).
    #[error("The wallet file you are attempting to create already exists. Please delete it first.")]
    WalletFileAlreadyExists,

    /// `WalletGreen` `WRONG_VERSION` (16): the container's first byte is not a supported
    /// serialization version, or the file is empty (`WalletGreen::load`).
    #[error(
        "This wallet file appears to be from a newer or older version of the software, that we \
         do not support."
    )]
    WrongVersion,

    /// `WalletGreen` `BAD_ADDRESS` (7): the container mixes tracking (view-only) and spend
    /// records (`WalletGreen::loadSpendKeys`, "All addresses must be whether tracking or not").
    #[error("This wallet file appears to have gotten corrupted.")]
    MixedTrackingRecords,
}

impl FileError {
    /// The C++ `ErrorCode` number (`src/errors/errors.h`) that wallet-api and zedwallet++ report
    /// for this error. Container-only errors map to the closest `WalletBackend` code:
    /// [`FileError::WrongVersion`] to `UNSUPPORTED_WALLET_FILE_FORMAT_VERSION` and
    /// [`FileError::MixedTrackingRecords`] to `WALLET_FILE_CORRUPTED`.
    #[must_use]
    pub fn code(&self) -> u32 {
        match self {
            Self::FilenameNonExistent => 1,
            Self::InvalidWalletFilename(_) => 2,
            Self::NotAWalletFile => 3,
            Self::WalletFileCorrupted(_) | Self::MixedTrackingRecords => 4,
            Self::WrongPassword => 5,
            Self::UnsupportedWalletFileFormatVersion | Self::WrongVersion => 6,
            Self::WalletFileAlreadyExists => 8,
        }
    }

    /// The `WalletGreen` `cryptonote::error::WalletErrorCodes` number
    /// (`src/wallet/wallet_errors.h`) that walletd and zedwallet would raise, if any. C++ raises
    /// a plain `std::runtime_error` (no code) for a container whose layout does not add up
    /// (`FileMappedVector::open`) and for file-system failures; those give `None`.
    #[must_use]
    pub fn green_code(&self) -> Option<u32> {
        match self {
            Self::WrongPassword => Some(4),
            Self::MixedTrackingRecords => Some(7),
            Self::WrongVersion => Some(16),
            _ => None,
        }
    }

    /// The `WalletGreen` error text (`WalletErrorCategory::message`) for [`FileError::green_code`];
    /// otherwise the `Display` text.
    #[must_use]
    pub fn green_message(&self) -> String {
        match self {
            Self::WrongPassword => "The password is wrong".to_owned(),
            Self::MixedTrackingRecords => "Bad address".to_owned(),
            Self::WrongVersion => "Wrong version".to_owned(),
            other => other.to_string(),
        }
    }

    pub(crate) fn corrupted(what: impl Into<String>) -> Self {
        Self::WalletFileCorrupted(what.into())
    }

    /// Maps an error from opening an existing file the way C++ `openWallet` does: a missing
    /// file is `FILENAME_NON_EXISTENT`, anything else `INVALID_WALLET_FILENAME`.
    pub(crate) fn from_open(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::NotFound {
            Self::FilenameNonExistent
        } else {
            Self::InvalidWalletFilename(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_messages_match_cpp() {
        assert_eq!(FileError::NotAWalletFile.code(), 3);
        assert_eq!(FileError::corrupted("x").code(), 4);
        assert_eq!(FileError::WrongPassword.code(), 5);
        assert_eq!(FileError::UnsupportedWalletFileFormatVersion.code(), 6);
        assert_eq!(FileError::WalletFileAlreadyExists.code(), 8);
        assert_eq!(FileError::FilenameNonExistent.code(), 1);
        assert_eq!(
            FileError::WrongPassword.to_string(),
            "The password given for this wallet is incorrect."
        );
        assert_eq!(
            FileError::corrupted("json").to_string(),
            "This wallet file appears to have gotten corrupted."
        );
        assert_eq!(
            FileError::NotAWalletFile.to_string(),
            "This file is not a wallet file, or is not a wallet file type supported by this \
             wallet version."
        );
        assert_eq!(FileError::WrongPassword.green_code(), Some(4));
        assert_eq!(FileError::WrongVersion.green_code(), Some(16));
        assert_eq!(FileError::MixedTrackingRecords.green_code(), Some(7));
        assert_eq!(FileError::WrongVersion.green_message(), "Wrong version");
        assert_eq!(FileError::corrupted("layout").green_code(), None);
    }

    #[test]
    fn open_errors() {
        let e = FileError::from_open(io::Error::from(io::ErrorKind::NotFound));
        assert!(matches!(e, FileError::FilenameNonExistent));
        let e = FileError::from_open(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(matches!(e, FileError::InvalidWalletFilename(_)));
        assert_eq!(e.code(), 2);
    }
}
