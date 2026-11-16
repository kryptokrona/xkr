//! Pool admission and maintenance with real ring signatures over a fake chain.

use std::collections::{HashMap, HashSet};

use xkr_consensus::TxError;
use xkr_consensus::{Fork14Params, ForkSchedule, RingLookupError, RingSizeRule, TESTNET};
use xkr_crypto::{clsag_sign, generate_key_image, generate_keys, generate_ring_signature, os_rng};
use xkr_serialization::{encode_transaction, transaction_hash_from_blob, transaction_prefix_hash};
use xkr_types::{
    KeyImage, PublicKey, SecretKey, Transaction, TransactionPrefix, TxInput, TxOutput,
    TxOutputTarget,
};

use crate::{
    PoolChainState, PoolConfig, PoolError, TxPool, TxSource, canonical_ring_indexes,
    ring_global_indexes,
};

#[derive(Default)]
struct FakeChain {
    /// (amount, global index) -> key
    outputs: HashMap<(u64, u32), PublicKey>,
    spent: HashSet<KeyImage>,
    /// Fork table override (default: mainnet, fork 14 not scheduled).
    forks: Option<ForkSchedule>,
    /// Top block index override (default 100).
    top: Option<u32>,
}

impl PoolChainState for FakeChain {
    fn top_index(&self) -> u32 {
        self.top.unwrap_or(100)
    }
    fn block_median_size(&self) -> u64 {
        100_000
    }
    fn key_image_spent(&self, key_image: &KeyImage) -> bool {
        self.spent.contains(key_image)
    }
    fn ring_members(
        &self,
        amount: u64,
        global_indexes: &[u32],
    ) -> Result<Vec<PublicKey>, RingLookupError> {
        // Like the storage view: ascending, each index once, missing outputs left out.
        Ok(canonical_ring_indexes(global_indexes.to_vec())
            .into_iter()
            .filter_map(|i| self.outputs.get(&(amount, i)).copied())
            .collect())
    }
    fn fork_schedule(&self) -> ForkSchedule {
        self.forks.unwrap_or_else(|| xkr_consensus::MAINNET.forks())
    }
}

/// A one-input, one-output spend of output `(amount, index)` owned by a fresh key.
fn spend(chain: &mut FakeChain, amount: u64, index: u32, fee: u64) -> (Vec<u8>, KeyImage) {
    let mut rng = os_rng();
    let (public, secret) = generate_keys(&mut rng);
    chain.outputs.insert((amount, index), public);
    let key_image = generate_key_image(&public, &secret);
    let (dest, _) = generate_keys(&mut rng);
    let prefix = TransactionPrefix {
        version: 1,
        unlock_time: 0,
        inputs: vec![TxInput::Key {
            amount,
            output_indexes: vec![index],
            key_image,
        }],
        outputs: vec![TxOutput {
            amount: amount - fee,
            target: TxOutputTarget::Key(dest),
        }],
        extra: vec![],
    };
    let prefix_hash = transaction_prefix_hash(&prefix);
    let sigs =
        generate_ring_signature(&prefix_hash, &key_image, &[public], &secret, 0, &mut rng).unwrap();
    let tx = Transaction {
        prefix,
        signatures: vec![sigs],
        clsag_signatures: vec![],
    };
    (encode_transaction(&tx).unwrap(), key_image)
}

#[test]
fn admits_valid_and_rejects_conflicts() {
    let mut chain = FakeChain::default();
    let pool = TxPool::new(PoolConfig::default());
    let (blob, key_image) = spend(&mut chain, 1000, 7, 100);
    let hash = pool
        .add(blob.clone(), &chain, TxSource::Local, 1_000)
        .unwrap();
    assert_eq!(hash, transaction_hash_from_blob(&blob));
    assert!(pool.contains(&hash));
    assert_eq!(pool.len(), 1);
    assert_eq!(
        pool.add(blob.clone(), &chain, TxSource::Peer, 1_001),
        Err(PoolError::AlreadyInPool)
    );
    assert_eq!(pool.txs_not_in(&[]), vec![blob.clone()]);
    assert_eq!(pool.txs_not_in(&[hash]), Vec::<Vec<u8>>::new());

    // An altered copy (one prefix byte flipped) is a different transaction whose ring
    // signature no longer verifies.
    let mut other = blob.clone();
    let last = other.len() - 70;
    other[last] ^= 1;
    assert!(pool.add(other, &chain, TxSource::Peer, 1_002).is_err());

    // Spent in the chain -> rejected.
    let (blob2, ki2) = spend(&mut chain, 2000, 1, 100);
    chain.spent.insert(ki2);
    assert!(matches!(
        pool.add(blob2, &chain, TxSource::Local, 1_003),
        Err(PoolError::Invalid(_))
    ));

    // The block includes our tx: removed.
    pool.remove_for_block(&[hash], &[key_image]);
    assert!(pool.is_empty());
}

#[test]
fn fee_and_ring_rules() {
    let mut chain = FakeChain::default();
    let pool = TxPool::new(PoolConfig::default());
    // Fee below MINIMUM_FEE (10) and not a fusion transaction.
    let (cheap, _) = spend(&mut chain, 1000, 3, 1);
    assert!(matches!(
        pool.add(cheap, &chain, TxSource::Local, 10),
        Err(PoolError::Policy(_))
    ));
    // Unknown ring member.
    let (blob, _) = spend(&mut chain, 5000, 9, 100);
    chain.outputs.clear();
    assert!(matches!(
        pool.add(blob, &chain, TxSource::Local, 10),
        Err(PoolError::Invalid(_))
    ));
}

#[test]
fn recently_seen_and_ttl() {
    let mut chain = FakeChain::default();
    let pool = TxPool::new(PoolConfig::default());
    let (blob, _) = spend(&mut chain, 1000, 1, 1);
    // Rejected once (fee too small), then filtered as recently seen when relayed again.
    assert!(pool.add(blob.clone(), &chain, TxSource::Peer, 100).is_err());
    assert_eq!(
        pool.add(blob, &chain, TxSource::Peer, 101),
        Err(PoolError::RecentlySeen)
    );

    let (good, _) = spend(&mut chain, 3000, 2, 50);
    let hash = pool.add(good.clone(), &chain, TxSource::Peer, 200).unwrap();
    assert_eq!(pool.clean(200 + 60, 100), Vec::<xkr_types::Hash>::new());
    assert_eq!(pool.clean(200 + pool.config().tx_livetime, 100), vec![hash]);
    assert!(pool.is_empty());
    // Evicted transactions are not re-admitted for a while.
    assert_eq!(
        pool.add(
            good,
            &chain,
            TxSource::Local,
            200 + pool.config().tx_livetime + 1
        ),
        Err(PoolError::RecentlyDeleted)
    );
}

#[test]
fn byte_budget_evicts_cheapest() {
    let mut chain = FakeChain::default();
    let (a, _) = spend(&mut chain, 1000, 1, 20);
    let (b, _) = spend(&mut chain, 1000, 2, 500);
    let pool = TxPool::new(PoolConfig {
        max_bytes: a.len() + 10,
        ..PoolConfig::default()
    });
    let ha = pool.add(a, &chain, TxSource::Local, 1).unwrap();
    let hb = pool.add(b, &chain, TxSource::Local, 2).unwrap();
    assert!(!pool.contains(&ha));
    assert!(pool.contains(&hb));
    let (c, _) = spend(&mut chain, 1000, 3, 10);
    assert_eq!(
        pool.add(c, &chain, TxSource::Local, 3),
        Err(PoolError::PoolFull)
    );
}

/// A one-input, one-output version-2 (CLSAG, tagged output) spend of output `(amount, index)`.
fn spend_v2(chain: &mut FakeChain, amount: u64, index: u32, fee: u64) -> (Vec<u8>, KeyImage) {
    let mut rng = os_rng();
    let (public, secret) = generate_keys(&mut rng);
    chain.outputs.insert((amount, index), public);
    let key_image = generate_key_image(&public, &secret);
    let (dest, _) = generate_keys(&mut rng);
    let prefix = TransactionPrefix {
        version: 2,
        unlock_time: 0,
        inputs: vec![TxInput::Key {
            amount,
            output_indexes: vec![index],
            key_image,
        }],
        outputs: vec![TxOutput {
            amount: amount - fee,
            target: TxOutputTarget::TaggedKey {
                key: dest,
                view_tag: 0x42,
            },
        }],
        extra: vec![],
    };
    let sig = clsag_sign(
        &transaction_prefix_hash(&prefix),
        &[public],
        &secret,
        0,
        &mut rng,
    )
    .unwrap();
    let tx = Transaction {
        prefix,
        signatures: vec![],
        clsag_signatures: vec![sig],
    };
    (encode_transaction(&tx).unwrap(), key_image)
}

#[test]
fn version_2_only_after_fork_14_and_version_1_until_the_grace_ends() {
    let params = |fork14: u32| {
        TESTNET
            .with_fork14_height(Some(fork14))
            .with_fork14_params(Fork14Params {
                grace_blocks: 10,
                v2_ring_size: RingSizeRule::MixinLimits,
            })
            .forks()
    };
    // Mainnet table (fork 14 not scheduled): v2 refused, v1 fine.
    let mut chain = FakeChain::default();
    let pool = TxPool::new(PoolConfig::default());
    let (v2, _) = spend_v2(&mut chain, 1000, 1, 100);
    assert_eq!(
        pool.add(v2.clone(), &chain, TxSource::Local, 1),
        Err(PoolError::Invalid(
            xkr_consensus::TxError::WrongTransactionVersion
        ))
    );
    // Next block (top 100 + 1) is after fork 14 at 95: v2 accepted, v1 too (grace).
    chain.forks = Some(params(95));
    let h2 = pool.add(v2, &chain, TxSource::Local, 2).unwrap();
    let (v1, _) = spend(&mut chain, 1000, 2, 100);
    let h1 = pool.add(v1.clone(), &chain, TxSource::Local, 3).unwrap();
    // Templates and eviction follow the table: at 106 the grace (96..=105) is over.
    let late = params(95);
    let allowed = |t: &Transaction| late.allows_tx_version(106, t.prefix.version);
    let sel = pool.fill_block_template_with(106, 100_000, 100_000, 10_000, allowed);
    assert_eq!(sel.txs.iter().map(|e| e.hash).collect::<Vec<_>>(), vec![h2]);
    assert_eq!(pool.remove_disallowed(4, allowed), vec![h1]);
    assert!(pool.contains(&h2));
    // After the grace a new v1 transaction is refused at admission.
    chain.forks = Some(params(80));
    let (v1b, _) = spend(&mut chain, 1000, 3, 100);
    assert_eq!(
        pool.add(v1b, &chain, TxSource::Local, 5),
        Err(PoolError::Invalid(
            xkr_consensus::TxError::WrongTransactionVersion
        ))
    );
    // A v1 and a v2 spend of the same key image conflict in the pool.
    let mut chain2 = FakeChain {
        forks: Some(params(95)),
        ..FakeChain::default()
    };
    let pool2 = TxPool::new(PoolConfig::default());
    let (public, secret) = generate_keys(&mut os_rng());
    chain2.outputs.insert((1000, 9), public);
    let key_image = generate_key_image(&public, &secret);
    let make = |version: u8| {
        let mut rng = os_rng();
        let (dest, _) = generate_keys(&mut rng);
        let target = if version == 2 {
            TxOutputTarget::TaggedKey {
                key: dest,
                view_tag: 1,
            }
        } else {
            TxOutputTarget::Key(dest)
        };
        let prefix = TransactionPrefix {
            version,
            unlock_time: 0,
            inputs: vec![TxInput::Key {
                amount: 1000,
                output_indexes: vec![9],
                key_image,
            }],
            outputs: vec![TxOutput {
                amount: 900,
                target,
            }],
            extra: vec![],
        };
        let msg = transaction_prefix_hash(&prefix);
        let tx = if version == 2 {
            Transaction {
                clsag_signatures: vec![clsag_sign(&msg, &[public], &secret, 0, &mut rng).unwrap()],
                signatures: vec![],
                prefix,
            }
        } else {
            Transaction {
                signatures: vec![
                    generate_ring_signature(&msg, &key_image, &[public], &secret, 0, &mut rng)
                        .unwrap(),
                ],
                clsag_signatures: vec![],
                prefix,
            }
        };
        encode_transaction(&tx).unwrap()
    };
    let first = pool2.add(make(1), &chain2, TxSource::Local, 1).unwrap();
    assert_eq!(
        pool2.add(make(2), &chain2, TxSource::Local, 2),
        Err(PoolError::KeyImageConflict(first))
    );
}

/// A blob above `max_transaction_allowed_size(median)` is refused
/// with the policy error before it is decoded (garbage of that size is `TooBig`, not `Decode`).
#[test]
fn oversized_blob_is_refused_before_decoding() {
    let chain = FakeChain::default(); // median 100 000 -> max 2 * 100 000 - 600
    let pool = TxPool::new(PoolConfig::default());
    let max = 2 * 100_000 - 600;
    let garbage = vec![0xffu8; max + 1];
    assert_eq!(
        pool.add(garbage, &chain, TxSource::Peer, 1),
        Err(PoolError::Policy(
            xkr_consensus::validation::PoolPolicyError::TooBig {
                size: max as u64 + 1,
                max: max as u64,
            }
        ))
    );
    // At the limit the blob is decoded (and fails as garbage).
    assert!(matches!(
        pool.add(vec![0xffu8; max], &chain, TxSource::Peer, 1),
        Err(PoolError::Decode(_))
    ));
}

/// The "recently seen" filter keeps at most `max_seen_entries` hashes, oldest out first.
#[test]
fn seen_filter_is_bounded() {
    let chain = FakeChain::default();
    let pool = TxPool::new(PoolConfig {
        max_seen_entries: 3,
        ..PoolConfig::default()
    });
    let blob = |i: u8| vec![i; 40];
    for i in 0..5 {
        assert!(matches!(
            pool.add(blob(i), &chain, TxSource::Peer, 10),
            Err(PoolError::Decode(_))
        ));
    }
    // 0 and 1 were forgotten: decoded again (and refused again), not filtered.
    assert!(matches!(
        pool.add(blob(0), &chain, TxSource::Peer, 11),
        Err(PoolError::Decode(_))
    ));
    // 4 is still remembered.
    assert_eq!(
        pool.add(blob(4), &chain, TxSource::Peer, 11),
        Err(PoolError::RecentlySeen)
    );
}

/// A local resubmission of a blob that failed decoding gets the cached answer for the
/// recently-seen livetime; chain-dependent rejections (an unknown ring member) are not cached.
#[test]
fn context_free_rejections_are_cached_for_local_submissions() {
    let mut chain = FakeChain::default();
    let pool = TxPool::new(PoolConfig::default());
    let garbage = vec![7u8; 64];
    let first = pool.add(garbage.clone(), &chain, TxSource::Local, 100);
    assert!(matches!(first, Err(PoolError::Decode(_))));
    assert_eq!(
        pool.add(garbage.clone(), &chain, TxSource::Local, 101),
        first
    );
    let ttl = pool.config().recently_seen_livetime;
    pool.clean(100 + ttl, 100);
    assert_eq!(pool.add(garbage, &chain, TxSource::Local, 100 + ttl), first);

    // Unknown ring member: refused, then accepted once the output exists.
    let (blob, _) = spend(&mut chain, 5000, 9, 100);
    let key = chain.outputs.remove(&(5000, 9)).expect("inserted by spend");
    assert!(matches!(
        pool.add(blob.clone(), &chain, TxSource::Local, 200),
        Err(PoolError::Invalid(_))
    ));
    chain.outputs.insert((5000, 9), key);
    assert!(pool.add(blob, &chain, TxSource::Local, 201).is_ok());
}

/// The `NOTIFY_REQUEST_TX_POOL` answer is capped in bytes.
#[test]
fn pool_listing_is_capped_in_bytes() {
    let mut chain = FakeChain::default();
    let pool = TxPool::new(PoolConfig::default());
    let mut sizes = Vec::new();
    for i in 0..4 {
        let (blob, _) = spend(&mut chain, 1000, 10 + i, 100);
        sizes.push(blob.len());
        pool.add(blob, &chain, TxSource::Local, 1).unwrap();
    }
    assert_eq!(pool.txs_not_in_capped(&[], usize::MAX).len(), 4);
    let max = sizes.iter().max().copied().unwrap();
    let two = pool.txs_not_in_capped(&[], 2 * max);
    assert!((1..=2).contains(&two.len()), "{}", two.len());
    assert!(two.iter().map(Vec::len).sum::<usize>() <= 2 * max);
    assert_eq!(pool.txs_not_in_capped(&[], 0), Vec::<Vec<u8>>::new());
}

// ------------------------------------------------------------------ ring order

/// A one-input version-1 spend of `amount` with the packed `offsets`, whose ring signature
/// covers `ring` in the given order (signed with `secret`, the key of `ring[real]`), plus
/// `extra_sigs` copies of the first signature element.
fn ring_spend(
    amount: u64,
    offsets: Vec<u32>,
    ring: &[PublicKey],
    secret: &SecretKey,
    real: usize,
    extra_sigs: usize,
) -> Vec<u8> {
    let mut random = os_rng();
    let key_image = generate_key_image(&ring[real], secret);
    let (dest, _) = generate_keys(&mut random);
    let prefix = TransactionPrefix {
        version: 1,
        unlock_time: 0,
        inputs: vec![TxInput::Key {
            amount,
            output_indexes: offsets,
            key_image,
        }],
        outputs: vec![TxOutput {
            amount: amount - 100,
            target: TxOutputTarget::Key(dest),
        }],
        extra: vec![],
    };
    let mut sigs = generate_ring_signature(
        &transaction_prefix_hash(&prefix),
        &key_image,
        ring,
        secret,
        real,
        &mut random,
    )
    .unwrap();
    for _ in 0..extra_sigs {
        sigs.push(sigs[0]);
    }
    encode_transaction(&Transaction {
        prefix,
        signatures: vec![sigs],
        clsag_signatures: vec![],
    })
    .unwrap()
}

/// The same spend as version 2 (CLSAG over `ring` in the given order, `extra_s` more
/// responses).
fn ring_spend_v2(
    amount: u64,
    offsets: Vec<u32>,
    ring: &[PublicKey],
    secret: &SecretKey,
    real: usize,
    extra_s: usize,
) -> Vec<u8> {
    let mut random = os_rng();
    let key_image = generate_key_image(&ring[real], secret);
    let (dest, _) = generate_keys(&mut random);
    let prefix = TransactionPrefix {
        version: 2,
        unlock_time: 0,
        inputs: vec![TxInput::Key {
            amount,
            output_indexes: offsets,
            key_image,
        }],
        outputs: vec![TxOutput {
            amount: amount - 100,
            target: TxOutputTarget::TaggedKey {
                key: dest,
                view_tag: 7,
            },
        }],
        extra: vec![],
    };
    let msg = transaction_prefix_hash(&prefix);
    let mut sig = clsag_sign(&msg, ring, secret, real, &mut random).unwrap();
    for _ in 0..extra_s {
        sig.s.push(sig.s[0]);
    }
    encode_transaction(&Transaction {
        prefix,
        signatures: vec![],
        clsag_signatures: vec![sig],
    })
    .unwrap()
}

/// Outputs `(amount, index)` for `indexes`, each with a fresh key; returns the keys and the
/// secret of the first one.
fn ring_outputs(
    chain: &mut FakeChain,
    amount: u64,
    indexes: &[u32],
) -> (Vec<PublicKey>, SecretKey) {
    let mut rng = os_rng();
    let mut keys = Vec::new();
    let mut first_secret = None;
    for i in indexes {
        let (public, secret) = generate_keys(&mut rng);
        chain.outputs.insert((amount, *i), public);
        keys.push(public);
        first_secret.get_or_insert(secret);
    }
    (keys, first_secret.unwrap())
}

#[test]
fn ring_indexes_are_sorted_and_unique() {
    let input = |output_indexes: Vec<u32>| TxInput::Key {
        amount: 1,
        output_indexes,
        key_image: KeyImage::ZERO,
    };
    // Plain ascending offsets.
    assert_eq!(ring_global_indexes(&input(vec![3, 1, 5])), vec![3, 4, 9]);
    // A wrapping delta: absolute [9, 5] resolves as [5, 9].
    assert_eq!(
        ring_global_indexes(&input(vec![9, 5u32.wrapping_sub(9)])),
        vec![5, 9]
    );
    // [5, 0xFFFFFFFF, 1] is absolute [5, 4, 5]: one member each.
    assert_eq!(
        ring_global_indexes(&input(vec![5, u32::MAX, 1])),
        vec![4, 5]
    );
    assert_eq!(
        ring_global_indexes(&TxInput::Coinbase { height: 1 }),
        Vec::<u32>::new()
    );
    assert_eq!(canonical_ring_indexes(vec![7, 7, 2, 7]), vec![2, 7]);
}

#[test]
fn wrapped_offsets_are_resolved_in_index_order() {
    let mut chain = FakeChain {
        top: Some(1_800_000),
        ..FakeChain::default()
    };
    // The real output is index 9, the decoy index 5.
    let (keys, secret) = ring_outputs(&mut chain, 1000, &[9, 5]);
    let (k9, k5) = (keys[0], keys[1]);
    let offsets = vec![9, 5u32.wrapping_sub(9)];

    // Signed over [K5, K9] (ascending index): accepted.
    let pool = TxPool::new(PoolConfig::default());
    let sorted = ring_spend(1000, offsets.clone(), &[k5, k9], &secret, 1, 0);
    assert!(pool.add(sorted, &chain, TxSource::Local, 1).is_ok());

    // Signed over [K9, K5] (offset order): the signature does not verify.
    let pool = TxPool::new(PoolConfig::default());
    let wire = ring_spend(1000, offsets, &[k9, k5], &secret, 0, 0);
    assert_eq!(
        pool.add(wire, &chain, TxSource::Local, 1),
        Err(PoolError::Invalid(TxError::InputInvalidSignatures))
    );
}

#[test]
fn repeated_ring_members_count_once() {
    // Offsets [5, 0xFFFFFFFF, 1] = absolute [5, 4, 5]: the ring is [K4, K5], three
    // signatures were sent.
    let offsets = vec![5, u32::MAX, 1];
    let build = |chain: &mut FakeChain| {
        let (keys, secret) = ring_outputs(chain, 1000, &[5, 4]);
        ring_spend(1000, offsets.clone(), &[keys[1], keys[0]], &secret, 1, 1)
    };

    // From TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT on, the counts must match.
    let mut high = FakeChain {
        top: Some(1_800_000),
        ..FakeChain::default()
    };
    let blob = build(&mut high);
    assert_eq!(
        TxPool::new(PoolConfig::default()).add(blob, &high, TxSource::Local, 1),
        Err(PoolError::Invalid(TxError::InputInvalidSignaturesCount))
    );

    // Below it the extra signature is ignored and the two-member ring verifies.
    let mut low = FakeChain::default();
    let blob = build(&mut low);
    assert!(
        TxPool::new(PoolConfig::default())
            .add(blob, &low, TxSource::Local, 1)
            .is_ok()
    );
}

#[test]
fn missing_ring_member_is_left_out() {
    // Absolute [4, 6]; only 4 exists. The ring is [K4] against two signatures.
    let mut chain = FakeChain {
        top: Some(1_800_000),
        ..FakeChain::default()
    };
    let (keys, secret) = ring_outputs(&mut chain, 1000, &[4]);
    let (decoy, _) = generate_keys(&mut os_rng());
    let blob = ring_spend(1000, vec![4, 2], &[keys[0], decoy], &secret, 0, 0);
    assert_eq!(
        TxPool::new(PoolConfig::default()).add(blob, &chain, TxSource::Local, 1),
        Err(PoolError::Invalid(TxError::InputInvalidSignaturesCount))
    );
}

#[test]
fn version_2_rings_use_the_same_order() {
    let forks = TESTNET
        .with_fork14_height(Some(95))
        .with_fork14_params(Fork14Params {
            grace_blocks: 10,
            v2_ring_size: RingSizeRule::MixinLimits,
        })
        .forks();
    let mut chain = FakeChain {
        forks: Some(forks),
        ..FakeChain::default()
    };
    let (keys, secret) = ring_outputs(&mut chain, 1000, &[9, 5]);
    let (k9, k5) = (keys[0], keys[1]);
    let offsets = vec![9, 5u32.wrapping_sub(9)];

    let sorted = ring_spend_v2(1000, offsets.clone(), &[k5, k9], &secret, 1, 0);
    assert!(
        TxPool::new(PoolConfig::default())
            .add(sorted, &chain, TxSource::Local, 1)
            .is_ok()
    );
    let wire = ring_spend_v2(1000, offsets, &[k9, k5], &secret, 0, 0);
    assert_eq!(
        TxPool::new(PoolConfig::default()).add(wire, &chain, TxSource::Local, 1),
        Err(PoolError::Invalid(TxError::InputInvalidSignatures))
    );

    // A repeated index shortens the ring; version 2 always compares the counts.
    let (dup_keys, dup_secret) = ring_outputs(&mut chain, 2000, &[5, 4]);
    let dup = ring_spend_v2(
        2000,
        vec![5, u32::MAX, 1],
        &[dup_keys[1], dup_keys[0]],
        &dup_secret,
        1,
        1,
    );
    assert_eq!(
        TxPool::new(PoolConfig::default()).add(dup, &chain, TxSource::Local, 1),
        Err(PoolError::Invalid(TxError::InputInvalidSignaturesCount))
    );
}

#[test]
fn revalidate_removes_transactions_whose_ring_changed() {
    let mut chain = FakeChain::default();
    let pool = TxPool::new(PoolConfig::default());
    let (kept, kept_image) = spend(&mut chain, 1000, 1, 100);
    let (moved, _) = spend(&mut chain, 1000, 2, 100);
    let kept = pool.add(kept, &chain, TxSource::Local, 1).unwrap();
    let moved = pool.add(moved, &chain, TxSource::Local, 1).unwrap();
    assert_eq!(pool.revalidate(&chain), Vec::<xkr_types::Hash>::new());

    // Index 2 now holds another key (as after a reorg).
    let (other, _) = generate_keys(&mut os_rng());
    chain.outputs.insert((1000, 2), other);
    assert_eq!(pool.revalidate(&chain), vec![moved]);
    assert!(pool.contains(&kept));
    assert!(!pool.contains(&moved));

    // A spent key image goes too.
    chain.spent.insert(kept_image);
    assert_eq!(pool.revalidate(&chain), vec![kept]);
    assert_eq!(pool.len(), 0);
}
