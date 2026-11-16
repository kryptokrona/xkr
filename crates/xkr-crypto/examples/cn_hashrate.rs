//! CryptoNight hashrate on this machine: `cn_slow_hash` (new scratchpad per hash, table AES)
//! against `CnHasher` with software and hardware AES, for comparing with the C++ `miner`.
//!
//! ```text
//! cargo run --release -p xkr-crypto --example cn_hashrate -- [--threads N] [--seconds S]
//!     [--algo turtle|lite|cn] [--mode all|alloc|software|hardware]
//! ```
//!
//! Defaults: 1 thread, 10 s per mode, CN-Turtle-Lite v2 (blocks of major version 5 and later),
//! every mode. Each thread hashes a 76-byte blob with its own nonce sequence at offset 39.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use xkr_crypto::cryptonight::{
    AesImpl, CnHasher, CnParams, Variant, cn_slow_hash, hardware_aes_available,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Alloc,
    Software,
    Hardware,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Alloc => "cn_slow_hash (alloc + table AES)",
            Self::Software => "CnHasher, software AES",
            Self::Hardware => "CnHasher, hardware AES",
        }
    }
}

#[derive(Debug)]
struct Args {
    threads: usize,
    seconds: u64,
    variant: Variant,
    params: CnParams,
    algo: &'static str,
    modes: Vec<Mode>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        threads: 1,
        seconds: 10,
        variant: Variant::V2,
        params: CnParams::CN_TURTLE_LITE,
        algo: "cn-turtle-lite v2",
        modes: vec![Mode::Alloc, Mode::Software, Mode::Hardware],
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--threads" => {
                args.threads = value()?.parse().map_err(|e| format!("--threads: {e}"))?;
            }
            "--seconds" => {
                args.seconds = value()?.parse().map_err(|e| format!("--seconds: {e}"))?;
            }
            "--algo" => match value()?.as_str() {
                "turtle" => {}
                "lite" => {
                    (args.variant, args.params, args.algo) =
                        (Variant::V1, CnParams::CN_LITE, "cn-lite v1");
                }
                "cn" => {
                    (args.variant, args.params, args.algo) = (Variant::V0, CnParams::CN, "cn v0");
                }
                other => return Err(format!("unknown --algo {other}")),
            },
            "--mode" => {
                args.modes = match value()?.as_str() {
                    "all" => vec![Mode::Alloc, Mode::Software, Mode::Hardware],
                    "alloc" => vec![Mode::Alloc],
                    "software" => vec![Mode::Software],
                    "hardware" => vec![Mode::Hardware],
                    other => return Err(format!("unknown --mode {other}")),
                };
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    args.threads = args.threads.max(1);
    args.seconds = args.seconds.max(1);
    Ok(args)
}

/// Hashes on `threads` threads for `seconds`; returns hashes per second.
#[expect(clippy::cast_precision_loss, reason = "a hashrate is approximate")]
fn run(args: &Args, mode: Mode) -> f64 {
    let stop = AtomicBool::new(false);
    let total = AtomicU64::new(0);
    let start = Instant::now();
    std::thread::scope(|s| {
        for t in 0..args.threads {
            let (stop, total) = (&stop, &total);
            s.spawn(move || {
                let mut blob = [0x5a_u8; 76];
                blob[0] = 7;
                let mut hasher = match mode {
                    Mode::Hardware => Some(CnHasher::with_aes(
                        args.variant,
                        args.params,
                        AesImpl::Hardware,
                    )),
                    Mode::Software => Some(CnHasher::with_aes(
                        args.variant,
                        args.params,
                        AesImpl::Software,
                    )),
                    Mode::Alloc => None,
                };
                let mut nonce = t as u32;
                let mut count = 0u64;
                let mut sink = 0u8;
                while !stop.load(Ordering::Relaxed) {
                    blob[39..43].copy_from_slice(&nonce.to_le_bytes());
                    let hash = match &mut hasher {
                        Some(h) => h.hash(&blob),
                        None => cn_slow_hash(&blob, args.variant, &args.params),
                    }
                    .unwrap_or_default();
                    sink ^= hash[0];
                    count += 1;
                    nonce = nonce.wrapping_add(args.threads as u32);
                }
                total.fetch_add(count, Ordering::Relaxed);
                std::hint::black_box(sink);
            });
        }
        std::thread::sleep(Duration::from_secs(args.seconds));
        stop.store(true, Ordering::Relaxed);
    });
    total.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64()
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    println!(
        "algo {}, {} thread(s), {} s per mode, CPU AES instructions: {}",
        args.algo,
        args.threads,
        args.seconds,
        if hardware_aes_available() {
            "yes"
        } else {
            "no"
        }
    );
    for &mode in &args.modes {
        let rate = run(&args, mode);
        #[expect(clippy::cast_precision_loss, reason = "a hashrate is approximate")]
        let per_thread = rate / args.threads as f64;
        println!(
            "{:<36} {rate:>10.1} H/s  ({per_thread:.1} H/s per thread)",
            mode.name()
        );
    }
}
