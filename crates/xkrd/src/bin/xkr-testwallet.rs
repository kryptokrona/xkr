//! `xkr-testwallet` — a deliberately tiny wallet for the testnet end-to-end flow
//! (`docs/mining.md`): keys in a JSON file, a full rescan through the daemon's
//! `/getwalletsyncdata` on every command, transfers built with `xkr-wallet-core` and sent with
//! `/getrandom_outs` + `/sendrawtransaction`. It talks plain HTTP/1.1 to any C++-compatible
//! daemon (xkrd or kryptokronad). Not for real money: the keys are stored unencrypted (in a
//! file only the owner may read, on Unix), no seed words, no persistence of the scan.
//!
//! Fork 14 (`docs/tx-v2.md`): outputs that carry a view tag (`"view_tag"` in
//! `/getwalletsyncdata`) are scanned with the fast path (`scan_tagged_outputs_with_stats`), and
//! `balance` reports how many outputs the tags filtered out. `send --tx-version 2` builds a
//! version-2 transaction (tagged outputs, CLSAG); the default is the newest version the
//! daemon's `/getinfo` `tx_versions` allows for the next block (1 without that field unless the
//! top block is major 7 or higher).
//!
//! `mine` extends a testnet chain fast (scenario 33): `getblocktemplate`, set the merge-mining
//! tag like a miner does (`xkr_chain::set_merge_mining_tag`; the template carries a zero merkle
//! root, so without it every block fails the proof of work check), submit with the template's
//! nonce 0 (the testnet's fixed difficulty 1 accepts any hash), repeat. `--to` is a top block
//! index (`getlastblockheader` height, `/getheight` - 1); `--address` defaults to the wallet's.
//!
//! ```text
//! xkr-testwallet --wallet-dir DIR [--daemon 127.0.0.1:11899] address
//! xkr-testwallet --wallet-dir DIR [--daemon ...] balance
//! xkr-testwallet --wallet-dir DIR [--daemon ...] send SEKR... 12.5 [--fee 0.01] [--mixin N]
//!     [--tx-version 1|2] [--data TEXT]
//! xkr-testwallet --wallet-dir DIR [--daemon ...] mine [--address SEKR...] (--to HEIGHT | --count N)
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use xkr_chain::set_merge_mining_tag;
use xkr_consensus::RingSizeRule;
use xkr_consensus::forks::{FORK_14_PARAMS_MAINNET, FORK_14_PARAMS_TESTNET};
use xkr_crypto::os_rng;
use xkr_serialization::{decode_block, encode_block};
use xkr_types::{Hash, KeyImage, PublicKey, SecretKey};
use xkr_wallet_core::rules::{
    allowed_tx_versions, dust_threshold, is_unlocked, mixin_range, ring_rule_allows, tx_version_for,
};
use xkr_wallet_core::{
    Address, Destination, InputSource, ScanStats, TransferOptions, TxVersion, WalletKeys,
    build_transaction, format_amount, parse_amount, scan_tagged_outputs_with_stats,
};

/// File inside `--wallet-dir`.
const WALLET_FILE: &str = "testwallet.json";
/// Blocks per `/getwalletsyncdata` request.
const SYNC_BATCH: u64 = 100;
/// `mine`: progress line every this many blocks.
const MINE_PROGRESS_EVERY: u64 = 1_000;
/// `mine`: give up after this many refused (or failed) blocks in a row.
const MINE_MAX_REFUSALS: u32 = 5;

/// Minimal Kryptokrona test wallet (testnet use only).
#[derive(Debug, Parser)]
#[command(name = "xkr-testwallet", version, about)]
struct Args {
    /// Directory holding `testwallet.json` (created on first use).
    #[arg(long, value_name = "DIR")]
    wallet_dir: PathBuf,
    /// Daemon RPC `host:port`.
    #[arg(long, value_name = "HOST:PORT", default_value = "127.0.0.1:11899")]
    daemon: String,
    /// First block to scan.
    #[arg(long, value_name = "HEIGHT", default_value_t = 0)]
    from_height: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the wallet address (creates the wallet if needed).
    Address,
    /// Scan the chain and print the balance and owned outputs.
    Balance,
    /// Send `amount` XKR to `address`.
    Send {
        /// Destination address.
        address: String,
        /// Amount in XKR (up to 5 decimals).
        amount: String,
        /// Fee in XKR.
        #[arg(long, default_value = "0.01")]
        fee: String,
        /// Ring size - 1 (default: the minimum allowed at the current height, within the
        /// fork-14 ring-size rule for version 2).
        #[arg(long)]
        mixin: Option<u64>,
        /// Transaction version: 1 (CryptoNote ring signatures) or 2 (fork 14: view tags,
        /// CLSAG). Default: the newest version the daemon accepts for the next block.
        #[arg(long, value_name = "1|2", value_parser = clap::value_parser!(u8).range(1..=2))]
        tx_version: Option<u8>,
        /// Data for the extra nonce (like a Hugin payload).
        #[arg(long)]
        data: Option<String>,
    },
    /// Testnet only (difficulty 1): extend the daemon's chain with `getblocktemplate` +
    /// `submitblock` until its top block index reaches `--to` (or `--count` more blocks).
    Mine {
        /// Address the block rewards go to (default: this wallet's address).
        #[arg(long, value_name = "SEKR...")]
        address: Option<String>,
        /// Stop once the top block index (`getlastblockheader` height) is at least this.
        #[arg(
            long,
            value_name = "HEIGHT",
            required_unless_present = "count",
            conflicts_with = "count"
        )]
        to: Option<u64>,
        /// Mine this many blocks on top of the current top block.
        #[arg(long, value_name = "N")]
        count: Option<u64>,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

// ------------------------------------------------------------------ wallet file

fn load_or_create(dir: &Path) -> Result<WalletKeys, String> {
    let path = dir.join(WALLET_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let spend: SecretKey = v["spend_secret"]
            .as_str()
            .unwrap_or_default()
            .parse()
            .map_err(|_| "bad spend_secret")?;
        let view: SecretKey = v["view_secret"]
            .as_str()
            .unwrap_or_default()
            .parse()
            .map_err(|_| "bad view_secret")?;
        return WalletKeys::from_secrets(&spend, &view)
            .ok_or_else(|| "invalid keys in the wallet file".to_owned());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let keys = WalletKeys::generate(&mut os_rng());
    let v = json!({
        "spend_secret": keys.spend_secret.to_string(),
        "view_secret": keys.view_secret.to_string(),
        "address": keys.address().to_string(),
    });
    write_private(
        &path,
        serde_json::to_string_pretty(&v)
            .unwrap_or_default()
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    eprintln!("created {}", path.display());
    Ok(keys)
}

/// Writes `data` to `path`, readable by the owner only on Unix (the file holds the secret keys).
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        opts.mode(0o600);
        let mut file = opts.open(path)?;
        // An existing file keeps its mode on open.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(data)
    }
    #[cfg(not(unix))]
    {
        opts.open(path)?.write_all(data)
    }
}

// ------------------------------------------------------------------ HTTP

/// `POST path` with a JSON body; returns the JSON response.
fn post(daemon: &str, path: &str, body: &Value) -> Result<Value, String> {
    let mut stream = TcpStream::connect(daemon).map_err(|e| format!("connect {daemon}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|e| e.to_string())?;
    let body = body.to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {daemon}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    // The C++ daemon keeps the connection open even when asked to close it, so reading to EOF
    // would wait for the read timeout: read the head, then exactly `Content-Length` bytes (or
    // the chunked body), and fall back to EOF only when neither is announced.
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let split = loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("malformed HTTP response".into());
        }
        raw.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let mut payload = raw[split + 4..].to_vec();
    let content_length = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok());
    let chunked = head.contains("transfer-encoding: chunked");
    if let Some(len) = content_length {
        while payload.len() < len {
            let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("truncated HTTP response".into());
            }
            payload.extend_from_slice(&buf[..n]);
        }
        payload.truncate(len);
    } else if chunked {
        while dechunk(&payload).is_none() {
            let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            payload.extend_from_slice(&buf[..n]);
        }
        payload = dechunk(&payload).ok_or("malformed chunked body")?;
    } else {
        stream
            .read_to_end(&mut payload)
            .map_err(|e| e.to_string())?;
    }
    let status_ok = head.lines().next().is_some_and(|l| l.contains(" 200"));
    let value: Value = serde_json::from_slice(&payload)
        .map_err(|e| format!("{path}: {e}: {}", String::from_utf8_lossy(&payload)))?;
    if !status_ok {
        return Err(format!("{path}: HTTP error: {value}"));
    }
    Ok(value)
}

/// JSON-RPC 2.0 call on `/json_rpc`; returns `result`, or the `error` object as an error.
fn json_rpc(daemon: &str, method: &str, params: &Value) -> Result<Value, String> {
    let mut resp = post(
        daemon,
        "/json_rpc",
        &json!({ "jsonrpc": "2.0", "id": 0, "method": method, "params": params }),
    )?;
    if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
        return Err(format!(
            "{method}: error {}: {}",
            err["code"],
            err["message"].as_str().unwrap_or_default()
        ));
    }
    resp.get_mut("result")
        .map(Value::take)
        .ok_or_else(|| format!("{method}: no result in {resp}"))
}

fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&data[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(data.get(..size)?);
        data = data.get(size + 2..)?;
    }
}

// ------------------------------------------------------------------ scanning

#[derive(Debug, Clone)]
struct Owned {
    tx_hash: Hash,
    index: usize,
    amount: u64,
    key: PublicKey,
    secret: SecretKey,
    key_image: KeyImage,
    unlock_time: u64,
    height: u64,
    spent: bool,
}

fn parse_key<T: std::str::FromStr>(v: &Value) -> Option<T> {
    v.as_str()?.parse().ok()
}

/// A one-byte view tag as two hex digits (`"view_tag"` of a version-2 output).
fn parse_view_tag(v: &Value) -> Option<u8> {
    let text = v.as_str()?;
    if text.len() != 2 {
        return None;
    }
    u8::from_str_radix(text, 16).ok()
}

/// What a rescan found.
struct ScanResult {
    owned: Vec<Owned>,
    /// Node height (`/getheight`: top index + 1).
    height: u64,
    stats: ScanStats,
    elapsed: Duration,
}

fn scan_tx(
    keys: &WalletKeys,
    tx: &Value,
    height: u64,
    owned: &mut Vec<Owned>,
    images: &mut Vec<KeyImage>,
    stats: &mut ScanStats,
) {
    if let Some(inputs) = tx["inputs"].as_array() {
        images.extend(
            inputs
                .iter()
                .filter_map(|i| parse_key::<KeyImage>(&i["k_image"])),
        );
    }
    let (Some(tx_hash), Some(tx_pub)) = (
        parse_key::<Hash>(&tx["hash"]),
        parse_key::<PublicKey>(&tx["txPublicKey"]),
    ) else {
        return;
    };
    // (amount, key, view tag): the tag is present for version-2 outputs only.
    let outputs: Vec<(u64, PublicKey, Option<u8>)> = tx["outputs"].as_array().map_or_default(|a| {
        a.iter()
            .filter_map(|o| {
                Some((
                    o["amount"].as_u64()?,
                    parse_key(&o["key"])?,
                    parse_view_tag(&o["view_tag"]),
                ))
            })
            .collect()
    });
    let (found, tx_stats) = scan_tagged_outputs_with_stats(keys, &tx_pub, &outputs);
    stats.add(&tx_stats);
    for f in found {
        owned.push(Owned {
            tx_hash,
            index: f.index,
            amount: f.amount,
            key: f.key,
            secret: f.secret,
            key_image: f.key_image,
            unlock_time: tx["unlockTime"].as_u64().unwrap_or(0),
            height,
            spent: false,
        });
    }
}

/// Rescans from `from` and returns the owned outputs (spent ones flagged), the node height and
/// the view-tag statistics.
fn scan(args: &Args, keys: &WalletKeys) -> Result<ScanResult, String> {
    let started = Instant::now();
    let mut owned = Vec::new();
    let mut images = Vec::new();
    let mut stats = ScanStats::default();
    let mut next = args.from_height;
    loop {
        let resp = post(
            &args.daemon,
            "/getwalletsyncdata",
            &json!({ "startHeight": next, "blockCount": SYNC_BATCH, "blockHashCheckpoints": [] }),
        )?;
        let items = resp["items"].as_array().cloned().unwrap_or_default();
        let Some(last) = items.last() else { break };
        let last_height = last["blockHeight"].as_u64().ok_or("missing blockHeight")?;
        for item in &items {
            let height = item["blockHeight"].as_u64().unwrap_or(0);
            scan_tx(
                keys,
                &item["coinbaseTX"],
                height,
                &mut owned,
                &mut images,
                &mut stats,
            );
            for tx in item["transactions"].as_array().into_iter().flatten() {
                scan_tx(keys, tx, height, &mut owned, &mut images, &mut stats);
            }
        }
        if last_height < next {
            break;
        }
        next = last_height + 1;
    }
    for o in &mut owned {
        o.spent = images.contains(&o.key_image);
    }
    let elapsed = started.elapsed();
    let height = post(&args.daemon, "/getheight", &json!({}))?["height"]
        .as_u64()
        .ok_or("missing height")?;
    Ok(ScanResult {
        owned,
        height,
        stats,
        elapsed,
    })
}

/// What the daemon says about the next block: allowed transaction versions and the network's
/// fork-14 ring-size rule.
struct DaemonForkInfo {
    tx_versions: Vec<u8>,
    ring_rule: RingSizeRule,
}

/// `/getinfo`: `tx_versions` (xkrd), else version 2 only if the top block is major 7 or higher
/// (`xkr_wallet_core::rules::allowed_tx_versions`).
fn daemon_fork_info(args: &Args) -> Result<DaemonForkInfo, String> {
    let info = post(&args.daemon, "/getinfo", &json!({}))?;
    let listed: Option<Vec<u8>> = info["tx_versions"].as_array().map(|list| {
        list.iter()
            .filter_map(|v| v.as_u64().and_then(|v| u8::try_from(v).ok()))
            .collect()
    });
    let major = info["major_version"]
        .as_u64()
        .map(|m| u8::try_from(m).unwrap_or(u8::MAX));
    let tx_versions = allowed_tx_versions(listed.as_deref(), major);
    let params = if info["testnet"].as_bool() == Some(true) {
        FORK_14_PARAMS_TESTNET
    } else {
        FORK_14_PARAMS_MAINNET
    };
    Ok(DaemonForkInfo {
        tx_versions,
        ring_rule: params.v2_ring_size,
    })
}

/// The daemon's top block index (`/getheight` is top index + 1).
fn top_index(daemon: &str) -> Result<u64, String> {
    post(daemon, "/getheight", &json!({}))?["height"]
        .as_u64()
        .and_then(|h| h.checked_sub(1))
        .ok_or_else(|| "/getheight: missing height".to_owned())
}

/// `getblocktemplate` for `address`.
fn block_template(daemon: &str, address: &str) -> Result<Value, String> {
    json_rpc(
        daemon,
        "getblocktemplate",
        &json!({ "wallet_address": address, "reserve_size": 0 }),
    )
}

/// One block: template, merge-mining tag (what `MinerManager::adjustMergeMiningTag` does), the
/// template's nonce 0 unchanged (difficulty 1 accepts any hash), `submitblock`.
fn mine_block(daemon: &str, address: &str) -> Result<(), String> {
    let template = block_template(daemon, address)?;
    let blob = template["blocktemplate_blob"]
        .as_str()
        .ok_or("getblocktemplate: no blocktemplate_blob")?;
    let blob = hex::decode(blob).map_err(|e| format!("blocktemplate_blob: {e}"))?;
    let mut block = decode_block(&blob).map_err(|e| format!("decoding the template: {e}"))?;
    set_merge_mining_tag(&mut block).map_err(|e| format!("merge-mining tag: {e}"))?;
    let blob = encode_block(&block).map_err(|e| format!("encoding the block: {e}"))?;
    let resp = json_rpc(daemon, "submitblock", &json!([hex::encode(blob)]))?;
    if resp["status"] == "OK" {
        Ok(())
    } else {
        Err(format!("submitblock: {resp}"))
    }
}

/// `mine`: blocks until the top index reaches `to` (or `count` more), then one summary line.
fn mine(args: &Args, address: &str, to: Option<u64>, count: Option<u64>) -> Result<(), String> {
    let daemon = args.daemon.as_str();
    let start_top = top_index(daemon)?;
    let target = match (to, count) {
        (Some(to), None) => to,
        (None, Some(count)) => start_top.saturating_add(count),
        _ => return Err("mine: give exactly one of --to and --count".into()),
    };
    let difficulty = block_template(daemon, address)?["difficulty"]
        .as_u64()
        .ok_or("getblocktemplate: missing difficulty")?;
    if difficulty != 1 {
        return Err(format!(
            "mine submits unhashed templates and needs difficulty 1 (testnet); the daemon wants {difficulty}"
        ));
    }
    let started = Instant::now();
    let mut top = start_top;
    let mut mined = 0u64;
    let mut refusals = 0u32;
    while top < target {
        match mine_block(daemon, address) {
            Ok(()) => {
                mined += 1;
                top += 1;
                refusals = 0;
                if mined.is_multiple_of(MINE_PROGRESS_EVERY) {
                    println!(
                        "mined {mined} blocks, top {top} of {target} ({} s)",
                        started.elapsed().as_secs()
                    );
                }
            }
            Err(e) => {
                refusals += 1;
                eprintln!("block refused ({refusals} in a row) at top {top}: {e}");
                if refusals >= MINE_MAX_REFUSALS {
                    return Err(format!(
                        "the daemon refused {refusals} blocks in a row at top {top} (mined {mined})"
                    ));
                }
                std::thread::sleep(Duration::from_secs(1));
                top = top_index(daemon).unwrap_or(top);
            }
        }
    }
    let elapsed = started.elapsed();
    let top = top_index(daemon).unwrap_or(top);
    let rate = u128::from(mined) * 1_000 / elapsed.as_millis().max(1);
    println!(
        "mined {mined} blocks to height {top} in {:.1} s ({rate} blocks/s)",
        elapsed.as_secs_f64()
    );
    Ok(())
}

/// Whether a transaction built now may spend `o` (C++ `utilities::isInputUnlocked` with the
/// node height `/getheight`, `xkr_wallet_core::rules::is_unlocked`).
fn unlocked(o: &Owned, node_height: u64) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    is_unlocked(o.unlock_time, node_height, now)
}

// ------------------------------------------------------------------ commands

fn run(args: &Args) -> Result<(), String> {
    let keys = load_or_create(&args.wallet_dir)?;
    match &args.command {
        Command::Address => {
            println!("{}", keys.address());
            Ok(())
        }
        Command::Balance => {
            let ScanResult {
                owned,
                height,
                stats,
                elapsed,
            } = scan(args, &keys)?;
            let mut unlocked_sum = 0u64;
            let mut locked_sum = 0u64;
            for o in owned.iter().filter(|o| !o.spent) {
                if unlocked(o, height) {
                    unlocked_sum += o.amount;
                } else {
                    locked_sum += o.amount;
                }
                println!(
                    "{} {:>16} height {} tx {}:{}",
                    if unlocked(o, height) {
                        "unlocked"
                    } else {
                        "locked  "
                    },
                    format_amount(o.amount),
                    o.height,
                    o.tx_hash,
                    o.index
                );
            }
            println!("address  {}", keys.address());
            println!("height   {height}");
            println!("unlocked {}", format_amount(unlocked_sum));
            println!("locked   {}", format_amount(locked_sum));
            println!(
                "scan     {} outputs in {} ms: {} with view tags, {} skipped by view tag, {} full derivations",
                stats.outputs,
                elapsed.as_millis(),
                stats.tagged,
                stats.skipped_by_tag,
                stats.full_checks
            );
            Ok(())
        }
        Command::Send {
            address,
            amount,
            fee,
            mixin,
            tx_version,
            data,
        } => {
            let dest = Address::parse(address).map_err(|e| format!("destination: {e}"))?;
            let amount = parse_amount(amount).ok_or("bad amount")?;
            let fee = parse_amount(fee).ok_or("bad fee")?;
            let ScanResult { owned, height, .. } = scan(args, &keys)?;
            let fork = daemon_fork_info(args)?;
            let version = match tx_version {
                Some(2) => TxVersion::V2,
                Some(_) => TxVersion::V1,
                None => tx_version_for(Some(fork.tx_versions.as_slice()), None),
            };
            let version_number: u8 = match version {
                TxVersion::V1 => 1,
                TxVersion::V2 => 2,
            };
            if !fork.tx_versions.contains(&version_number) {
                eprintln!(
                    "warning: the daemon accepts transaction versions {:?} for the next block, \
                     building version {version_number} anyway",
                    fork.tx_versions
                );
            }
            // Default: the smallest mixin allowed at the height (and by the fork-14 ring-size
            // rule for version 2), so a young testnet chain has enough decoys.
            let mixin = mixin.unwrap_or_else(|| mixin_range(height, version, fork.ring_rule).min);
            if !ring_rule_allows(mixin, version, fork.ring_rule) {
                eprintln!(
                    "warning: ring size {} is outside the fork-14 rule {:?}",
                    mixin + 1,
                    fork.ring_rule
                );
            }

            // Largest unlocked outputs first until amount + fee is covered.
            let mut spendable: Vec<&Owned> = owned
                .iter()
                .filter(|o| !o.spent && unlocked(o, height))
                .collect();
            spendable.sort_by_key(|o| std::cmp::Reverse(o.amount));
            let need = amount.checked_add(fee).ok_or("amount overflow")?;
            let mut chosen = Vec::new();
            let mut have = 0u64;
            for o in spendable {
                if have >= need {
                    break;
                }
                have += o.amount;
                chosen.push(o);
            }
            if have < need {
                return Err(format!(
                    "not enough unlocked funds: have {}, need {}",
                    format_amount(have),
                    format_amount(need)
                ));
            }

            // Global indexes of the chosen outputs.
            let mut inputs = Vec::with_capacity(chosen.len());
            for o in &chosen {
                let resp = post(
                    &args.daemon,
                    "/get_o_indexes",
                    &json!({ "txid": o.tx_hash.to_string() }),
                )?;
                let global = resp["o_indexes"]
                    .as_array()
                    .and_then(|a| a.get(o.index))
                    .and_then(Value::as_u64)
                    .ok_or_else(|| format!("no global index for {}:{}", o.tx_hash, o.index))?;
                inputs.push(InputSource {
                    amount: o.amount,
                    global_index: u32::try_from(global).map_err(|_| "global index too large")?,
                    key: o.key,
                    secret: o.secret,
                    key_image: o.key_image,
                    decoys: Vec::new(),
                });
            }

            // Decoys: one more than needed per amount so our own output can be skipped.
            if mixin > 0 {
                let amounts: Vec<u64> = inputs.iter().map(|i| i.amount).collect();
                let resp = post(
                    &args.daemon,
                    "/getrandom_outs",
                    &json!({ "amounts": amounts, "outs_count": mixin + 1 }),
                )?;
                if resp["status"] != "OK" {
                    return Err(format!("getrandom_outs failed: {resp}"));
                }
                let outs = resp["outs"].as_array().cloned().unwrap_or_default();
                for (input, entry) in inputs.iter_mut().zip(&outs) {
                    let decoys: Vec<(u32, PublicKey)> = entry["outs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|o| {
                            Some((
                                u32::try_from(o["global_amount_index"].as_u64()?).ok()?,
                                parse_key(&o["out_key"])?,
                            ))
                        })
                        .filter(|(i, _)| *i != input.global_index)
                        .take(usize::try_from(mixin).unwrap_or(usize::MAX))
                        .collect();
                    if (decoys.len() as u64) < mixin {
                        return Err(format!(
                            "only {} decoys for amount {}, need {mixin}; try --mixin {}",
                            decoys.len(),
                            input.amount,
                            decoys.len()
                        ));
                    }
                    input.decoys = decoys;
                }
            }

            let opts = TransferOptions {
                version,
                fee,
                dust_threshold: dust_threshold(height),
                data: data.as_ref().map(|d| d.as_bytes().to_vec()),
                ..TransferOptions::default()
            };
            let built = build_transaction(
                &inputs,
                &[Destination {
                    address: dest,
                    amount,
                }],
                &keys.address(),
                &opts,
                &mut os_rng(),
            )
            .map_err(|e| e.to_string())?;
            let resp = post(
                &args.daemon,
                "/sendrawtransaction",
                &json!({ "tx_as_hex": hex::encode(&built.blob) }),
            )?;
            println!("transaction {}", built.hash);
            println!(
                "version {version_number} inputs {} mixin {mixin} fee {} size {} bytes",
                inputs.len(),
                format_amount(fee),
                built.blob.len()
            );
            println!("daemon: {resp}");
            if resp["status"] == "OK" {
                Ok(())
            } else {
                Err("the daemon refused the transaction".into())
            }
        }
        Command::Mine { address, to, count } => {
            let address = address
                .clone()
                .unwrap_or_else(|| keys.address().to_string());
            mine(args, &address, *to, *count)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn wallet_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("xkr-testwallet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keys.json");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"{}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        let fresh = dir.join("fresh.json");
        write_private(&fresh, b"x").unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
