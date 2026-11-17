//! Replays the answers of the C++ wallet-api recorded by `testnet/fixtures/capture-wallet-api.sh`
//! (`tests/fixtures/cpp/*.jsonl`, lines `{name, request, response}` normalised by
//! `testnet/fixtures/lib-wallet.sh` `fx_normalise`) against this server, with wallets on an
//! in-process xkrd testnet chain built the way the capture script builds xkrd "X".
//!
//! The test is skipped when the directory has no `.jsonl` files.
//!
//! How it replays:
//! * The capture script's order is replayed ([`PLAN`]): the groups run in script order, the
//!   chain is mined at the same points (blocks 1..30 pay wallet A, filler to 130, 12 blocks after
//!   sends), wallets are waited for at the same points (`fx_api_wait_synced`), and the wallet
//!   files the script prepares (`not-a-wallet.wallet`, truncated and corrupted copies of
//!   `api-a.wallet`) are made at the same points. Heights, rewards and the fixed keys are those of
//!   the capture run, so most answers compare exactly.
//! * Wallet files under `/w/` live in a temporary directory; requests named `cors/…` and
//!   `options/cors/…` go to a second server with `--enable-cors https://fixture.example`.
//! * Placeholders (`<tx:3>`, `<address:1>`, …) stand for run-specific values. Each file has its
//!   own numbering, as `fx_normalise` numbers per file: a placeholder is bound to the value in
//!   this server's answer where it first appears, must stay the same value afterwards, and is
//!   substituted into later request paths and bodies. A placeholder first seen in a request gets
//!   a fresh value of its kind, replaced when an answer shows the real one. `<timestamp>`,
//!   `<peers>`, `<n>` (answers named `…~shape`) match any number.
//! * Arrays may come in another order (C++ keeps subwallets and transfers in hash maps, and the
//!   order of transactions within a block depends on their hashes).
//! * Checked per answer: the status; `Content-Type`, `Allow` and the `Access-Control-*` headers;
//!   the body with the placeholders bound; that this server's body is the nlohmann `dump(4)`
//!   text; and, for bodies without placeholders, that the C++ `Content-Length` equals the
//!   length of the `dump(4)` text of the recorded body (so the formatting matches the C++ bytes).
//! * Where an exact replay is not possible only the status (and `errorCode`) is compared:
//!   entries in [`STATUS_ONLY`] and files the plan does not know.
//! * Request values the script derived from others are set in [`PRESETS`]; requests this
//!   server answers differently on purpose are sent as in [`REQUEST_EDITS`] or left out
//!   ([`NOT_SENT`]), and recorded values that differ for a stated reason are adjusted in
//!   [`RECORDED_EDITS`]; balances in [`TOTALS`] are compared as `locked + unlocked`.
//! * Sends whose recorded outcome follows from the random choice of inputs are sent again
//!   while this server's choice fails for that reason ([`RETRIED`]), or left out when the
//!   recorded choice failed and a success here would change the chain ([`NOT_SENT`]).
//! * Calls the script makes without recording them (closing the wallet) and rescans after a
//!   subwallet import are steps of [`PLAN`].

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Api, CORS, DUMMY_ADDR, SEED_A, TestNode};
use serde_json::Value;
use xkr_crypto::{generate_keys, os_rng};
use xkr_wallet::validate::create_integrated_address;
use xkr_wallet_api::respond::dump;
use xkr_wallet_core::{WalletKeys, mnemonic_from_secret, secret_from_mnemonic};

/// The fixture directory.
fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cpp")
}

/// Response headers compared (case-insensitive names).
const HEADERS: &[&str] = &[
    "content-type",
    "allow",
    "access-control-allow-origin",
    "access-control-allow-methods",
    "access-control-allow-headers",
];

/// Entries whose body cannot match the C++ answer, with the reason; their status (and
/// `errorCode`) is compared.
const STATUS_ONLY: &[(&str, &str, &str)] = &[
    (
        "transactions-pending",
        "unconfirmed/for-address-a",
        "this server lists the unconfirmed transactions of the address",
    ),
    (
        "send",
        "advanced/mixin-500",
        "the message names an amount of the randomly chosen inputs",
    ),
    (
        "node",
        "balance/after-reset",
        "balances after a rescan of a wallet that has sent transactions differ from the C++ build",
    ),
    (
        "node",
        "balances/after-reset-from-fund-end",
        "balances after a rescan of a wallet that has sent transactions differ from the C++ build",
    ),
    (
        "node",
        "transactions/after-reset-from-fund-end~shape",
        "this server lists what the rescan from height 30 finds",
    ),
    (
        "view-wallet",
        "allowed/balance",
        "the view wallet's amounts are change outputs, which depend on the randomly chosen inputs",
    ),
    (
        "view-wallet",
        "allowed/balances",
        "the view wallet's amounts are change outputs, which depend on the randomly chosen inputs",
    ),
    (
        "view-wallet",
        "allowed/transactions",
        "the view wallet's amounts are change outputs, which depend on the randomly chosen inputs",
    ),
    (
        "wallet-b",
        "transactions",
        "this server's wallet B also lists the transfer of 200000 with payment ID 0123…",
    ),
];

/// Entries whose balances are compared as `locked + unlocked` per object: which outputs are
/// still locked depends on the randomly chosen inputs (the change of a transfer with an unlock
/// time is locked with it), and this server counts the change of the payment-ID transfer as
/// unlocked once it is in a block.
const TOTALS: &[(&str, &str)] = &[
    ("transactions", "balance"),
    ("transactions", "balance/a"),
    ("transactions", "balances"),
];

/// Requests not sent, with the reason: `(file, name, reason)`.
const NOT_SENT: &[(&str, &str, &str)] = &[
    (
        "send",
        "fusion/advanced",
        "the daemon refused the recorded fusion transaction (DAEMON_ERROR); this server's is \
         accepted, so the request is left out to keep the chain as in the recorded run",
    ),
    (
        "wallet-k",
        "send/basic-to-a",
        "the inputs are chosen at random: C++ took K's output of 300000, which has only two \
         outputs of its amount on the fixture chain, and answered NOT_ENOUGH_FAKE_OUTPUTS; most \
         other choices have enough ring members, and the transfer this server would then send \
         would change the lists and balances recorded after it",
    ),
];

/// Requests sent again while this server answers 400 with the given `errorCode`, with the
/// reason: `(file, name, errorCode, reason)`. At most [`RETRY_ATTEMPTS`] attempts.
const RETRIED: &[(&str, &str, i64, &str)] = &[
    (
        "send",
        "fusion/basic",
        36,
        "the fusion inputs come from one power-of-ten group chosen at random; wallet A's \
         one-digit group holds only outputs below the fusion dust threshold (10 at these \
         heights), so that choice is FULLY_OPTIMIZED (36); the recorded C++ choice was \
         another group, so it is sent again",
    ),
    (
        "send-change",
        "advanced/change-to-sub1",
        28,
        "the inputs are chosen at random, and at this point wallet A holds fusion and change \
     outputs whose amounts have fewer than four outputs on the fixture chain; a choice with \
     one of them is NOT_ENOUGH_FAKE_OUTPUTS (28). The recorded C++ choice was not, and K's \
     history (wallet-k) holds the transfer, so it is sent until a choice has enough ring \
     members",
    ),
];

/// Attempts at most for an entry of [`RETRIED`].
const RETRY_ATTEMPTS: usize = 40;

/// The `errorCode` of an error answer.
fn error_code(text: &str) -> Option<i64> {
    serde_json::from_str::<Value>(text).ok()?["errorCode"].as_i64()
}

/// Request values the capture script derived from other values (so the fixture shows only a
/// placeholder): `(file, placeholder, value)`.
#[derive(Debug, Clone, Copy)]
enum Preset {
    /// `${A:0:98}` plus `a` (or `b` when A ends in `a`): wallet A's address with a wrong
    /// checksum.
    TypoOfA,
}

const PRESETS: &[(&str, &str, Preset)] = &[("send", "<address:1>", Preset::TypoOfA)];

/// Requests sent without a JSON key, with the reason: `(file, name, key, reason)`.
const REQUEST_EDITS: &[(&str, &str, &str, &str)] = &[(
    "send",
    "advanced/payment-id-not-hex",
    "paymentID",
    "payment IDs must be 64 hex characters here (PAYMENT_ID_INVALID, tests/api.rs); the transfer \
     is sent without the ID so the chain stays as in the recorded run",
)];

/// Recorded values replaced before comparing (`name` `*`: every entry of the file), as JSON
/// texts: `(file, name, from, to, reason)`.
const RECORDED_EDITS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "transactions-pending",
        "unconfirmed~shape",
        r#""zz23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef""#,
        r#""""#,
        "the transfer of REQUEST_EDITS is sent without a payment ID",
    ),
    (
        "transactions",
        "*",
        "-7001000",
        "-200010",
        "the node now returns the outputs of transactions whose extra starts with a payment ID \
         (the recording node did not), so this server's wallet A finds the change of the \
         transfer with payment ID 0123… and its net amount is -(200000 + 10)",
    ),
    (
        "wallet-b",
        "balance",
        "1410000",
        "1610000",
        "the node now returns the outputs of transactions whose extra starts with a payment ID \
         (the recording node did not), so this server's wallet B also finds the transfer of \
         200000 with payment ID 0123…",
    ),
    (
        "wallet-b",
        "balances/one-subwallet",
        "1410000",
        "1610000",
        "the node now returns the outputs of transactions whose extra starts with a payment ID \
         (the recording node did not), so this server's wallet B also finds the transfer of \
         200000 with payment ID 0123…",
    ),
];

fn replace_values(v: &Value, from: &Value, to: &Value) -> Value {
    if v == from {
        return to.clone();
    }
    match v {
        Value::Array(a) => Value::Array(a.iter().map(|x| replace_values(x, from, to)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), replace_values(x, from, to)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Where `actual` first departs from `recorded` (placeholders match anything; arrays in order).
fn first_difference(recorded: &Value, actual: &Value, at: &str) -> String {
    match (recorded, actual) {
        (Value::String(r), _) if has_placeholder(recorded) => {
            if actual.is_string() || parts(r) == [Part::Wild] {
                String::new()
            } else {
                format!("{at}: {actual} (C++ {recorded})")
            }
        }
        (Value::Array(r), Value::Array(a)) => {
            if r.len() != a.len() {
                return format!("{at}: {} entries (C++ {})", a.len(), r.len());
            }
            r.iter()
                .zip(a)
                .enumerate()
                .map(|(i, (x, y))| first_difference(x, y, &format!("{at}[{i}]")))
                .find(|d| !d.is_empty())
                .unwrap_or_default()
        }
        (Value::Object(r), Value::Object(a)) => {
            for (k, x) in r {
                let Some(y) = a.get(k) else {
                    return format!("{at}.{k}: missing");
                };
                let d = first_difference(x, y, &format!("{at}.{k}"));
                if !d.is_empty() {
                    return d;
                }
            }
            a.keys()
                .find(|k| !r.contains_key(*k))
                .map_or_default(|k| format!("{at}.{k}: not in the C++ answer"))
        }
        _ if recorded == actual => String::new(),
        _ => format!("{at}: {actual} (C++ {recorded})"),
    }
}

/// Every object with numeric `locked` and `unlocked` becomes `{"total": locked + unlocked}`
/// (other keys kept).
fn totals(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(totals).collect()),
        Value::Object(m) => {
            let mut out: serde_json::Map<String, Value> =
                m.iter().map(|(k, x)| (k.clone(), totals(x))).collect();
            if let (Some(l), Some(u)) = (
                m.get("locked").and_then(Value::as_u64),
                m.get("unlocked").and_then(Value::as_u64),
            ) {
                out.remove("locked");
                out.remove("unlocked");
                out.insert("total".to_owned(), Value::from(l + u));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// One recorded exchange.
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
    status: u16,
    response_headers: Vec<(String, String)>,
    response_body: Value,
}

fn header_list(v: &Value) -> Vec<(String, String)> {
    v.as_object().map_or_default(|m| {
        m.iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
            .collect()
    })
}

fn find_header<'a>(list: &'a [(String, String)], name: &str) -> Option<&'a str> {
    list.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn parse_entry(line: &str) -> Entry {
    let v: Value = serde_json::from_str(line).expect("fixture line is JSON");
    let req = &v["request"];
    let resp = &v["response"];
    Entry {
        name: v["name"].as_str().unwrap_or_default().to_owned(),
        method: req["method"].as_str().unwrap_or("GET").to_owned(),
        path: req["path"].as_str().unwrap_or("/").to_owned(),
        headers: header_list(&req["headers"]),
        body: req["body"].clone(),
        status: resp["status"].as_u64().unwrap_or(0) as u16,
        response_headers: header_list(&resp["headers"]),
        response_body: resp["body"].clone(),
    }
}

/// Every `.jsonl` file by stem.
fn load(dir: &Path) -> BTreeMap<String, Vec<Entry>> {
    let mut out = BTreeMap::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for item in read.flatten() {
        let path = item.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let stem = path
            .file_stem()
            .map_or_default(|s| s.to_string_lossy().into_owned());
        let text = std::fs::read_to_string(&path).expect("fixture file");
        let entries = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(parse_entry)
            .collect();
        out.insert(stem, entries);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// placeholders

/// A part of a recorded string.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Lit(String),
    /// `<kind:N>`.
    Named(String),
    /// `<timestamp>`, `<peers>`, `<n>`, `<change>`, `<hexdata>`.
    Wild,
}

/// The token at the start of `s` (`<kind:N>` or `<word>`) and its length.
fn token(s: &str) -> Option<(Part, usize)> {
    let inner = s.strip_prefix('<')?;
    let end = inner.find('>')?;
    let body = &inner[..end];
    let (kind, number) = match body.split_once(':') {
        Some((k, n)) => (k, Some(n)),
        None => (body, None),
    };
    if kind.is_empty() || !kind.bytes().all(|c| c.is_ascii_lowercase()) {
        return None;
    }
    match number {
        Some(n) if !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()) => {
            Some((Part::Named(format!("<{body}>")), end + 2))
        }
        Some(_) => None,
        None => Some((Part::Wild, end + 2)),
    }
}

fn parts(s: &str) -> Vec<Part> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < s.len() {
        if let Some((part, len)) = token(&s[i..]) {
            if !lit.is_empty() {
                out.push(Part::Lit(std::mem::take(&mut lit)));
            }
            out.push(part);
            i += len;
        } else {
            let c = s[i..].chars().next().expect("char");
            lit.push(c);
            i += c.len_utf8();
        }
    }
    if !lit.is_empty() {
        out.push(Part::Lit(lit));
    }
    out
}

fn has_placeholder(v: &Value) -> bool {
    match v {
        Value::String(s) => parts(s).iter().any(|p| !matches!(p, Part::Lit(_))),
        Value::Array(a) => a.iter().any(has_placeholder),
        Value::Object(m) => m.values().any(has_placeholder),
        _ => false,
    }
}

#[derive(Debug, Clone)]
struct Bound {
    value: String,
    provisional: bool,
}

/// Placeholder bindings of one fixture file.
#[derive(Debug, Clone, Default)]
struct Bindings {
    by_name: HashMap<String, Bound>,
    by_value: HashMap<String, String>,
}

impl Bindings {
    fn firm(&self, name: &str) -> Option<&str> {
        self.by_name
            .get(name)
            .filter(|b| !b.provisional)
            .map(|b| b.value.as_str())
    }

    fn bind(&mut self, name: &str, value: &str) -> bool {
        if let Some(current) = self.by_name.get(name) {
            if current.value == value {
                return true;
            }
            if !current.provisional {
                return false;
            }
        }
        if self.by_value.get(value).is_some_and(|other| other != name) {
            return false;
        }
        if let Some(old) = self.by_name.remove(name) {
            self.by_value.remove(&old.value);
        }
        self.by_name.insert(
            name.to_owned(),
            Bound {
                value: value.to_owned(),
                provisional: false,
            },
        );
        self.by_value.insert(value.to_owned(), name.to_owned());
        true
    }

    /// The value to send for `name`: the bound one, or a fresh one of its kind.
    fn value_for(&mut self, name: &str) -> String {
        if let Some(b) = self.by_name.get(name) {
            return b.value.clone();
        }
        let kind = name
            .trim_start_matches('<')
            .split(':')
            .next()
            .unwrap_or_default();
        let fresh = || WalletKeys::generate(&mut os_rng()).address().to_string();
        let value = match kind {
            "address" => fresh(),
            "integrated" => {
                create_integrated_address(&fresh(), &"00".repeat(32)).expect("integrated address")
            }
            "mnemonic" => mnemonic_from_secret(&generate_keys(&mut os_rng()).1),
            _ => generate_keys(&mut os_rng()).0.to_hex(),
        };
        self.by_name.insert(
            name.to_owned(),
            Bound {
                value: value.clone(),
                provisional: true,
            },
        );
        self.by_value.insert(value.clone(), name.to_owned());
        value
    }
}

/// A recorded request string with the placeholders replaced and `/w/` moved to `wdir`.
fn substitute(s: &str, b: &mut Bindings, wdir: &Path) -> String {
    let mut out = String::new();
    for part in parts(s) {
        match part {
            Part::Lit(l) => out.push_str(&l),
            Part::Named(n) => out.push_str(&b.value_for(&n)),
            Part::Wild => out.push('0'),
        }
    }
    match out.strip_prefix("/w/") {
        Some(rest) => wdir.join(rest).to_string_lossy().into_owned(),
        None => out,
    }
}

fn substitute_value(v: &Value, b: &mut Bindings, wdir: &Path) -> Value {
    match v {
        Value::String(s) => Value::String(substitute(s, b, wdir)),
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute_value(x, b, wdir)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), substitute_value(x, b, wdir)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Matches `s` against recorded `parts`, binding placeholders.
fn match_parts(parts: &[Part], s: &str, b: &mut Bindings) -> bool {
    let Some((first, rest)) = parts.split_first() else {
        return s.is_empty();
    };
    if let Part::Lit(l) = first {
        return s
            .strip_prefix(l.as_str())
            .is_some_and(|r| match_parts(rest, r, b));
    }
    if let Part::Named(n) = first
        && let Some(v) = b.firm(n).map(str::to_owned)
    {
        return s
            .strip_prefix(v.as_str())
            .is_some_and(|r| match_parts(rest, r, b));
    }
    let ends: Vec<usize> = match rest.first() {
        None => vec![s.len()],
        Some(Part::Lit(l)) => s
            .match_indices(l.as_str())
            .map(|(i, _)| i)
            .filter(|i| *i > 0)
            .collect(),
        Some(_) => s.char_indices().map(|(i, _)| i).skip(1).collect(),
    };
    for end in ends {
        let mut trial = b.clone();
        let bound = match first {
            Part::Named(n) => trial.bind(n, &s[..end]),
            _ => true,
        };
        if bound && match_parts(rest, &s[end..], &mut trial) {
            *b = trial;
            return true;
        }
    }
    false
}

/// Whether this server's `actual` value matches the recorded `expected` one.
fn unify(expected: &Value, actual: &Value, b: &mut Bindings) -> bool {
    match (expected, actual) {
        (Value::String(e), _) if !matches!(actual, Value::String(_)) => {
            // a whole-value wildcard (`<timestamp>`, `<n>`, ...) stands for a number
            parts(e) == [Part::Wild] && actual.is_number()
        }
        (Value::String(e), Value::String(a)) => match_parts(&parts(e), a, b),
        (Value::Array(e), Value::Array(a)) => unify_arrays(e, a, b),
        (Value::Object(e), Value::Object(a)) => {
            e.len() == a.len()
                && e.iter()
                    .all(|(k, ev)| a.get(k).is_some_and(|av| unify(ev, av, b)))
        }
        _ => expected == actual,
    }
}

fn unify_arrays(e: &[Value], a: &[Value], b: &mut Bindings) -> bool {
    if e.len() != a.len() {
        return false;
    }
    let mut trial = b.clone();
    if e.iter().zip(a).all(|(x, y)| unify(x, y, &mut trial)) {
        *b = trial;
        return true;
    }
    let mut used = vec![false; a.len()];
    assign(e, a, 0, &mut used, b)
}

fn assign(e: &[Value], a: &[Value], i: usize, used: &mut [bool], b: &mut Bindings) -> bool {
    if i == e.len() {
        return true;
    }
    for (j, item) in a.iter().enumerate() {
        if used[j] {
            continue;
        }
        let mut trial = b.clone();
        if unify(&e[i], item, &mut trial) {
            used[j] = true;
            if assign(e, a, i + 1, used, &mut trial) {
                *b = trial;
                return true;
            }
            used[j] = false;
        }
    }
    false
}

// ---------------------------------------------------------------------------------------------
// the plan

#[derive(Debug, Clone, Copy)]
enum Hook {
    /// `printf 'this is not a wallet file\n' > not-a-wallet.wallet`.
    NotAWallet,
    /// `api-a-truncated.wallet` (16 bytes short) and `api-a-corrupted.wallet` (middle byte
    /// flipped).
    DamagedCopies,
    /// `fx_mine A 30`, `fx_mine dummy 130`.
    Fund,
    /// `fx_api_wait_synced`.
    WaitSynced,
    /// `fx_confirm`: 12 blocks to the filler address.
    Confirm,
    /// `fx_api_q DELETE /wallet` (not recorded).
    CloseWallet,
}

#[derive(Debug, Clone, Copy)]
enum Step {
    /// Entries of a file up to and including the named one (`None`: to the end).
    Run(&'static str, Option<&'static str>),
    Hook(Hook),
}

use Hook::{CloseWallet, Confirm, DamagedCopies, Fund, NotAWallet, WaitSynced};
use Step::{Hook as Do, Run};

/// The order of `capture-wallet-api.sh`.
const PLAN: &[Step] = &[
    Do(NotAWallet),
    Run("auth", None),
    Run("state", None),
    Run("wallet", Some("import/key/not-reduced")),
    Do(CloseWallet),
    Run("wallet", Some("import/view/bad-address")),
    Do(CloseWallet),
    Run("wallet", Some("status/right-after-import~shape")),
    Run("state-open", None),
    Run("wallet", Some("close/again")),
    Do(DamagedCopies),
    Run("wallet", Some("create/already-exists")),
    Do(Fund),
    Run("wallet", Some("status/during-sync~shape")),
    Do(WaitSynced),
    Run("wallet", None),
    Run("status", None),
    // the import rescans the wallet from block 0; C++ answered the next requests with the
    // balances it had before the import, this server once the rescan is done
    Run("addresses", Some("import/sub1")),
    Do(WaitSynced),
    Run("addresses", None),
    Run("send", None),
    Run("transactions-pending", None),
    Do(Confirm),
    Do(WaitSynced),
    Run("transactions", None),
    Run("send-change", None),
    Run("node", Some("status/after-reset~shape")),
    Do(WaitSynced),
    Run("node", Some("reset/scan-height")),
    Do(WaitSynced),
    Run("node", Some("transactions/after-reset-from-fund-end~shape")),
    Do(Confirm),
    Do(WaitSynced),
    Run("node", None),
    Run("wallet-k", Some("open/k")),
    Do(WaitSynced),
    Run("wallet-k", Some("send/basic-to-a")),
    Do(Confirm),
    Do(WaitSynced),
    Run("wallet-k", None),
    Run("view-wallet", Some("open")),
    Do(WaitSynced),
    Run("view-wallet", None),
    Run("wallet-b", Some("open/empty-password")),
    Do(WaitSynced),
    Run("wallet-b", None),
];

struct Replay {
    node: TestNode,
    plain: Api,
    cors: Api,
    wdir: tempfile::TempDir,
    files: BTreeMap<String, Vec<Entry>>,
    cursor: HashMap<String, usize>,
    bindings: HashMap<String, Bindings>,
    mismatches: Vec<String>,
    notes: Vec<String>,
    checked: usize,
    skipped: usize,
    a_address: String,
}

fn excerpt(s: &str) -> String {
    const MAX: usize = 1500;
    if s.len() <= MAX {
        return s.to_owned();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

impl Replay {
    async fn hook(&mut self, hook: Hook) {
        let w = self.wdir.path();
        match hook {
            Hook::NotAWallet => {
                std::fs::write(
                    w.join("not-a-wallet.wallet"),
                    b"this is not a wallet file\n",
                )
                .expect("write");
            }
            Hook::DamagedCopies => match std::fs::read(w.join("api-a.wallet")) {
                Ok(bytes) => {
                    let short = &bytes[..bytes.len().saturating_sub(16)];
                    std::fs::write(w.join("api-a-truncated.wallet"), short).expect("write");
                    let mut flipped = bytes.clone();
                    let mid = flipped.len() / 2;
                    if let Some(byte) = flipped.get_mut(mid) {
                        *byte ^= 0xff;
                    }
                    std::fs::write(w.join("api-a-corrupted.wallet"), flipped).expect("write");
                }
                Err(e) => self.notes.push(format!("no api-a.wallet to damage: {e}")),
            },
            Hook::Fund => {
                self.node.mine_to(&self.a_address, 30).await;
                self.node.mine_to(DUMMY_ADDR, 130).await;
            }
            Hook::WaitSynced => {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
                loop {
                    if self.plain.synced(&self.node).await {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        break;
                    }
                    if tokio::time::Instant::now() > deadline {
                        let status = self.plain.get("/status").await;
                        self.mismatches.push(format!(
                            "wait for sync at top {}: {} {}",
                            self.node.top(),
                            status.status,
                            status.text
                        ));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
            Hook::Confirm => self.node.mine(DUMMY_ADDR, 12).await,
            Hook::CloseWallet => {
                self.plain.app.close_wallet().await;
            }
        }
    }

    async fn run(&mut self, file: &str, until: Option<&str>) {
        let Some(entries) = self.files.get(file).cloned() else {
            self.notes.push(format!("{file}.jsonl is missing"));
            return;
        };
        let start = self.cursor.get(file).copied().unwrap_or(0);
        if let Some(name) = until
            && !entries[start..].iter().any(|e| e.name == name)
        {
            self.notes.push(format!(
                "{file}: no entry {name:?} after #{start}; ran to the end"
            ));
        }
        let mut i = start;
        while i < entries.len() {
            let entry = &entries[i];
            i += 1;
            let status_only = !PLAN
                .iter()
                .any(|s| matches!(s, Step::Run(f, _) if *f == file))
                || STATUS_ONLY
                    .iter()
                    .any(|(f, n, _)| *f == file && *n == entry.name);
            self.replay(file, entry, status_only).await;
            if until.is_some_and(|n| n == entry.name) {
                break;
            }
        }
        self.cursor.insert(file.to_owned(), i);
    }

    async fn replay(&mut self, file: &str, e: &Entry, status_only: bool) {
        if e.status == 0 || NOT_SENT.iter().any(|(f, n, _)| *f == file && *n == e.name) {
            self.skipped += 1;
            return;
        }
        self.checked += 1;
        let wdir = self.wdir.path().to_path_buf();
        if !self.bindings.contains_key(file) {
            let mut fresh = Bindings::default();
            for (f, name, preset) in PRESETS {
                if *f == file {
                    let value = match preset {
                        Preset::TypoOfA => {
                            let a = &self.a_address;
                            let last = if a[98..].starts_with('a') { "b" } else { "a" };
                            format!("{}{last}", &a[..98])
                        }
                    };
                    fresh.bind(name, &value);
                }
            }
            self.bindings.insert(file.to_owned(), fresh);
        }
        let b = self.bindings.entry(file.to_owned()).or_default();
        let path = substitute(&e.path, b, &wdir);
        let headers: Vec<(String, String)> = e
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), substitute(v, b, &wdir)))
            .collect();
        let mut request_body = e.body.clone();
        for (f, name, key, _) in REQUEST_EDITS {
            if *f == file
                && *name == e.name
                && let Some(map) = request_body.as_object_mut()
            {
                map.remove(*key);
            }
        }
        let body = match &request_body {
            Value::Null => None,
            Value::String(s) => Some(substitute(s, b, &wdir)),
            other => Some(substitute_value(other, b, &wdir).to_string()),
        };
        let api = if e.name.starts_with("cors/") || e.name.starts_with("options/cors/") {
            &self.cors
        } else {
            &self.plain
        };
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut answer = api
            .request(&e.method, &path, &header_refs, body.as_deref())
            .await;
        if let Some((_, _, code, _)) = RETRIED
            .iter()
            .find(|(f, n, _, _)| *f == file && *n == e.name)
        {
            let mut attempts = 1;
            while attempts < RETRY_ATTEMPTS
                && answer.status == 400
                && error_code(&answer.text) == Some(*code)
            {
                answer = api
                    .request(&e.method, &path, &header_refs, body.as_deref())
                    .await;
                attempts += 1;
            }
            if attempts > 1 {
                self.notes
                    .push(format!("{file}: {} sent {attempts} times", e.name));
            }
        }
        let what = format!("{file}: {} ({} {path})", e.name, e.method);
        if answer.status != e.status {
            self.mismatches.push(format!(
                "{what}: status {} (C++ {}); body {}; C++ body {}",
                answer.status,
                e.status,
                excerpt(&answer.text),
                excerpt(&e.response_body.to_string())
            ));
            return;
        }
        let actual: Value = if answer.text.is_empty() {
            Value::String(String::new())
        } else {
            serde_json::from_str(&answer.text)
                .unwrap_or_else(|_| Value::String(answer.text.clone()))
        };
        let mut recorded = e.response_body.clone();
        for (f, name, from, to, _) in RECORDED_EDITS {
            if *f == file && (*name == "*" || *name == e.name) {
                let from: Value = serde_json::from_str(from).expect("edit JSON");
                let to: Value = serde_json::from_str(to).expect("edit JSON");
                recorded = replace_values(&recorded, &from, &to);
            }
        }
        let compared = if TOTALS.iter().any(|(f, n)| *f == file && *n == e.name) {
            recorded = totals(&recorded);
            totals(&actual)
        } else {
            actual.clone()
        };
        let b = self.bindings.entry(file.to_owned()).or_default();
        if status_only {
            let (ours, theirs) = (&actual["errorCode"], &e.response_body["errorCode"]);
            if ours != theirs {
                self.mismatches
                    .push(format!("{what}: errorCode {ours} (C++ {theirs})"));
            }
            return;
        }
        for name in HEADERS {
            let theirs = find_header(&e.response_headers, name);
            let ours = answer.header(name);
            if theirs != ours {
                self.mismatches
                    .push(format!("{what}: header {name}: {ours:?} (C++ {theirs:?})"));
            }
        }
        if !unify(&recorded, &compared, b) {
            self.mismatches.push(format!(
                "{what}: first difference {}; body {} (C++ {})",
                first_difference(&recorded, &compared, "$"),
                excerpt(&answer.text),
                excerpt(&recorded.to_string())
            ));
            return;
        }
        if !answer.text.is_empty() && answer.text != dump(&actual) {
            self.mismatches.push(format!(
                "{what}: not dump(4) text: {:?}",
                excerpt(&answer.text)
            ));
        }
        let recorded_length = find_header(&e.response_headers, "content-length")
            .and_then(|l| l.parse::<usize>().ok());
        let exact = !has_placeholder(&e.response_body) && !e.name.ends_with("~shape");
        if exact
            && !matches!(&e.response_body, Value::String(s) if s.is_empty())
            && let Some(length) = recorded_length
            && length != dump(&e.response_body).len()
        {
            self.mismatches.push(format!(
                "{what}: C++ Content-Length {length}, dump(4) of the recorded body is {}",
                dump(&e.response_body).len()
            ));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn replay_cpp_wallet_api_fixtures() {
    // the engine's warnings (a node that refuses or does not answer) next to the report
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "xkr_wallet=warn,xkr_rpc=warn",
        ))
        .with_writer(std::io::stderr)
        .try_init();
    let files = load(&fixture_dir());
    if files.is_empty() {
        eprintln!(
            "skipped: no C++ wallet-api fixtures in {} (testnet/fixtures/capture-wallet-api.sh writes them)",
            fixture_dir().display()
        );
        return;
    }
    let spend = secret_from_mnemonic(SEED_A).expect("seed A");
    let a_address = WalletKeys::from_spend_secret(&spend)
        .expect("seed A keys")
        .address()
        .to_string();
    let node = TestNode::new();
    let plain = Api::new(&node, None);
    let cors = Api::new(&node, Some(CORS));
    let mut replay = Replay {
        node,
        plain,
        cors,
        wdir: tempfile::tempdir().expect("tempdir"),
        files,
        cursor: HashMap::new(),
        bindings: HashMap::new(),
        mismatches: Vec::new(),
        notes: Vec::new(),
        checked: 0,
        skipped: 0,
        a_address,
    };
    for step in PLAN {
        match *step {
            Step::Run(file, until) => replay.run(file, until).await,
            Step::Hook(hook) => replay.hook(hook).await,
        }
    }
    // files the plan does not know: replayed afterwards, status and errorCode only
    let unknown: Vec<String> = replay
        .files
        .keys()
        .filter(|f| {
            !PLAN
                .iter()
                .any(|s| matches!(s, Step::Run(p, _) if *p == f.as_str()))
        })
        .cloned()
        .collect();
    for file in unknown {
        replay
            .notes
            .push(format!("{file}.jsonl is not in the plan: status only"));
        replay.run(&file, None).await;
    }
    replay.plain.app.close_wallet().await;
    for note in &replay.notes {
        eprintln!("note: {note}");
    }
    eprintln!(
        "replayed {} C++ wallet-api answers ({} skipped: no answer recorded or in NOT_SENT), {} mismatches",
        replay.checked,
        replay.skipped,
        replay.mismatches.len()
    );
    for m in &replay.mismatches {
        eprintln!("MISMATCH {m}");
    }
    assert!(
        replay.mismatches.is_empty(),
        "{} of {} answers differ from the C++ wallet-api (see MISMATCH lines)",
        replay.mismatches.len(),
        replay.checked
    );
}

#[test]
fn placeholder_matching() {
    let mut b = Bindings::default();
    assert_eq!(
        parts("The address given (<address:3>) is <n> x"),
        vec![
            Part::Lit("The address given (".into()),
            Part::Named("<address:3>".into()),
            Part::Lit(") is ".into()),
            Part::Wild,
            Part::Lit(" x".into()),
        ]
    );
    assert_eq!(parts("a<b"), vec![Part::Lit("a<b".into())]);
    assert!(match_parts(
        &parts("The address given (<address:3>) does not exist"),
        "The address given (SEKRabc) does not exist",
        &mut b
    ));
    assert_eq!(b.firm("<address:3>"), Some("SEKRabc"));
    // bound placeholders keep their value, and one value has one placeholder
    assert!(!unify(
        &Value::String("<address:3>".into()),
        &Value::String("SEKRother".into()),
        &mut b
    ));
    assert!(!unify(
        &Value::String("<address:4>".into()),
        &Value::String("SEKRabc".into()),
        &mut b
    ));
    assert!(unify(
        &serde_json::json!({"timestamp": "<timestamp>", "list": ["<tx:1>", "<tx:2>"]}),
        &serde_json::json!({"timestamp": 17, "list": ["h2", "h1"]}),
        &mut b
    ));
    // arrays in another order
    let mut b2 = b.clone();
    assert!(unify(
        &serde_json::json!(["<tx:1>", "<tx:2>"]),
        &serde_json::json!(["h1", "h2"]),
        &mut b2
    ));
    assert!(!unify(
        &serde_json::json!(["<tx:1>", "<tx:1>"]),
        &serde_json::json!(["h1", "h2"]),
        &mut b2
    ));
    assert!(!unify(
        &serde_json::json!({"a": 1}),
        &serde_json::json!({"a": 1, "b": 2}),
        &mut b
    ));
    // a placeholder first seen in a request is replaced by what an answer shows
    let w = Path::new("/tmp/x");
    let sent = substitute("/transactions/privatekey/<hex:1>", &mut b, w);
    assert_eq!(sent.len(), "/transactions/privatekey/".len() + 64);
    assert!(unify(
        &Value::String("<hex:1>".into()),
        &Value::String("ff".repeat(32)),
        &mut b
    ));
    assert_eq!(b.firm("<hex:1>"), Some("ff".repeat(32).as_str()));
    assert_eq!(
        substitute("/w/a.wallet", &mut b, w),
        w.join("a.wallet").to_string_lossy()
    );
    assert!(has_placeholder(&serde_json::json!({"a": ["x <tx:1>"]})));
    assert!(has_placeholder(
        &serde_json::json!({"a": ["x <timestamp>"]})
    ));
    assert!(!has_placeholder(&serde_json::json!({"a": "a<b", "b": 1})));
}
