//! Real mainnet blocks (fetched from a public node, see `tests/fixtures/README.md`):
//! decode -> re-encode must be byte-identical and every hash must match the node's.

use serde_json::Value;
use xkr_serialization::{
    block_hash, block_hashing_blob, block_pow_blob, decode_block, decode_raw_block,
    decode_transaction, encode_block, encode_raw_block, encode_transaction, transaction_hash,
    transaction_hash_from_blob, transaction_prefix_hash,
};
use xkr_types::{Hash, RawBlock, TxInput};

fn fixtures() -> Vec<Value> {
    serde_json::from_str(include_str!("fixtures/blocks.json")).expect("fixture json")
}

fn hex_field(v: &Value, k: &str) -> Vec<u8> {
    hex::decode(v[k].as_str().unwrap_or_else(|| panic!("missing {k}"))).expect("hex")
}

fn hash_field(v: &Value, k: &str) -> Hash {
    v[k].as_str().unwrap().parse().expect("hash")
}

#[test]
fn blocks_roundtrip_and_hash() {
    let blocks = fixtures();
    assert!(blocks.len() >= 30);
    for f in &blocks {
        let height = f["height"].as_u64().unwrap();
        let blob = hex_field(f, "blob");
        let block = decode_block(&blob).unwrap_or_else(|e| panic!("height {height}: {e}"));
        assert_eq!(
            encode_block(&block).unwrap(),
            blob,
            "height {height}: re-encode"
        );
        assert_eq!(
            block_hash(&block).unwrap(),
            hash_field(f, "hash"),
            "height {height}: block hash"
        );

        assert_eq!(u64::from(block.height()), height);
        assert_eq!(
            u64::from(block.header.major_version),
            f["major_version"].as_u64().unwrap()
        );
        assert_eq!(
            u64::from(block.header.minor_version),
            f["minor_version"].as_u64().unwrap()
        );
        assert_eq!(block.header.timestamp, f["timestamp"].as_u64().unwrap());
        assert_eq!(u64::from(block.header.nonce), f["nonce"].as_u64().unwrap());
        assert_eq!(block.header.prev_hash, hash_field(f, "prev_hash"));
        assert_eq!(
            block.parent_block.is_some(),
            block.header.major_version >= 2
        );
        assert_eq!(
            transaction_hash(&block.miner_tx).unwrap(),
            hash_field(f, "miner_tx_hash")
        );
        assert_eq!(
            block.miner_tx.signatures,
            Vec::<Vec<xkr_types::Signature>>::new()
        );
        assert!(matches!(
            block.miner_tx.prefix.inputs.as_slice(),
            [TxInput::Coinbase { .. }]
        ));

        // pow blob: 76 bytes-ish header for v1, parent header-only blob for v2+
        let pow = block_pow_blob(&block).unwrap();
        if block.header.major_version == 1 {
            assert_eq!(pow, block_hashing_blob(&block).unwrap());
        }
        assert!(pow.len() >= 43, "variant 1 needs 43 bytes");

        let txs = f["txs"].as_array().unwrap();
        assert_eq!(
            txs.len(),
            block.tx_hashes.len(),
            "height {height}: tx count"
        );
        let mut cumulative = encode_transaction(&block.miner_tx).unwrap().len();
        let mut raw = RawBlock {
            block: blob.clone(),
            transactions: Vec::new(),
        };
        for (t, expected_hash) in txs.iter().zip(&block.tx_hashes) {
            let tx_blob = hex_field(t, "blob");
            let tx_hash = hash_field(t, "hash");
            assert_eq!(&tx_hash, expected_hash);
            assert_eq!(transaction_hash_from_blob(&tx_blob), tx_hash);
            let tx = decode_transaction(&tx_blob).unwrap_or_else(|e| panic!("tx {tx_hash}: {e}"));
            assert_eq!(
                encode_transaction(&tx).unwrap(),
                tx_blob,
                "tx {tx_hash}: re-encode"
            );
            assert_eq!(transaction_hash(&tx).unwrap(), tx_hash);
            assert_eq!(tx.signatures.len(), tx.prefix.inputs.len());
            // the prefix hash is the hash of the blob without the trailing signatures
            let sig_bytes: usize = tx.signatures.iter().map(|s| s.len() * 64).sum();
            assert_eq!(
                transaction_prefix_hash(&tx.prefix),
                xkr_crypto::cn_fast_hash(&tx_blob[..tx_blob.len() - sig_bytes])
            );
            cumulative += tx_blob.len();
            raw.transactions.push(tx_blob);
        }
        assert_eq!(
            cumulative as u64,
            f["cumulative_size"].as_u64().unwrap(),
            "height {height}: cumulative size"
        );
        let raw_blob = encode_raw_block(&raw);
        assert_eq!(decode_raw_block(&raw_blob).unwrap(), raw);
    }
}

#[test]
fn truncated_blocks_are_errors_not_panics() {
    for f in fixtures().iter().take(12) {
        let blob = hex_field(f, "blob");
        for cut in 0..blob.len() {
            assert!(decode_block(&blob[..cut]).is_err());
        }
        let mut longer = blob.clone();
        longer.push(0);
        assert!(decode_block(&longer).is_err());
    }
}

#[test]
fn genesis_block_from_config_hex() {
    // cryptonote_config.h GENESIS_COINBASE_TX_HEX + Currency::generateGenesisBlock:
    // major 1, minor 0, timestamp 0, nonce 70, prev = 0.
    let miner_tx = decode_transaction(
        &hex::decode("010a01ff000188f3b501029b2e4c0281c0b02e7c53291a94d1d0cbff8883f8024f5142ee494ffbbd088071210142694232c5b04151d9e4c27d31ec7a68ea568b19488cfcb422659a07a0e44dd5").unwrap(),
    )
    .unwrap();
    let block = xkr_types::Block {
        header: xkr_types::BlockHeader {
            major_version: 1,
            minor_version: 0,
            timestamp: 0,
            prev_hash: Hash::ZERO,
            nonce: 70,
        },
        parent_block: None,
        miner_tx,
        tx_hashes: Vec::new(),
    };
    assert_eq!(
        block_hash(&block).unwrap().to_string(),
        "7fb97df81221dd1366051b2d0bc7f49c66c22ac4431d879c895b06d66ef66f4c"
    );
    let genesis_fixture = &fixtures()[0];
    assert_eq!(
        encode_block(&block).unwrap(),
        hex_field(genesis_fixture, "blob")
    );
}
