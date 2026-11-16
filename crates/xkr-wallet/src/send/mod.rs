//! Sending (`src/wallet_backend/transfer.cpp` `send_transaction::sendTransactionBasic`,
//! `sendTransactionAdvanced`, `makeTransaction`, `storeSentTransaction`,
//! `storeUnconfirmedIncomingInputs`; checks of `src/errors/validate_parameters.cpp`
//! `validateTransaction`).
//!
//! A send is prepared ([`prepare_transfer`]: checks, input selection, ring members, signing)
//! and then relayed and recorded ([`store_sent`]). The transaction version is the highest the
//! daemon's `/info` `tx_versions` allows (`xkr_wallet_core::rules::tx_version_for`); the node
//! fee from `/fee` is added as a destination when the node asks for one.

pub mod delayed;
pub mod fusion;
pub mod relay;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use xkr_consensus::RingSizeRule;
use xkr_crypto::{
    derive_secret_key, generate_key_derivation, generate_key_image, os_rng,
    secret_key_to_public_key,
};
use xkr_daemon_client::Transport;
use xkr_types::{Hash, PublicKey, SecretKey, TxInput, tx_public_key};
use xkr_wallet_core::rules::{self, check_fee, minimum_fee};
use xkr_wallet_core::select::{self, pretty_amount_power};
use xkr_wallet_core::{
    Address, BuiltTransaction, Destination, InputSource, SpendKeys, TransferError, TransferOptions,
    TxVersion, build_transaction, scan_for_subwallets,
};
use xkr_wallet_files::backend::UnknownFields;

use crate::error::{ErrorCode, WalletError, walletd_codes};
use crate::state::{
    Candidate, OwnedInput, Transaction, TransferDestination, TransferKind, UnconfirmedInput,
    WalletState, unix_now,
};
use crate::sync::refresh_info;
use crate::validate::{validate_address, validate_payment_id};
use crate::wallet::Shared;

/// A transfer (wallet-api `/transactions/send/advanced`, walletd `sendTransaction`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendRequest {
    /// `(address, amount)` pairs; integrated addresses carry their payment id.
    pub destinations: Vec<(String, u64)>,
    /// Ring size - 1; `None` for the network default at the current height.
    pub mixin: Option<u64>,
    /// Fee; `None` for `MINIMUM_FEE`.
    pub fee: Option<u64>,
    /// Payment id (64 hex characters), empty for none.
    pub payment_id: String,
    /// Addresses to take the inputs from; empty for all.
    pub source_addresses: Vec<String>,
    /// Where the change goes; `None` for the primary address.
    pub change_address: Option<String>,
    /// Unlock time of the outputs.
    pub unlock_time: u64,
    /// Raw bytes appended to `extra` (walletd `extra`).
    pub extra: Vec<u8>,
}

impl SendRequest {
    /// `sendTransactionBasic`: one destination, default fee and mixin, change to the primary
    /// address.
    #[must_use]
    pub fn basic(destination: &str, amount: u64, payment_id: &str) -> Self {
        Self {
            destinations: vec![(destination.to_owned(), amount)],
            payment_id: payment_id.to_owned(),
            ..Self::default()
        }
    }
}

/// What a send needs to know about the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NetworkContext {
    /// `Nigel::networkBlockCount`.
    pub height: u64,
    /// Transaction version to build.
    pub version: TxVersion,
    /// The network's ring-size rule for version 2.
    pub ring_rule: RingSizeRule,
    /// The node fee to pay.
    pub node_fee: Option<(Address, u64)>,
    /// Unix time.
    pub now: u64,
}

/// Fresh daemon facts for a send (the cached ones if `/info` fails now).
pub(crate) async fn context<T: Transport>(
    shared: &Shared<T>,
) -> Result<NetworkContext, WalletError> {
    let info = match refresh_info(shared).await {
        Ok(info) => info,
        Err(e) => shared
            .daemon
            .read()
            .info
            .clone()
            .ok_or_else(|| relay::daemon_offline(&e))?,
    };
    let node_fee = shared.daemon.read().payable_fee();
    Ok(NetworkContext {
        height: info.network_block_count(),
        version: rules::tx_version_for(info.tx_versions.as_deref(), Some(info.major_version)),
        ring_rule: shared.config.network.params().fork14.v2_ring_size,
        node_fee,
        now: unix_now(),
    })
}

/// A signed transaction and what the wallet records when it is sent.
#[derive(Clone)]
pub(crate) struct Prepared {
    /// The transaction.
    pub built: BuiltTransaction,
    /// The own outputs it spends, with their subwallet.
    pub inputs: Vec<(PublicKey, OwnedInput)>,
    /// Fee.
    pub fee: u64,
    /// Payment id (hex) or empty.
    pub payment_id: String,
    /// Change subwallet and amount (`storeSentTransaction`'s `changeRequired`).
    pub change: Option<(PublicKey, u64)>,
    /// The destinations as given (integrated addresses as standard ones).
    pub destinations: Vec<TransferDestination>,
    /// Own outputs of the transaction (change, payments to self).
    pub unconfirmed: Vec<(PublicKey, UnconfirmedInput)>,
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepared")
            .field("hash", &self.built.hash)
            .field("fee", &self.fee)
            .field("inputs", &self.inputs.len())
            .finish_non_exhaustive()
    }
}

impl Prepared {
    /// The wallet's record of the sent transaction (`storeSentTransaction`): minus every input
    /// per subwallet, plus the change; no block, timestamp or unlock time yet.
    #[must_use]
    pub(crate) fn record(&self) -> Transaction {
        let mut tx = Transaction::new(self.built.hash);
        for (owner, input) in &self.inputs {
            tx.add_transfer(*owner, -(input.amount as i64));
        }
        if let Some((owner, amount)) = self.change {
            tx.add_transfer(owner, amount as i64);
        }
        tx.fee = self.fee;
        tx.payment_id.clone_from(&self.payment_id);
        tx.extra = Some(self.built.tx.prefix.extra.clone());
        tx.destinations.clone_from(&self.destinations);
        tx.spent_key_images = self.inputs.iter().map(|(_, i)| i.key_image).collect();
        tx
    }
}

/// Records a relayed transaction (`storeSentTransaction`, `storeUnconfirmedIncomingInputs`,
/// `storeTxPrivateKey`, `markInputAsLocked`) and returns its record.
pub(crate) fn store_sent(state: &mut WalletState, prepared: &Prepared) -> Transaction {
    let tx = prepared.record();
    state.add_unconfirmed(tx.clone());
    for (owner, input) in &prepared.unconfirmed {
        state.store_unconfirmed_incoming(owner, input.clone());
    }
    state.store_tx_private_key(prepared.built.hash, prepared.built.tx_secret_key);
    for (owner, input) in &prepared.inputs {
        state.mark_locked(owner, &input.key_image);
    }
    tx
}

/// `validateOurAddresses` for one address: its spend key, which must be a subwallet.
pub(crate) fn our_key(state: &WalletState, address: &str) -> Result<PublicKey, WalletError> {
    let parsed = validate_address(address, false)?;
    if state.subwallet(&parsed.spend).is_none() {
        return Err(WalletError::with_message(
            ErrorCode::AddressNotInWallet,
            format!(
                "The address given ({address}) does not exist in the wallet container, but it \
                 is required to exist for this operation."
            ),
        ));
    }
    Ok(parsed.spend)
}

/// `validateOurAddresses` for a list: `None` (all subwallets) for an empty list.
pub(crate) fn our_keys(
    state: &WalletState,
    addresses: &[String],
) -> Result<Option<Vec<PublicKey>>, WalletError> {
    if addresses.is_empty() {
        return Ok(None);
    }
    addresses
        .iter()
        .map(|a| our_key(state, a))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Maps a construction error to the C++ code.
pub(crate) fn transfer_error(e: &TransferError) -> WalletError {
    let code = match e {
        TransferError::NoInputs | TransferError::NotEnoughFunds { .. } => {
            ErrorCode::NotEnoughBalance
        }
        TransferError::NoDestinations => ErrorCode::NoDestinationsGiven,
        TransferError::ZeroAmount => ErrorCode::AmountIsZero,
        TransferError::Overflow => ErrorCode::WillOverflow,
        TransferError::ConflictingPaymentIds => ErrorCode::ConflictingPaymentIds,
        TransferError::InvalidKey => ErrorCode::AddressNotValid,
        TransferError::ExtraTooLarge(_) => {
            return WalletError::new(ErrorCode::TooManyInputsToFitInBlock)
                .with_detail(e.to_string())
                .with_walletd_code(walletd_codes::EXTRA_TOO_LARGE);
        }
        TransferError::FusionTooLarge(_) | TransferError::NotFusion => ErrorCode::FullyOptimized,
        TransferError::DuplicateRingMember(_)
        | TransferError::Signing(_)
        | TransferError::Encode(_) => ErrorCode::FailedToCreateRingSignature,
    };
    WalletError::new(code).with_detail(e.to_string())
}

/// The one-time secrets of the chosen outputs, with their rings (`setupInputs`, `genKeyImage`):
/// `INVALID_GENERATED_KEYIMAGE` if a secret does not give the output's key or key image.
pub(crate) fn input_sources(
    chosen: &[Candidate],
    rings: &[Vec<(u32, PublicKey)>],
    view_secret: &SecretKey,
) -> Result<Vec<InputSource>, WalletError> {
    chosen
        .iter()
        .zip(rings)
        .map(|(c, decoys)| {
            let bad = || WalletError::new(ErrorCode::InvalidGeneratedKeyimage);
            let derivation = generate_key_derivation(&c.input.transaction_public_key, view_secret)
                .ok_or_else(bad)?;
            let secret = derive_secret_key(&derivation, c.input.transaction_index, &c.spend_secret);
            if secret_key_to_public_key(&secret) != Some(c.input.key) {
                return Err(bad());
            }
            let key_image = generate_key_image(&c.input.key, &secret);
            if key_image != c.input.key_image {
                return Err(bad());
            }
            let global_index = c
                .input
                .global_output_index
                .and_then(|g| u32::try_from(g).ok())
                .ok_or_else(|| {
                    WalletError::new(ErrorCode::DaemonError)
                        .with_detail("the global index of an output is not known")
                })?;
            Ok(InputSource {
                amount: c.input.amount,
                global_index,
                key: c.input.key,
                secret,
                key_image,
                decoys: decoys.clone(),
            })
        })
        .collect()
}

/// `verifyAmounts`: every output amount is in `PRETTY_AMOUNTS`.
pub(crate) fn verify_amounts(built: &BuiltTransaction) -> Result<(), WalletError> {
    if built
        .tx
        .prefix
        .outputs
        .iter()
        .all(|o| pretty_amount_power(o.amount).is_some())
    {
        Ok(())
    } else {
        Err(ErrorCode::AmountsNotPretty.into())
    }
}

/// `verifyTransactionFee`: inputs - outputs is the fee asked for.
pub(crate) fn verify_fee(built: &BuiltTransaction, fee: u64) -> Result<(), WalletError> {
    let inputs: u64 = built
        .tx
        .prefix
        .inputs
        .iter()
        .map(|i| match i {
            TxInput::Key { amount, .. } => *amount,
            TxInput::Coinbase { .. } => 0,
        })
        .fold(0, u64::wrapping_add);
    let outputs: u64 = built
        .tx
        .prefix
        .outputs
        .iter()
        .map(|o| o.amount)
        .fold(0, u64::wrapping_add);
    if inputs.wrapping_sub(outputs) == fee {
        Ok(())
    } else {
        Err(ErrorCode::UnexpectedFee.into())
    }
}

/// `storeUnconfirmedIncomingInputs`: the outputs of `built` that belong to a subwallet.
pub(crate) fn own_outputs(
    view_secret: &SecretKey,
    keys: &SpendKeys,
    built: &BuiltTransaction,
) -> Vec<(PublicKey, UnconfirmedInput)> {
    let Some(tx_pub) = tx_public_key(&built.tx.prefix.extra) else {
        return Vec::new();
    };
    let outputs: Vec<(u64, PublicKey, Option<u8>)> = built
        .tx
        .prefix
        .outputs
        .iter()
        .map(|o| (o.amount, *o.key(), o.view_tag()))
        .collect();
    scan_for_subwallets(view_secret, keys, &tx_pub, &outputs)
        .into_iter()
        .map(|o| {
            (
                o.owner,
                UnconfirmedInput {
                    amount: o.amount,
                    key: o.key,
                    parent_transaction_hash: built.hash,
                    other: UnknownFields::new(),
                },
            )
        })
        .collect()
}

/// Parsed destinations with the payment id of their integrated addresses.
type ParsedDestinations = (Vec<(Address, u64)>, Option<Hash>);

/// The parsed destinations of a transfer (`validateDestinations`) and the payment id of its
/// integrated addresses (`validateIntegratedAddresses`).
fn parse_destinations(
    destinations: &[(String, u64)],
    payment_id: &str,
) -> Result<ParsedDestinations, WalletError> {
    if destinations.is_empty() {
        return Err(ErrorCode::NoDestinationsGiven.into());
    }
    if destinations.iter().any(|(_, amount)| *amount == 0) {
        return Err(ErrorCode::AmountIsZero.into());
    }
    let parsed = destinations
        .iter()
        .map(|(a, amount)| validate_address(a, true).map(|a| (a, *amount)))
        .collect::<Result<Vec<_>, _>>()?;
    let given = validate_payment_id(payment_id).ok().flatten();
    let mut integrated: Option<Hash> = None;
    for (address, _) in &parsed {
        let Some(pid) = address.payment_id else {
            continue;
        };
        match given.or(integrated) {
            Some(p) if p != pid => return Err(ErrorCode::ConflictingPaymentIds.into()),
            _ => integrated = Some(pid),
        }
    }
    Ok((parsed, integrated))
}

/// What [`prepare_transfer`] takes from the state under the lock.
struct Chosen {
    inputs: Vec<Candidate>,
    sum: u64,
    total: u64,
    parsed: Vec<(Address, u64)>,
    payment_id: Option<Hash>,
    change_key: PublicKey,
    view_secret: SecretKey,
    view_public: PublicKey,
    keys: Arc<SpendKeys>,
}

/// `sendTransactionAdvanced` up to the relay: checks (`validateTransaction`), input selection
/// (`getTransactionInputsForAmount`), ring members, signing, size, amount and fee checks.
pub(crate) async fn prepare_transfer<T: Transport + 'static>(
    shared: &Arc<Shared<T>>,
    request: &SendRequest,
) -> Result<Prepared, WalletError> {
    if shared.state.lock().is_view_wallet {
        return Err(ErrorCode::IllegalViewWalletOperation.into());
    }
    let ctx = context(shared).await?;
    let mut destinations = request.destinations.clone();
    if let Some((address, amount)) = ctx.node_fee {
        // walletd lists the node fee first (`convertWalletRpcOrdersToWalletOrders`),
        // `WalletBackend::sendTransactionAdvanced` appends it
        let fee = (address.to_string(), amount);
        if shared.node_fee_first.load(Ordering::Relaxed) {
            destinations.insert(0, fee);
        } else {
            destinations.push(fee);
        }
    }
    let fee = request.fee.unwrap_or_else(minimum_fee);
    let range = rules::mixin_range(ctx.height, ctx.version, ctx.ring_rule);
    let mixin = request.mixin.unwrap_or(range.default);

    let chosen = {
        let s = shared.state.lock();
        let (parsed, integrated) = parse_destinations(&destinations, &request.payment_id)?;
        let sources = our_keys(&s, &request.source_addresses)?;
        check_fee(fee)?;
        let mut total = fee;
        for (_, amount) in &parsed {
            total = total
                .checked_add(*amount)
                .ok_or_else(|| WalletError::new(ErrorCode::WillOverflow))?;
        }
        if total > s.balance(sources.as_deref(), ctx.height, ctx.now).unlocked {
            return Err(ErrorCode::NotEnoughBalance.into());
        }
        range.check(mixin)?;
        let given = validate_payment_id(&request.payment_id)?;
        let change_text = request
            .change_address
            .clone()
            .unwrap_or_else(|| s.try_primary().map_or_default(|p| p.address.clone()));
        let change_key = our_key(&s, &change_text)?;
        let candidates = s.spendable(sources.as_deref(), ctx.height, ctx.now);
        let selection =
            select::inputs_for_amount(&candidates, total, ctx.height, ctx.now, &mut os_rng())?;
        Chosen {
            sum: selection.sum,
            inputs: selection.inputs.into_iter().cloned().collect(),
            total,
            parsed,
            payment_id: integrated.or(given),
            change_key,
            view_secret: s.view_secret,
            view_public: s.view_public,
            keys: Arc::clone(&s.spend_keys),
        }
    };

    let mut inputs = chosen.inputs;
    inputs.sort_by_key(|c| c.input.amount);
    let node = shared.node();
    let rings = relay::ring_members(&node.client, mixin, &inputs).await?;
    let sources = input_sources(&inputs, &rings, &chosen.view_secret)?;
    let dests: Vec<Destination> = chosen
        .parsed
        .iter()
        .map(|(address, amount)| Destination {
            address: address.standard(),
            amount: *amount,
        })
        .collect();
    let change_address = Address::new(chosen.change_key, chosen.view_public);
    let options = TransferOptions {
        version: ctx.version,
        fee,
        unlock_time: request.unlock_time,
        payment_id: chosen.payment_id,
        data: None,
        dust_threshold: rules::dust_threshold(ctx.height),
        extra_raw: request.extra.clone(),
    };
    let built = build_transaction(&sources, &dests, &change_address, &options, &mut os_rng())
        .map_err(|e| transfer_error(&e))?;
    rules::check_tx_size(built.blob.len() as u64, ctx.height)?;
    verify_amounts(&built)?;
    verify_fee(&built, fee)?;
    let unconfirmed = own_outputs(&chosen.view_secret, &chosen.keys, &built);
    let change = chosen.sum - chosen.total;
    // walletd lists the change after the destinations (`WalletGreen::prepareTransaction`)
    let mut destinations: Vec<TransferDestination> = chosen
        .parsed
        .iter()
        .map(|(address, amount)| TransferDestination {
            address: address.standard().to_string(),
            amount: *amount as i64,
            kind: TransferKind::Usual,
        })
        .collect();
    if change > 0 {
        destinations.push(TransferDestination {
            address: change_address.to_string(),
            amount: change as i64,
            kind: TransferKind::Change,
        });
    }
    Ok(Prepared {
        built,
        inputs: inputs.into_iter().map(|c| (c.owner, c.input)).collect(),
        fee,
        payment_id: chosen.payment_id.as_ref().map_or_default(Hash::to_hex),
        change: (change > 0).then_some((chosen.change_key, change)),
        destinations,
        unconfirmed,
    })
}
