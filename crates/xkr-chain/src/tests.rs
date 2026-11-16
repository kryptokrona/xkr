//! Chain tests: real mainnet blocks 1-10 (fixtures) and synthetic testnet chains (fixed
//! difficulty 1, so blocks can be built without mining) for transactions, alternative blocks
//! and reorgs.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use xkr_consensus::reward::get_block_reward;
use xkr_consensus::{ConsensusError, Network, TxError, check_proof_of_work_with_hash};
use xkr_crypto::{
    clsag_sign, generate_key_derivation, generate_key_image, generate_keys,
    generate_ring_signature, os_rng, view_tag,
};
use xkr_serialization::{
    aux_block_header_hash, block_hash, block_long_hash, decode_block, encode_block,
    encode_transaction, transaction_hash_from_blob, transaction_prefix_hash,
};
use xkr_storage::{
    PruneMode, PruneState, PruningSeed, STRIPE_SPAN, Store, VerifiedBlock, VerifiedTx, ops,
    schema::CHUNK_BLOCKS,
};
use xkr_txpool::{PoolConfig, TxPool, TxSource};
use xkr_types::{
    Block, BlockHeader, Hash, KeyImage, ParentBlock, PublicKey, RawBlock, SecretKey, Signature,
    Transaction, TransactionPrefix, TxInput, TxOutput, TxOutputTarget,
};

use crate::{
    AddBlockOutcome, BatchErrorKind, Chain, ChainConfig, Checkpoints, RejectReason, load_verified,
    unix_now,
};

struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "xkr-chain-test-{}-{}-{nanos}.redb",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn open(&self, network: Network) -> Chain {
        self.open_with(ChainConfig::new(network))
    }

    fn open_with(&self, cfg: ChainConfig) -> Chain {
        let store = Store::open(&self.0).unwrap();
        Chain::open(store, cfg, Arc::new(TxPool::new(PoolConfig::default()))).unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ------------------------------------------------------------------ mainnet fixtures

fn fixtures() -> Vec<Value> {
    serde_json::from_str(include_str!(
        "../../xkr-serialization/tests/fixtures/blocks.json"
    ))
    .unwrap()
}

fn fixture(height: u64) -> Value {
    fixtures()
        .into_iter()
        .find(|f| f["height"].as_u64() == Some(height))
        .unwrap()
}

fn raw_fixture(f: &Value) -> RawBlock {
    RawBlock {
        block: hex::decode(f["blob"].as_str().unwrap()).unwrap(),
        transactions: f["txs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| hex::decode(t["blob"].as_str().unwrap()).unwrap())
            .collect(),
    }
}

fn fixture_hash(f: &Value) -> Hash {
    f["hash"].as_str().unwrap().parse().unwrap()
}

#[test]
fn mainnet_blocks_1_to_10_sync_and_match_the_node() {
    let db = TempDb::new();
    let chain = db.open(Network::Mainnet);
    assert_eq!(chain.tip().height, 0);
    assert_eq!(chain.tip().hash, fixture_hash(&fixture(0)));

    let fs: Vec<Value> = (1..=10).map(fixture).collect();
    let raws: Vec<RawBlock> = fs.iter().map(raw_fixture).collect();
    let ids: Vec<Hash> = fs.iter().map(fixture_hash).collect();
    let report = chain.add_blocks(raws, Some(&ids)).unwrap();
    assert!(report.error.is_none(), "{:?}", report.error);
    assert_eq!(report.added_main, 10);
    assert_eq!(report.tip.height, 10);
    assert_eq!(chain.tip().hash, ids[9]);

    let reader = chain.reader().unwrap();
    for f in &fs {
        let h = f["height"].as_u64().unwrap();
        let info = reader.block_info(h).unwrap().unwrap();
        assert_eq!(Hash(info.hash), fixture_hash(f), "hash at {h}");
        assert_eq!(
            info.difficulty,
            f["difficulty"].as_u64().unwrap(),
            "difficulty at {h}"
        );
        assert_eq!(
            info.already_generated_coins,
            f["already_generated_coins"].as_u64().unwrap(),
            "coins at {h}"
        );
        assert_eq!(
            info.block_size,
            f["cumulative_size"].as_u64().unwrap(),
            "size at {h}"
        );
    }
    // The stored blob is served back byte for byte.
    let raw = chain
        .with_compressor(|c| reader.raw_block(5, c).unwrap().unwrap())
        .unwrap();
    assert_eq!(raw, raw_fixture(&fs[4]));
}

#[test]
fn invalid_block_keeps_the_valid_prefix() {
    let db = TempDb::new();
    let chain = db.open(Network::Mainnet);
    let mut raws: Vec<RawBlock> = (1..=7).map(|h| raw_fixture(&fixture(h))).collect();
    // Block 6 pays its miner one atomic unit too much.
    let mut b6 = decode_block(&raws[5].block).unwrap();
    b6.miner_tx.prefix.outputs[0].amount += 1;
    raws[5].block = encode_block(&b6).unwrap();
    let report = chain.add_blocks(raws, None).unwrap();
    assert_eq!(report.added_main, 5);
    let err = report.error.unwrap();
    assert_eq!(err.index, 5);
    assert_eq!(err.kind, BatchErrorKind::Invalid);
    assert!(
        matches!(
            err.reason,
            RejectReason::Consensus(ConsensusError::Block(
                xkr_consensus::BlockError::BlockRewardMismatch { .. }
            ))
        ),
        "{:?}",
        err.reason
    );
    assert_eq!(chain.tip().height, 5);
}

#[test]
fn known_blocks_are_skipped_orphans_reported_and_restart_keeps_the_chain() {
    let db = TempDb::new();
    {
        let chain = db.open(Network::Mainnet);
        let orphan = chain
            .add_blocks((5..=7).map(|h| raw_fixture(&fixture(h))).collect(), None)
            .unwrap();
        assert_eq!(
            orphan.error.as_ref().map(|e| (e.index, e.kind)),
            Some((0, BatchErrorKind::Orphan))
        );

        chain
            .add_blocks((1..=5).map(|h| raw_fixture(&fixture(h))).collect(), None)
            .unwrap();
        let r = chain
            .add_blocks((3..=7).map(|h| raw_fixture(&fixture(h))).collect(), None)
            .unwrap();
        assert!(r.error.is_none(), "{:?}", r.error);
        assert_eq!((r.skipped, r.added_main), (3, 2));
        chain.flush().unwrap();
    }
    let chain = db.open(Network::Mainnet);
    assert_eq!(chain.tip().height, 7);
    assert_eq!(chain.tip().hash, fixture_hash(&fixture(7)));
    let single = chain.add_block(raw_fixture(&fixture(8))).unwrap();
    assert_eq!(single.outcome, AddBlockOutcome::AddedToMain);
    assert_eq!(
        chain.add_block(raw_fixture(&fixture(8))).unwrap().outcome,
        AddBlockOutcome::AlreadyExists
    );
    assert_eq!(
        chain.add_block(raw_fixture(&fixture(10))).unwrap().outcome,
        AddBlockOutcome::Orphan
    );
}

/// `add_block` checks the raw sizes before decoding, and answers a known block, an orphan or a
/// header that fails against its parent's context from the template alone, before any transaction
/// is decoded or the slow hash is computed.
#[test]
fn live_blocks_get_the_cheap_checks_first() {
    let db = TempDb::new();
    let chain = db.open(Network::Mainnet);
    let r = chain
        .add_blocks((1..=3).map(|h| raw_fixture(&fixture(h))).collect(), None)
        .unwrap();
    assert!(r.error.is_none(), "{:?}", r.error);
    let too_big = |outcome: &AddBlockOutcome| {
        matches!(
            outcome,
            AddBlockOutcome::Invalid(RejectReason::Consensus(ConsensusError::Block(
                xkr_consensus::BlockError::CumulativeBlockSizeTooBig { .. }
            )))
        )
    };
    // Far above any cumulative-size limit: refused as too big, never decoded.
    let huge = RawBlock {
        block: vec![0u8; 2_000_000],
        transactions: Vec::new(),
    };
    let res = chain.add_block(huge).unwrap();
    assert!(too_big(&res.outcome), "{:?}", res.outcome);
    let mut heavy = raw_fixture(&fixture(4));
    heavy.transactions.push(vec![0u8; 2_000_000]);
    let res = chain.add_block(heavy).unwrap();
    assert!(too_big(&res.outcome), "{:?}", res.outcome);

    // A garbage transaction behind a template that fails cheaply: the template decides.
    let with_garbage_tx = |height: u64, edit: &dyn Fn(&mut Block)| {
        let mut raw = raw_fixture(&fixture(height));
        let mut block = decode_block(&raw.block).unwrap();
        edit(&mut block);
        block.tx_hashes.push(Hash([1; 32]));
        raw.block = encode_block(&block).unwrap();
        raw.transactions.push(vec![0xff; 64]);
        raw
    };
    // Known: block 3 itself.
    assert_eq!(
        chain.add_block(raw_fixture(&fixture(3))).unwrap().outcome,
        AddBlockOutcome::AlreadyExists
    );
    // Orphan: block 5's parent (4) is unknown.
    assert_eq!(
        chain
            .add_block(with_garbage_tx(5, &|_: &mut Block| {}))
            .unwrap()
            .outcome,
        AddBlockOutcome::Orphan
    );
    // Extends the tip but is dated a day ahead: the timestamp rule, not the transaction.
    let future = with_garbage_tx(4, &|b: &mut Block| b.header.timestamp = unix_now() + 86_400);
    let res = chain.add_block(future).unwrap();
    assert!(
        matches!(
            res.outcome,
            AddBlockOutcome::Invalid(RejectReason::Consensus(ConsensusError::Block(
                xkr_consensus::BlockError::TimestampTooFarInFuture { .. }
            )))
        ),
        "{:?}",
        res.outcome
    );
    assert_eq!(res.height, Some(4));
    // The real block 4 is still accepted.
    assert_eq!(
        chain.add_block(raw_fixture(&fixture(4))).unwrap().outcome,
        AddBlockOutcome::AddedToMain
    );
}

// ------------------------------------------------------------------ synthetic testnet chains

fn base_reward(coins: u64) -> u64 {
    get_block_reward(5, 0, 100, coins, 0).unwrap().0
}

/// Builds testnet blocks (major 5, 6 after `fork13`, difficulty 1) with a valid merge-mining
/// tag.
struct Builder {
    /// Block id -> (height, already generated coins after it).
    known: HashMap<Hash, (u64, u64)>,
    /// Block id -> amount of its single coinbase output.
    paid: HashMap<Hash, u64>,
    /// Fork-13 height: blocks above it get major version 6.
    fork13: Option<u64>,
    /// Fork-14 height: blocks above it get major version 7 and a version-2 coinbase.
    fork14: Option<u64>,
    /// Major version override (for blocks with the wrong version).
    force_major: Option<u8>,
}

impl Builder {
    fn new(chain: &Chain) -> Self {
        let tip = chain.tip();
        let mut known = HashMap::new();
        known.insert(tip.hash, (0, tip.already_generated_coins));
        Self {
            known,
            paid: HashMap::new(),
            fork13: None,
            fork14: None,
            force_major: None,
        }
    }

    /// A block on `prev` paying `miner` the reward plus the fees of `txs` (`(blob, fee)`).
    fn block(
        &mut self,
        prev: Hash,
        miner: PublicKey,
        txs: &[(Vec<u8>, u64)],
        salt: u64,
    ) -> (RawBlock, Hash) {
        let (prev_height, coins) = self.known[&prev];
        let height = prev_height + 1;
        let base = base_reward(coins);
        let fees: u64 = txs.iter().map(|t| t.1).sum();
        let mut miner_tx = Transaction {
            prefix: TransactionPrefix {
                version: 1,
                unlock_time: height + 1,
                inputs: vec![TxInput::Coinbase {
                    height: height as u32,
                }],
                outputs: vec![TxOutput {
                    amount: base + fees,
                    target: TxOutputTarget::Key(miner),
                }],
                extra: vec![],
            },
            signatures: vec![],
            clsag_signatures: vec![],
        };
        if self.fork14.is_some_and(|f| height > f) {
            miner_tx.prefix.version = 2;
            miner_tx.prefix.outputs[0].target = TxOutputTarget::TaggedKey {
                key: miner,
                view_tag: 0,
            };
        }
        let major_version =
            self.force_major
                .unwrap_or(if self.fork14.is_some_and(|f| height > f) {
                    7
                } else if self.fork13.is_some_and(|f| height > f) {
                    6
                } else {
                    5
                });
        let mut block = Block {
            header: BlockHeader {
                major_version,
                minor_version: 0,
                timestamp: unix_now() - 10_000 + height * 10 + salt,
                prev_hash: prev,
                nonce: salt as u32,
            },
            parent_block: Some(ParentBlock {
                major_version: 1,
                minor_version: 0,
                prev_hash: Hash::ZERO,
                transaction_count: 1,
                base_transaction_branch: vec![],
                base_transaction: Transaction::default(),
                blockchain_branch: vec![],
            }),
            miner_tx,
            tx_hashes: txs
                .iter()
                .map(|t| transaction_hash_from_blob(&t.0))
                .collect(),
        };
        block
            .parent_block
            .as_mut()
            .unwrap()
            .base_transaction
            .prefix
            .version = 1;
        let aux = aux_block_header_hash(&block).unwrap();
        let mut extra = vec![0x03, 33, 0x00];
        extra.extend_from_slice(&aux.0);
        block
            .parent_block
            .as_mut()
            .unwrap()
            .base_transaction
            .prefix
            .extra = extra;
        let hash = block_hash(&block).unwrap();
        self.known.insert(hash, (height, coins + base));
        self.paid.insert(hash, base + fees);
        (
            RawBlock {
                block: encode_block(&block).unwrap(),
                transactions: txs.iter().map(|t| t.0.clone()).collect(),
            },
            hash,
        )
    }

    fn reward_of(&self, block: &Hash) -> u64 {
        self.paid[block]
    }
}

/// A transaction spending output `(amount, index)` owned by `(public, secret)`.
fn spend(public: PublicKey, secret: &SecretKey, amount: u64, index: u32, fee: u64) -> Vec<u8> {
    let mut rng = os_rng();
    let key_image = generate_key_image(&public, secret);
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
    let sigs = generate_ring_signature(
        &transaction_prefix_hash(&prefix),
        &key_image,
        &[public],
        secret,
        0,
        &mut rng,
    )
    .unwrap();
    encode_transaction(&Transaction {
        prefix,
        signatures: vec![sigs],
        clsag_signatures: vec![],
    })
    .unwrap()
}

/// A version-2 (CLSAG, tagged output) transaction spending output `(amount, index)`.
fn spend_v2(public: PublicKey, secret: &SecretKey, amount: u64, index: u32, fee: u64) -> Vec<u8> {
    let mut rng = os_rng();
    let key_image = generate_key_image(&public, secret);
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
                view_tag: 0x5a,
            },
        }],
        extra: vec![],
    };
    let sig = clsag_sign(
        &transaction_prefix_hash(&prefix),
        &[public],
        secret,
        0,
        &mut rng,
    )
    .unwrap();
    encode_transaction(&Transaction {
        prefix,
        signatures: vec![],
        clsag_signatures: vec![sig],
    })
    .unwrap()
}

fn random_key() -> PublicKey {
    generate_keys(&mut os_rng()).0
}

#[test]
fn testnet_transaction_spend_pool_and_double_spend() {
    let db = TempDb::new();
    let chain = db.open(Network::Testnet);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let (owner_pub, owner_sec) = generate_keys(&mut os_rng());

    let (r1, h1) = b.block(genesis, owner_pub, &[], 0);
    let (r2, h2) = b.block(h1, random_key(), &[], 0);
    let rep = chain.add_blocks(vec![r1, r2], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);

    let amount = b.reward_of(&h1);
    let tx = spend(owner_pub, &owner_sec, amount, 0, 1_000);
    let tx_hash = transaction_hash_from_blob(&tx);

    // Into the pool first.
    let added = chain
        .submit_transaction(tx.clone(), TxSource::Local)
        .unwrap()
        .unwrap();
    assert_eq!(added, tx_hash);
    assert!(chain.pool().contains(&tx_hash));

    // Mined in block 3: stored, out of the pool.
    let (r3, h3) = b.block(h2, random_key(), &[(tx.clone(), 1_000)], 0);
    let rep = chain.add_blocks(vec![r3], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    assert!(!chain.pool().contains(&tx_hash));
    let loc = chain
        .reader()
        .unwrap()
        .tx_location(&tx_hash)
        .unwrap()
        .unwrap();
    assert_eq!((loc.height, loc.tx_index), (3, 1));

    // The same spend again is a double spend.
    let (r4, _) = b.block(h3, random_key(), &[(tx.clone(), 1_000)], 0);
    let res = chain.add_block(r4).unwrap();
    assert!(
        matches!(
            res.outcome,
            AddBlockOutcome::Invalid(RejectReason::Consensus(ConsensusError::Transaction {
                error: TxError::InputKeyImageAlreadySpent,
                ..
            }))
        ),
        "{:?}",
        res.outcome
    );
    assert!(
        chain
            .submit_transaction(tx, TxSource::Local)
            .unwrap()
            .is_err()
    );
    assert_eq!(chain.tip().height, 3);
}

#[test]
fn testnet_spend_inside_one_batch_uses_the_overlay() {
    let db = TempDb::new();
    let chain = db.open(Network::Testnet);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let (owner_pub, owner_sec) = generate_keys(&mut os_rng());
    let (r1, h1) = b.block(genesis, owner_pub, &[], 0);
    let tx = spend(owner_pub, &owner_sec, b.reward_of(&h1), 0, 50);
    let (r2, _) = b.block(h1, random_key(), &[(tx, 50)], 0);
    let rep = chain.add_blocks(vec![r1, r2], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    assert_eq!(rep.added_main, 2);
}

#[test]
fn testnet_reorg_to_heavier_branch_and_back_to_the_pool() {
    let db = TempDb::new();
    let chain = db.open(Network::Testnet);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let (owner_pub, owner_sec) = generate_keys(&mut os_rng());

    // Main: G - A1 - A2 - A3(tx)
    let (a1, ha1) = b.block(genesis, owner_pub, &[], 0);
    let (a2, ha2) = b.block(ha1, random_key(), &[], 0);
    let tx = spend(owner_pub, &owner_sec, b.reward_of(&ha1), 0, 500);
    let tx_hash = transaction_hash_from_blob(&tx);
    let (a3, ha3) = b.block(ha2, random_key(), &[(tx, 500)], 0);
    let rep = chain.add_blocks(vec![a1, a2, a3], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    assert_eq!(chain.tip().hash, ha3);

    // Alternative: A1 - B2 - B3 - B4 (B4 makes it heavier).
    let (b2, hb2) = b.block(ha1, random_key(), &[], 7);
    let (b3, hb3) = b.block(hb2, random_key(), &[], 7);
    let (b4, hb4) = b.block(hb3, random_key(), &[], 7);
    assert_eq!(
        chain.add_block(b2).unwrap().outcome,
        AddBlockOutcome::AddedToAlternative
    );
    assert_eq!(
        chain.add_block(b3).unwrap().outcome,
        AddBlockOutcome::AddedToAlternative
    );
    assert!(chain.is_alt_block(&hb2));
    assert_eq!(chain.tip().hash, ha3);
    assert_eq!(
        chain.add_block(b4).unwrap().outcome,
        AddBlockOutcome::AddedAndSwitched
    );

    assert_eq!(chain.tip().hash, hb4);
    assert_eq!(chain.tip().height, 4);
    let reader = chain.reader().unwrap();
    assert_eq!(reader.block_hash(2).unwrap(), Some(hb2));
    assert_eq!(reader.block_hash(1).unwrap(), Some(ha1));
    assert!(reader.tx_location(&tx_hash).unwrap().is_none());
    assert!(chain.is_alt_block(&ha2) && chain.is_alt_block(&ha3));
    assert!(!chain.is_alt_block(&hb4));
    // A1's output is still unspent on the new branch: the transaction is back in the pool.
    assert!(chain.pool().contains(&tx_hash));

    // An orphan stays an orphan.
    let mut lone = Builder::new(&chain);
    lone.known.insert(Hash([9; 32]), (10, 0));
    let (orphan, _) = lone.block(Hash([9; 32]), random_key(), &[], 0);
    assert_eq!(
        chain.add_block(orphan).unwrap().outcome,
        AddBlockOutcome::Orphan
    );
}

/// A heavier alternative branch that fails part way: the old main chain comes back, and with it
/// the pool transaction that a pushed block of the branch had included.
#[test]
fn testnet_failed_switch_keeps_the_pool() {
    let db = TempDb::new();
    let chain = db.open(Network::Testnet);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let (owner_pub, owner_sec) = generate_keys(&mut os_rng());

    // Main: G - A1 - A2 - A3, the spend of A1's output waits in the pool.
    let (a1, ha1) = b.block(genesis, owner_pub, &[], 0);
    let (a2, ha2) = b.block(ha1, random_key(), &[], 0);
    let (a3, ha3) = b.block(ha2, random_key(), &[], 0);
    let rep = chain.add_blocks(vec![a1, a2, a3], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    let reward = b.reward_of(&ha1);
    let tx = spend(owner_pub, &owner_sec, reward, 0, 500);
    let tx_hash = transaction_hash_from_blob(&tx);
    assert_eq!(
        chain
            .submit_transaction(tx.clone(), TxSource::Local)
            .unwrap(),
        Ok(tx_hash)
    );

    // Alternative: A1 - B2 - B3(tx) - B4(another spend of the same output).
    let (b2, hb2) = b.block(ha1, random_key(), &[], 7);
    let (b3, hb3) = b.block(hb2, random_key(), &[(tx, 500)], 7);
    let double = spend(owner_pub, &owner_sec, reward, 0, 600);
    let (b4, _) = b.block(hb3, random_key(), &[(double, 600)], 7);
    for alt in [b2, b3] {
        assert_eq!(
            chain.add_block(alt).unwrap().outcome,
            AddBlockOutcome::AddedToAlternative
        );
    }
    let outcome = chain.add_block(b4).unwrap().outcome;
    assert!(
        matches!(outcome, AddBlockOutcome::Invalid(_)),
        "{outcome:?}"
    );
    assert_eq!(chain.tip().hash, ha3);
    assert_eq!(chain.tip().height, 3);
    assert!(chain.pool().contains(&tx_hash));
}

/// A database pruned completely (`xkrd db prune --all`, here below 64): it reads back with the
/// "recent blocks only" seed, `Chain::prune` does nothing for it (nor for the full database
/// before), and a heavier alternative branch whose switch would pop a pruned block with a
/// transaction is refused (`Switch::Refused`): its blocks stay alternative, the main chain is
/// unchanged.
#[test]
fn testnet_reorg_into_pruned_blocks_is_refused() {
    let db = TempDb::new();
    let chain = db.open(Network::Testnet);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let (owner_pub, owner_sec) = generate_keys(&mut os_rng());

    // Main: G - A1 - A2 - A3(tx) - A4 - ... - A70.
    let (a1, ha1) = b.block(genesis, owner_pub, &[], 0);
    let (a2, ha2) = b.block(ha1, random_key(), &[], 0);
    let tx = spend(owner_pub, &owner_sec, b.reward_of(&ha1), 0, 500);
    let (a3, ha3) = b.block(ha2, random_key(), &[(tx, 500)], 0);
    let mut main = vec![a1, a2, a3];
    let mut tip = ha3;
    for _ in 4..=70 {
        let (r, h) = b.block(tip, random_key(), &[], 0);
        main.push(r);
        tip = h;
    }
    let rep = chain.add_blocks(main, None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    assert_eq!(chain.tip().hash, tip);
    assert!(
        chain.prune().unwrap().is_none(),
        "a full database prunes nothing"
    );

    chain.flush().unwrap();
    drop(chain);
    let store = Store::open(&db.0).unwrap();
    let report = store.prune(PruneMode::All, 64, &mut |_| {}).unwrap();
    assert_eq!(report.state.pruned_below, 64);
    assert_eq!(report.state.seed, PruningSeed::RECENT_ONLY);
    drop(store);
    let chain = db.open(Network::Testnet);
    assert_eq!(
        chain.reader().unwrap().pruning_seed(),
        PruningSeed::RECENT_ONLY
    );
    assert!(
        chain.prune().unwrap().is_none(),
        "no stripe seed: nothing to prune"
    );

    // Alternative from A2: B3 .. B72 becomes heavier at B71, but switching would pop A3.
    let mut prev = ha2;
    for _ in 3..=72 {
        let (r, h) = b.block(prev, random_key(), &[], 9);
        assert_eq!(
            chain.add_block(r).unwrap().outcome,
            AddBlockOutcome::AddedToAlternative,
            "{h}"
        );
        prev = h;
    }
    assert!(chain.is_alt_block(&prev));
    assert_eq!(chain.tip().hash, tip);
    assert_eq!(chain.tip().height, 70);
}

/// Whether [`unchecked_block`] at `height` has a transaction (one block in 512).
fn has_unchecked_tx(height: u64) -> bool {
    height % 512 == 7
}

/// A block for `height` that is never verified: one miner output and, at
/// [`has_unchecked_tx`] heights, one transaction with a 64-byte signature. It is written
/// straight into the store ([`push_unchecked`]), so it only has to be consistent for storage.
fn unchecked_block(height: u64) -> VerifiedBlock {
    let bytes = |tag: u8| {
        let mut b = [tag; 32];
        b[..8].copy_from_slice(&height.to_le_bytes());
        b
    };
    let miner_tx = Transaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time: height + 1,
            inputs: vec![TxInput::Coinbase {
                height: height as u32,
            }],
            outputs: vec![TxOutput {
                amount: 100,
                target: TxOutputTarget::Key(PublicKey(bytes(1))),
            }],
            extra: vec![],
        },
        signatures: vec![],
        clsag_signatures: vec![],
    };
    let mut txs = Vec::new();
    if has_unchecked_tx(height) {
        let tx = Transaction {
            prefix: TransactionPrefix {
                version: 1,
                unlock_time: 0,
                inputs: vec![TxInput::Key {
                    amount: 100,
                    output_indexes: vec![0],
                    key_image: KeyImage(bytes(2)),
                }],
                outputs: vec![TxOutput {
                    amount: 90,
                    target: TxOutputTarget::Key(PublicKey(bytes(3))),
                }],
                extra: vec![],
            },
            signatures: vec![vec![Signature([4; 64])]],
            clsag_signatures: vec![],
        };
        let blob = encode_transaction(&tx).expect("transaction encodes");
        txs.push(VerifiedTx {
            hash: Hash(bytes(5)),
            prefix_len: blob.len() - 64,
            payment_id: None,
            tx,
            blob,
        });
    }
    let block = Block {
        header: BlockHeader {
            major_version: 1,
            minor_version: 0,
            timestamp: 1_600_000_000 + height * 30,
            prev_hash: Hash(bytes(6)),
            nonce: 0,
        },
        parent_block: None,
        miner_tx,
        tx_hashes: txs.iter().map(|t| t.hash).collect(),
    };
    VerifiedBlock {
        height,
        hash: Hash(bytes(7)),
        block_blob: encode_block(&block).expect("block encodes"),
        block,
        miner_tx_hash: Hash(bytes(8)),
        difficulty: 1,
        already_generated_coins: 1_000_000 * (height + 1),
        block_size: 200,
        txs,
    }
}

/// Appends [`unchecked_block`]s `from..to` to `store` without the chain.
fn push_unchecked(store: &Store, from: u64, to: u64) {
    let mut comp = store.compressor().expect("compressor");
    let heights: Vec<u64> = (from..to).collect();
    for batch in heights.chunks(2_048) {
        let blocks: Vec<VerifiedBlock> = batch.iter().map(|h| unchecked_block(*h)).collect();
        let txn = store.begin_write().expect("write transaction");
        ops::push_blocks(&txn, &blocks, &mut comp).expect("push");
        txn.commit().expect("commit");
    }
}

/// `Chain::prune` follows `prune_bound` (`min(last checkpoint, tip - 2 * STRIPE_SPAN)` rounded
/// down to a span) as the tip grows: nothing below three spans or without a checkpoint above
/// genesis, the next span once the tip passes the next boundary, never past the last
/// checkpoint, nothing for an unchanged bound; the newly covered span is kept or dropped by the
/// stripe rule. No test hook shrinks `STRIPE_SPAN` and 16 k verified testnet blocks (a slow
/// hash each) would dominate the suite, so the chain grows with unchecked blocks written
/// straight into the store between reopenings: the tip, the stored prune state and the
/// checkpoints are everything `Chain::prune` reads.
#[test]
fn chain_prune_follows_the_bound_as_the_tip_grows() {
    let s = STRIPE_SPAN;
    let db = TempDb::new();
    let genesis = db.open(Network::Testnet).genesis_hash();
    // Stripe 1 of 2: span 0 is kept, span 1 is dropped once it is below the prune height.
    let seed = PruningSeed::new(2, 1).expect("valid seed");
    Store::open(&db.0)
        .expect("store")
        .set_pruning_seed(seed)
        .expect("seed stored");
    let state = |pruned_below| PruneState { seed, pruned_below };
    // Grows the chain to `tip` and opens it with the last checkpoint at `checkpoint` (`None`:
    // testnet's own list, genesis only).
    let open_at = |tip: u64, checkpoint: Option<u64>| {
        let store = Store::open(&db.0).expect("store");
        let top = store
            .reader()
            .expect("reader")
            .top_height()
            .expect("top")
            .expect("genesis");
        push_unchecked(&store, top + 1, tip + 1);
        let mut cfg = ChainConfig::new(Network::Testnet);
        if let Some(last) = checkpoint {
            cfg.checkpoints = Checkpoints::from_list([(0, genesis), (last, Hash::ZERO)]);
        }
        let pool = Arc::new(TxPool::new(PoolConfig::default()));
        let chain = Chain::open(store, cfg, pool).expect("chain");
        assert_eq!(chain.tip().height, tip);
        chain
    };
    let checkpoint = Some(2 * s + 100);

    // Tip 3s - 1: tip - 2s is still below the first span boundary.
    let chain = open_at(3 * s - 1, checkpoint);
    assert_eq!(chain.prune_bound(), 0);
    assert!(chain.prune().expect("prune").is_none());
    drop(chain);

    // Tip 3s: without a checkpoint above genesis (testnet's list) nothing is pruned ...
    let chain = open_at(3 * s, None);
    assert_eq!(chain.prune_bound(), 0);
    assert!(chain.prune().expect("prune").is_none());
    drop(chain);
    // ... with one the bound is one span. Span 0 is the kept stripe: nothing is deleted.
    let chain = open_at(3 * s, checkpoint);
    assert_eq!(chain.prune_bound(), s);
    let report = chain.prune().expect("prune").expect("the bound moved");
    assert_eq!(report.state, state(s));
    assert_eq!(
        (report.chunks, report.hot_rows, report.transactions),
        (0, 0, 0)
    );
    assert!(
        chain.prune().expect("prune").is_none(),
        "an unchanged bound prunes nothing"
    );
    {
        let r = chain.reader().expect("reader");
        assert_eq!(r.prune_state(), state(s));
        assert!(r.has_full_block(7) && r.has_full_block(s + 7));
    }
    drop(chain);

    // Tip 4s + 5 with the last checkpoint at s + 50: the checkpoint caps the bound at s.
    let chain = open_at(4 * s + 5, Some(s + 50));
    assert_eq!(chain.prune_bound(), s);
    assert!(chain.prune().expect("prune").is_none());
    drop(chain);

    // The same tip with the checkpoint at 2s + 100: tip - 2s = 2s + 5 puts the bound at 2s and
    // span 1 (sealed: the default seal lag is 1 024) loses its signatures.
    let chain = open_at(4 * s + 5, checkpoint);
    assert_eq!(chain.prune_bound(), 2 * s);
    let report = chain.prune().expect("prune").expect("the bound moved");
    let dropped = (s..2 * s).filter(|h| has_unchecked_tx(*h)).count() as u64;
    assert_eq!(dropped, 8);
    assert_eq!(report.state, state(2 * s));
    assert_eq!(
        (report.chunks, report.hot_rows, report.transactions),
        (dropped, 0, dropped)
    );
    assert!(chain.prune().expect("prune").is_none());
    {
        let r = chain.reader().expect("reader");
        assert_eq!(r.prune_state(), state(2 * s));
        for (h, whole) in [
            (7, true),
            (s + 7, false),
            (2 * s + 7, true),
            (3 * s + 7, true),
        ] {
            assert_eq!(r.has_full_block(h), whole, "block {h}");
            assert_eq!(
                r.has_signature_parts(h / CHUNK_BLOCKS).expect("parts"),
                whole,
                "signature parts of block {h}"
            );
            let loaded = chain
                .with_compressor(|c| load_verified(&r, h, c))
                .expect("compressor");
            if whole {
                assert_eq!(loaded.expect("whole block").txs.len(), 1, "block {h}");
            } else {
                assert!(
                    matches!(loaded, Err(crate::ChainError::Pruned(p)) if p == h),
                    "block {h}"
                );
                // The block blob (header, miner transaction, hashes) is kept.
                let blob = chain
                    .with_compressor(|c| r.block_blob(h, c))
                    .expect("compressor")
                    .expect("read")
                    .expect("block");
                assert_eq!(blob, unchecked_block(h).block_blob);
            }
        }
        // Blocks without transactions in the dropped span are not whole either.
        assert!(!r.has_full_block(s + 8) && r.has_full_block(s - 1));
    }
}

// ------------------------------------------------------------------ block templates

#[test]
fn testnet_block_templates_are_mined_and_accepted() {
    use crate::{MinerAddress, set_merge_mining_tag};

    let db = TempDb::new();
    let chain = db.open(Network::Testnet);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let (owner_pub, owner_sec) = generate_keys(&mut os_rng());
    let (r1, h1) = b.block(genesis, owner_pub, &[], 0);
    let (r2, _) = b.block(h1, random_key(), &[], 0);
    let rep = chain.add_blocks(vec![r1, r2], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    let tx = spend(owner_pub, &owner_sec, b.reward_of(&h1), 0, 1_000);
    let tx_hash = transaction_hash_from_blob(&tx);
    chain
        .submit_transaction(tx, TxSource::Local)
        .unwrap()
        .unwrap();

    let miner = MinerAddress {
        spend_public: random_key(),
        view_public: random_key(),
    };
    for round in 0..3u32 {
        let t = chain.block_template(&miner, &[0u8; 8]).unwrap();
        assert_eq!(t.height, chain.tip().height + 1);
        assert_eq!(t.prev_hash, chain.tip().hash);
        assert_eq!(t.difficulty, 1);
        assert!(t.reserved_offset > 0);
        // The C++ daemon adds 3 to the position of the tx public key (it counts the pubkey tag
        // that precedes the key), so the reported offset is one byte *into* the nonce. Pools
        // were written against that quirk, so we reproduce it: the eight zero bytes start one
        // byte before `reserved_offset`.
        assert_eq!(
            t.blob[t.reserved_offset - 1..t.reserved_offset + 7],
            [0u8; 8]
        );
        assert_eq!(
            t.blob[t.reserved_offset - 3..t.reserved_offset - 1],
            [0x02, 8]
        );
        assert_eq!(decode_block(&t.blob).unwrap(), t.block);
        if round == 0 {
            assert_eq!(t.block.tx_hashes, vec![tx_hash]);
        }
        let mut block = t.block.clone();
        set_merge_mining_tag(&mut block).unwrap();
        block.header.nonce = 12_345 + round;
        let raw = RawBlock {
            block: encode_block(&block).unwrap(),
            transactions: t.transactions.clone(),
        };
        let r = chain.add_block(raw).unwrap();
        assert_eq!(
            r.outcome,
            AddBlockOutcome::AddedToMain,
            "round {round}: {:?}",
            r.outcome
        );
        assert_eq!(chain.tip().hash, block_hash(&block).unwrap());
    }
    assert!(!chain.pool().contains(&tx_hash));
    assert_eq!(
        chain
            .reader()
            .unwrap()
            .tx_location(&tx_hash)
            .unwrap()
            .unwrap()
            .height,
        3
    );

    // The submitblock path: transactions come from the pool.
    let t = chain.block_template(&miner, &[]).unwrap();
    assert_eq!(t.reserved_offset, 0);
    let mut block = t.block;
    set_merge_mining_tag(&mut block).unwrap();
    let raw = chain
        .assemble_mined_block(encode_block(&block).unwrap())
        .unwrap();
    assert_eq!(
        chain.add_block(raw).unwrap().outcome,
        AddBlockOutcome::AddedToMain
    );

    // Decoys for the miner outputs: the chain is long enough (testnet unlock window 1).
    let outs = chain.random_outputs(b.reward_of(&h1), 3).unwrap().unwrap();
    assert_eq!(outs.len(), 1);
    assert_eq!(outs[0].1, owner_pub);
}

// ------------------------------------------------------------------ fork 13

#[test]
fn testnet_fork13_switches_the_block_version() {
    use crate::MinerAddress;
    use xkr_consensus::BlockError;

    let db = TempDb::new();
    let mut cfg = ChainConfig::new(Network::Testnet);
    cfg.fork13_height = Some(3);
    let chain = db.open_with(cfg);
    assert_eq!(chain.params().fork13_height, Some(3));
    let mut b = Builder::new(&chain);
    b.fork13 = Some(3);
    let genesis = chain.tip().hash;
    let (r1, h1) = b.block(genesis, random_key(), &[], 0);
    let (r2, h2) = b.block(h1, random_key(), &[], 0);
    let rep = chain.add_blocks(vec![r1, r2], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);

    let wrong = |expected: u8, got: u8| {
        AddBlockOutcome::Invalid(RejectReason::Consensus(ConsensusError::Block(
            BlockError::WrongVersion { expected, got },
        )))
    };

    // Height 3 = F is the last v5 block: v6 is refused there.
    b.force_major = Some(6);
    let (early, _) = b.block(h2, random_key(), &[], 1);
    let r = chain.add_block(early).unwrap();
    assert_eq!(r.outcome, wrong(5, 6));
    if let AddBlockOutcome::Invalid(reason) = &r.outcome {
        assert_eq!(reason.older_block_version(), None);
    }
    b.force_major = None;
    let (r3, h3) = b.block(h2, random_key(), &[], 0);
    assert_eq!(
        chain.add_block(r3).unwrap().outcome,
        AddBlockOutcome::AddedToMain
    );

    // Height 4 = F + 1: v5 is refused (an old node's block), v6 is accepted.
    b.force_major = Some(5);
    let (old, _) = b.block(h3, random_key(), &[], 2);
    let r = chain.add_block(old).unwrap();
    assert_eq!(r.outcome, wrong(6, 5));
    if let AddBlockOutcome::Invalid(reason) = &r.outcome {
        assert_eq!(reason.older_block_version(), Some((6, 5)));
    }
    b.force_major = None;
    let (r4, h4) = b.block(h3, random_key(), &[], 0);
    assert_eq!(
        chain.add_block(r4).unwrap().outcome,
        AddBlockOutcome::AddedToMain
    );
    assert_eq!(chain.tip().hash, h4);

    // Templates follow the table and signal the supported fork index.
    let miner = MinerAddress {
        spend_public: random_key(),
        view_public: random_key(),
    };
    let t = chain.block_template(&miner, &[]).unwrap();
    assert_eq!(t.block.header.major_version, 6);
    assert_eq!(
        t.block.header.minor_version,
        xkr_consensus::SUPPORTED_FORK_INDEX
    );

    let status = chain.fork_status().unwrap();
    assert_eq!(status.active_index, Some(13));
    assert_eq!(status.next_block_major_version, 6);
    assert_eq!(status.next_index, None);
    assert_eq!(status.readiness, None);
    let rate = chain.block_rate(unix_now()).unwrap();
    assert_eq!(rate.status, crate::BlockRateStatus::FixedDifficulty);
}

#[test]
fn testnet_fork13_readiness_counts_signalling_blocks() {
    let db = TempDb::new();
    let mut cfg = ChainConfig::new(Network::Testnet);
    cfg.fork13_height = Some(100);
    let chain = db.open_with(cfg);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let (r1, h1) = b.block(genesis, random_key(), &[], 0);
    let (r2, _) = b.block(h1, random_key(), &[], 0);
    chain.add_blocks(vec![r1, r2], None).unwrap();
    let status = chain.fork_status().unwrap();
    assert_eq!(status.next_index, Some(13));
    assert_eq!(status.next_height, Some(100));
    assert_eq!(status.signal_index, Some(13));
    assert_eq!(status.window, 2);
    // The builder writes minor 0: nothing signals.
    assert_eq!(status.signalling, 0);
    assert!(status.readiness.is_some_and(|r| r.abs() < f64::EPSILON));
}

// ------------------------------------------------------------------ fork 14 (tx version 2)

#[test]
#[allow(clippy::similar_names)] // v1_spend_c / v2_spend_c name the two versions on purpose
fn testnet_fork14_transaction_version_2() {
    use crate::{MinerAddress, set_merge_mining_tag};
    use xkr_consensus::{BlockError, Fork14Params, RingSizeRule};

    let db = TempDb::new();
    let mut cfg = ChainConfig::new(Network::Testnet);
    cfg.fork13_height = Some(2);
    cfg.fork14_height = Some(4);
    cfg.fork14 = Fork14Params {
        grace_blocks: 3,
        v2_ring_size: RingSizeRule::MixinLimits,
    };
    let chain = db.open_with(cfg);
    assert_eq!(chain.params().fork14_height, Some(4));
    let mut b = Builder::new(&chain);
    b.fork13 = Some(2);
    b.fork14 = Some(4);
    let genesis = chain.tip().hash;
    let owners: Vec<(PublicKey, SecretKey)> =
        (0..4).map(|_| generate_keys(&mut os_rng())).collect();

    let invalid = |error: TxError| {
        AddBlockOutcome::Invalid(RejectReason::Consensus(ConsensusError::Transaction {
            tx_index: Some(0),
            error,
        }))
    };

    // Heights 1-3 pay owners 0-2 (version-1 coinbases, v5 then v6 blocks).
    let (r1, h1) = b.block(genesis, owners[0].0, &[], 0);
    let (r2, h2) = b.block(h1, owners[1].0, &[], 0);
    let (r3, h3) = b.block(h2, owners[2].0, &[], 0);
    let rep = chain.add_blocks(vec![r1, r2, r3], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    let (a0, a1, a2) = (b.reward_of(&h1), b.reward_of(&h2), b.reward_of(&h3));

    // Height 4 = F: version 2 is not allowed yet, neither in a block nor in the pool.
    let early_v2 = spend_v2(owners[0].0, &owners[0].1, a0, 0, 100);
    let (bad, _) = b.block(h3, random_key(), &[(early_v2.clone(), 100)], 1);
    assert_eq!(
        chain.add_block(bad).unwrap().outcome,
        invalid(TxError::WrongTransactionVersion)
    );
    assert!(
        chain
            .submit_transaction(early_v2, TxSource::Local)
            .unwrap()
            .is_err()
    );
    // Owner 0 spends with version 1 at height 4 (major 6, v1 coinbase).
    let v1_spend_a = spend(owners[0].0, &owners[0].1, a0, 0, 100);
    let (r4, h4) = b.block(h3, random_key(), &[(v1_spend_a, 100)], 0);
    assert_eq!(
        chain.add_block(r4).unwrap().outcome,
        AddBlockOutcome::AddedToMain
    );

    // Height 5 = F + 1: block v7 with a version-2 coinbase. A v6 block or a v1 coinbase fails.
    b.force_major = Some(6);
    let (old, _) = b.block(h4, random_key(), &[], 2);
    b.force_major = None;
    assert_eq!(
        chain.add_block(old).unwrap().outcome,
        AddBlockOutcome::Invalid(RejectReason::Consensus(ConsensusError::Block(
            BlockError::WrongVersion {
                expected: 7,
                got: 6
            }
        )))
    );
    b.fork14 = None;
    b.force_major = Some(7);
    let (v1_coinbase, _) = b.block(h4, random_key(), &[], 3);
    b.force_major = None;
    b.fork14 = Some(4);
    assert_eq!(
        chain.add_block(v1_coinbase).unwrap().outcome,
        AddBlockOutcome::Invalid(RejectReason::Consensus(ConsensusError::Transaction {
            tx_index: None,
            error: TxError::WrongTransactionVersion,
        }))
    );
    // A version-2 spend of owner 0's output (already spent by version 1) is a double spend.
    let v2_spend_a = spend_v2(owners[0].0, &owners[0].1, a0, 0, 100);
    let (dup, _) = b.block(h4, random_key(), &[(v2_spend_a, 100)], 4);
    assert_eq!(
        chain.add_block(dup).unwrap().outcome,
        invalid(TxError::InputKeyImageAlreadySpent)
    );
    // Owner 1 spends with version 2 (through the pool), accepted at height 5.
    let v2_spend_b = spend_v2(owners[1].0, &owners[1].1, a1, 0, 100);
    let v2_hash = transaction_hash_from_blob(&v2_spend_b);
    chain
        .submit_transaction(v2_spend_b.clone(), TxSource::Local)
        .unwrap()
        .unwrap();
    let (r5, h5) = b.block(h4, random_key(), &[(v2_spend_b, 100)], 0);
    assert_eq!(
        chain.add_block(r5).unwrap().outcome,
        AddBlockOutcome::AddedToMain
    );
    assert!(!chain.pool().contains(&v2_hash));

    // Height 6 (grace): a version-1 spend of owner 1's output is a double spend across
    // versions; a version-1 spend of owner 2's output is still fine.
    let v1_spend_b = spend(owners[1].0, &owners[1].1, a1, 0, 100);
    let (dup, _) = b.block(h5, random_key(), &[(v1_spend_b, 100)], 5);
    assert_eq!(
        chain.add_block(dup).unwrap().outcome,
        invalid(TxError::InputKeyImageAlreadySpent)
    );
    let (r6, h6) = b.block(h5, random_key(), &[], 0);
    let (r7, h7) = b.block(h6, random_key(), &[], 0);
    let rep = chain.add_blocks(vec![r6, r7], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);

    // Height 8 = F + 4: the grace (5..=7) is over, version 1 is refused everywhere.
    let v1_spend_c = spend(owners[2].0, &owners[2].1, a2, 0, 100);
    assert!(matches!(
        chain
            .submit_transaction(v1_spend_c.clone(), TxSource::Local)
            .unwrap(),
        Err(xkr_txpool::PoolError::Invalid(
            TxError::WrongTransactionVersion
        ))
    ));
    let (late, _) = b.block(h7, random_key(), &[(v1_spend_c, 100)], 6);
    assert_eq!(
        chain.add_block(late).unwrap().outcome,
        invalid(TxError::WrongTransactionVersion)
    );
    let v2_spend_c = spend_v2(owners[2].0, &owners[2].1, a2, 0, 100);
    let v2c_hash = transaction_hash_from_blob(&v2_spend_c);
    chain
        .submit_transaction(v2_spend_c, TxSource::Local)
        .unwrap()
        .unwrap();

    // The template is v7 with a version-2 coinbase whose view tags match the miner's
    // derivation, carries the pool's version-2 transaction, and is accepted.
    let (view_public, view_secret) = generate_keys(&mut os_rng());
    let miner = MinerAddress {
        spend_public: random_key(),
        view_public,
    };
    let t = chain.block_template(&miner, &[0u8; 8]).unwrap();
    assert_eq!(t.height, 8);
    assert_eq!(t.block.header.major_version, 7);
    assert_eq!(t.block.header.minor_version, xkr_consensus::forks::FORK_14);
    assert_eq!(t.block.miner_tx.prefix.version, 2);
    assert_eq!(t.block.tx_hashes, vec![v2c_hash]);
    let tx_pub = xkr_types::tx_public_key(&t.block.miner_tx.prefix.extra).unwrap();
    let derivation = generate_key_derivation(&tx_pub, &view_secret).unwrap();
    for (i, o) in t.block.miner_tx.prefix.outputs.iter().enumerate() {
        assert_eq!(o.view_tag(), Some(view_tag(&derivation, i as u64)));
    }
    assert_eq!(decode_block(&t.blob).unwrap(), t.block);
    let mut block = t.block.clone();
    set_merge_mining_tag(&mut block).unwrap();
    let raw = RawBlock {
        block: encode_block(&block).unwrap(),
        transactions: t.transactions.clone(),
    };
    assert_eq!(
        chain.add_block(raw).unwrap().outcome,
        AddBlockOutcome::AddedToMain
    );
    assert_eq!(chain.tip().height, 8);
    assert!(!chain.pool().contains(&v2c_hash));
    let status = chain.fork_status().unwrap();
    assert_eq!(status.active_index, Some(14));
    assert_eq!(status.next_block_major_version, 7);
    assert_eq!(status.supported_index, 14);
    assert_eq!(status.next_index, None);
    assert_eq!(chain.params().forks().supported_height(), 4);
}

// ------------------------------------------------------------------ ring order

/// A version-1 transaction with one input of `amount` and the packed `offsets`, its ring
/// signature over `ring` in the given order (signed with `secret`, the key of `ring[real]`)
/// plus `extra_sigs` copies of the first element, paying `outputs`.
fn ring_tx(
    amount: u64,
    offsets: Vec<u32>,
    ring: &[PublicKey],
    (secret, real): (&SecretKey, usize),
    extra_sigs: usize,
    outputs: &[(u64, PublicKey)],
) -> Vec<u8> {
    let mut random = os_rng();
    let key_image = generate_key_image(&ring[real], secret);
    let prefix = TransactionPrefix {
        version: 1,
        unlock_time: 0,
        inputs: vec![TxInput::Key {
            amount,
            output_indexes: offsets,
            key_image,
        }],
        outputs: outputs
            .iter()
            .map(|(amount, key)| TxOutput {
                amount: *amount,
                target: TxOutputTarget::Key(*key),
            })
            .collect(),
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

/// Two outputs of one amount: block 1 pays `owner`, block 3 carries a transaction that splits
/// that reward into two equal outputs (global indexes 0 and 1 of their amount) owned by the
/// returned keys. Returns the blocks, the ids, the split amount and the two key pairs.
#[allow(clippy::type_complexity)]
fn two_outputs_of_one_amount(
    b: &mut Builder,
    genesis: Hash,
) -> (Vec<RawBlock>, Vec<Hash>, u64, [(PublicKey, SecretKey); 2]) {
    let (owner_pub, owner_sec) = generate_keys(&mut os_rng());
    let pair = [generate_keys(&mut os_rng()), generate_keys(&mut os_rng())];
    let (r1, h1) = b.block(genesis, owner_pub, &[], 0);
    let (r2, h2) = b.block(h1, random_key(), &[], 0);
    let reward = b.reward_of(&h1);
    let half = (reward - 1_000) / 2;
    let fee = reward - 2 * half;
    let split = ring_tx(
        reward,
        vec![0],
        &[owner_pub],
        (&owner_sec, 0),
        0,
        &[(half, pair[0].0), (half, pair[1].0)],
    );
    let (r3, h3) = b.block(h2, random_key(), &[(split, fee)], 0);
    (vec![r1, r2, r3], vec![h1, h2, h3], half, pair)
}

/// Offsets `[1, 0xFFFFFFFF]` are absolute `[1, 0]`: the ring is resolved in index order
/// `[K0, K1]`. A signature over that order is valid in a block and in the pool, one over the
/// offset order `[K1, K0]` is not.
#[test]
fn wrapped_offsets_are_resolved_in_index_order() {
    let db = TempDb::new();
    let chain = db.open(Network::Testnet);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let (raws, ids, amount, [(k0, _), (k1, s1)]) = two_outputs_of_one_amount(&mut b, genesis);
    let rep = chain.add_blocks(raws, None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    let pay = [(amount - 500, random_key())];
    let offsets = vec![1, u32::MAX];

    let offset_order = ring_tx(amount, offsets.clone(), &[k1, k0], (&s1, 0), 0, &pay);
    let (bad, _) = b.block(ids[2], random_key(), &[(offset_order.clone(), 500)], 1);
    assert_eq!(
        chain.add_block(bad).unwrap().outcome,
        AddBlockOutcome::Invalid(RejectReason::Consensus(ConsensusError::Transaction {
            tx_index: Some(0),
            error: TxError::InputInvalidSignatures,
        }))
    );
    assert_eq!(
        chain
            .submit_transaction(offset_order, TxSource::Local)
            .unwrap(),
        Err(xkr_txpool::PoolError::Invalid(
            TxError::InputInvalidSignatures
        ))
    );

    let index_order = ring_tx(amount, offsets, &[k0, k1], (&s1, 1), 0, &pay);
    let hash = chain
        .submit_transaction(index_order.clone(), TxSource::Local)
        .unwrap()
        .unwrap();
    let (good, _) = b.block(ids[2], random_key(), &[(index_order, 500)], 0);
    assert_eq!(
        chain.add_block(good).unwrap().outcome,
        AddBlockOutcome::AddedToMain
    );
    assert!(!chain.pool().contains(&hash));
}

/// The same rule for ring members created earlier in the same batch, and a repeated member:
/// offsets `[1, 0xFFFFFFFF, 1]` (absolute `[1, 0, 1]`) are the ring `[K0, K1]`; below
/// `TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT` the third signature is ignored.
#[test]
fn ring_members_from_the_batch_are_resolved_in_index_order() {
    let db = TempDb::new();
    let chain = db.open(Network::Testnet);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let (mut raws, ids, amount, [(k0, _), (k1, s1)]) = two_outputs_of_one_amount(&mut b, genesis);
    let pay = [(amount - 500, random_key())];
    let repeated = ring_tx(amount, vec![1, u32::MAX, 1], &[k0, k1], (&s1, 1), 1, &pay);
    let (r4, h4) = b.block(ids[2], random_key(), &[(repeated, 500)], 0);
    raws.push(r4);
    let rep = chain.add_blocks(raws, None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    assert_eq!(rep.added_main, 4);
    assert_eq!(chain.tip().hash, h4);
}

/// `docs/test-vectors/ring-order.json`: absolute indexes and ring order of packed offsets.
#[test]
fn ring_order_vectors() {
    let doc: Value =
        serde_json::from_str(include_str!("../../../docs/test-vectors/ring-order.json")).unwrap();
    let list = |v: &Value| -> Vec<u32> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|x| u32::try_from(x.as_u64().unwrap()).unwrap())
            .collect()
    };
    let cases = doc["cases"].as_array().unwrap();
    assert!(cases.len() >= 10, "{}", cases.len());
    for c in cases {
        let input = TxInput::Key {
            amount: 1,
            output_indexes: list(&c["offsets"]),
            key_image: KeyImage::ZERO,
        };
        assert_eq!(input.absolute_output_indexes(), list(&c["absolute"]), "{c}");
        assert_eq!(
            xkr_txpool::ring_global_indexes(&input),
            list(&c["ring"]),
            "{c}"
        );
    }
}

// ------------------------------------------------------------------ checkpoints and reorgs

/// Testnet checkpoints with one far above the test range.
fn checkpoints_with(genesis: Hash, extra: (u64, Hash)) -> Checkpoints {
    Checkpoints::from_list([(0, genesis), extra])
}

/// A block on the tip below the next checkpoint does not keep the node from switching to a
/// heavier branch that replaces it.
#[test]
fn branch_below_the_next_checkpoint_replaces_the_tip() {
    let db = TempDb::new();
    let genesis = db.open(Network::Testnet).genesis_hash();
    let mut cfg = ChainConfig::new(Network::Testnet);
    cfg.checkpoints = checkpoints_with(genesis, (1_000, Hash([7; 32])));
    let chain = db.open_with(cfg);
    let mut b = Builder::new(&chain);

    let (other, _) = b.block(genesis, random_key(), &[], 99);
    assert_eq!(
        chain.add_block(other).unwrap().outcome,
        AddBlockOutcome::AddedToMain
    );
    let (r1, h1) = b.block(genesis, random_key(), &[], 0);
    let (r2, h2) = b.block(h1, random_key(), &[], 0);
    let rep = chain.add_blocks(vec![r1, r2], Some(&[h1, h2])).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    assert!(rep.switched);
    assert_eq!(chain.tip().hash, h2);
    assert_eq!(chain.reader().unwrap().block_hash(1).unwrap(), Some(h1));
}

/// Once the main chain holds a checkpoint, alternative blocks at or below it are refused;
/// above it they are accepted.
#[test]
fn alternative_blocks_below_a_reached_checkpoint_are_refused() {
    let scratch = TempDb::new();
    let genesis = scratch.open(Network::Testnet).genesis_hash();
    let mut b = Builder::new(&scratch.open(Network::Testnet));
    let (r1, h1) = b.block(genesis, random_key(), &[], 0);
    let (r2, h2) = b.block(h1, random_key(), &[], 0);
    let (r3, h3) = b.block(h2, random_key(), &[], 0);
    let (low, _) = b.block(genesis, random_key(), &[], 5);
    let (high, _) = b.block(h2, random_key(), &[], 5);

    let db = TempDb::new();
    let mut cfg = ChainConfig::new(Network::Testnet);
    cfg.checkpoints = checkpoints_with(genesis, (2, h2));
    let chain = db.open_with(cfg);
    let rep = chain.add_blocks(vec![r1, r2, r3], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    assert_eq!(chain.tip().hash, h3);
    assert_eq!(
        chain.add_block(low).unwrap().outcome,
        AddBlockOutcome::Invalid(RejectReason::BelowCheckpoint {
            height: 1,
            checkpoint: 2
        })
    );
    assert_eq!(
        chain.add_block(high).unwrap().outcome,
        AddBlockOutcome::AddedToAlternative
    );
}

/// The blocks of a requested batch are compared with the requested ids before their
/// transactions are decoded: a different block is reported as unexpected whatever its
/// transactions hold, and the blocks before it are added.
#[test]
fn requested_batches_compare_ids_before_transactions() {
    let db = TempDb::new();
    let chain = db.open(Network::Mainnet);
    let fs: Vec<Value> = (1..=5).map(fixture).collect();
    let mut raws: Vec<RawBlock> = fs.iter().map(raw_fixture).collect();
    let ids: Vec<Hash> = fs.iter().map(fixture_hash).collect();
    // Block 3 also lists an undecodable transaction (so its id changes).
    let mut b3 = decode_block(&raws[2].block).unwrap();
    b3.tx_hashes.push(Hash([1; 32]));
    raws[2].block = encode_block(&b3).unwrap();
    raws[2].transactions.push(vec![0xff; 64]);
    let got = block_hash(&b3).unwrap();

    let rep = chain.add_blocks(raws.clone(), Some(&ids)).unwrap();
    assert_eq!(rep.added_main, 2);
    let err = rep.error.unwrap();
    assert_eq!(err.index, 2);
    assert_eq!(err.kind, BatchErrorKind::Invalid);
    assert_eq!(err.hash, Some(got));
    assert_eq!(
        err.reason,
        RejectReason::UnexpectedBlock {
            expected: ids[2],
            got
        }
    );
    assert_eq!(chain.tip().height, 2);

    // Not requested: the same block fails on its transaction.
    let rep = chain.add_blocks(vec![raws[2].clone()], None).unwrap();
    assert_eq!(rep.added_main, 0);
    let err = rep.error.unwrap();
    assert_eq!(err.index, 0);
    assert!(
        !matches!(err.reason, RejectReason::UnexpectedBlock { .. }),
        "{:?}",
        err.reason
    );
}

/// Mainnet block 5 (difficulty 74 529) with another nonce, so its proof of work fails; with
/// `garbage_tx` it also lists and carries an undecodable transaction.
fn block_5_with_weak_pow(genesis: &Hash, garbage_tx: bool) -> RawBlock {
    let f = fixture(5);
    let difficulty = f["difficulty"].as_u64().unwrap();
    let mut raw = raw_fixture(&f);
    let mut block = decode_block(&raw.block).unwrap();
    if garbage_tx {
        block.tx_hashes.push(Hash([1; 32]));
        raw.transactions.push(vec![0xff; 64]);
    }
    for _ in 0..64 {
        block.header.nonce = block.header.nonce.wrapping_add(1);
        let long_hash = block_long_hash(&block).unwrap();
        if check_proof_of_work_with_hash(&block, &long_hash, difficulty, genesis).is_err() {
            raw.block = encode_block(&block).unwrap();
            return raw;
        }
    }
    panic!("no nonce with a failing proof of work");
}

/// With `--fast-sync`, a single relayed block below the last checkpoint still has its proof of
/// work checked; a requested batch skips it.
#[test]
fn relayed_blocks_have_their_pow_checked_with_fast_sync() {
    let open = |db: &TempDb| {
        let mut cfg = ChainConfig::new(Network::Mainnet);
        cfg.fast_sync = true;
        db.open_with(cfg)
    };
    let first_four = || -> (Vec<RawBlock>, Vec<Hash>) {
        let fs: Vec<Value> = (1..=4).map(fixture).collect();
        (
            fs.iter().map(raw_fixture).collect(),
            fs.iter().map(fixture_hash).collect(),
        )
    };

    let db = TempDb::new();
    let chain = open(&db);
    let (raws, ids) = first_four();
    let rep = chain.add_blocks(raws, Some(&ids)).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    let weak_pow = AddBlockOutcome::Invalid(RejectReason::Consensus(ConsensusError::Block(
        xkr_consensus::BlockError::ProofOfWorkTooWeak,
    )));
    let weak = block_5_with_weak_pow(&chain.genesis_hash(), false);
    let res = chain.add_block(weak.clone()).unwrap();
    assert_eq!(res.outcome, weak_pow);
    assert_eq!(res.height, Some(5));
    // The proof of work is checked before the transactions are decoded.
    let res = chain
        .add_block(block_5_with_weak_pow(&chain.genesis_hash(), true))
        .unwrap();
    assert_eq!(res.outcome, weak_pow);
    assert_eq!(
        chain.add_block(raw_fixture(&fixture(5))).unwrap().outcome,
        AddBlockOutcome::AddedToMain
    );

    // The same block in a requested batch: below the last checkpoint, not checked.
    let db = TempDb::new();
    let chain = open(&db);
    let (mut raws, mut ids) = first_four();
    ids.push(block_hash(&decode_block(&weak.block).unwrap()).unwrap());
    raws.push(weak);
    let rep = chain.add_blocks(raws, Some(&ids)).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    assert_eq!(rep.added_main, 5);
}

/// After a reorg the pool is validated again: a transaction whose ring index now holds the
/// output of the new branch leaves the pool.
#[test]
fn pool_is_validated_again_after_a_reorg() {
    let db = TempDb::new();
    let chain = db.open(Network::Testnet);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let (owner_pub, owner_sec) = generate_keys(&mut os_rng());

    // Main: G - A1 - A2(owner) - A3 - A4.
    let (a1, ha1) = b.block(genesis, random_key(), &[], 0);
    let (a2, ha2) = b.block(ha1, owner_pub, &[], 0);
    let (a3, ha3) = b.block(ha2, random_key(), &[], 0);
    let (a4, _ha4) = b.block(ha3, random_key(), &[], 0);
    let rep = chain.add_blocks(vec![a1, a2, a3, a4], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);

    // The pool spends A2's output (index 0 of its amount).
    let tx = spend(owner_pub, &owner_sec, b.reward_of(&ha2), 0, 500);
    let tx_hash = chain
        .submit_transaction(tx, TxSource::Local)
        .unwrap()
        .unwrap();

    // Branch from A1: B2 pays the same amount to another key at the same index.
    let (b2, hb2) = b.block(ha1, random_key(), &[], 7);
    assert_eq!(b.reward_of(&hb2), b.reward_of(&ha2));
    let (b3, hb3) = b.block(hb2, random_key(), &[], 7);
    let (b4, hb4) = b.block(hb3, random_key(), &[], 7);
    let (b5, hb5) = b.block(hb4, random_key(), &[], 7);
    let rep = chain
        .add_blocks(vec![b2, b3, b4, b5], Some(&[hb2, hb3, hb4, hb5]))
        .unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    assert!(rep.switched);
    assert_eq!(chain.tip().hash, hb5);
    assert!(!chain.pool().contains(&tx_hash));
}

// ------------------------------------------------------------------ pool hooks

/// The tip watch fires once per call that moves the main-chain tip (append, switch to a
/// heavier branch) and stays quiet for alternative blocks that do not.
#[test]
fn tip_watch_fires_on_add_and_on_reorg() {
    let db = TempDb::new();
    let chain = db.open(Network::Testnet);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let mut rx = chain.subscribe_tip();
    assert_eq!(rx.borrow_and_update().hash, genesis);
    assert!(!rx.has_changed().unwrap());

    // Main: G - A1 - A2 - A3.
    let (a1, ha1) = b.block(genesis, random_key(), &[], 0);
    let (a2, ha2) = b.block(ha1, random_key(), &[], 0);
    let (a3, ha3) = b.block(ha2, random_key(), &[], 0);
    let a3_copy = a3.clone();
    let rep = chain.add_blocks(vec![a1, a2], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);
    assert!(rx.has_changed().unwrap());
    let tip = *rx.borrow_and_update();
    assert_eq!((tip.height, tip.hash), (2, ha2));
    assert_eq!(
        chain.add_block(a3).unwrap().outcome,
        AddBlockOutcome::AddedToMain
    );
    assert!(rx.has_changed().unwrap());
    assert_eq!(rx.borrow_and_update().hash, ha3);

    // Alternative from A1: B2, B3 (no switch), B4 (switch).
    let (b2, hb2) = b.block(ha1, random_key(), &[], 7);
    let (b3, hb3) = b.block(hb2, random_key(), &[], 7);
    let (b4, hb4) = b.block(hb3, random_key(), &[], 7);
    for raw in [b2, b3] {
        assert_eq!(
            chain.add_block(raw).unwrap().outcome,
            AddBlockOutcome::AddedToAlternative
        );
        assert!(!rx.has_changed().unwrap());
    }
    // A known block does not fire either.
    assert_eq!(
        chain.add_block(a3_copy).unwrap().outcome,
        AddBlockOutcome::AlreadyExists
    );
    assert!(!rx.has_changed().unwrap());
    assert_eq!(
        chain.add_block(b4).unwrap().outcome,
        AddBlockOutcome::AddedAndSwitched
    );
    assert!(rx.has_changed().unwrap());
    let tip = *rx.borrow_and_update();
    assert_eq!((tip.height, tip.hash), (4, hb4));
    assert_eq!(tip, chain.tip());
}

/// `Chain::miner_data` describes the next block on the current tip, and the main-chain header
/// lookups agree with the stored block infos.
#[test]
fn miner_data_and_main_chain_headers_follow_the_tip() {
    let db = TempDb::new();
    let chain = db.open(Network::Testnet);
    let mut b = Builder::new(&chain);
    let genesis = chain.tip().hash;
    let (a1, ha1) = b.block(genesis, random_key(), &[], 0);
    let (a2, ha2) = b.block(ha1, random_key(), &[], 0);
    let rep = chain.add_blocks(vec![a1, a2], None).unwrap();
    assert!(rep.error.is_none(), "{:?}", rep.error);

    let tip = chain.tip();
    let data = chain.miner_data().unwrap();
    assert_eq!(data.height, tip.height + 1);
    assert_eq!(data.prev_id, ha2);
    assert_eq!(data.difficulty, chain.next_difficulty().unwrap());
    assert_eq!(data.already_generated_coins, tip.already_generated_coins);
    assert_eq!(
        data.unlock_window,
        u64::from(chain.params().mined_money_unlock_window)
    );
    assert_eq!(
        data.major_version,
        xkr_consensus::block_major_version_for_height(chain.params(), 3)
    );
    let reader = chain.reader().unwrap();
    assert_eq!(
        data.median_weight,
        chain.block_median_size(&reader).unwrap()
    );
    drop(reader);
    assert!(data.tx_backlog.is_empty());

    let headers = chain.main_chain_headers(0, 10).unwrap();
    assert_eq!(headers.len(), 3);
    assert_eq!(
        headers.iter().map(|h| h.hash).collect::<Vec<_>>(),
        vec![genesis, ha1, ha2]
    );
    assert_eq!(
        headers.iter().map(|h| h.height).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(headers[2].timestamp, tip.timestamp);
    assert_eq!(headers[2].difficulty, tip.difficulty);
    assert_eq!(headers[2].cumulative_difficulty, tip.cumulative_difficulty);
    assert!(chain.main_chain_headers(5, 10).unwrap().is_empty());
    assert_eq!(
        chain.main_chain_header_by_hash(&ha1).unwrap(),
        Some(headers[1])
    );
    assert_eq!(
        chain.main_chain_header_by_hash(&Hash([7; 32])).unwrap(),
        None
    );
}

/// `Chain::close` with handles left in other threads (tasks that outlive the stop): the file is
/// closed cleanly, so the next open skips redb's check, and the late handles see `Closed`
/// errors instead of panicking.
#[test]
fn close_with_leftover_handles_leaves_a_clean_file() {
    use std::sync::mpsc;

    let db = TempDb::new();
    let chain = Arc::new(db.open(Network::Mainnet));
    let report = chain
        .add_blocks((1..=5).map(|h| raw_fixture(&fixture(h))).collect(), None)
        .unwrap();
    assert!(report.error.is_none(), "{:?}", report.error);

    // a thread that keeps reading until the database is closed, then stops quietly
    let (started_tx, started_rx) = mpsc::channel();
    let reading = {
        let chain = Arc::clone(&chain);
        std::thread::spawn(move || {
            let mut sent = false;
            loop {
                match chain.reader() {
                    Ok(r) => {
                        let _ = r.top_height();
                    }
                    Err(e) if e.is_closed() => return,
                    // a reader racing the close may also see the lock taken
                    Err(_) => {}
                }
                if !sent {
                    started_tx.send(()).unwrap();
                    sent = true;
                }
                std::thread::yield_now();
            }
        })
    };
    let late = Arc::clone(&chain);
    started_rx.recv().unwrap();

    chain.close().unwrap();
    assert!(chain.is_closed());
    reading.join().unwrap();
    assert!(late.reader().unwrap_err().is_closed());
    assert!(late.flush().unwrap_err().is_closed());
    assert_eq!(late.tip().height, 5);
    // closing again does nothing
    late.close().unwrap();

    // `chain` and `late` are still alive: the file was closed anyway
    let repaired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = Arc::clone(&repaired);
    let store = Store::open_observed(&db.0, 16 << 20, move |e: &xkr_storage::OpenEvent| {
        if matches!(e, xkr_storage::OpenEvent::Repair { .. }) {
            seen.store(true, Ordering::Relaxed);
        }
        true
    })
    .unwrap();
    assert!(
        !repaired.load(Ordering::Relaxed),
        "a closed chain must not be checked again"
    );
    let reopened = Chain::open(
        store,
        ChainConfig::new(Network::Mainnet),
        Arc::new(TxPool::new(PoolConfig::default())),
    )
    .unwrap();
    assert_eq!(reopened.tip().height, 5);
    assert_eq!(reopened.tip().hash, fixture_hash(&fixture(5)));
    drop(late);
}
