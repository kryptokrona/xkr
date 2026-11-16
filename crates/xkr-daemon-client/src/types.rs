//! Typed answers of the daemon, with the JSON keys of the C++ serializers
//! (`src/rpc/core_rpc_server_commands_definitions.h`, `src/rpc/rpc_server.cpp`,
//! `include/wallet_types.h` `from_json`) and the xkr additions (`crates/xkr-rpc`), which are
//! all optional so C++ answers parse too.

use std::collections::HashSet;
use std::str::FromStr;

use serde::Deserialize;
use serde::de::{self, Deserializer};
use xkr_types::{Hash, HexError, KeyImage, PublicKey};
use xkr_wallet_core::Address;

/// Hex fields as the byte newtypes of `xkr-types`.
pub(crate) mod hex_field {
    use super::{Deserialize, Deserializer, FromStr, HexError, de};

    /// One hex value.
    pub fn deserialize<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr<Err = HexError>,
    {
        let text = String::deserialize(d)?;
        text.trim().parse().map_err(de::Error::custom)
    }
}

/// Lists of hex values.
pub(crate) mod hex_list {
    use super::{Deserialize, Deserializer, FromStr, HashSet, HexError, de};

    /// A JSON array of hex values, in order.
    pub fn deserialize<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr<Err = HexError>,
    {
        let texts = Vec::<String>::deserialize(d)?;
        texts
            .iter()
            .map(|t| t.trim().parse::<T>().map_err(de::Error::custom))
            .collect()
    }

    /// A JSON array of hex values as a set (C++ `std::unordered_set<crypto::Hash>`).
    pub fn set<'de, D, T>(d: D) -> Result<HashSet<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr<Err = HexError> + Eq + std::hash::Hash,
    {
        let list: Vec<T> = deserialize(d)?;
        Ok(list.into_iter().collect())
    }
}

/// `/getinfo` (`RpcServer::on_get_info`, `COMMAND_RPC_GET_INFO::response`).
///
/// Nigel (`src/nigel/nigel.cpp` `Nigel::getDaemonInfo`) needs only `height`, `network_height`,
/// the two connection counts and `difficulty`; the other C++ fields default when missing. The
/// xkr fields are `None` on a C++ node.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Info {
    /// Top block index + 1.
    pub height: u64,
    /// Highest height seen from peers (at least `height` on xkrd).
    pub network_height: u64,
    /// Difficulty of the next block.
    pub difficulty: u64,
    /// Outgoing peer connections.
    pub outgoing_connections_count: u64,
    /// Incoming peer connections.
    pub incoming_connections_count: u64,
    /// Transactions in the chain, coinbases excluded.
    #[serde(default)]
    pub tx_count: u64,
    /// Transactions in the pool.
    #[serde(default)]
    pub tx_pool_size: u64,
    /// Alternative blocks kept.
    #[serde(default)]
    pub alt_blocks_count: u64,
    /// White peer list size.
    #[serde(default)]
    pub white_peerlist_size: u64,
    /// Grey peer list size.
    #[serde(default)]
    pub grey_peerlist_size: u64,
    /// `network_height - 1`.
    #[serde(default)]
    pub last_known_block_index: u64,
    /// C++ `FORK_HEIGHTS`.
    #[serde(default)]
    pub upgrade_heights: Vec<u64>,
    /// Height up to which this node's forks are known.
    #[serde(default)]
    pub supported_height: u64,
    /// `difficulty / DIFFICULTY_TARGET` (rounded on xkrd).
    #[serde(default)]
    pub hashrate: u64,
    /// Major version of the top block.
    #[serde(default)]
    pub major_version: u8,
    /// Minor version of the top block.
    #[serde(default)]
    pub minor_version: u8,
    /// Node version string.
    #[serde(default)]
    pub version: String,
    /// Unix start time of the node.
    #[serde(default)]
    pub start_time: u64,
    /// Whether the node is at the network height.
    #[serde(default)]
    pub synced: bool,
    /// Whether the node runs on testnet.
    #[serde(default)]
    pub testnet: bool,
    /// xkr: transaction versions the next block accepts, ascending (`[1]` before fork 14,
    /// `[1, 2]` in its grace period, `[2]` after it). `None` on a C++ node, which only takes
    /// version 1.
    #[serde(default)]
    pub tx_versions: Option<Vec<u8>>,
    /// xkr: Hyperswarm connections.
    #[serde(default)]
    pub hyper_connections_count: Option<u64>,
    /// xkr: share of recent blocks signalling the next fork (`None`: no fork pending or a C++
    /// node).
    #[serde(default)]
    pub fork_readiness: Option<f64>,
    /// xkr: raw stripe pruning seed (0 = full node).
    #[serde(default)]
    pub pruning_seed: Option<u32>,
}

impl Info {
    /// Peer count as Nigel computes it (`incoming + outgoing`).
    #[must_use]
    pub fn peer_count(&self) -> u64 {
        self.incoming_connections_count + self.outgoing_connections_count
    }

    /// Top block index (Nigel's `localDaemonBlockCount`: `height - 1`, 0 for 0).
    #[must_use]
    pub fn local_block_count(&self) -> u64 {
        self.height.saturating_sub(1)
    }

    /// Network top index (Nigel's `networkBlockCount`: `network_height - 1`, 0 for 0).
    #[must_use]
    pub fn network_block_count(&self) -> u64 {
        self.network_height.saturating_sub(1)
    }
}

/// `/fee` (`RpcServer::on_get_fee_info`): the node operator's fee. A node without one answers an
/// empty address and amount 0.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NodeFee {
    /// Fee address (`""` when the node takes no fee).
    #[serde(default)]
    pub address: String,
    /// Fee per transaction, atomic units (C++ `uint32_t`).
    #[serde(default)]
    pub amount: u64,
}

impl NodeFee {
    /// The fee to add to a transaction, if any: a valid standard (not integrated) address and an
    /// amount above 0. Mirrors `src/nigel/nigel.cpp` `Nigel::getFeeInfo` (address check with
    /// integrated addresses refused) and `src/wallet_backend/transfer.cpp`, which adds the node
    /// fee as a destination only when it is not 0.
    #[must_use]
    pub fn payable(&self) -> Option<(Address, u64)> {
        if self.amount == 0 {
            return None;
        }
        let address = Address::parse(self.address.trim()).ok()?;
        if address.payment_id.is_some() {
            return None;
        }
        Some((address, self.amount))
    }
}

/// Request of `/getwalletsyncdata` (`COMMAND_RPC_GET_WALLET_SYNC_DATA::request`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalletSyncRequest {
    /// `blockHashCheckpoints`: the wallet's known block hashes, newest first; the node starts
    /// after the first one on its main chain.
    pub checkpoints: Vec<Hash>,
    /// `startHeight` (0: use `startTimestamp`).
    pub start_height: u64,
    /// `startTimestamp` (0: none).
    pub start_timestamp: u64,
    /// `blockCount` (at most 100 are returned). `None` leaves it out, as Nigel does; the node
    /// then sends its default of 100.
    pub block_count: Option<u64>,
}

/// One block of `/getwalletsyncdata` (`wallet_types::WalletBlockInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WalletBlock {
    /// The miner transaction.
    #[serde(rename = "coinbaseTX")]
    pub coinbase: RawCoinbaseTransaction,
    /// The other transactions, in block order.
    pub transactions: Vec<RawTransaction>,
    /// Block index.
    #[serde(rename = "blockHeight")]
    pub height: u64,
    /// Block id.
    #[serde(rename = "blockHash", with = "hex_field")]
    pub hash: Hash,
    /// Block timestamp.
    #[serde(rename = "blockTimestamp")]
    pub timestamp: u64,
}

/// A miner transaction as wallets see it (`wallet_types::RawCoinbaseTransaction`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RawCoinbaseTransaction {
    /// Outputs in order.
    pub outputs: Vec<KeyOutput>,
    /// Transaction hash.
    #[serde(with = "hex_field")]
    pub hash: Hash,
    /// Transaction public key from `extra` (zeros when absent: C++ `Core::getPubKeyFromExtra`).
    #[serde(rename = "txPublicKey", with = "hex_field")]
    pub tx_public_key: PublicKey,
    /// Unlock time (height or timestamp).
    #[serde(rename = "unlockTime")]
    pub unlock_time: u64,
}

/// A transaction as wallets see it (`wallet_types::RawTransaction`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RawTransaction {
    /// Outputs in order.
    pub outputs: Vec<KeyOutput>,
    /// Transaction hash.
    #[serde(with = "hex_field")]
    pub hash: Hash,
    /// Transaction public key from `extra` (zeros when absent).
    #[serde(rename = "txPublicKey", with = "hex_field")]
    pub tx_public_key: PublicKey,
    /// Unlock time (height or timestamp).
    #[serde(rename = "unlockTime")]
    pub unlock_time: u64,
    /// Payment id from `extra` (`""` on the wire when there is none).
    #[serde(rename = "paymentID", deserialize_with = "payment_id")]
    pub payment_id: Option<Hash>,
    /// Key inputs (C++ `cryptonote::KeyInput`).
    pub inputs: Vec<KeyInput>,
}

/// One output (`wallet_types::KeyOutput`); version-2 outputs carry a view tag (xkr, fork 14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct KeyOutput {
    /// One-time output key.
    #[serde(with = "hex_field")]
    pub key: PublicKey,
    /// Amount, atomic units.
    pub amount: u64,
    /// xkr: view tag of a tagged (version-2) output, two hex digits on the wire.
    #[serde(default, deserialize_with = "view_tag")]
    pub view_tag: Option<u8>,
}

/// One key input (C++ `cryptonote::KeyInput` JSON: `amount`, `key_offsets`, `k_image`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct KeyInput {
    /// Amount, atomic units.
    pub amount: u64,
    /// Relative global output indexes of the ring.
    pub key_offsets: Vec<u64>,
    /// Key image.
    #[serde(rename = "k_image", with = "hex_field")]
    pub key_image: KeyImage,
}

fn payment_id<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Hash>, D::Error> {
    let text = String::deserialize(d)?;
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse().map(Some).map_err(de::Error::custom)
}

fn view_tag<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u8>, D::Error> {
    let Some(text) = Option::<String>::deserialize(d)? else {
        return Ok(None);
    };
    let mut byte = [0u8; 1];
    hex::decode_to_slice(text.trim(), &mut byte).map_err(de::Error::custom)?;
    Ok(Some(byte[0]))
}

/// The ring members offered for one amount (`cryptonote::RandomOuts`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RandomOuts {
    /// The amount asked for.
    pub amount: u64,
    /// Outputs, sorted by global index on xkrd.
    pub outs: Vec<OutputEntry>,
}

/// One ring member candidate (`cryptonote::OutputEntry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct OutputEntry {
    /// Global index of the output among outputs of its amount (C++ `uint32_t`).
    pub global_amount_index: u64,
    /// Output key.
    #[serde(with = "hex_field")]
    pub out_key: PublicKey,
}

/// What became of a sent transaction (`/sendrawtransaction`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// The node took it into its pool. The hash is the node's `transactionHash` (xkrd) or,
    /// for C++ nodes that answer only `status`, `cn_fast_hash` of the blob.
    Ok(Hash),
    /// The node refused it: xkrd's `error` text, or the bare `status` (`"Failed"`) of a C++
    /// node.
    Failed(String),
}

impl SendOutcome {
    /// Whether the node took the transaction.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }
}

/// `/get_transactions_status` (`COMMAND_RPC_GET_TRANSACTIONS_STATUS::response`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct TransactionsStatus {
    /// In the pool.
    #[serde(rename = "transactionsInPool", deserialize_with = "hex_list::set")]
    pub in_pool: HashSet<Hash>,
    /// In a main-chain block.
    #[serde(rename = "transactionsInBlock", deserialize_with = "hex_list::set")]
    pub in_block: HashSet<Hash>,
    /// Unknown to the node.
    #[serde(rename = "transactionsUnknown", deserialize_with = "hex_list::set")]
    pub unknown: HashSet<Hash>,
}

/// `/gettransactions` (`COMMAND_RPC_GET_TRANSACTIONS::response`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transactions {
    /// Blobs of the transactions found, in request order (each hash once on xkrd).
    pub blobs: Vec<Vec<u8>>,
    /// Hashes the node does not have.
    pub missed: Vec<Hash>,
}

/// `/gettransactions` on the wire.
#[derive(Debug, Deserialize)]
pub(crate) struct TransactionsWire {
    #[serde(default)]
    pub txs_as_hex: Vec<String>,
    #[serde(default, deserialize_with = "hex_list::deserialize")]
    pub missed_tx: Vec<Hash>,
}

/// A block header (`block_header_response` of `rpc_server.cpp` `fill_block_header_response`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BlockHeader {
    /// Major version.
    pub major_version: u8,
    /// Minor version.
    pub minor_version: u8,
    /// Timestamp.
    pub timestamp: u64,
    /// Previous block id.
    #[serde(with = "hex_field")]
    pub prev_hash: Hash,
    /// Nonce.
    pub nonce: u32,
    /// Whether the block is off the main chain.
    pub orphan_status: bool,
    /// Block index.
    pub height: u64,
    /// Blocks on top of it.
    pub depth: u64,
    /// Block id.
    #[serde(with = "hex_field")]
    pub hash: Hash,
    /// Difficulty the block was mined at.
    pub difficulty: u64,
    /// Coinbase total.
    pub reward: u64,
    /// Transactions including the coinbase.
    pub num_txes: u64,
    /// Block size.
    pub block_size: u64,
}

/// `getblocktemplate` (`COMMAND_RPC_GETBLOCKTEMPLATE::response`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockTemplate {
    /// The block to mine (hashing blob source; the nonce goes at the usual offset).
    pub blob: Vec<u8>,
    /// Difficulty to reach.
    pub difficulty: u64,
    /// Height of the block.
    pub height: u64,
    /// Offset of the reserved `extra` nonce space in `blob` (0 when `reserve_size` was 0).
    pub reserved_offset: u64,
}

/// `getblocktemplate` on the wire.
#[derive(Debug, Deserialize)]
pub(crate) struct BlockTemplateWire {
    pub blocktemplate_blob: String,
    pub difficulty: u64,
    pub height: u64,
    #[serde(default)]
    pub reserved_offset: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn wallet_blocks_parse_with_and_without_view_tags() {
        let key = "11".repeat(32);
        let block: WalletBlock = serde_json::from_value(json!({
            "coinbaseTX": {
                "outputs": [{ "key": key, "amount": 5, "view_tag": "0a" }],
                "hash": "22".repeat(32),
                "txPublicKey": "33".repeat(32),
                "unlockTime": 10,
            },
            "transactions": [{
                "inputs": [{ "amount": 7, "key_offsets": [1, 2], "k_image": "44".repeat(32) }],
                "paymentID": "",
                "outputs": [{ "key": key, "amount": 6 }],
                "hash": "55".repeat(32),
                "txPublicKey": "66".repeat(32),
                "unlockTime": 0,
            }],
            "blockHeight": 9,
            "blockHash": "77".repeat(32),
            "blockTimestamp": 1_750_000_000u64,
        }))
        .unwrap();
        assert_eq!(block.coinbase.outputs[0].view_tag, Some(0x0a));
        assert_eq!(block.transactions[0].outputs[0].view_tag, None);
        assert_eq!(block.transactions[0].payment_id, None);
        assert_eq!(block.transactions[0].inputs[0].key_offsets, vec![1, 2]);
        assert_eq!(block.hash, Hash([0x77; 32]));

        let with_pid: RawTransaction = serde_json::from_value(json!({
            "inputs": [], "paymentID": "ab".repeat(32), "outputs": [],
            "hash": "55".repeat(32), "txPublicKey": "00".repeat(32), "unlockTime": 0,
        }))
        .unwrap();
        assert_eq!(with_pid.payment_id, Some(Hash([0xab; 32])));
        assert!(with_pid.tx_public_key.is_zero());

        let bad_tag = serde_json::from_value::<KeyOutput>(
            json!({ "key": "11".repeat(32), "amount": 1, "view_tag": "abc" }),
        );
        assert!(bad_tag.is_err());
        let bad_key = serde_json::from_value::<KeyOutput>(json!({ "key": "11", "amount": 1 }));
        assert!(bad_key.is_err());
    }

    #[test]
    fn node_fee_is_payable_only_to_a_standard_address() {
        let none = NodeFee {
            address: String::new(),
            amount: 0,
        };
        assert_eq!(none.payable(), None);
        let garbage = NodeFee {
            address: "not an address".into(),
            amount: 10,
        };
        assert_eq!(garbage.payable(), None);
    }
}
