//! Replays the answers captured from the C++ kryptokrona-service
//! (`tests/fixtures/cpp/*.jsonl`, written by `testnet/fixtures/capture-walletd.sh`) against
//! xkr-walletd with a wallet on an in-process xkrd. Skipped when the fixtures are absent.
//!
//! The chain is laid out as in the capture: blocks 1–30 pay wallet A (the seed of the
//! fixtures), 31–130 another address K; the sends of `send.jsonl` and `delayed.jsonl` are mined
//! into block 131, followed by eleven more blocks; the sends of `addresses.jsonl` go into
//! block 143; then blocks up to 154.
//!
//! What is compared:
//! * status, `Server`, `Connection` and `Access-Control-Allow-Origin`, and `Content-Length`
//!   when the body is identical;
//! * error answers exactly (code, message, `data.application_code`, `id`, `jsonrpc`);
//! * results with the same keys, the same JSON types, strings exactly (addresses, keys, seeds,
//!   payment ids), and numbers exactly except the ones that depend on the chain (balances,
//!   heights, timestamps, amounts, counts). The captured placeholders (`<tx:1>`, `<block:3>`,
//!   …) are bound to this run's values on first sight and must stay consistent within a file;
//!   `<timestamp>`, `<peers>`, `<change>` and `<n>` match any value.
//! * Lists of transactions are paired by `amount` (deterministic for the sends); lists whose
//!   length depends on the chain (`items`, `transactions`, `transfers`, …) are compared by shape
//!   when their lengths differ.
//!
//! Answers that cannot match would be listed in [`DEVIATIONS`] with the reason; they are
//! reported but do not fail the test. The tracking flow runs on one container as in the
//! capture: its only address is deleted, then a tracking address is added.

#[path = "../../xkr-wallet-core/tests/vectors/mod.rs"]
mod vectors;

#[allow(dead_code)]
#[path = "../../xkr-rpc/tests/tx_v2_walletsync/node.rs"]
mod node;

mod common;

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use common::{Chain, RouterTransport, http, wait_synced, wallet_config};
use serde_json::Value;
use xkr_consensus::Network;
use xkr_wallet::Wallet;
use xkr_wallet_core::{Address, WalletKeys};
use xkr_walletd::config::Config;
use xkr_walletd::container::{self, Generated};
use xkr_walletd::{RpcSettings, Service, router};

/// Seed A of the capture (`walletd-main.wallet`, password `walletd-pass`).
const SEED_A: &str = "skulls woozy ouch summon gifts huts waffle ourselves obtains hexagon \
                      tadpoles hacksaw dormant hence abort listen history atom cadets stylishly \
                      snout vegan girth guest history";
/// The address of seed A.
const ADDRESS_A: &str = "SEKReX4MYn2HAJFoHvcqVBPyHYom2ynKeK6dpYptbp8gQNzdzE73ZDkNmNurqfhhcMSUXpS1ZGEJKiKJUcPCyw7vYaCc35yBZCf";
/// Address K of the capture (keys K), the recipient of the sends.
const ADDRESS_K: &str = "SEKReZCo6myNqvP1x4MuTVFxqVydgF2PBatbBKdER2LP6uH56q3s4EbEaCRGvRc8uXEeoz5PaR5EgDZd1FTbCeVeYFqjboZpBZn";
/// `--rpc-password` of the capture.
const RPC_PASSWORD: &str = "fixture-rpc-pass";
/// `--enable-cors` of the capture.
const CORS: &str = "https://fixture.example";

/// The fixture files in capture order; other `.jsonl` files found are replayed afterwards.
const FILES: [&str; 10] = [
    "transport.jsonl",
    "methods.jsonl",
    "send.jsonl",
    "delayed.jsonl",
    "transactions.jsonl",
    "fusion.jsonl",
    "addresses.jsonl",
    "save-export-reset.jsonl",
    "legacy-cors.jsonl",
    "tracking.jsonl",
];

/// Fixture files that are not walletd answers.
const NOT_WALLETD: [&str; 1] = ["node-routes.jsonl"];

/// Answers that cannot match, with the reason.
const DEVIATIONS: [(&str, &str); 0] = [];

/// Answers that depend on the ring members the chain holds for the inputs chosen: a
/// transaction hash, or `MIXIN_COUNT_TOO_BIG` when the node returns too few outputs of an input's
/// amount. Either is accepted.
const CHAIN_ANSWERS: [(&str, &str); 1] = [(
    "fusion.jsonl:sendFusionTransaction",
    "a fusion transaction or MIXIN_COUNT_TOO_BIG, depending on the outputs of the chain",
)];

/// Methods whose inputs are chosen at random; a transfer the capture sent is sent again while
/// this run answers `MIXIN_COUNT_TOO_BIG` ([`is_too_few_ring_members`]).
const RANDOM_INPUT_METHODS: [&str; 2] = ["sendTransaction", "createDelayedTransaction"];

/// Attempts at most for a transfer of [`RANDOM_INPUT_METHODS`].
const RETRY_ATTEMPTS: usize = 40;

/// Whether `body` is walletd's `MIXIN_COUNT_TOO_BIG` (application code 6), the answer for an
/// input whose amount has too few outputs on the chain to form its ring.
fn is_too_few_ring_members(body: &[u8]) -> bool {
    let answer: Value = serde_json::from_slice(body).unwrap_or_default();
    answer["error"]["data"]["application_code"] == 6
        && answer["error"]["message"] == "MixIn count is too big"
}

/// Numbers that depend on the chain and the input selection.
const CHAIN_NUMBERS: [&str; 11] = [
    "availableBalance",
    "lockedAmount",
    "blockCount",
    "knownBlockCount",
    "localDaemonBlockCount",
    "peerCount",
    "amount",
    "timestamp",
    "blockIndex",
    "fusionReadyCount",
    "totalOutputCount",
];

/// Lists whose length depends on the chain.
const CHAIN_LISTS: [&str; 4] = ["items", "transactions", "transactionHashes", "transfers"];

/// Placeholder kinds that stand for 64 hex characters.
const HEX_KINDS: [&str; 8] = [
    "tx", "block", "spendkey", "spendpub", "viewkey", "txkey", "txpub", "hex",
];

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cpp")
}

/// One captured exchange.
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    request: Value,
    response: Value,
}

fn load(file: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).expect("fixture line is JSON");
            Entry {
                name: v["name"].as_str().unwrap_or_default().to_owned(),
                request: v["request"].clone(),
                response: v["response"].clone(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// placeholders

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    /// The placeholder as written, its kind, and whether it is numbered (`<tx:1>`).
    Hole(String, String, bool),
}

fn parts(text: &str) -> Vec<Part> {
    let mut out = Vec::new();
    let mut literal = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        literal.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let hole = after.find('>').and_then(|end| {
            let inner = &after[..end];
            let (kind, number) = inner.split_once(':').unwrap_or((inner, ""));
            let ok = !kind.is_empty()
                && kind.bytes().all(|b| b.is_ascii_lowercase())
                && number.bytes().all(|b| b.is_ascii_digit());
            ok.then(|| (end, kind.to_owned(), !number.is_empty()))
        });
        if let Some((end, kind, numbered)) = hole {
            if !literal.is_empty() {
                out.push(Part::Text(std::mem::take(&mut literal)));
            }
            out.push(Part::Hole(
                rest[start..start + end + 2].to_owned(),
                kind,
                numbered,
            ));
            rest = &after[end + 1..];
        } else {
            literal.push('<');
            rest = after;
        }
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        out.push(Part::Text(literal));
    }
    out
}

fn has_holes(v: &Value) -> bool {
    match v {
        Value::String(s) => parts(s).iter().any(|p| matches!(p, Part::Hole(..))),
        Value::Array(a) => a.iter().any(has_holes),
        Value::Object(o) => o.values().any(has_holes),
        _ => false,
    }
}

type Bindings = HashMap<String, String>;

/// Matches `ours` against the captured text, binding numbered placeholders.
fn match_text(captured: &str, ours: &str, bindings: &mut Bindings) -> Result<(), String> {
    let ps = parts(captured);
    if !ps.iter().any(|p| matches!(p, Part::Hole(..))) {
        return if captured == ours {
            Ok(())
        } else {
            Err(format!("{ours:?} != {captured:?}"))
        };
    }
    let mut new = Bindings::new();
    let mut pos = 0usize;
    for (i, part) in ps.iter().enumerate() {
        match part {
            Part::Text(t) => {
                if !ours[pos..].starts_with(t.as_str()) {
                    return Err(format!("{ours:?} does not fit {captured:?}"));
                }
                pos += t.len();
            }
            Part::Hole(token, kind, numbered) => {
                let len = if HEX_KINDS.contains(&kind.as_str()) {
                    64
                } else {
                    match ps.get(i + 1) {
                        Some(Part::Text(next)) => ours[pos..]
                            .find(next.as_str())
                            .ok_or_else(|| format!("{ours:?} does not fit {captured:?}"))?,
                        _ => ours.len() - pos,
                    }
                };
                let Some(value) = ours.get(pos..pos + len) else {
                    return Err(format!("{ours:?} does not fit {captured:?}"));
                };
                if HEX_KINDS.contains(&kind.as_str())
                    && !value.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err(format!("{ours:?}: {token} is not hex"));
                }
                if *numbered {
                    let known = bindings.get(token).or_else(|| new.get(token));
                    match known {
                        Some(v) if v != value => {
                            return Err(format!("{token} was {v}, here {value}"));
                        }
                        Some(_) => {}
                        None => {
                            new.insert(token.clone(), value.to_owned());
                        }
                    }
                }
                pos += len;
            }
        }
    }
    if pos != ours.len() {
        return Err(format!("{ours:?} is longer than {captured:?}"));
    }
    bindings.extend(new);
    Ok(())
}

/// Puts this run's values in for the placeholders of a request; `Err` names one that is not
/// bound.
fn substitute(v: &Value, bindings: &Bindings) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) => {
            let mut out = String::new();
            for part in parts(s) {
                match part {
                    Part::Text(t) => out.push_str(&t),
                    Part::Hole(token, _, _) => match bindings.get(&token) {
                        Some(value) => out.push_str(value),
                        None => return Err(token),
                    },
                }
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| substitute(x, bindings))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| substitute(x, bindings).map(|x| (k.clone(), x)))
                .collect::<Result<_, _>>()?,
        ),
        other => other.clone(),
    })
}

// ---------------------------------------------------------------------------------------------
// comparison

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A whole-value placeholder without a number: matches any scalar.
fn is_wildcard(s: &str) -> bool {
    matches!(parts(s).as_slice(), [Part::Hole(_, _, false)])
}

/// Same JSON types and keys, values not compared.
fn shape(captured: &Value, ours: &Value, path: &str, diffs: &mut Vec<String>) {
    match (captured, ours) {
        (Value::String(s), Value::Number(_)) if is_wildcard(s) => {}
        (Value::Object(c), Value::Object(o)) => {
            let captured_keys: BTreeSet<&String> = c.keys().collect();
            let our_keys: BTreeSet<&String> = o.keys().collect();
            if captured_keys != our_keys {
                diffs.push(format!("{path}: keys {our_keys:?} != {captured_keys:?}"));
                return;
            }
            for (k, v) in c {
                shape(v, &o[k], &format!("{path}.{k}"), diffs);
            }
        }
        (Value::Array(c), Value::Array(o)) => {
            if let Some(first) = c.first() {
                for (i, x) in o.iter().enumerate() {
                    shape(first, x, &format!("{path}[{i}]"), diffs);
                }
            }
        }
        (c, o) if type_name(c) == type_name(o) => {}
        (c, o) => diffs.push(format!("{path}: {} != {}", type_name(o), type_name(c))),
    }
}

/// Whether a list holds transactions (paired by amount).
fn transaction_list(list: &[Value]) -> bool {
    list.iter()
        .all(|x| x.get("transactionHash").is_some() && x.get("amount").is_some())
}

fn compare(
    captured: &Value,
    ours: &Value,
    key: &str,
    path: &str,
    bindings: &mut Bindings,
    diffs: &mut Vec<String>,
) {
    match (captured, ours) {
        (Value::String(s), Value::Number(_) | Value::String(_)) if is_wildcard(s) => {}
        (Value::String(c), Value::String(o)) => {
            if let Err(e) = match_text(c, o, bindings) {
                diffs.push(format!("{path}: {e}"));
            }
        }
        (Value::Number(c), Value::Number(o)) => {
            if c != o && !CHAIN_NUMBERS.contains(&key) {
                diffs.push(format!("{path}: {o} != {c}"));
            }
        }
        (Value::Object(c), Value::Object(o)) => {
            let captured_keys: BTreeSet<&String> = c.keys().collect();
            let our_keys: BTreeSet<&String> = o.keys().collect();
            if captured_keys != our_keys {
                diffs.push(format!("{path}: keys {our_keys:?} != {captured_keys:?}"));
            }
            for (k, v) in c {
                if let Some(x) = o.get(k) {
                    compare(v, x, k, &format!("{path}.{k}"), bindings, diffs);
                }
            }
        }
        (Value::Array(c), Value::Array(o)) => {
            if c.len() == o.len() {
                let mut order: Vec<usize> = (0..o.len()).collect();
                if transaction_list(c) && transaction_list(o) {
                    let mut used = vec![false; o.len()];
                    let paired: Option<Vec<usize>> = c
                        .iter()
                        .map(|x| {
                            let j = (0..o.len())
                                .find(|&j| !used[j] && o[j]["amount"] == x["amount"])?;
                            used[j] = true;
                            Some(j)
                        })
                        .collect();
                    if let Some(p) = paired {
                        order = p;
                    }
                }
                for (i, x) in c.iter().enumerate() {
                    compare(
                        x,
                        &o[order[i]],
                        key,
                        &format!("{path}[{i}]"),
                        bindings,
                        diffs,
                    );
                }
            } else if CHAIN_LISTS.contains(&key) {
                if c.is_empty() == o.is_empty() {
                    shape(captured, ours, path, diffs);
                } else {
                    diffs.push(format!("{path}: {} entries, captured {}", o.len(), c.len()));
                }
            } else {
                diffs.push(format!("{path}: {} entries, captured {}", o.len(), c.len()));
            }
        }
        (c, o) if c == o => {}
        (c, o) => diffs.push(format!("{path}: {o} != {c}")),
    }
}

// ---------------------------------------------------------------------------------------------
// the run

#[derive(Default)]
struct Replay {
    bindings: Bindings,
    passed: usize,
    report: String,
    failures: Vec<String>,
    /// `(application_code, message)` of every error answered.
    codes: BTreeSet<(i64, String)>,
}

impl Replay {
    async fn run(&mut self, router: &Router, file: &str, e: &Entry) {
        let id = format!("{file}:{}", e.name);
        let deviation = DEVIATIONS.iter().find(|(n, _)| *n == id).map(|(_, r)| *r);
        let is_error = e.response["body"].get("error").is_some();
        let req = &e.request;
        let body = match substitute(&req["body"], &self.bindings) {
            Ok(b) => b,
            Err(token) if is_error => {
                // the answer is an error either way: any value of the right form will do
                let mut b = self.bindings.clone();
                b.insert(token.clone(), "ab".repeat(32));
                match substitute(&req["body"], &b) {
                    Ok(v) => v,
                    Err(t) => {
                        let _ = writeln!(self.report, "SKIP {id}: {t} not bound");
                        return;
                    }
                }
            }
            Err(token) => {
                let _ = writeln!(self.report, "SKIP {id}: {token} not bound in this run");
                return;
            }
        };
        let bytes = match &body {
            Value::Null => Vec::new(),
            Value::String(s) => s.as_bytes().to_vec(),
            other => serde_json::to_vec(other).expect("json"),
        };
        let headers: Vec<(String, String)> =
            req["headers"].as_object().map_or_else(Vec::new, |h| {
                h.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                    .collect()
            });
        let method = req["method"].as_str().unwrap_or("POST");
        let path = req["path"].as_str().unwrap_or("/json_rpc");
        let mut answer = http(router, method, path, &headers, bytes.clone()).await;
        // The inputs of a transfer are chosen at random. When this run's choice takes an
        // output whose amount has too few other outputs on the chain (fusion and change
        // outputs, see CHAIN_ANSWERS), the answer is MIXIN_COUNT_TOO_BIG; the recorded C++
        // choice was not, so the transfer is sent again with a new choice.
        let sent_in_capture = !is_error
            && RANDOM_INPUT_METHODS.contains(&body["method"].as_str().unwrap_or_default());
        let mut attempts = 1;
        while sent_in_capture && attempts < RETRY_ATTEMPTS && is_too_few_ring_members(&answer.body)
        {
            answer = http(router, method, path, &headers, bytes.clone()).await;
            attempts += 1;
        }
        if attempts > 1 {
            let _ = writeln!(self.report, "NOTE {id}: sent {attempts} times");
        }

        if let Some((_, reason)) = CHAIN_ANSWERS.iter().find(|(n, _)| *n == id) {
            let ours: Value = serde_json::from_slice(&answer.body).unwrap_or_default();
            let sent = ours["result"]["transactionHash"]
                .as_str()
                .is_some_and(|h| h.len() == 64);
            let short = ours["error"]["data"]["application_code"] == 6
                && ours["error"]["message"] == "MixIn count is too big";
            if (sent || short) && ours["id"] == e.request["body"]["id"] {
                let _ = writeln!(self.report, "NOTE {id}: {reason}; answered {ours}");
                self.passed += 1;
            } else {
                let line = format!("FAIL {id}: {reason}; answered {ours}");
                let _ = writeln!(self.report, "{line}");
                self.failures.push(line);
            }
            return;
        }

        let mut diffs = Vec::new();
        let expected = &e.response;
        if expected["status"].as_u64() != Some(u64::from(answer.status)) {
            diffs.push(format!(
                "status {} != {}",
                answer.status, expected["status"]
            ));
        }
        let captured_headers = expected["headers"].as_object().cloned().unwrap_or_default();
        for name in ["Server", "Connection", "Access-Control-Allow-Origin"] {
            let want = captured_headers.get(name).and_then(Value::as_str);
            let got = answer.headers.get(name).and_then(|v| v.to_str().ok());
            if want != got {
                diffs.push(format!("header {name}: {got:?} != {want:?}"));
            }
        }
        let captured_body = &expected["body"];
        match captured_body {
            Value::String(text) => {
                if answer.body != text.as_bytes() {
                    diffs.push(format!(
                        "body {:?} != {text:?}",
                        String::from_utf8_lossy(&answer.body)
                    ));
                }
            }
            _ => match serde_json::from_slice::<Value>(&answer.body) {
                Ok(ours) => {
                    if let Some(error) = ours.get("error")
                        && let (Some(code), Some(message)) = (
                            error["data"]["application_code"].as_i64(),
                            error["message"].as_str(),
                        )
                    {
                        self.codes.insert((code, message.to_owned()));
                    }
                    compare(
                        captured_body,
                        &ours,
                        "",
                        "body",
                        &mut self.bindings,
                        &mut diffs,
                    );
                    let exact = !has_holes(captured_body) && *captured_body == ours;
                    if exact {
                        let want = captured_headers
                            .get("Content-Length")
                            .and_then(Value::as_str)
                            .and_then(|v| v.parse::<usize>().ok());
                        if want.is_some_and(|w| w != answer.body.len()) {
                            diffs.push(format!("Content-Length {} != {want:?}", answer.body.len()));
                        }
                    }
                }
                Err(_) => diffs.push(format!(
                    "body is not JSON: {:?}",
                    String::from_utf8_lossy(&answer.body)
                )),
            },
        }

        match (diffs.is_empty(), deviation) {
            (true, None) => self.passed += 1,
            (true, Some(_)) => {
                let _ = writeln!(self.report, "NOTE {id}: listed as a deviation but matches");
                self.passed += 1;
            }
            (false, Some(reason)) => {
                let _ = writeln!(
                    self.report,
                    "DEVIATION {id}: {reason}: {}",
                    diffs.join("; ")
                );
            }
            (false, None) => {
                let line = format!(
                    "FAIL {id}: {}; answered {}",
                    diffs.join("; "),
                    String::from_utf8_lossy(&answer.body)
                );
                let _ = writeln!(self.report, "{line}");
                self.failures.push(line);
            }
        }
    }
}

/// The `amount`s of the transactions of the first block in the captured answer `name`, in
/// the captured order (the order of the transactions in that block).
fn captured_block_order(entries: &[Entry], name: &str) -> Vec<i64> {
    entries
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| e.response["body"]["result"]["items"][0]["transactions"].as_array())
        .map_or_else(Vec::new, |txs| {
            txs.iter().filter_map(|t| t["amount"].as_i64()).collect()
        })
}

/// Mines the wallet's pending transactions into the next block, ordered by the captured
/// amounts. A pending send to an own address carries another amount than once confirmed, so
/// the slots without a match take the remaining transactions in their order.
async fn mine_in_order(
    chain: &mut Chain,
    wallet: &Wallet<RouterTransport>,
    miner: &Address,
    order: &[i64],
) {
    let mut rest: Vec<(xkr_types::Hash, i64)> = wallet
        .unconfirmed(None)
        .expect("unconfirmed")
        .iter()
        .map(|t| (t.hash, t.total_amount()))
        .collect();
    let mut slots: Vec<Option<xkr_types::Hash>> = order
        .iter()
        .map(|amount| {
            let pos = rest.iter().position(|(_, a)| a == amount)?;
            Some(rest.remove(pos).0)
        })
        .collect();
    let mut leftover = rest.into_iter().map(|(h, _)| h);
    for slot in &mut slots {
        if slot.is_none() {
            *slot = leftover.next();
        }
    }
    let mut hashes: Vec<xkr_types::Hash> = slots.into_iter().flatten().collect();
    hashes.extend(leftover);
    let txs = chain.pool_txs(&hashes).await;
    chain.mine(miner, &txs);
}

/// A wallet on `chain` served with the capture's settings.
fn serve(wallet: &Arc<Wallet<RouterTransport>>, chain: &Chain, settings: RpcSettings) -> Router {
    router(Arc::new(Service::new(
        Arc::clone(wallet),
        chain.client.clone(),
        Network::Testnet,
        settings,
    )))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_cpp_fixtures() {
    // the engine's warnings (a node that refuses or does not answer) next to the report
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "xkr_wallet=warn,xkr_rpc=warn",
        ))
        .with_writer(std::io::stderr)
        .try_init();
    let dir = fixtures_dir();
    if !dir.join("methods.jsonl").exists() {
        eprintln!("no C++ fixtures in {}; skipped", dir.display());
        return;
    }
    let a = Address::parse(ADDRESS_A).unwrap();
    let k = Address::parse(ADDRESS_K).unwrap();
    let mut chain = Chain::new();
    chain.mine_many(&a, 30);
    chain.mine_many(&k, 100);

    let tmp = tempfile::tempdir().unwrap();
    let main_wallet = Arc::new(
        Wallet::import_seed(
            tmp.path().join("walletd-main.wallet"),
            "walletd-pass",
            SEED_A,
            0,
            chain.node(),
            wallet_config(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(main_wallet.primary_address(), ADDRESS_A);
    wait_synced(&main_wallet, chain.tip).await;
    let main = serve(
        &main_wallet,
        &chain,
        RpcSettings::with_password(RPC_PASSWORD, ""),
    );
    let legacy = serve(&main_wallet, &chain, RpcSettings::legacy(CORS));

    let mut files: Vec<String> = FILES.iter().map(|f| (*f).to_owned()).collect();
    let mut others: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| {
            Path::new(n)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))
                && !FILES.contains(&n.as_str())
                && !NOT_WALLETD.contains(&n.as_str())
        })
        .collect();
    others.sort();
    files.extend(others);

    let mut replay = Replay::default();
    let mut wallet = Arc::clone(&main_wallet);
    let mut current = main.clone();
    for file in &files {
        let entries = load(&dir.join(file));
        replay.bindings.clear();
        match file.as_str() {
            "transactions.jsonl" => {
                // the sends of send.jsonl and delayed.jsonl in block 131, in the captured
                // block order, then 11 more blocks
                let order = captured_block_order(&entries, "getTransactions/by-index-tx-block");
                mine_in_order(&mut chain, &main_wallet, &k, &order).await;
                chain.mine_many(&k, 11);
                // the capture asks by the hash of block 131 (`BH`); its placeholder is the
                // one in the request of `getTransactions/by-block-hash`
                if let Some(name) = entries
                    .iter()
                    .find(|e| e.name == "getTransactions/by-block-hash")
                    .and_then(|e| e.request["body"]["params"]["blockHash"].as_str())
                {
                    replay
                        .bindings
                        .insert(name.to_owned(), chain.hashes[131].to_hex());
                }
            }
            "save-export-reset.jsonl" => {
                chain.mine_pending(&main_wallet, &k).await;
                let to = 154u64.saturating_sub(chain.height);
                chain.mine_many(&k, to);
            }
            "legacy-cors.jsonl" => current = legacy.clone(),
            "tracking.jsonl" => {
                // a spending container first (`walletd-random.wallet`)
                let keys = WalletKeys::generate(&mut xkr_crypto::os_rng());
                let spending = Arc::new(
                    Wallet::import_keys(
                        tmp.path().join("walletd-random.wallet"),
                        "pässwörd ✓",
                        keys.spend_secret,
                        keys.view_secret,
                        0,
                        chain.node(),
                        wallet_config(),
                    )
                    .await
                    .unwrap(),
                );
                current = serve(
                    &spending,
                    &chain,
                    RpcSettings::with_password(RPC_PASSWORD, ""),
                );
                wallet = spending;
            }
            _ => current = main.clone(),
        }
        for e in &entries {
            if (file.as_str(), e.name.as_str())
                == ("addresses.jsonl", "getTransactions/empty-items-quirk")
            {
                // the sends to the subwallets in block 143
                wait_synced(&wallet, chain.tip).await;
                chain.mine_pending(&main_wallet, &k).await;
            }
            // a container without addresses does not sync (`deleteAddress` of the last one)
            if wallet.subwallet_count() > 0 {
                wait_synced(&wallet, chain.tip).await;
            }
            replay.run(&current, file, e).await;
        }
    }

    // the application codes the C++ service answered with
    if let Ok(text) = std::fs::read_to_string(dir.join("application-codes.json")) {
        let listed: Vec<Value> = serde_json::from_str(&text).unwrap_or_default();
        for c in listed {
            if let (Some(code), Some(message)) =
                (c["application_code"].as_i64(), c["message"].as_str())
                && !replay.codes.contains(&(code, message.to_owned()))
            {
                let _ = writeln!(
                    replay.report,
                    "NOTE application code {code} {message:?} (first {}) not answered in this run",
                    c["first"]
                );
            }
        }
    }

    println!(
        "replayed {} answers exactly or by shape\n{}",
        replay.passed, replay.report
    );
    assert!(
        replay.failures.is_empty(),
        "{} answers differ from the C++ service:\n{}",
        replay.failures.len(),
        replay.report
    );
}

#[tokio::test]
async fn generate_like_the_cpp_service() {
    let dir = fixtures_dir().join("generate");
    let exit_codes: HashMap<String, i64> = std::fs::read_to_string(dir.join("exit-codes.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if exit_codes.is_empty() {
        eprintln!("no generate fixtures in {}; skipped", dir.display());
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let config = |name: &str| {
        let mut c = Config::defaults(Network::Testnet);
        c.container_file = tmp.path().join(name).to_str().unwrap().to_owned();
        c.container_password = "walletd-pass".into();
        c.generate = true;
        c
    };
    let exit_code = |r: &Result<Generated, String>| i64::from(r.is_err());

    // seed A: the address C++ logged
    let mut c = config("seed-a.wallet");
    c.mnemonic_seed = SEED_A.into();
    let r = container::generate(&c).await;
    assert_eq!(r, Ok(Generated::Created(ADDRESS_A.to_owned())));
    if let Some(code) = exit_codes.get("seed-a") {
        assert_eq!(exit_code(&r), *code);
    }
    if let Ok(log) = std::fs::read_to_string(dir.join("seed-a.txt")) {
        assert!(log.contains(&format!("New wallet added {ADDRESS_A}")));
    }
    // the same file again
    let r = container::generate(&c).await;
    assert!(r.is_err());
    if let Some(code) = exit_codes.get("file-exists") {
        assert_eq!(exit_code(&r), *code);
    }
    // a bad checksum word, a view key alone, a malformed spend key: logged, exit code 0
    let mut c = config("bad-seed.wallet");
    c.mnemonic_seed = SEED_A.replace("guest history", "guest guest");
    let r = container::generate(&c).await;
    assert!(matches!(r, Ok(Generated::Refused(_))), "{r:?}");
    if let Some(code) = exit_codes.get("bad-seed-checksum") {
        assert_eq!(exit_code(&r), *code);
    }
    let mut c = config("view-only.wallet");
    c.view_key = "11".repeat(32);
    let r = container::generate(&c).await;
    assert_eq!(
        r,
        Ok(Generated::Refused(
            "Need both secret spend key and secret view key.".to_owned()
        ))
    );
    if let Some(code) = exit_codes.get("view-key-only") {
        assert_eq!(exit_code(&r), *code);
    }
    c.spend_key = "not hex".into();
    let r = container::generate(&c).await;
    assert_eq!(r, Ok(Generated::Refused("Invalid spend key".to_owned())));
    if let Some(code) = exit_codes.get("bad-spend-key") {
        assert_eq!(exit_code(&r), *code);
    }
}

#[test]
fn placeholders() {
    let mut b = Bindings::new();
    let hex = "ab".repeat(32);
    assert!(match_text("<tx:1>", &hex, &mut b).is_ok());
    assert_eq!(b["<tx:1>"], hex);
    assert!(match_text("<tx:1>", &"cd".repeat(32), &mut b).is_err());
    assert!(
        match_text(
            &format!("01<txpub:1>02{}", "00"),
            &format!("01{hex}0200"),
            &mut b
        )
        .is_ok()
    );
    assert!(match_text("<address:1>", ADDRESS_A, &mut b).is_ok());
    assert!(is_wildcard("<n>") && is_wildcard("<change>") && !is_wildcard("<tx:1>"));
    assert_eq!(parts("a<b"), vec![Part::Text("a<b".into())]);
    assert_eq!(
        substitute(&Value::String("<tx:1>".into()), &b).unwrap(),
        Value::String(hex.clone())
    );
    assert_eq!(
        substitute(&Value::String("<tx:9>".into()), &b),
        Err("<tx:9>".to_owned())
    );
}
