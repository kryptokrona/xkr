//! `processBlockOutputs`: the output scan the mobile wallet installs with
//! `setBlockOutputProcessFunc` (wallet-backend-js), with exactly the input and output shape of
//! the two native modules it replaces (`TurtleCoinModule.mm` / `Java.cpp`
//! `processBlockOutputs` and `processTransactionOutputs`).
//!
//! Input (`block`, the wallet-backend `Block` as `JSON.stringify` writes it; other fields are
//! ignored):
//!
//! ```text
//! { coinbaseTransaction?: RawTx | null, transactions: RawTx[] }
//! RawTx     = { keyOutputs: KeyOutput[], hash: string, transactionPublicKey: string }
//! KeyOutput = { key: string, amount: number, globalIndex?: number, viewTag?: string }
//! ```
//!
//! `spendKeys`: `[{ publicKey, privateKey }]` (the `javaSpendKeys` of `NativeTest.js`).
//!
//! Output, in order (coinbase first when processed, then the transactions; outputs in index
//! order):
//!
//! ```text
//! [{ publicSpendKey, input: { keyImage, amount, transactionIndex, globalOutputIndex, key,
//!                             parentTransactionHash } }]
//! ```
//!
//! Per transaction the derivation `D = 8 * a * R` is computed once; output `i` belongs to the
//! subwallet whose public spend key equals `underive_public_key(D, i, P)`. For a spend wallet
//! the key image is `generate_key_image(P, derive_secret_key(D, i, b))`; a view wallet gets the
//! all-zero key image (wallet-backend's `nullKey`). A missing `globalIndex` gives
//! `globalOutputIndex: -1`, which `NativeTest.js` turns into `undefined`. A tagged output
//! (transaction version 2, `viewTag` set by wallet-backend's `KeyOutput.fromJSON`) whose tag does
//! not match `view_tag(D, i)` is skipped before the point arithmetic; the result is the same as
//! without the tag. A transaction public key that does not parse or decode owns nothing, and
//! so does an output key that does not decode.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use xkr_types::{KeyImage, SecretKey};
use xkr_wallet_core::{SpendKeys, scan_subwallet_output};

use crate::api::{parse_public, parse_secret};
use crate::error::{CryptoError, CryptoResult};

/// One output of a transaction (`KeyOutput` of wallet-backend).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyOutput {
    /// The one-time output key (64 hex characters).
    pub key: String,
    /// The amount in atomic units.
    pub amount: u64,
    /// The global output index, `-1` when the daemon did not send it.
    pub global_index: i64,
    /// The view tag (transaction version 2), if the output has one.
    pub view_tag: Option<u8>,
}

/// One transaction of a block (`RawCoinbaseTransaction` / `RawTransaction` of wallet-backend).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTransaction {
    /// The outputs, in transaction order.
    pub key_outputs: Vec<KeyOutput>,
    /// The transaction hash, passed through unchanged as `parentTransactionHash`.
    pub hash: String,
    /// The transaction public key `R` (64 hex characters).
    pub transaction_public_key: String,
}

/// The parts of a block the scan reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WalletBlock {
    /// The coinbase transaction, if the block has one.
    pub coinbase_transaction: Option<RawTransaction>,
    /// The other transactions, in block order.
    pub transactions: Vec<RawTransaction>,
}

/// A subwallet spend key pair (`{ publicKey, privateKey }`).
#[derive(Clone, PartialEq, Eq)]
pub struct SpendKey {
    /// The public spend key (64 hex characters).
    pub public_key: String,
    /// The private spend key (64 hex characters); not read for a view wallet.
    pub private_key: String,
}

impl std::fmt::Debug for SpendKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpendKey")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

/// An owned output (`TransactionInput` of the native modules).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionInput {
    /// The key image (all zero for a view wallet).
    pub key_image: String,
    /// The amount.
    pub amount: u64,
    /// The index of the output in its transaction.
    pub transaction_index: u64,
    /// The global output index, `-1` if unknown.
    pub global_output_index: i64,
    /// The one-time output key, lowercase hex.
    pub key: String,
    /// The hash of the transaction, as given.
    pub parent_transaction_hash: String,
}

/// One element of the result: the owning subwallet and the input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundInput {
    /// The public spend key of the owning subwallet, lowercase hex.
    pub public_spend_key: String,
    /// The input.
    pub input: TransactionInput,
}

/// Scans the outputs of `block`; see the module documentation for the semantics. The
/// ownership test and the key images are `xkr_wallet_core::scan_subwallet_output`.
pub fn process_block_outputs(
    block: &WalletBlock,
    private_view_key: &str,
    spend_keys: &[SpendKey],
    is_view_wallet: bool,
    process_coinbase_transactions: bool,
) -> CryptoResult<Vec<FoundInput>> {
    let view_key = parse_secret("privateViewKey", private_view_key)?;
    let mut owners = SpendKeys::new();
    for pair in spend_keys {
        let public = parse_public("spendKeys.publicKey", &pair.public_key)?;
        // A view wallet has no private spend keys; the native modules never read them.
        let secret = if is_view_wallet {
            None
        } else {
            Some(parse_secret("spendKeys.privateKey", &pair.private_key)?)
        };
        // As C++ `spendKeys[publicKey] = privateKey`: the last pair for a key wins.
        owners.insert(public, secret);
    }

    let mut found = Vec::new();
    if process_coinbase_transactions && let Some(tx) = &block.coinbase_transaction {
        scan_transaction(tx, &view_key, &owners, &mut found);
    }
    for tx in &block.transactions {
        scan_transaction(tx, &view_key, &owners, &mut found);
    }
    Ok(found)
}

fn scan_transaction(
    tx: &RawTransaction,
    view_key: &SecretKey,
    owners: &SpendKeys,
    found: &mut Vec<FoundInput>,
) {
    let Ok(tx_public) = parse_public("transactionPublicKey", &tx.transaction_public_key) else {
        return;
    };
    let Some(derivation) = xkr_crypto::generate_key_derivation(&tx_public, view_key) else {
        return;
    };
    for (index, output) in tx.key_outputs.iter().enumerate() {
        // An output key that does not parse owns nothing; the others keep their index.
        let Ok(key) = parse_public("key", &output.key) else {
            continue;
        };
        let Some(owned) = scan_subwallet_output(
            &derivation,
            index,
            output.amount,
            &key,
            output.view_tag,
            owners,
        ) else {
            continue;
        };
        found.push(FoundInput {
            public_spend_key: owned.owner.to_hex(),
            input: TransactionInput {
                // A view wallet gets the all-zero key image (wallet-backend's `nullKey`).
                key_image: owned
                    .key_image
                    .as_ref()
                    .map_or_else(|| "0".repeat(64), KeyImage::to_hex),
                amount: owned.amount,
                transaction_index: index as u64,
                global_output_index: output.global_index,
                key: owned.key.to_hex(),
                parent_transaction_hash: tx.hash.clone(),
            },
        });
    }
}

// ---------------------------------------------------------------------------------------------
// JSON (the C ABI and the React Native module pass the JavaScript objects as JSON text)

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonBlock {
    #[serde(default)]
    coinbase_transaction: Option<JsonTransaction>,
    transactions: Vec<JsonTransaction>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonTransaction {
    key_outputs: Vec<JsonKeyOutput>,
    hash: String,
    transaction_public_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonKeyOutput {
    key: String,
    amount: Value,
    #[serde(default)]
    global_index: Value,
    #[serde(default)]
    view_tag: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonSpendKey {
    public_key: String,
    #[serde(default)]
    private_key: Option<String>,
}

/// A JavaScript number as the native modules read it: an integer, or a non-negative finite
/// double truncated toward zero (`unsignedLongLongValue` / `(long) getDouble`).
fn json_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| f as u64)
    })
}

/// `globalIndex`: an integer (`-1` and other negatives pass through as Android's `long` does),
/// `-1` when absent or `null`.
fn json_global_index(value: &Value) -> CryptoResult<i64> {
    if value.is_null() {
        return Ok(-1);
    }
    value
        .as_i64()
        .or_else(|| value.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
        .ok_or_else(|| CryptoError::InvalidJson("keyOutputs[].globalIndex".to_owned()))
}

/// `viewTag`: two hex characters or a number below 256; anything else is treated as untagged
/// (the full check then decides).
fn json_view_tag(value: &Value) -> Option<u8> {
    match value {
        Value::String(s) if s.len() == 2 && s.bytes().all(|b| b.is_ascii_hexdigit()) => {
            u8::from_str_radix(s, 16).ok()
        }
        Value::Number(n) => n.as_u64().and_then(|n| u8::try_from(n).ok()),
        _ => None,
    }
}

impl JsonTransaction {
    fn into_transaction(self) -> CryptoResult<RawTransaction> {
        let key_outputs = self
            .key_outputs
            .into_iter()
            .map(|o| {
                Ok(KeyOutput {
                    amount: json_u64(&o.amount).ok_or_else(|| {
                        CryptoError::InvalidJson("keyOutputs[].amount".to_owned())
                    })?,
                    global_index: json_global_index(&o.global_index)?,
                    view_tag: json_view_tag(&o.view_tag),
                    key: o.key,
                })
            })
            .collect::<CryptoResult<Vec<KeyOutput>>>()?;
        Ok(RawTransaction {
            key_outputs,
            hash: self.hash,
            transaction_public_key: self.transaction_public_key,
        })
    }
}

/// Parses the `block` argument from its JSON text.
pub fn parse_block_json(block_json: &str) -> CryptoResult<WalletBlock> {
    let block: JsonBlock =
        serde_json::from_str(block_json).map_err(|e| CryptoError::InvalidJson(e.to_string()))?;
    Ok(WalletBlock {
        coinbase_transaction: block
            .coinbase_transaction
            .map(JsonTransaction::into_transaction)
            .transpose()?,
        transactions: block
            .transactions
            .into_iter()
            .map(JsonTransaction::into_transaction)
            .collect::<CryptoResult<Vec<RawTransaction>>>()?,
    })
}

/// Parses the `spendKeys` argument (`[{ publicKey, privateKey }]`) from its JSON text.
pub fn parse_spend_keys_json(spend_keys_json: &str) -> CryptoResult<Vec<SpendKey>> {
    let keys: Vec<JsonSpendKey> = serde_json::from_str(spend_keys_json)
        .map_err(|e| CryptoError::InvalidJson(e.to_string()))?;
    Ok(keys
        .into_iter()
        .map(|k| SpendKey {
            public_key: k.public_key,
            private_key: k.private_key.unwrap_or_default(),
        })
        .collect())
}

/// [`process_block_outputs`] from and to JSON text: what the C ABI and the React Native module
/// use (`JSON.stringify(block)`, `JSON.stringify(spendKeys)` in, `JSON.parse(result)` out).
pub fn process_block_outputs_json(
    block_json: &str,
    private_view_key: &str,
    spend_keys_json: &str,
    is_view_wallet: bool,
    process_coinbase_transactions: bool,
) -> CryptoResult<String> {
    let block = parse_block_json(block_json)?;
    let spend_keys = parse_spend_keys_json(spend_keys_json)?;
    let found = process_block_outputs(
        &block,
        private_view_key,
        &spend_keys,
        is_view_wallet,
        process_coinbase_transactions,
    )?;
    serde_json::to_string(&found).map_err(|e| CryptoError::InvalidJson(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_read_as_the_native_modules_read_them() {
        assert_eq!(json_u64(&serde_json::json!(5)), Some(5));
        assert_eq!(json_u64(&serde_json::json!(5.9)), Some(5));
        assert_eq!(json_u64(&serde_json::json!(-1)), None);
        assert_eq!(json_u64(&serde_json::json!("5")), None);
        assert_eq!(json_global_index(&Value::Null), Ok(-1));
        assert_eq!(json_global_index(&serde_json::json!(-1)), Ok(-1));
        assert_eq!(json_global_index(&serde_json::json!(12345)), Ok(12345));
        assert_eq!(json_view_tag(&serde_json::json!("a7")), Some(0xa7));
        assert_eq!(json_view_tag(&serde_json::json!(7)), Some(7));
        assert_eq!(json_view_tag(&serde_json::json!("a")), None);
        assert_eq!(json_view_tag(&serde_json::json!("+f")), None);
        assert_eq!(json_view_tag(&serde_json::json!(300)), None);
    }

    #[test]
    fn block_without_coinbase_parses() {
        let block = parse_block_json(r#"{"transactions":[],"blockHeight":5}"#).unwrap();
        assert_eq!(block, WalletBlock::default());
        let block = parse_block_json(r#"{"coinbaseTransaction":null,"transactions":[]}"#).unwrap();
        assert_eq!(block, WalletBlock::default());
        assert!(matches!(
            parse_block_json(r#"{"coinbaseTransaction":null}"#),
            Err(CryptoError::InvalidJson(_))
        ));
    }
}
