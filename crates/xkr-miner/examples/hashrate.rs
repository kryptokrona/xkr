//! Hashrate of the miner's hashing threads on this machine, without a daemon or pool: the
//! same `HashPool` the `xkr-miner` binary and xkrd's built-in miner use, on a fixed job whose
//! target is never met. For comparing with the C++ `miner` at the same thread count.
//!
//! ```text
//! cargo run --release -p xkr-miner --example hashrate -- [--threads N] [--seconds S]
//!     [--major 1..7] [--aes auto|hardware|software]
//! ```
//!
//! Defaults: 1 thread, 20 s, major version 7 (CN-Turtle-Lite v2), AES detected. Prints the
//! hashrate every 5 s and the average at the end.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use xkr_miner::{AesImpl, HashConfig, HashPool, Job, Target, format_speed};

fn parse() -> Result<(usize, u64, u8, AesImpl), String> {
    let (mut threads, mut seconds, mut major, mut aes) = (1usize, 20u64, 7u8, AesImpl::detect());
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--threads" => threads = value.parse().map_err(|e| format!("--threads: {e}"))?,
            "--seconds" => seconds = value.parse().map_err(|e| format!("--seconds: {e}"))?,
            "--major" => major = value.parse().map_err(|e| format!("--major: {e}"))?,
            "--aes" => {
                aes = match value.as_str() {
                    "auto" => AesImpl::detect(),
                    "hardware" => AesImpl::Hardware,
                    "software" => AesImpl::Software,
                    other => return Err(format!("unknown --aes {other}")),
                };
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok((threads.max(1), seconds.max(1), major, aes))
}

#[expect(clippy::cast_precision_loss, reason = "a hashrate is approximate")]
fn main() {
    let (threads, seconds, major, aes) = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let mut blob = vec![0x5a_u8; 76];
    blob[0] = major;
    let job = match Job::new("bench".into(), blob, 39, Target::Block(u64::MAX), 0, major) {
        Ok(j) => Arc::new(j),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let (tx, _rx) = mpsc::unbounded_channel();
    let cfg = HashConfig {
        threads,
        aes,
        thread_name: "bench".into(),
    };
    let pool = HashPool::start(&cfg, tx).expect("hashing threads");
    println!(
        "{} with {threads} thread(s), {} AES, {seconds} s",
        job.algo().name(),
        aes.name()
    );
    pool.set_job(Some(job));
    let start = Instant::now();
    let mut last = (start, 0u64);
    while start.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_secs(5).min(Duration::from_secs(seconds)));
        let now = Instant::now();
        let total = pool.hashes();
        let rate = (total - last.1) as f64 / now.duration_since(last.0).as_secs_f64();
        println!("  {}", format_speed(rate));
        last = (now, total);
    }
    let total = pool.hashes();
    let rate = total as f64 / start.elapsed().as_secs_f64();
    pool.stop();
    println!(
        "average {} ({:.1} H/s per thread, {total} hashes)",
        format_speed(rate),
        rate / threads as f64
    );
}
