//! Opens a wallet file made by the C++ tools (wallet-api / xkrwallet-beta file or a
//! kryptokrona-service / xkrwallet container) and prints its public keys and addresses under
//! both prefixes as JSON, so a test can compare them with what the C++ tools report.
//!
//! ```text
//! cargo run -p xkr-wallet-files --example wallet_check -- <file> <password> [--mnemonic "<25 words>"]
//! ```
//!
//! With `--mnemonic`, the keys restored from the seed are compared with the file's keys.
//! No private key is printed.

use std::path::PathBuf;

use serde_json::json;
use xkr_wallet_core::{ADDRESS_PREFIX_ALT, Address, WalletKeys, secret_from_mnemonic};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(file), Some(password)) = (args.next(), args.next()) else {
        eprintln!("usage: wallet_check <file> <password> [--mnemonic \"<25 words>\"]");
        std::process::exit(2);
    };
    let mut mnemonic = None;
    while let Some(arg) = args.next() {
        if arg == "--mnemonic" {
            mnemonic = args.next();
        }
    }

    let summary = match xkr_wallet_files::summarize_file(&PathBuf::from(&file), &password) {
        Ok(s) => s,
        Err(e) => {
            println!("{}", json!({ "file": file, "error": e.to_string() }));
            std::process::exit(1);
        }
    };
    let view = summary.view_public_key;
    let addresses: Vec<_> = summary
        .addresses
        .iter()
        .map(|a| {
            let (sekr, xkr) = view.map_or((None, None), |view| {
                let standard = Address::new(a.spend_public_key, view);
                let alt = Address {
                    prefix: ADDRESS_PREFIX_ALT,
                    ..standard
                };
                (Some(standard.encode()), Some(alt.encode()))
            });
            let reparsed_alt = xkr
                .as_deref()
                .and_then(|x| Address::parse(x).ok())
                .map(|p| p.spend == a.spend_public_key && Some(p.view) == view);
            json!({
                "stored": a.address,
                "spendPublicKey": a.spend_public_key.to_hex(),
                "hasSpendSecret": a.has_spend_secret,
                "spendKeyMatches": a.spend_key_matches,
                "sekr": sekr,
                "xkr": xkr,
                "storedEqualsSekr": a.address.as_ref().map(|s| Some(s) == sekr.as_ref()),
                "xkrParsesToSameKeys": reparsed_alt,
                "startTimestamp": a.start_timestamp,
                "startHeight": a.start_height,
                "unspent": a.unspent,
            })
        })
        .collect();

    let seed = mnemonic.map(|m| match secret_from_mnemonic(&m) {
        Ok(secret) => {
            let keys = WalletKeys::from_spend_secret(&secret).expect("valid spend key");
            let address = Address::new(keys.spend_public, keys.view_public).encode();
            json!({
                "address": address,
                "viewKeyMatchesFile": Some(keys.view_public) == view,
                "spendKeyInFile": summary.addresses.iter().any(|a| a.spend_public_key == keys.spend_public),
            })
        }
        Err(e) => json!({ "error": e.to_string() }),
    });

    let out = json!({
        "file": file,
        "kind": format!("{:?}", summary.kind),
        "viewPublicKey": view.map(|v| v.to_hex()),
        "viewKeyValid": summary.view_key_valid,
        "viewOnly": summary.view_only,
        "addresses": addresses,
        "transactions": summary.transaction_count,
        "syncHeight": summary.sync_height,
        "seed": seed,
    });
    println!("{}", serde_json::to_string_pretty(&out).expect("json"));
}
