//! The node operator's fee address: where the node's shares are paid to.
//!
//! The Hugin relay submits its apps' shares to p2pool under this address
//! (`docs/hugin-relay.md` §11), and the built-in p2pool (`--p2pool`, `crate::p2pool`) builds
//! its block templates with it as the payout wallet; both take it from the same handle
//! ([`FeeAddress::subscribe`]), which is loaded once when either runs.
//!
//! Precedence: `--hugin <address>` (saved at once), then the saved address, then the default
//! donation address [`DEFAULT_FEE_ADDRESS`] ("nilsjr"). The value is stored in
//! `<data dir>/hugin/identity.json` (`feeAddress`, `feeAsked`; mode 0600, written atomically),
//! and only this module reads and writes it there. The console changes it while the node runs
//! (`fee-address <address>`); the change reaches every subscriber at once.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::watch;
use xkr_consensus::Network;
use xkr_hugin::NodeIdentity;
use xkr_wallet_core::Address;

/// The default fee address: a donation to the developer of xkrd, [`DEFAULT_FEE_NAME`].
pub const DEFAULT_FEE_ADDRESS: &str = "SEKReSw2cP5bv43mgSmUq2FqLRopwDqrseW6CHxDJuAfRnxaJNvzd2dY6QExxP6JER6RBHcLMYi8ESrzV9M8xPKkFNjSkcBJZGc";
/// Who [`DEFAULT_FEE_ADDRESS`] belongs to, as shown to the operator.
pub const DEFAULT_FEE_NAME: &str = "nilsjr";

/// The fee address in effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fee {
    /// `None`: no fee address (shares are not submitted).
    pub address: Option<String>,
    /// `address` is the default donation address.
    pub donation: bool,
}

impl Fee {
    /// The address shortened for a terminal line (`SEKReSw2cP…kcBJZGc`), or `none`.
    #[must_use]
    pub fn short(&self) -> String {
        self.address
            .as_deref()
            .map_or_else(|| "none".to_owned(), short_address)
    }

    /// One line for `status` and the start-up step: `donation to nilsjr (SEKR…)`,
    /// `SEKR…`, or `none (shares are not submitted)`.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.address {
            None => "none (shares are not submitted)".to_owned(),
            Some(a) if self.donation => {
                format!("donation to {DEFAULT_FEE_NAME} ({})", short_address(a))
            }
            Some(a) => short_address(a),
        }
    }
}

/// `SEKReSw2cP…kcBJZGc`: the first 10 and the last 7 characters of a long address.
#[must_use]
pub fn short_address(a: &str) -> String {
    let chars: Vec<char> = a.chars().collect();
    if chars.len() <= 20 {
        return a.to_owned();
    }
    let head: String = chars[..10].iter().collect();
    let tail: String = chars[chars.len() - 7..].iter().collect();
    format!("{head}…{tail}")
}

/// Checks a fee address for `network`: a standard address with either accepted prefix (`SEKR…`
/// or `Xkr…`, the same on the testnet); integrated addresses are refused. Returns the address
/// without surrounding spaces.
pub fn parse_fee_address(text: &str, network: Network) -> Result<String, String> {
    let text = text.trim();
    let address = Address::parse(text).map_err(|e| format!("not a valid XKR address ({e})"))?;
    if address.payment_id.is_some() {
        return Err(
            "an integrated address cannot be a fee address; use the standard address".into(),
        );
    }
    // Both networks use the same address prefixes (`xkr_wallet_core::address`).
    let _ = network;
    Ok(text.to_owned())
}

/// The default fee address on `network`, if it is valid there.
#[must_use]
pub fn default_fee_address(network: Network) -> Option<&'static str> {
    parse_fee_address(DEFAULT_FEE_ADDRESS, network)
        .ok()
        .map(|_| DEFAULT_FEE_ADDRESS)
}

/// The fee in effect for a saved address (`None`: the default).
fn effective(saved: Option<String>, network: Network) -> Fee {
    match saved {
        Some(a) => Fee {
            donation: a == DEFAULT_FEE_ADDRESS,
            address: Some(a),
        },
        None => Fee {
            address: default_fee_address(network).map(str::to_owned),
            donation: default_fee_address(network).is_some(),
        },
    }
}

/// The fee address handle: the value in effect (a watch channel), its file, and whether the
/// operator was asked about it.
#[derive(Debug)]
pub struct FeeAddress {
    file: PathBuf,
    network: Network,
    tx: watch::Sender<Fee>,
    /// The operator answered the question once (`feeAsked`).
    asked: AtomicBool,
    /// The console shows the question and waits for its answer.
    question: AtomicBool,
    /// Serializes the file writes.
    save: Mutex<()>,
}

impl FeeAddress {
    /// Loads the saved address from `file` (`identity.json`; created with a new node identity
    /// if missing). `flag` (`--hugin <address>`) wins and is saved at once.
    pub fn load(file: &Path, network: Network, flag: Option<&str>) -> Result<Arc<Self>, String> {
        let mut id =
            NodeIdentity::load_or_create(file).map_err(|e| format!("{}: {e}", file.display()))?;
        if let Some(text) = flag {
            let address = parse_fee_address(text, network).map_err(|e| format!("--hugin: {e}"))?;
            if id.fee_address.as_deref() != Some(address.as_str()) || !id.fee_asked {
                id.fee_address = Some(address);
                id.fee_asked = true;
                id.save(file)
                    .map_err(|e| format!("saving {}: {e}", file.display()))?;
            }
        }
        let fee = effective(id.fee_address.clone(), network);
        Ok(Arc::new(Self {
            file: file.to_owned(),
            network,
            tx: watch::channel(fee).0,
            asked: AtomicBool::new(id.fee_asked),
            question: AtomicBool::new(false),
            save: Mutex::new(()),
        }))
    }

    /// The fee address in effect.
    #[must_use]
    pub fn get(&self) -> Fee {
        self.tx.borrow().clone()
    }

    /// Every change from now on (the current value is marked seen).
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Fee> {
        self.tx.subscribe()
    }

    /// Sets, saves and applies a new address (the operator's answer to the question too).
    pub fn set(&self, text: &str) -> Result<Fee, String> {
        let address = parse_fee_address(text, self.network)?;
        self.persist(|id| {
            id.fee_address = Some(address.clone());
            id.fee_asked = true;
        })?;
        let fee = effective(Some(address), self.network);
        self.tx.send_replace(fee.clone());
        self.asked.store(true, Ordering::Relaxed);
        self.question.store(false, Ordering::Relaxed);
        Ok(fee)
    }

    /// The operator keeps the address in effect: saved as answered, not asked again.
    pub fn keep(&self) -> Result<(), String> {
        self.persist(|id| id.fee_asked = true)?;
        self.asked.store(true, Ordering::Relaxed);
        self.question.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn persist(&self, change: impl FnOnce(&mut NodeIdentity)) -> Result<(), String> {
        let _guard = self.save.lock().unwrap_or_else(PoisonError::into_inner);
        let mut id = NodeIdentity::load_or_create(&self.file)
            .map_err(|e| format!("{}: {e}", self.file.display()))?;
        change(&mut id);
        id.save(&self.file)
            .map_err(|e| format!("saving {}: {e}", self.file.display()))
    }

    /// Whether the start-up should ask: the default donation is in effect and the operator
    /// never answered.
    #[must_use]
    pub fn should_ask(&self) -> bool {
        self.get().donation && !self.asked.load(Ordering::Relaxed)
    }

    /// The console shows the question from now on.
    pub fn ask(&self) {
        self.question.store(true, Ordering::Relaxed);
    }

    /// Whether the question waits for its answer.
    #[must_use]
    pub fn question_pending(&self) -> bool {
        self.question.load(Ordering::Relaxed)
    }

    /// Another command was typed: the question is dropped (and asked again at the next start).
    pub fn drop_question(&self) {
        self.question.store(false, Ordering::Relaxed);
    }
}

/// The file that holds the fee address: `<data dir>/hugin/identity.json` (the Hugin relay's
/// identity file, also used when only the built-in p2pool runs).
#[must_use]
pub fn identity_file(data_dir: &Path) -> PathBuf {
    data_dir
        .join(crate::hyper::HUGIN_DIR)
        .join(xkr_hugin::identity::IDENTITY_FILE)
}

/// The start-up notice while donating (also the warning line without a terminal), naming what
/// pays: the Hugin relay's fee, the built-in p2pool's payout, or both.
#[must_use]
pub fn donation_notice(hugin: bool, p2pool: bool) -> String {
    let what = match (hugin, p2pool) {
        (true, true) => "hugin fee and p2pool payout",
        (false, true) => "p2pool payout",
        _ => "hugin node fee",
    };
    format!("You are donating your {what} to {DEFAULT_FEE_NAME}")
}

/// The question under the notice, in lines of at most 60 columns.
pub const QUESTION: [&str; 1] = ["Change it? Type fee-address <address>, or Enter to keep."];

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("xkrd-fee-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn default_and_validation() {
        assert_eq!(DEFAULT_FEE_ADDRESS.len(), 99);
        assert_eq!(
            parse_fee_address(&format!("  {DEFAULT_FEE_ADDRESS}\n"), Network::Mainnet).unwrap(),
            DEFAULT_FEE_ADDRESS
        );
        assert_eq!(
            default_fee_address(Network::Mainnet),
            Some(DEFAULT_FEE_ADDRESS)
        );
        // the testnet uses the same prefixes: the default is valid there too
        assert_eq!(
            default_fee_address(Network::Testnet),
            Some(DEFAULT_FEE_ADDRESS)
        );
        let other = NodeIdentity::generate().address;
        assert!(parse_fee_address(&other, Network::Testnet).is_ok());
        // the Xkr form of the same keys
        let mut alt = Address::parse(&other).unwrap();
        alt.prefix = xkr_wallet_core::address::ADDRESS_PREFIX_ALT;
        assert!(parse_fee_address(&alt.encode(), Network::Mainnet).is_ok());
        // integrated: refused
        let integrated = Address::parse(&other)
            .unwrap()
            .integrated(xkr_types::Hash([7; 32]))
            .encode();
        assert!(
            parse_fee_address(&integrated, Network::Mainnet)
                .unwrap_err()
                .contains("integrated")
        );
        for bad in ["", "SEKR", "hello", &other[..98]] {
            assert!(parse_fee_address(bad, Network::Mainnet).is_err(), "{bad:?}");
        }
        assert_eq!(short_address(DEFAULT_FEE_ADDRESS), "SEKReSw2cP…kcBJZGc");
        assert_eq!(short_address("SEKRshort"), "SEKRshort");
        assert_eq!(
            donation_notice(true, false),
            "You are donating your hugin node fee to nilsjr"
        );
        assert_eq!(
            donation_notice(false, true),
            "You are donating your p2pool payout to nilsjr"
        );
        assert_eq!(
            donation_notice(true, true),
            "You are donating your hugin fee and p2pool payout to nilsjr"
        );
        assert!(donation_notice(true, true).chars().count() <= 60);
        assert_eq!(
            identity_file(Path::new("/d")),
            Path::new("/d/hugin/identity.json")
        );
        assert!(QUESTION.iter().all(|l| l.chars().count() <= 58));
    }

    /// Flag > saved > default; `set` and `keep` are saved and reach subscribers; the question
    /// is asked until it was answered once.
    #[test]
    fn precedence_persistence_and_the_question() {
        let dir = TempDir::new("precedence");
        let file = dir
            .path()
            .join("hugin")
            .join(xkr_hugin::identity::IDENTITY_FILE);

        // nothing saved: the donation, and the question
        let fee = FeeAddress::load(&file, Network::Mainnet, None).unwrap();
        assert_eq!(
            fee.get(),
            Fee {
                address: Some(DEFAULT_FEE_ADDRESS.into()),
                donation: true
            }
        );
        assert!(fee.should_ask());
        assert!(
            fee.get()
                .describe()
                .starts_with("donation to nilsjr (SEKReSw2cP…")
        );

        // kept: no question at the next start, still the donation
        fee.ask();
        assert!(fee.question_pending());
        fee.keep().unwrap();
        assert!(!fee.question_pending());
        let fee = FeeAddress::load(&file, Network::Mainnet, None).unwrap();
        assert!(fee.get().donation);
        assert!(!fee.should_ask());

        // set while running: saved, applied to subscribers
        let mut rx = fee.subscribe();
        let mine = NodeIdentity::generate().address;
        let set = fee.set(&format!(" {mine} ")).unwrap();
        assert_eq!(set.address.as_deref(), Some(mine.as_str()));
        assert!(!set.donation);
        assert!(rx.has_changed().unwrap());
        assert_eq!(
            rx.borrow_and_update().address.as_deref(),
            Some(mine.as_str())
        );
        assert!(fee.set("SEKRnope").is_err());
        assert_eq!(fee.get().address.as_deref(), Some(mine.as_str()));
        let saved = NodeIdentity::load_or_create(&file).unwrap();
        assert_eq!(saved.fee_address.as_deref(), Some(mine.as_str()));
        assert!(saved.fee_asked);

        // the flag wins and is saved
        let flagged = NodeIdentity::generate().address;
        let fee = FeeAddress::load(&file, Network::Testnet, Some(&flagged)).unwrap();
        assert_eq!(fee.get().address.as_deref(), Some(flagged.as_str()));
        assert!(!fee.should_ask());
        let fee = FeeAddress::load(&file, Network::Testnet, None).unwrap();
        assert_eq!(fee.get().address.as_deref(), Some(flagged.as_str()));
        // an invalid flag is an error, nothing changes
        assert!(
            FeeAddress::load(&file, Network::Mainnet, Some("bogus"))
                .unwrap_err()
                .starts_with("--hugin:")
        );
        assert_eq!(
            FeeAddress::load(&file, Network::Mainnet, None)
                .unwrap()
                .get()
                .address
                .as_deref(),
            Some(flagged.as_str())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// The fee address saved by an earlier hugin-node `config.json` (`payoutAddress`) is used.
    #[test]
    fn hugin_node_payout_address_is_imported() {
        let dir = TempDir::new("import");
        let file = dir.path().join(xkr_hugin::identity::IDENTITY_FILE);
        let node = NodeIdentity::generate();
        let operator = NodeIdentity::generate().address;
        std::fs::write(
            &file,
            format!(
                r#"{{ "payoutAddress": "{operator}", "private": false, "nodeId": "{}" }}"#,
                node.node_id()
            ),
        )
        .unwrap();
        let fee = FeeAddress::load(&file, Network::Mainnet, None).unwrap();
        assert_eq!(fee.get().address.as_deref(), Some(operator.as_str()));
        assert!(!fee.get().donation);
        assert!(!fee.should_ask());
    }
}
