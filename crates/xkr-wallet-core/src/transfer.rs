//! Transaction construction (CryptoNote v1: per-amount rings, `generateRingSignatures`).
//!
//! The caller picks the inputs (owned outputs with their one-time secrets, see
//! [`crate::scan`]) and, per input, decoys of the **same amount** (from the daemon's
//! `/getrandom_outs`). This module:
//!
//! 1. creates a fresh transaction key pair `(R, r)`;
//! 2. splits every destination amount and the change into denominations
//!    (`decompose_amount_into_digits` with the dust threshold of the height);
//! 3. derives one-time output keys `H_s(8*r*A || i)*G + B` per output index;
//! 4. builds each ring sorted by global index, encodes the indexes relatively
//!    (`absoluteOutputOffsetsToRelative`) and signs it with `generate_ring_signature` over the
//!    prefix hash.
//!
//! The result passes `validate_semantic` and the pool policy (fee, extra size) when the caller
//! respects the mixin limits of the current height.
//!
//! With [`TransferOptions::version`] = [`TxVersion::V2`] (fork 14, `docs/tx-v2.md`) the outputs
//! are tagged keys (`view_tag(8rA, i)`) and every input gets a single-key CLSAG signature
//! (`clsag_sign`) instead; rings, key images and amounts are the same.

use rand_core::CryptoRng;
use xkr_consensus::params::FUSION_TX_MAX_SIZE;
use xkr_consensus::reward::{
    decompose_amount, default_fusion_dust_threshold, is_fusion_transaction,
};
use xkr_crypto::{
    clsag_sign, cn_fast_hash, derive_public_key, generate_key_derivation, generate_keys,
    generate_ring_signature, view_tag,
};
use xkr_serialization::{
    CURRENT_TRANSACTION_VERSION, TRANSACTION_VERSION_2, encode_transaction, transaction_prefix_hash,
};
use xkr_types::{
    Hash, KeyDerivation, KeyImage, PublicKey, SecretKey, Transaction, TransactionPrefix, TxInput,
    TxOutput, TxOutputTarget, absolute_to_relative,
};

use crate::address::Address;
use crate::extra::{MAX_EXTRA_LEN, build_extra};

/// An owned output to spend, with its ring.
#[derive(Clone, PartialEq, Eq)]
pub struct InputSource {
    /// Amount (every ring member has it too).
    pub amount: u64,
    /// Global index of the real output among outputs of this amount.
    pub global_index: u32,
    /// One-time public key of the real output.
    pub key: PublicKey,
    /// One-time secret of the real output ([`crate::FoundOutput::secret`]).
    pub secret: SecretKey,
    /// Key image of the real output.
    pub key_image: KeyImage,
    /// Decoys `(global index, key)` of the same amount; the ring size is `decoys.len() + 1`.
    pub decoys: Vec<(u32, PublicKey)>,
}

impl std::fmt::Debug for InputSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputSource")
            .field("amount", &self.amount)
            .field("global_index", &self.global_index)
            .field("key", &self.key)
            .field("decoys", &self.decoys.len())
            .finish_non_exhaustive()
    }
}

/// A payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Destination {
    /// Recipient (an integrated address carries its payment id).
    pub address: Address,
    /// Amount in atomic units.
    pub amount: u64,
}

/// Which transaction format to build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TxVersion {
    /// Version 1: plain output keys, CryptoNote ring signatures (valid until the end of the
    /// fork-14 grace period).
    #[default]
    V1,
    /// Version 2: tagged output keys, CLSAG signatures (valid after fork 14).
    V2,
}

/// Everything else about the transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferOptions {
    /// Transaction format (default version 1).
    pub version: TxVersion,
    /// Fee in atomic units (pool minimum `MINIMUM_FEE` = 10 unless it is a fusion transaction).
    pub fee: u64,
    /// `unlock_time` (0 = spendable at once).
    pub unlock_time: u64,
    /// Payment id (overrides none; must agree with integrated destinations).
    pub payment_id: Option<Hash>,
    /// Arbitrary data in the extra nonce (Hugin), at most ~2150 bytes.
    pub data: Option<Vec<u8>>,
    /// Dust threshold for the denominations (`default_dust_threshold(height)`: 10 below height
    /// 620 000, 0 above).
    pub dust_threshold: u64,
    /// Raw bytes appended to `extra` after the standard fields (transaction public key, payment
    /// id, data), as walletd appends its `extra` parameter (`WalletGreen::makeTransaction`
    /// `appendExtra`). Empty for none.
    pub extra_raw: Vec<u8>,
}

/// A signed transaction.
#[derive(Clone, PartialEq, Eq)]
pub struct BuiltTransaction {
    /// The transaction.
    pub tx: Transaction,
    /// Its serialization (`tx_as_hex` for `/sendrawtransaction`).
    pub blob: Vec<u8>,
    /// Its hash.
    pub hash: Hash,
    /// The transaction secret key `r` (proves payments).
    pub tx_secret_key: SecretKey,
    /// Fee paid.
    pub fee: u64,
}

impl std::fmt::Debug for BuiltTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltTransaction")
            .field("hash", &self.hash)
            .field("fee", &self.fee)
            .finish_non_exhaustive()
    }
}

/// Why no transaction could be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransferError {
    /// No inputs given.
    #[error("no inputs")]
    NoInputs,
    /// No destinations given.
    #[error("no destinations")]
    NoDestinations,
    /// A destination of amount zero.
    #[error("destination amount is zero")]
    ZeroAmount,
    /// Inputs do not cover destinations plus fee.
    #[error("not enough funds: have {have}, need {need}")]
    NotEnoughFunds {
        /// Sum of the inputs.
        have: u64,
        /// Destinations plus fee.
        need: u64,
    },
    /// An amount sum overflows.
    #[error("amount overflow")]
    Overflow,
    /// A ring uses the same global index twice.
    #[error("ring of input {0} contains a global index twice")]
    DuplicateRingMember(usize),
    /// Integrated destinations with different payment ids.
    #[error("destinations carry different payment ids")]
    ConflictingPaymentIds,
    /// A destination key is not a valid point.
    #[error("invalid destination key")]
    InvalidKey,
    /// `generate_ring_signature` failed (a decoy key does not decode).
    #[error("ring signature of input {0} could not be generated")]
    Signing(usize),
    /// `extra` would be refused by the pool.
    #[error("extra of {0} bytes is too large")]
    ExtraTooLarge(usize),
    /// Serialization failed.
    #[error("encode: {0}")]
    Encode(String),
    /// A fusion transaction larger than `FUSION_TX_MAX_SIZE` (take fewer inputs).
    #[error("fusion transaction of {0} bytes is too large")]
    FusionTooLarge(usize),
    /// The inputs do not make a fusion transaction (fewer than `FUSION_TX_MIN_INPUT_COUNT`, fewer
    /// than `FUSION_TX_MIN_IN_OUT_COUNT_RATIO` inputs per output, or an input below the fusion
    /// dust threshold).
    #[error("the inputs do not make a fusion transaction")]
    NotFusion,
}

/// Builds and signs a transaction paying `destinations`, returning the change to `change`.
pub fn build_transaction<R: CryptoRng + ?Sized>(
    inputs: &[InputSource],
    destinations: &[Destination],
    change: &Address,
    opts: &TransferOptions,
    rng: &mut R,
) -> Result<BuiltTransaction, TransferError> {
    if inputs.is_empty() {
        return Err(TransferError::NoInputs);
    }
    if destinations.is_empty() {
        return Err(TransferError::NoDestinations);
    }
    if destinations.iter().any(|d| d.amount == 0) {
        return Err(TransferError::ZeroAmount);
    }
    let have = inputs
        .iter()
        .try_fold(0u64, |acc, i| acc.checked_add(i.amount))
        .ok_or(TransferError::Overflow)?;
    let paid = destinations
        .iter()
        .try_fold(0u64, |acc, d| acc.checked_add(d.amount))
        .ok_or(TransferError::Overflow)?;
    let need = paid.checked_add(opts.fee).ok_or(TransferError::Overflow)?;
    if have < need {
        return Err(TransferError::NotEnoughFunds { have, need });
    }
    let change_amount = have - need;

    let mut payment_id = opts.payment_id;
    for d in destinations {
        if let Some(pid) = d.address.payment_id {
            match payment_id {
                Some(p) if p != pid => return Err(TransferError::ConflictingPaymentIds),
                _ => payment_id = Some(pid),
            }
        }
    }

    // Denominations, smallest first.
    let mut pieces: Vec<(Address, u64)> = Vec::new();
    for d in destinations {
        pieces.extend(
            decompose_amount(d.amount, opts.dust_threshold)
                .into_iter()
                .map(|a| (d.address, a)),
        );
    }
    if change_amount > 0 {
        pieces.extend(
            decompose_amount(change_amount, opts.dust_threshold)
                .into_iter()
                .map(|a| (*change, a)),
        );
    }
    pieces.sort_by_key(|p| p.1);

    let (tx_public_key, tx_secret_key) = generate_keys(rng);
    let mut derivations: Vec<(PublicKey, KeyDerivation)> = Vec::new();
    let mut outputs = Vec::with_capacity(pieces.len());
    for (index, (address, amount)) in pieces.iter().enumerate() {
        let derivation = if let Some((_, d)) = derivations.iter().find(|(v, _)| *v == address.view)
        {
            *d
        } else {
            let d = generate_key_derivation(&address.view, &tx_secret_key)
                .ok_or(TransferError::InvalidKey)?;
            derivations.push((address.view, d));
            d
        };
        let key = derive_public_key(&derivation, index as u64, &address.spend)
            .ok_or(TransferError::InvalidKey)?;
        let target = match opts.version {
            TxVersion::V1 => TxOutputTarget::Key(key),
            TxVersion::V2 => TxOutputTarget::TaggedKey {
                key,
                view_tag: view_tag(&derivation, index as u64),
            },
        };
        outputs.push(TxOutput {
            amount: *amount,
            target,
        });
    }

    // Rings sorted by global index.
    let mut rings: Vec<(Vec<PublicKey>, usize)> = Vec::with_capacity(inputs.len());
    let mut tx_inputs = Vec::with_capacity(inputs.len());
    for (n, input) in inputs.iter().enumerate() {
        let mut members: Vec<(u32, PublicKey)> = input.decoys.clone();
        members.push((input.global_index, input.key));
        members.sort_by_key(|m| m.0);
        if members.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(TransferError::DuplicateRingMember(n));
        }
        let real = members
            .iter()
            .position(|m| m.0 == input.global_index && m.1 == input.key)
            .ok_or(TransferError::DuplicateRingMember(n))?;
        let absolute: Vec<u32> = members.iter().map(|m| m.0).collect();
        tx_inputs.push(TxInput::Key {
            amount: input.amount,
            output_indexes: absolute_to_relative(&absolute),
            key_image: input.key_image,
        });
        rings.push((members.into_iter().map(|m| m.1).collect(), real));
    }

    let mut extra = build_extra(&tx_public_key, payment_id.as_ref(), opts.data.as_deref());
    extra.extend_from_slice(&opts.extra_raw);
    if extra.len() > MAX_EXTRA_LEN {
        return Err(TransferError::ExtraTooLarge(extra.len()));
    }
    let prefix = TransactionPrefix {
        version: match opts.version {
            TxVersion::V1 => CURRENT_TRANSACTION_VERSION,
            TxVersion::V2 => TRANSACTION_VERSION_2,
        },
        unlock_time: opts.unlock_time,
        inputs: tx_inputs,
        outputs,
        extra,
    };
    let prefix_hash = transaction_prefix_hash(&prefix);
    let mut signatures = Vec::new();
    let mut clsag_signatures = Vec::new();
    for (n, (input, (ring, real))) in inputs.iter().zip(&rings).enumerate() {
        match opts.version {
            TxVersion::V1 => signatures.push(
                generate_ring_signature(
                    &prefix_hash,
                    &input.key_image,
                    ring,
                    &input.secret,
                    *real,
                    rng,
                )
                .ok_or(TransferError::Signing(n))?,
            ),
            TxVersion::V2 => clsag_signatures.push(
                clsag_sign(&prefix_hash, ring, &input.secret, *real, rng)
                    .ok_or(TransferError::Signing(n))?,
            ),
        }
    }
    let tx = Transaction {
        prefix,
        signatures,
        clsag_signatures,
    };
    let blob = encode_transaction(&tx).map_err(|e| TransferError::Encode(e.to_string()))?;
    let hash = cn_fast_hash(&blob);
    Ok(BuiltTransaction {
        tx,
        blob,
        hash,
        tx_secret_key,
        fee: opts.fee,
    })
}

/// Builds a fusion transaction: every input goes, in denominations, to `destination` with fee 0
/// and no payment id (`src/wallet_backend/transfer.cpp`
/// `send_transaction::sendFusionTransactionAdvanced`, `WalletGreen::createFusionTransaction`).
///
/// The outputs are the decomposition of the input sum with the fusion dust threshold of
/// `height` (`decomposeAmount(sum, defaultFusionDustThreshold(height))`, which is 0 from height
/// 800 000 on and then the same as C++ `splitAmountIntoDenominations`). The result must pass
/// `Currency::isFusionTransaction` at `height`: [`TransferError::FusionTooLarge`] when it is
/// larger than `FUSION_TX_MAX_SIZE` (the caller drops an input and tries again, as C++ does),
/// [`TransferError::NotFusion`] for the other rules. Pick the inputs with
/// [`crate::select::fusion_inputs`].
pub fn build_fusion_transaction<R: CryptoRng + ?Sized>(
    inputs: &[InputSource],
    destination: &Address,
    version: TxVersion,
    height: u32,
    rng: &mut R,
) -> Result<BuiltTransaction, TransferError> {
    let sum = inputs
        .iter()
        .try_fold(0u64, |acc, i| acc.checked_add(i.amount))
        .ok_or(TransferError::Overflow)?;
    // A fusion transaction carries no payment id, so an integrated destination counts as its
    // standard address.
    let to = Address::new(destination.spend, destination.view);
    let opts = TransferOptions {
        version,
        fee: 0,
        dust_threshold: default_fusion_dust_threshold(height),
        ..TransferOptions::default()
    };
    let built = build_transaction(
        inputs,
        &[Destination {
            address: to,
            amount: sum,
        }],
        &to,
        &opts,
        rng,
    )?;
    let size = built.blob.len();
    if size > FUSION_TX_MAX_SIZE {
        return Err(TransferError::FusionTooLarge(size));
    }
    if !is_fusion_transaction(&built.tx, size, height) {
        return Err(TransferError::NotFusion);
    }
    Ok(built)
}

#[cfg(test)]
mod tests {
    use xkr_consensus::validate_semantic;
    use xkr_crypto::{check_ring_signature, clsag_verify, os_rng};
    use xkr_serialization::decode_transaction;
    use xkr_types::tx_public_key;

    use super::*;
    use crate::keys::WalletKeys;
    use crate::scan::{scan_outputs, scan_transaction};

    /// Sends an output to `keys` in a fake previous transaction and returns it as an input.
    fn owned_output(
        keys: &WalletKeys,
        amount: u64,
        global_index: u32,
        decoys: Vec<(u32, PublicKey)>,
    ) -> InputSource {
        let mut rng = os_rng();
        let (r_pub, r_sec) = generate_keys(&mut rng);
        let d = generate_key_derivation(&keys.view_public, &r_sec).unwrap();
        let key = derive_public_key(&d, 0, &keys.spend_public).unwrap();
        let found = scan_outputs(keys, &r_pub, &[(amount, key)]).pop().unwrap();
        InputSource {
            amount,
            global_index,
            key,
            secret: found.secret,
            key_image: found.key_image,
            decoys,
        }
    }

    #[test]
    fn builds_a_valid_ring_signed_transaction() {
        let mut rng = os_rng();
        let me = WalletKeys::generate(&mut rng);
        let them = WalletKeys::generate(&mut rng);
        let decoys = vec![
            (3, generate_keys(&mut rng).0),
            (9, generate_keys(&mut rng).0),
        ];
        let input = owned_output(&me, 1_000_000, 5, decoys);
        let opts = TransferOptions {
            fee: 10,
            dust_threshold: 0,
            data: Some(b"hello hugin".to_vec()),
            ..TransferOptions::default()
        };
        let built = build_transaction(
            std::slice::from_ref(&input),
            &[Destination {
                address: them.address(),
                amount: 123_456,
            }],
            &me.address(),
            &opts,
            &mut rng,
        )
        .unwrap();

        let tx = decode_transaction(&built.blob).unwrap();
        assert_eq!(tx, built.tx);
        assert_eq!(validate_semantic(&tx), Ok(10));
        let TxInput::Key {
            output_indexes,
            key_image,
            ..
        } = &tx.prefix.inputs[0]
        else {
            panic!("key input")
        };
        assert_eq!(output_indexes, &vec![3, 2, 4]);
        // ring order 3, 5, 9: the real output is in the middle
        let ring_keys = vec![input.decoys[0].1, input.key, input.decoys[1].1];
        assert!(check_ring_signature(
            &transaction_prefix_hash(&tx.prefix),
            key_image,
            &ring_keys,
            &tx.signatures[0]
        ));

        // The recipient finds exactly its 123 456, the sender its change.
        let r = tx_public_key(&tx.prefix.extra).unwrap();
        let tx_outputs: Vec<(u64, PublicKey)> = tx
            .prefix
            .outputs
            .iter()
            .map(|o| (o.amount, *o.key()))
            .collect();
        let to_them: u64 = scan_outputs(&them, &r, &tx_outputs)
            .iter()
            .map(|f| f.amount)
            .sum();
        let to_me: u64 = scan_outputs(&me, &r, &tx_outputs)
            .iter()
            .map(|f| f.amount)
            .sum();
        assert_eq!(to_them, 123_456);
        assert_eq!(to_me, 1_000_000 - 123_456 - 10);
    }

    #[test]
    fn builds_a_valid_version_2_transaction() {
        let mut rng = os_rng();
        let me = WalletKeys::generate(&mut rng);
        let them = WalletKeys::generate(&mut rng);
        let decoys = vec![
            (3, generate_keys(&mut rng).0),
            (9, generate_keys(&mut rng).0),
            (11, generate_keys(&mut rng).0),
        ];
        let input = owned_output(&me, 1_000_000, 5, decoys);
        let opts = TransferOptions {
            version: TxVersion::V2,
            fee: 10,
            ..TransferOptions::default()
        };
        let built = build_transaction(
            std::slice::from_ref(&input),
            &[Destination {
                address: them.address(),
                amount: 123_456,
            }],
            &me.address(),
            &opts,
            &mut rng,
        )
        .unwrap();
        let tx = decode_transaction(&built.blob).unwrap();
        assert_eq!(tx, built.tx);
        assert_eq!(tx.prefix.version, 2);
        assert_eq!(tx.signatures, Vec::<Vec<xkr_types::Signature>>::new());
        assert_eq!(validate_semantic(&tx), Ok(10));
        assert!(tx.prefix.outputs.iter().all(|o| o.view_tag().is_some()));
        // 32 * (ring size + 1) bytes of signature instead of 64 * ring size
        assert_eq!(tx.signature_section_len(), 32 * 5);
        let ring_keys = vec![
            input.decoys[0].1,
            input.key,
            input.decoys[1].1,
            input.decoys[2].1,
        ];
        assert!(clsag_verify(
            &transaction_prefix_hash(&tx.prefix),
            &ring_keys,
            &input.key_image,
            &tx.clsag_signatures[0]
        ));
        // View-tag scanning finds the same outputs.
        let to_them: u64 = scan_transaction(&them, &tx.prefix)
            .iter()
            .map(|f| f.amount)
            .sum();
        let to_me: u64 = scan_transaction(&me, &tx.prefix)
            .iter()
            .map(|f| f.amount)
            .sum();
        assert_eq!(to_them, 123_456);
        assert_eq!(to_me, 1_000_000 - 123_456 - 10);
    }

    #[test]
    fn rejects_bad_requests() {
        let mut rng = os_rng();
        let me = WalletKeys::generate(&mut rng);
        let input = owned_output(&me, 100, 0, vec![(0, generate_keys(&mut rng).0)]);
        let dest = [Destination {
            address: me.address(),
            amount: 50,
        }];
        let opts = TransferOptions {
            fee: 10,
            ..TransferOptions::default()
        };
        assert_eq!(
            build_transaction(
                std::slice::from_ref(&input),
                &dest,
                &me.address(),
                &opts,
                &mut rng
            ),
            Err(TransferError::DuplicateRingMember(0))
        );
        let input = InputSource {
            decoys: vec![],
            ..input
        };
        let big = [Destination {
            address: me.address(),
            amount: 95,
        }];
        assert_eq!(
            build_transaction(
                std::slice::from_ref(&input),
                &big,
                &me.address(),
                &opts,
                &mut rng
            ),
            Err(TransferError::NotEnoughFunds {
                have: 100,
                need: 105
            })
        );
        assert!(
            build_transaction(
                std::slice::from_ref(&input),
                &dest,
                &me.address(),
                &opts,
                &mut rng
            )
            .is_ok()
        );
    }

    #[test]
    fn extra_raw_follows_the_standard_fields() {
        let mut rng = os_rng();
        let me = WalletKeys::generate(&mut rng);
        let them = WalletKeys::generate(&mut rng);
        let input = owned_output(&me, 1_000, 0, vec![]);
        let pid = Hash([7u8; 32]);
        // a well-formed nonce field: tag 0x02, length 3
        let raw = vec![0x02, 0x03, 0xaa, 0xbb, 0xcc];
        let opts = TransferOptions {
            fee: 10,
            payment_id: Some(pid),
            extra_raw: raw.clone(),
            ..TransferOptions::default()
        };
        let dest = [Destination {
            address: them.address(),
            amount: 500,
        }];
        let built = build_transaction(
            std::slice::from_ref(&input),
            &dest,
            &me.address(),
            &opts,
            &mut rng,
        )
        .unwrap();
        let extra = &built.tx.prefix.extra;
        // 0x01 || R (33), payment id nonce (35), then the raw bytes
        assert_eq!(extra.len(), 33 + 35 + raw.len());
        assert_eq!(extra[0], 0x01);
        assert_eq!(&extra[33..35], &[0x02, 33]);
        assert!(extra.ends_with(&raw));
        assert_eq!(xkr_types::payment_id(extra), Some(pid));
        assert!(tx_public_key(extra).is_some());
        assert_eq!(validate_semantic(&built.tx), Ok(10));

        // the size limit counts the raw bytes too
        let opts = TransferOptions {
            fee: 10,
            extra_raw: vec![0u8; MAX_EXTRA_LEN],
            ..TransferOptions::default()
        };
        assert_eq!(
            build_transaction(
                std::slice::from_ref(&input),
                &dest,
                &me.address(),
                &opts,
                &mut rng
            ),
            Err(TransferError::ExtraTooLarge(33 + MAX_EXTRA_LEN))
        );
    }

    /// One input per amount, owned by `keys`, with `decoys` decoys each.
    fn owned_inputs(keys: &WalletKeys, amounts: &[u64], decoys: u32) -> Vec<InputSource> {
        let mut random = os_rng();
        amounts
            .iter()
            .enumerate()
            .map(|(n, &amount)| {
                let ring = (0..decoys)
                    .map(|d| (d, generate_keys(&mut random).0))
                    .collect();
                owned_output(keys, amount, 1_000 + n as u32, ring)
            })
            .collect()
    }

    #[test]
    fn fusion_transactions_pay_no_fee_and_follow_the_fusion_rules() {
        let mut rng = os_rng();
        let me = WalletKeys::generate(&mut rng);
        let height = 1_300_000;
        // 12 inputs of 100: 1 200 = 200 + 1000, two outputs, ratio 6
        let inputs = owned_inputs(&me, &[100; 12], 3);
        let built =
            build_fusion_transaction(&inputs, &me.address(), TxVersion::V1, height, &mut rng)
                .unwrap();
        assert_eq!(built.fee, 0);
        assert_eq!(validate_semantic(&built.tx), Ok(0));
        let amounts: Vec<u64> = built.tx.prefix.outputs.iter().map(|o| o.amount).collect();
        assert_eq!(amounts, vec![200, 1_000]);
        assert!(is_fusion_transaction(&built.tx, built.blob.len(), height));
        assert_eq!(xkr_types::payment_id(&built.tx.prefix.extra), None);
        let received: u64 = scan_transaction(&me, &built.tx.prefix)
            .iter()
            .map(|f| f.amount)
            .sum();
        assert_eq!(received, 1_200);
        // version 2 as well
        let built =
            build_fusion_transaction(&inputs, &me.address(), TxVersion::V2, height, &mut rng)
                .unwrap();
        assert_eq!(built.tx.prefix.version, 2);
        assert!(is_fusion_transaction(&built.tx, built.blob.len(), height));

        // 11 inputs are too few
        assert_eq!(
            build_fusion_transaction(
                &inputs[..11],
                &me.address(),
                TxVersion::V1,
                height,
                &mut rng
            ),
            Err(TransferError::NotFusion)
        );
        // 12 inputs summing to 123 456 789 make 9 outputs: fewer than 4 inputs per output
        let mut amounts = vec![1u64; 11];
        amounts.push(123_456_778);
        let spread = owned_inputs(&me, &amounts, 0);
        assert_eq!(
            build_fusion_transaction(&spread, &me.address(), TxVersion::V1, height, &mut rng),
            Err(TransferError::NotFusion)
        );
        // 12 rings of 41 members sign 12 * 41 * 64 bytes > FUSION_TX_MAX_SIZE
        let big = owned_inputs(&me, &[100; 12], 40);
        assert!(matches!(
            build_fusion_transaction(&big, &me.address(), TxVersion::V1, height, &mut rng),
            Err(TransferError::FusionTooLarge(size)) if size > FUSION_TX_MAX_SIZE
        ));
    }
}
