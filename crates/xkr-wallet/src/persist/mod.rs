//! Reading and writing the wallet files: format A ([`backend`]) and format B ([`green`]), told
//! apart by their contents (`xkr_wallet_files::detect`).
//!
//! Everything here is blocking (PBKDF2 with 500 000 rounds for format A, `cn_slow_hash` for
//! format B); [`crate::Wallet`] runs it on the blocking thread pool.

pub mod backend;
pub mod green;

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};

use xkr_wallet_files::backend::WalletBackendJson;
use xkr_wallet_files::green::GreenWallet;
use xkr_wallet_files::{FileError, FileKind, atomic_write, detect};
use zeroize::Zeroizing;

use crate::error::{ErrorCode, WalletError};
use crate::state::WalletState;

/// The file format a wallet is stored in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileFormat {
    /// Format A, the `WalletBackend` file of wallet-api, zedwallet++ and wallet-backend-js.
    #[default]
    WalletBackend,
    /// Format A in the string form of wallet-backend-js `encryptWalletToString`.
    WalletBackendJsString,
    /// Format B, the `WalletGreen` container of walletd and zedwallet, with the engine's state
    /// in `<container>.xkrstate`.
    WalletGreen,
}

/// Where and how the wallet is stored.
#[derive(Clone)]
pub(crate) struct WalletFile {
    /// The wallet file (the container for format B).
    pub path: PathBuf,
    /// The password.
    pub password: Zeroizing<String>,
    /// The format.
    pub format: FileFormat,
    /// Creation timestamp of the container's view keys (format B).
    pub view_timestamp: u64,
}

impl std::fmt::Debug for WalletFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletFile")
            .field("path", &self.path)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

/// C++ `openWallet` mapping of a read failure: a missing file is `FILENAME_NON_EXISTENT`,
/// anything else `INVALID_WALLET_FILENAME`.
fn read_error(e: &io::Error) -> WalletError {
    if e.kind() == io::ErrorKind::NotFound {
        ErrorCode::FilenameNonExistent.into()
    } else {
        WalletError::new(ErrorCode::InvalidWalletFilename).with_detail(e.to_string())
    }
}

/// `checkNewWalletFilename`: the file must not exist and must be creatable (the test file is
/// removed again).
pub(crate) fn check_new_file(path: &Path) -> Result<(), WalletError> {
    if path.exists() {
        return Err(ErrorCode::WalletFileAlreadyExists.into());
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            Ok(())
        }
        Err(e) => {
            Err(WalletError::new(ErrorCode::InvalidWalletFilename).with_detail(e.to_string()))
        }
    }
}

/// A wallet read from disk.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub state: WalletState,
    pub file: WalletFile,
}

/// Opens the wallet at `path` in whichever format it is (`WalletBackend::openWallet`, or
/// `WalletGreen::load` plus the sidecar). A wallet file of version 1 to 5 is converted to a
/// container first, as C++ did (`xkr_wallet_files::green::legacy::convert_file`).
///
/// Temporary files that a stopped save of this wallet left next to it are deleted first
/// (they are never read).
pub(crate) fn load(path: &Path, password: &str, now: u64) -> Result<Loaded, WalletError> {
    for file in [path.to_path_buf(), green::sidecar_path(path)] {
        if let Ok(n) = xkr_wallet_files::remove_leftover_temps(&file)
            && n > 0
        {
            tracing::info!("removed {n} unfinished save(s) of {}", file.display());
        }
    }
    let bytes = std::fs::read(path).map_err(|e| read_error(&e))?;
    let file = |format, view_timestamp| WalletFile {
        path: path.to_path_buf(),
        password: Zeroizing::new(password.to_owned()),
        format,
        view_timestamp,
    };
    match detect(&bytes) {
        FileKind::WalletBackend => {
            let json = xkr_wallet_files::backend::open(&bytes, password)?;
            Ok(Loaded {
                state: backend::state_from_json(json)?,
                file: file(FileFormat::WalletBackend, 0),
            })
        }
        FileKind::WalletBackendJsString => {
            let text = std::str::from_utf8(&bytes).map_err(|_| FileError::NotAWalletFile)?;
            let inner = xkr_wallet_files::backend::decode_js_buffer_string(text)?;
            let json = xkr_wallet_files::backend::open(&inner, password)?;
            Ok(Loaded {
                state: backend::state_from_json(json)?,
                file: file(FileFormat::WalletBackendJsString, 0),
            })
        }
        FileKind::WalletGreen => {
            let container = xkr_wallet_files::green::open(&bytes, password)?;
            let state = load_sidecar(path, password, &container)
                .map_or_else(|| green::state_from_green(&container, now), Ok)?;
            Ok(Loaded {
                state,
                file: file(FileFormat::WalletGreen, container.view.creation_timestamp),
            })
        }
        FileKind::WalletGreenLegacy(version) => {
            // As C++ WalletGreen::load: convert to a version 6 container on first open,
            // keeping the old file as <file>.backup, then go on as with any container.
            let container = xkr_wallet_files::green::legacy::convert_file(
                path,
                password,
                &mut xkr_crypto::os_rng(),
            )?;
            tracing::info!(
                "converted {} from wallet file version {version}; the old file is {}",
                path.display(),
                xkr_wallet_files::green::legacy::backup_path(path).display()
            );
            Ok(Loaded {
                state: green::state_from_green(&container, now)?,
                file: file(FileFormat::WalletGreen, container.view.creation_timestamp),
            })
        }
        FileKind::WalletGreenOtherVersion(_) => Err(FileError::WrongVersion.into()),
        FileKind::Unknown => Err(FileError::NotAWalletFile.into()),
    }
}

/// The sidecar state of a container, if it exists, opens with the password and belongs to the
/// container's keys.
fn load_sidecar(path: &Path, password: &str, container: &GreenWallet) -> Option<WalletState> {
    let sidecar = green::sidecar_path(path);
    if !sidecar.exists() {
        return None;
    }
    match xkr_wallet_files::backend::open_file(&sidecar, password)
        .map_err(WalletError::from)
        .and_then(backend::state_from_json)
    {
        Ok(state) if green::matches(&state, container) => Some(state),
        Ok(_) => {
            tracing::warn!(
                "{} belongs to other keys than the container; rescanning",
                sidecar.display()
            );
            None
        }
        Err(e) => {
            tracing::warn!("{} not used: {e}", sidecar.display());
            None
        }
    }
}

/// Writes the state (`WalletBackend::unsafeSave`; for format B the sidecar).
pub(crate) fn save(file: &WalletFile, json: &WalletBackendJson) -> Result<(), WalletError> {
    let mut rng = xkr_crypto::os_rng();
    match file.format {
        FileFormat::WalletBackend => {
            xkr_wallet_files::backend::save_file(&file.path, json, &file.password, &mut rng)?;
        }
        FileFormat::WalletBackendJsString => {
            let bytes = xkr_wallet_files::backend::save(json, &file.password, &mut rng);
            let text = xkr_wallet_files::backend::encode_js_buffer_string(&bytes);
            atomic_write(&file.path, text.as_bytes()).map_err(FileError::InvalidWalletFilename)?;
        }
        FileFormat::WalletGreen => {
            let sidecar = green::sidecar_path(&file.path);
            xkr_wallet_files::backend::save_file(&sidecar, json, &file.password, &mut rng)?;
        }
    }
    Ok(())
}

/// Writes a format B container with the keys and, if `container.history` is set, that
/// history as its cache (walletd `export`).
pub(crate) fn write_container_with_cache(
    path: &Path,
    password: &str,
    container: &GreenWallet,
) -> Result<(), WalletError> {
    let mut rng = xkr_crypto::os_rng();
    xkr_wallet_files::green::write_with_cache_file(path, container, password, &mut rng)?;
    Ok(())
}

/// Rewrites a format B container with the keys only (on a key change, a password change or
/// walletd `export` without history).
pub(crate) fn write_container(
    path: &Path,
    password: &str,
    container: &GreenWallet,
) -> Result<(), WalletError> {
    let mut rng = xkr_crypto::os_rng();
    xkr_wallet_files::green::write_keys_only_file(path, container, password, &mut rng)?;
    Ok(())
}
