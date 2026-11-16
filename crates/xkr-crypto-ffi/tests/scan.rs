//! `processBlockOutputs` on generated blocks: subwallets, coinbase, view wallets, view tags,
//! unusable transaction keys, and the exact JSON shape `NativeTest.js` reads. The key images are
//! checked against `xkr-wallet-core`'s scan (the node's wallet code).

use serde_json::{Value, json};
use xkr_crypto_ffi::api;
use xkr_crypto_ffi::scan::{self, SpendKey};
use xkr_types::{PublicKey, SecretKey};
use xkr_wallet_core::{WalletKeys, scan_tagged_outputs};

struct Wallet {
    view_secret: String,
    view_public: String,
    spends: Vec<api::KeyPair>,
}

fn wallet(subwallets: usize) -> Wallet {
    let view = api::generate_keys();
    Wallet {
        view_secret: view.private_key,
        view_public: view.public_key,
        spends: (0..subwallets).map(|_| api::generate_keys()).collect(),
    }
}

/// One output for `(wallet, subwallet)` at `index` of a transaction with secret `tx_secret`.
fn output_key(to: &Wallet, sub: usize, tx_secret: &str, index: u64) -> (String, String) {
    let d = api::generate_key_derivation(&to.view_public, tx_secret).unwrap();
    let key = api::derive_public_key(&d, index, &to.spends[sub].public_key).unwrap();
    let tag = api::derive_view_tag(&d, index).unwrap();
    (key, tag)
}

fn spend_keys_json(w: &Wallet) -> String {
    let keys: Vec<Value> = w
        .spends
        .iter()
        .map(|k| json!({ "publicKey": k.public_key, "privateKey": k.private_key }))
        .collect();
    Value::Array(keys).to_string()
}

fn run(block: &Value, w: &Wallet, view_wallet: bool, coinbase: bool) -> Vec<Value> {
    let out = scan::process_block_outputs_json(
        &block.to_string(),
        &w.view_secret,
        &spend_keys_json(w),
        view_wallet,
        coinbase,
    )
    .unwrap();
    serde_json::from_str::<Value>(&out)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

fn hex_bytes(s: &str) -> [u8; 32] {
    hex::decode(s).unwrap().try_into().unwrap()
}

#[test]
fn finds_the_outputs_of_every_subwallet_in_order() {
    let me = wallet(2);
    let other = wallet(1);
    let cb = api::generate_keys();
    let tx1 = api::generate_keys();
    let tx2 = api::generate_keys();

    let (cb0, _) = output_key(&me, 0, &cb.private_key, 0);
    let (a0, _) = output_key(&other, 0, &tx1.private_key, 0);
    let (a1, _) = output_key(&me, 1, &tx1.private_key, 1);
    let (a2, a2_tag) = output_key(&me, 0, &tx1.private_key, 2);
    let (a3, a3_tag) = output_key(&other, 0, &tx1.private_key, 3);
    let (b0, b_tag) = output_key(&me, 1, &tx2.private_key, 0);

    let block = json!({
        "coinbaseTransaction": {
            "keyOutputs": [{ "key": cb0, "amount": 2_000_000, "globalIndex": 900 }],
            "hash": "cb".repeat(32),
            "transactionPublicKey": cb.public_key,
            "unlockTime": 50,
        },
        "transactions": [
            {
                "keyOutputs": [
                    { "key": a0, "amount": 1, "globalIndex": 1000 },
                    { "key": a1, "amount": 20, "globalIndex": 1001 },
                    { "key": a2, "amount": 300, "viewTag": a2_tag },
                    { "key": a3, "amount": 4000, "globalIndex": 1003, "viewTag": a3_tag },
                ],
                "hash": "AA".repeat(32),
                "transactionPublicKey": tx1.public_key,
                "paymentID": "",
            },
            {
                // a transaction key that is not a point owns nothing
                "keyOutputs": [{ "key": a1, "amount": 5 }],
                "hash": "bb".repeat(32),
                "transactionPublicKey": "4a078e76cd41a3d3b534b83dc6f2ea2de500b653ca82273b7bfad8045d85a400",
            },
            {
                "keyOutputs": [{ "key": b0.to_uppercase(), "amount": 7, "globalIndex": 77, "viewTag": b_tag }],
                "hash": "cc".repeat(32),
                "transactionPublicKey": tx2.public_key,
            },
        ],
        "blockHeight": 123,
        "blockHash": "dd".repeat(32),
        "blockTimestamp": 1_700_000_000,
    });

    let found = run(&block, &me, false, true);
    let summary: Vec<(String, u64, u64, i64, String)> = found
        .iter()
        .map(|f| {
            let i = &f["input"];
            (
                f["publicSpendKey"].as_str().unwrap().to_owned(),
                i["amount"].as_u64().unwrap(),
                i["transactionIndex"].as_u64().unwrap(),
                i["globalOutputIndex"].as_i64().unwrap(),
                i["parentTransactionHash"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let s0 = me.spends[0].public_key.clone();
    let s1 = me.spends[1].public_key.clone();
    assert_eq!(
        summary,
        vec![
            (s0.clone(), 2_000_000, 0, 900, "cb".repeat(32)),
            (s1.clone(), 20, 1, 1001, "AA".repeat(32)),
            (s0, 300, 2, -1, "AA".repeat(32)),
            (s1, 7, 0, 77, "cc".repeat(32)),
        ]
    );
    // exactly the keys NativeTest.js reads
    for f in &found {
        let mut keys: Vec<&String> = f.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["input", "publicSpendKey"]);
        let mut input_keys: Vec<&String> = f["input"].as_object().unwrap().keys().collect();
        input_keys.sort();
        assert_eq!(
            input_keys,
            [
                "amount",
                "globalOutputIndex",
                "key",
                "keyImage",
                "parentTransactionHash",
                "transactionIndex"
            ]
        );
    }
    // output keys are lower case even when the block had upper case
    assert_eq!(found[3]["input"]["key"], json!(b0));

    // key images agree with the node's wallet scan
    for (f, (tx_public, outputs)) in found.iter().zip([
        (&cb.public_key, vec![cb0.clone()]),
        (
            &tx1.public_key,
            vec![a0.clone(), a1.clone(), a2.clone(), a3.clone()],
        ),
        (
            &tx1.public_key,
            vec![a0.clone(), a1.clone(), a2.clone(), a3.clone()],
        ),
        (&tx2.public_key, vec![b0.clone()]),
    ]) {
        let owner = me
            .spends
            .iter()
            .find(|k| k.public_key == f["publicSpendKey"].as_str().unwrap())
            .unwrap();
        let keys = WalletKeys::from_secrets(
            &SecretKey(hex_bytes(&owner.private_key)),
            &SecretKey(hex_bytes(&me.view_secret)),
        )
        .unwrap();
        let outs: Vec<(u64, PublicKey, Option<u8>)> = outputs
            .iter()
            .map(|k| (0, PublicKey(hex_bytes(k)), None))
            .collect();
        let node = scan_tagged_outputs(&keys, &PublicKey(hex_bytes(tx_public)), &outs);
        let index = usize::try_from(f["input"]["transactionIndex"].as_u64().unwrap()).unwrap();
        let hit = node.iter().find(|o| o.index == index).unwrap();
        assert_eq!(f["input"]["keyImage"], json!(hit.key_image.to_hex()));
    }

    // without coinbase processing the coinbase output is not reported
    assert_eq!(run(&block, &me, false, false).len(), 3);

    // a view wallet reports the same outputs with the all-zero key image, and does not need
    // the private spend keys
    let mut view_only = Wallet {
        view_secret: me.view_secret.clone(),
        view_public: me.view_public.clone(),
        spends: me.spends.clone(),
    };
    for k in &mut view_only.spends {
        k.private_key = String::new();
    }
    let viewed = run(&block, &view_only, true, true);
    assert_eq!(viewed.len(), 4);
    assert!(
        viewed
            .iter()
            .all(|f| f["input"]["keyImage"] == json!("0".repeat(64)))
    );

    // the other wallet sees its two outputs
    assert_eq!(run(&block, &other, false, true).len(), 2);
}

#[test]
fn a_wrong_view_tag_hides_the_output_and_a_malformed_tag_is_ignored() {
    let me = wallet(1);
    let tx = api::generate_keys();
    let (k0, tag0) = output_key(&me, 0, &tx.private_key, 0);
    let (k1, _) = output_key(&me, 0, &tx.private_key, 1);
    let wrong = format!("{:02x}", u8::from_str_radix(&tag0, 16).unwrap() ^ 1);
    let block = json!({
        "transactions": [{
            "keyOutputs": [
                { "key": k0, "amount": 1, "viewTag": wrong },
                { "key": k1, "amount": 2, "viewTag": "zz" },
            ],
            "hash": "ee".repeat(32),
            "transactionPublicKey": tx.public_key,
        }],
    });
    let found = run(&block, &me, false, true);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["input"]["amount"], json!(2));
}

#[test]
fn the_last_pair_for_a_spend_key_wins_and_bad_input_is_refused() {
    let me = wallet(1);
    let tx = api::generate_keys();
    let (k0, _) = output_key(&me, 0, &tx.private_key, 0);
    let block = json!({
        "transactions": [{
            "keyOutputs": [{ "key": k0, "amount": 1 }],
            "hash": "ff".repeat(32),
            "transactionPublicKey": tx.public_key,
        }],
    });
    let spend = &me.spends[0];
    let doubled = vec![
        SpendKey {
            public_key: spend.public_key.clone(),
            private_key: api::generate_keys().private_key,
        },
        SpendKey {
            public_key: spend.public_key.clone(),
            private_key: spend.private_key.clone(),
        },
    ];
    let parsed = scan::parse_block_json(&block.to_string()).unwrap();
    let found =
        scan::process_block_outputs(&parsed, &me.view_secret, &doubled, false, true).unwrap();
    let d = api::generate_key_derivation(&tx.public_key, &me.view_secret).unwrap();
    let secret = api::derive_secret_key(&d, 0, &spend.private_key).unwrap();
    assert_eq!(
        found[0].input.key_image,
        api::generate_key_image(&k0, &secret).unwrap()
    );

    // a spend wallet needs its private keys
    let missing = json!([{ "publicKey": spend.public_key }]).to_string();
    assert!(
        scan::process_block_outputs_json(
            &block.to_string(),
            &me.view_secret,
            &missing,
            false,
            true
        )
        .is_err()
    );
    // the shape of the block matters
    assert!(
        scan::process_block_outputs_json(
            r#"{"transactions":[{"hash":"00"}]}"#,
            &me.view_secret,
            &spend_keys_json(&me),
            false,
            true
        )
        .is_err()
    );
    assert!(
        scan::process_block_outputs_json(
            &block.to_string(),
            "00",
            &spend_keys_json(&me),
            false,
            true
        )
        .is_err()
    );
}
