//! Saves stopped by killing the process: a child process (this test binary again, running
//! [`save_loop_child`]) opens a wallet and saves in a tight loop while its state changes (one
//! more address per round); once it reported its first save, the parent kills it
//! (`Child::kill`, `SIGKILL` on Unix) after a random delay, opens the wallet and checks that it
//! holds one of the saved states. Three children run side by side, on wallets of both formats,
//! until 50 kills are done (or about a minute passed).
//!
//! Set `XKR_SKIP_KILL_TEST=1` to skip.

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{Mock, config, keys};
use rand_core::RngCore as _;
use xkr_consensus::Network;
use xkr_wallet::{FileFormat, Wallet};

/// The wallet file of the child.
const ENV_WALLET: &str = "XKR_SAVE_LOOP_WALLET";
/// `a` or `b`: the format of the child's wallet.
const ENV_FORMAT: &str = "XKR_SAVE_LOOP_FORMAT";

fn format_of(name: &str) -> FileFormat {
    if name == "b" {
        FileFormat::WalletGreen
    } else {
        FileFormat::WalletBackend
    }
}

/// The child: adds an address and saves, again and again, printing `saved N` (N addresses)
/// after each save. Does nothing unless started by [`killed_saves_leave_a_wallet_that_opens`].
#[test]
fn save_loop_child() {
    let Ok(path) = std::env::var(ENV_WALLET) else {
        return;
    };
    let format = format_of(&std::env::var(ENV_FORMAT).unwrap_or_default());
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mock = Mock::new();
        let w = Wallet::open(
            &path,
            "pw",
            mock.node(),
            config(Network::Mainnet, format, false),
        )
        .await
        .unwrap();
        loop {
            w.add_subwallet().await.unwrap();
            w.save().await.unwrap();
            let mut out = std::io::stdout();
            let _ = writeln!(out, "saved {}", w.addresses().len());
            let _ = out.flush();
        }
    });
}

/// The addresses of the wallet at `path`, opened as a user would.
async fn addresses(path: &Path, format: FileFormat) -> usize {
    let mock = Mock::new();
    let w = Wallet::open(
        path,
        "pw",
        mock.node(),
        config(Network::Mainnet, format, false),
    )
    .await
    .unwrap_or_else(|e| panic!("the wallet does not open after a kill: {e}"));
    w.addresses().len()
}

/// The files in `dir`, sorted.
fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Kills wanted over all lanes.
const KILLS: usize = 50;
/// Children running side by side.
const LANES: usize = 3;
/// Time after which no new child is started.
const BUDGET: Duration = Duration::from_secs(120);

/// A new wallet of `format` at `path`, saved.
fn new_wallet(rt: &tokio::runtime::Runtime, path: &Path, format: FileFormat) {
    rt.block_on(async {
        let me = keys();
        let mock = Mock::new();
        Wallet::import_keys(
            path,
            "pw",
            me.spend_secret,
            me.view_secret,
            0,
            mock.node(),
            config(Network::Mainnet, format, false),
        )
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    });
}

/// One lane: kills children on its own wallet until `KILLS` kills are done over all lanes or
/// the time is up; returns its kills.
fn lane(
    exe: &Path,
    name: &'static str,
    format: FileFormat,
    done: &AtomicUsize,
    start: Instant,
) -> usize {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("w.wallet");
    new_wallet(&rt, &path, format);
    let expected_files = if format == FileFormat::WalletGreen {
        vec!["w.wallet".to_owned(), "w.wallet.xkrstate".to_owned()]
    } else {
        vec!["w.wallet".to_owned()]
    };
    let mut rng = xkr_crypto::os_rng();
    let mut count = 1;
    let mut kills = 0;
    while done.load(Ordering::SeqCst) < KILLS && start.elapsed() < BUDGET {
        let mut child = Command::new(exe)
            .args([
                "save_loop_child",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ENV_WALLET, &path)
            .env(ENV_FORMAT, name)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // the child's reports, line by line
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<usize>();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(n) = line
                    .strip_prefix("saved ")
                    .and_then(|n| n.trim().parse().ok())
                    && tx.send(n).is_err()
                {
                    break;
                }
            }
        });
        let first = rx.recv_timeout(Duration::from_secs(60));
        let delay = u64::from(rng.next_u32() % 400);
        std::thread::sleep(Duration::from_millis(delay));
        child.kill().unwrap();
        child.wait().unwrap();
        let _ = reader.join();
        let first = first.unwrap_or_else(|_| panic!("{name}: the child saved nothing"));
        // saved: what the child reported; one more save may have finished unreported
        let saved = rx.try_iter().last().unwrap_or(first);
        let now = rt.block_on(addresses(&path, format));
        assert!(
            now == saved || now == saved + 1,
            "{name}, kill {kills} {delay} ms after the first save: {now} addresses, last saved \
             {saved}"
        );
        assert!(now >= count, "{name}, kill {kills}: {now} < {count}");
        // a temporary file the kill left behind is gone once the wallet was opened
        assert_eq!(files(dir.path()), expected_files, "{name}, kill {kills}");
        count = now;
        kills += 1;
        done.fetch_add(1, Ordering::SeqCst);
    }
    kills
}

#[test]
fn killed_saves_leave_a_wallet_that_opens() {
    if std::env::var_os("XKR_SKIP_KILL_TEST").is_some() {
        eprintln!("skipped (XKR_SKIP_KILL_TEST)");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let done = AtomicUsize::new(0);
    let start = Instant::now();
    let lanes: [(&'static str, FileFormat); LANES] = [
        ("a", FileFormat::WalletBackend),
        ("b", FileFormat::WalletGreen),
        ("a", FileFormat::WalletBackend),
    ];
    let kills: Vec<usize> = std::thread::scope(|scope| {
        let handles: Vec<_> = lanes
            .iter()
            .map(|&(name, format)| {
                let (exe, done) = (&exe, &done);
                scope.spawn(move || lane(exe, name, format, done, start))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let total: usize = kills.iter().sum();
    eprintln!(
        "{total} kills ({kills:?} per lane) in {:.1} s",
        start.elapsed().as_secs_f64()
    );
    // at least a few kills of each format however slow the machine; normally `KILLS`
    assert!(kills.iter().all(|k| *k >= 3), "{kills:?}");
}
