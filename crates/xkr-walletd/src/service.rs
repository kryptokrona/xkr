//! The open wallet behind the RPC server (`src/wallet_service/wallet_service.cpp`
//! `WalletService`), with what walletd reads from `WalletGreen` and its node that the engine
//! keeps in another form: the block count, block hashes from genesis, and per transaction the
//! full `extra` and the input and output totals.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use xkr_consensus::Network;
use xkr_daemon_client::{AnyTransport, DaemonClient, Transport};
use xkr_types::{Hash, PublicKey};
use xkr_wallet::{TransactionInfo, Wallet};

use crate::errors::{AppError, WalletCode};
use crate::model::{self, Transfer, TxFacts};

/// Most transactions asked for in one `/gettransactions` call.
const FACTS_CHUNK: usize = 100;

/// How the RPC server checks callers and what it adds to answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcSettings {
    /// `--rpc-legacy-security`: no password check.
    pub legacy_security: bool,
    /// `cn_slow_hash_v0(--rpc-password)` (`ConfigurationManager::rpcSecret`).
    pub secret: Hash,
    /// `--enable-cors`: the `Access-Control-Allow-Origin` value; empty for none.
    pub cors: String,
}

impl RpcSettings {
    /// Settings that check `password`.
    #[must_use]
    pub fn with_password(password: &str, cors: &str) -> Self {
        Self {
            legacy_security: false,
            secret: xkr_crypto::cn_slow_hash_v0(password.as_bytes()),
            cors: cors.to_owned(),
        }
    }

    /// Settings without a password check (`--rpc-legacy-security`).
    #[must_use]
    pub fn legacy(cors: &str) -> Self {
        Self {
            legacy_security: true,
            secret: Hash::ZERO,
            cors: cors.to_owned(),
        }
    }
}

/// The wallet, its daemon and the RPC settings.
pub struct Service<T: Transport + 'static = AnyTransport> {
    wallet: Arc<Wallet<T>>,
    daemon: DaemonClient<T>,
    genesis: Hash,
    rpc: RpcSettings,
    facts: Mutex<HashMap<Hash, TxFacts>>,
    /// The wallet's addresses with their spend keys, as last read.
    own: Mutex<Vec<(PublicKey, String)>>,
    /// `WalletService::readyEvent`: one method at a time.
    lock: tokio::sync::Mutex<()>,
}

impl<T: Transport + 'static> std::fmt::Debug for Service<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("wallet", &self.wallet)
            .field("rpc", &self.rpc)
            .finish_non_exhaustive()
    }
}

impl<T: Transport + 'static> Service<T> {
    /// A service for `wallet`; `daemon` is a client for the wallet's node.
    #[must_use]
    pub fn new(
        wallet: Arc<Wallet<T>>,
        daemon: DaemonClient<T>,
        network: Network,
        rpc: RpcSettings,
    ) -> Self {
        let genesis = xkr_consensus::genesis_hash(network.params()).unwrap_or(Hash::ZERO);
        // `convertWalletRpcOrdersToWalletOrders`: the node fee is the first order
        wallet.set_node_fee_first(true);
        // `WalletGreen(..., transactionSoftLockTime = 1)`: outputs of the last processed block
        // are not spent yet and count as pending
        wallet.set_spendable_age(1);
        Self {
            wallet,
            daemon,
            genesis,
            rpc,
            facts: Mutex::new(HashMap::new()),
            own: Mutex::new(Vec::new()),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The wallet.
    #[must_use]
    pub fn wallet(&self) -> &Arc<Wallet<T>> {
        &self.wallet
    }

    /// The RPC settings.
    #[must_use]
    pub fn rpc(&self) -> &RpcSettings {
        &self.rpc
    }

    /// Waits for the other methods to finish (`syst::EventLock lk(readyEvent)`).
    pub async fn serialize(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock.lock().await
    }

    /// The wallet's addresses with their public spend keys, in creation order.
    #[must_use]
    pub fn own_addresses(&self) -> Vec<(PublicKey, String)> {
        let addresses = self.wallet.addresses();
        let mut own = self.own.lock();
        let changed =
            own.len() != addresses.len() || own.iter().zip(&addresses).any(|((_, a), b)| a != b);
        if changed {
            // the wallet's own addresses are valid: only the spend key is read
            *own = addresses
                .into_iter()
                .filter_map(|a| {
                    let (_, data) = xkr_wallet_core::base58::decode_addr(&a)?;
                    let spend = PublicKey::from_slice(data.get(..32)?)?;
                    Some((spend, a))
                })
                .collect();
        }
        own.clone()
    }

    /// `WalletGreen::parseAddress` and the key index lookup: `BAD_ADDRESS` for an invalid
    /// address, `None` for one whose spend key is not in the wallet.
    pub fn own_key(&self, address: &str) -> Result<Option<PublicKey>, AppError> {
        let parsed = model::standard_address(address).ok_or(WalletCode::BadAddress)?;
        Ok(self
            .own_addresses()
            .into_iter()
            .find(|(k, _)| *k == parsed.spend)
            .map(|(k, _)| k))
    }

    /// The wallet's own address text for `address` (which may use the other prefix).
    #[must_use]
    pub fn own_address_text(&self, key: &PublicKey) -> Option<String> {
        self.own_addresses()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, a)| a)
    }

    /// `WalletGreen::getTrackingMode() == TRACKING` (`NONE` without addresses).
    #[must_use]
    pub fn is_tracking(&self) -> bool {
        self.has_addresses() && self.wallet.is_view_wallet()
    }

    /// Whether the container holds an address (`getTrackingMode() != NONE`).
    #[must_use]
    pub fn has_addresses(&self) -> bool {
        self.wallet.subwallet_count() > 0
    }

    /// `Nigel`/`NodeRpcProxy::getLastKnownBlockHeight`: the network's top block index.
    #[must_use]
    pub fn network_height(&self) -> u64 {
        self.wallet.daemon_status().network_block_count()
    }

    /// `WalletGreen::getBlockCount`: the blocks the wallet has processed, at least the genesis
    /// block.
    #[must_use]
    pub fn block_count(&self) -> u64 {
        let status = self.wallet.status();
        if status.last_block_hash.is_some() {
            status.wallet_block_count + 1
        } else {
            1
        }
    }

    /// `WalletGreen::getBlockHashes(index, count)`: the hashes of the processed blocks
    /// `index .. index + count` (fewer at the wallet's height, none past it).
    pub async fn block_hashes(&self, index: u64, count: u64) -> Result<Vec<Hash>, AppError> {
        let total = self.block_count();
        if index >= total || count == 0 {
            return Ok(Vec::new());
        }
        if self.wallet.status().last_block_hash.is_none() {
            return Ok(vec![self.genesis]);
        }
        let count = count.min(total - index);
        self.wallet
            .block_hashes(index, count)
            .await
            .map_err(AppError::from)
    }

    /// The hash of processed block `index`.
    pub async fn block_hash(&self, index: u64) -> Result<Hash, AppError> {
        self.block_hashes(index, 1)
            .await?
            .first()
            .copied()
            .ok_or_else(|| WalletCode::InternalWalletError.into())
    }

    /// The index of a processed block by its hash (`m_blockchain.get<BlockHashIndex>()`).
    pub async fn block_index_of(&self, hash: &Hash) -> Result<Option<u64>, AppError> {
        if *hash == self.genesis {
            return Ok(Some(0));
        }
        let index = self.wallet.block_height_of(hash).await?;
        Ok(index.filter(|i| *i < self.block_count()))
    }

    /// The daemon's copy of `hashes` that are not known yet. A daemon that does not answer
    /// leaves them unknown.
    pub async fn load_facts(&self, hashes: &[Hash]) {
        let missing: Vec<Hash> = {
            let known = self.facts.lock();
            hashes
                .iter()
                .filter(|h| !known.contains_key(h))
                .copied()
                .collect()
        };
        for chunk in missing.chunks(FACTS_CHUNK) {
            let answer = match self.daemon.transactions(chunk).await {
                Ok(answer) => answer,
                Err(e) => {
                    tracing::debug!("/gettransactions: {e}");
                    return;
                }
            };
            let found: Vec<Hash> = chunk
                .iter()
                .filter(|h| !answer.missed.contains(h))
                .copied()
                .collect();
            if found.len() != answer.blobs.len() {
                tracing::debug!("/gettransactions answered a different number of blobs");
                continue;
            }
            let mut known = self.facts.lock();
            for (hash, blob) in found.into_iter().zip(&answer.blobs) {
                match xkr_serialization::decode_transaction(blob) {
                    Ok(tx) => {
                        known.insert(hash, TxFacts::of(&tx));
                    }
                    Err(e) => tracing::debug!("transaction {hash} does not decode: {e}"),
                }
            }
        }
    }

    /// Forgets the cached daemon copies (after a reset).
    pub fn clear_facts(&self) {
        self.facts.lock().clear();
    }

    /// The `TransactionRpcInfo` JSON of each transaction, with transfers rebuilt as
    /// `WalletGreen` keeps them ([`model::wallet_transfers`]).
    pub async fn transactions_json(&self, infos: &[TransactionInfo]) -> Vec<serde_json::Value> {
        self.wallet_records(infos)
            .await
            .iter()
            .zip(infos)
            .map(|((transfers, extra), info)| model::transaction_json(info, transfers, extra))
            .collect()
    }

    /// The `WalletGreen` transfers and the full `extra` of each transaction.
    pub async fn wallet_records(&self, infos: &[TransactionInfo]) -> Vec<(Vec<Transfer>, Vec<u8>)> {
        let wanted: Vec<Hash> = infos
            .iter()
            .filter(|i| {
                let tx = &i.transaction;
                tx.extra.is_none() || (tx.destinations.is_empty() && !tx.is_coinbase)
            })
            .map(|i| i.transaction.hash)
            .collect();
        self.load_facts(&wanted).await;
        let hashes: Vec<Hash> = infos.iter().map(|i| i.transaction.hash).collect();
        let received = self.wallet.received_in(&hashes);
        let own_list = self.own_addresses();
        let own = model::Own::new(&own_list);
        let facts = self.facts.lock();
        infos
            .iter()
            .map(|info| {
                let tx = &info.transaction;
                let fact = facts.get(&tx.hash);
                let got = received.get(&tx.hash).map_or(&[][..], Vec::as_slice);
                let transfers: Vec<Transfer> =
                    model::wallet_transfers_with(tx, model::is_pending(info), &own, fact, got);
                let extra: &[u8] = tx
                    .extra
                    .as_deref()
                    .or_else(|| fact.map(|f| f.extra.as_slice()))
                    .unwrap_or_default();
                (transfers, extra.to_vec())
            })
            .collect()
    }
}
