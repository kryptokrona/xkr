//! The JSON inside a `WalletBackend` file.
//!
//! Field names and field order follow the C++ writers, so a file written by C++ comes out byte
//! for byte the same after a parse and re-serialisation:
//! * `src/wallet_backend/wallet_backend.cpp` `WalletBackend::toJSON` / `fromJSON`;
//! * `src/sub_wallets/sub_wallets.cpp` `SubWallets::toJSON` / `fromJSON`;
//! * `src/sub_wallets/sub_wallet.cpp` `SubWallet::toJSON` / `fromJSON`;
//! * `src/wallet_backend/wallet_synchronizer.cpp` `WalletSynchronizer::toJSON` / `fromJSON`;
//! * `src/wallet_backend/synchronization_status.cpp` `SynchronizationStatus::toJSON`;
//! * `include/wallet_types.h` `TransactionInput`, `Transaction`, `UnconfirmedInput`.
//!
//! wallet-backend-js (`lib/JsonSerialization.ts`, `lib/Types.ts`) writes the same fields in
//! another order and adds `subWallets.knownTransactions`, `privateEphemeral` on inputs and
//! `locked` on unconfirmed amounts. Every object keeps fields it does not know in `other`
//! (written after the known fields), so those files keep their extra data through a load and
//! save.

use core::fmt::Display;
use core::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use xkr_types::{Hash, KeyImage, PublicKey, SecretKey};

use crate::FileError;

/// C++ `Constants::WALLET_FILE_FORMAT_VERSION`.
pub const WALLET_FILE_FORMAT_VERSION: u64 = 0;

/// Fields of a JSON object that this crate does not model, kept as they were read.
pub type UnknownFields = Map<String, Value>;

/// The whole wallet: C++ `WalletBackend::toJSON`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletBackendJson {
    /// Always [`WALLET_FILE_FORMAT_VERSION`].
    pub wallet_file_format_version: u64,
    /// Keys, subwallets, transactions.
    pub sub_wallets: SubWalletsJson,
    /// Sync progress.
    pub wallet_synchronizer: WalletSynchronizerJson,
    /// Fields not modelled here.
    #[serde(flatten)]
    pub other: UnknownFields,
}

/// C++ `SubWallets::toJSON`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubWalletsJson {
    /// Public spend keys of all subwallets, in creation order.
    #[serde(with = "hex_pod::vec")]
    pub public_spend_keys: Vec<PublicKey>,
    /// The subwallets (C++ iterates an `unordered_map`, so the order is arbitrary).
    pub sub_wallet: Vec<SubWalletJson>,
    /// Confirmed transactions.
    pub transactions: Vec<TransactionJson>,
    /// Sent transactions not yet in a block.
    pub locked_transactions: Vec<TransactionJson>,
    /// The shared private view key.
    #[serde(with = "hex_pod")]
    pub private_view_key: SecretKey,
    /// True for a view-only wallet (all private spend keys are zero).
    pub is_view_wallet: bool,
    /// Private keys of sent transactions.
    pub tx_private_keys: Vec<TxPrivateKeyJson>,
    /// Fields not modelled here (wallet-backend-js `knownTransactions`).
    #[serde(flatten)]
    pub other: UnknownFields,
}

/// C++ `SubWallet::toJSON`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubWalletJson {
    /// Public spend key.
    #[serde(with = "hex_pod")]
    pub public_spend_key: PublicKey,
    /// Private spend key; all zero in a view wallet.
    #[serde(with = "hex_pod")]
    pub private_spend_key: SecretKey,
    /// The address (base58).
    pub address: String,
    /// Timestamp to start scanning from (0 if a height is used).
    pub sync_start_timestamp: u64,
    /// Outputs that can be spent.
    pub unspent_inputs: Vec<TransactionInputJson>,
    /// Outputs used by a sent transaction that is not yet confirmed.
    pub locked_inputs: Vec<TransactionInputJson>,
    /// Spent outputs (kept until they are deep enough to survive a reorg).
    pub spent_inputs: Vec<TransactionInputJson>,
    /// Height to start scanning from (0 if a timestamp is used).
    pub sync_start_height: u64,
    /// True for the primary address.
    pub is_primary_address: bool,
    /// Change of unconfirmed sent transactions.
    pub unconfirmed_incoming_amounts: Vec<UnconfirmedInputJson>,
    /// Fields not modelled here.
    #[serde(flatten)]
    pub other: UnknownFields,
}

/// C++ `wallet_types::TransactionInput::toJSON` (an owned output).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionInputJson {
    /// Key image of the output.
    #[serde(with = "hex_pod")]
    pub key_image: KeyImage,
    /// Amount in atomic units.
    pub amount: u64,
    /// Height of the block holding the transaction.
    pub block_height: u64,
    /// Transaction public key from `extra`.
    #[serde(with = "hex_pod")]
    pub transaction_public_key: PublicKey,
    /// Index of the output in the transaction.
    pub transaction_index: u64,
    /// Global output index (C++ writes 0 when unknown).
    pub global_output_index: u64,
    /// The output's one-time public key.
    #[serde(with = "hex_pod")]
    pub key: PublicKey,
    /// Height the output was spent at, 0 if unspent.
    pub spend_height: u64,
    /// Unlock time of the transaction.
    pub unlock_time: u64,
    /// Hash of the transaction holding the output.
    #[serde(with = "hex_pod")]
    pub parent_transaction_hash: Hash,
    /// Fields not modelled here (wallet-backend-js `privateEphemeral`).
    #[serde(flatten)]
    pub other: UnknownFields,
}

/// C++ `wallet_types::UnconfirmedInput::toJSON`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnconfirmedInputJson {
    /// Amount in atomic units.
    pub amount: u64,
    /// The output's one-time public key.
    #[serde(with = "hex_pod")]
    pub key: PublicKey,
    /// Hash of the sent transaction.
    #[serde(with = "hex_pod")]
    pub parent_transaction_hash: Hash,
    /// Fields not modelled here (wallet-backend-js `locked`).
    #[serde(flatten)]
    pub other: UnknownFields,
}

/// C++ `wallet_types::Transaction::toJSON`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionJson {
    /// Amount per own subwallet (negative for sent).
    pub transfers: Vec<TransferJson>,
    /// Transaction hash.
    #[serde(with = "hex_pod")]
    pub hash: Hash,
    /// Fee.
    pub fee: u64,
    /// Block height (0 while unconfirmed).
    pub block_height: u64,
    /// Block timestamp.
    pub timestamp: u64,
    /// Payment ID, empty if none.
    #[serde(rename = "paymentID")]
    pub payment_id: String,
    /// Unlock time.
    pub unlock_time: u64,
    /// True for a coinbase transaction.
    pub is_coinbase_transaction: bool,
    /// Fields not modelled here.
    #[serde(flatten)]
    pub other: UnknownFields,
}

/// One entry of C++ `Transaction::transfers` (written as an array of objects).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferJson {
    /// Public spend key of the subwallet.
    #[serde(with = "hex_pod")]
    pub public_key: PublicKey,
    /// Signed amount (C++ `Int64`).
    pub amount: i64,
    /// Fields not modelled here.
    #[serde(flatten)]
    pub other: UnknownFields,
}

/// One entry of C++ `SubWallets::m_transactionPrivateKeys`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxPrivateKeyJson {
    /// Transaction hash.
    #[serde(with = "hex_pod")]
    pub transaction_hash: Hash,
    /// The transaction private key.
    #[serde(with = "hex_pod")]
    pub tx_private_key: SecretKey,
    /// Fields not modelled here.
    #[serde(flatten)]
    pub other: UnknownFields,
}

/// C++ `WalletSynchronizer::toJSON`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletSynchronizerJson {
    /// Sync progress.
    pub transaction_synchronizer_status: SynchronizationStatusJson,
    /// Timestamp sync started from.
    pub start_timestamp: u64,
    /// Height sync started from.
    pub start_height: u64,
    /// The shared private view key.
    #[serde(with = "hex_pod")]
    pub private_view_key: SecretKey,
    /// Fields not modelled here.
    #[serde(flatten)]
    pub other: UnknownFields,
}

/// C++ `SynchronizationStatus::toJSON`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SynchronizationStatusJson {
    /// A block hash every 5000 blocks.
    #[serde(with = "hex_pod::vec")]
    pub block_hash_checkpoints: Vec<Hash>,
    /// The last 100 block hashes, newest first.
    #[serde(with = "hex_pod::vec")]
    pub last_known_block_hashes: Vec<Hash>,
    /// Height of the last processed block.
    pub last_known_block_height: u64,
    /// Fields not modelled here.
    #[serde(flatten)]
    pub other: UnknownFields,
}

/// Only the version field, read before the rest as C++ `WalletBackend::fromJSON` does.
#[derive(Deserialize)]
struct VersionProbe {
    #[serde(rename = "walletFileFormatVersion")]
    version: Option<u64>,
}

/// Parses the decrypted JSON text.
///
/// * not JSON, a missing or non-integer `walletFileFormatVersion`, a missing field or a
///   malformed key: [`FileError::WalletFileCorrupted`] (C++ catches `std::invalid_argument`
///   from the JSON getters and the parser error);
/// * `walletFileFormatVersion` other than 0: [`FileError::UnsupportedWalletFileFormatVersion`].
pub fn parse(json: &[u8]) -> Result<WalletBackendJson, FileError> {
    let probe: VersionProbe =
        serde_json::from_slice(json).map_err(|e| FileError::corrupted(e.to_string()))?;
    match probe.version {
        None => return Err(FileError::corrupted("walletFileFormatVersion missing")),
        Some(WALLET_FILE_FORMAT_VERSION) => {}
        Some(_) => return Err(FileError::UnsupportedWalletFileFormatVersion),
    }
    serde_json::from_slice(json).map_err(|e| FileError::corrupted(e.to_string()))
}

/// Serialises the wallet as compact JSON (C++ `rapidjson::Writer`, no whitespace).
#[must_use]
pub fn to_bytes(wallet: &WalletBackendJson) -> Vec<u8> {
    serde_json::to_vec(wallet).expect("wallet JSON has string keys only")
}

/// Hex (de)serialisation of the `xkr-types` byte newtypes (C++ `PublicKey::toJSON` writes
/// lower-case hex; `fromString` reads it back).
mod hex_pod {
    use super::{Display, FromStr};
    use serde::de::Error as _;
    use serde::{Deserialize as _, Deserializer, Serializer};
    use zeroize::Zeroize as _;

    pub fn serialize<T: Display, S: Serializer>(value: &T, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(value)
    }

    pub fn deserialize<'de, T, D>(d: D) -> Result<T, D::Error>
    where
        T: FromStr,
        T::Err: Display,
        D: Deserializer<'de>,
    {
        let mut text = String::deserialize(d)?;
        let parsed = text.parse().map_err(D::Error::custom);
        text.zeroize();
        parsed
    }

    pub mod vec {
        use super::{Display, FromStr};
        use serde::de::Error as _;
        use serde::{Deserialize as _, Deserializer, Serializer};

        pub fn serialize<T: Display, S: Serializer>(values: &[T], s: S) -> Result<S::Ok, S::Error> {
            s.collect_seq(values.iter().map(ToString::to_string))
        }

        pub fn deserialize<'de, T, D>(d: D) -> Result<Vec<T>, D::Error>
        where
            T: FromStr,
            T::Err: Display,
            D: Deserializer<'de>,
        {
            Vec::<String>::deserialize(d)?
                .iter()
                .map(|s| s.parse().map_err(D::Error::custom))
                .collect()
        }
    }
}
