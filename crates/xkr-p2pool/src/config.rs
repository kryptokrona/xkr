//! Sidechain configuration and consensus id (`SideChain::SideChain`, `SideChain::load_config`,
//! `SideChain::check_config`, `src/side_chain.cpp`).
//!
//! The consensus id names a sidechain: it is appended to every share before hashing (share ids),
//! enters the P2P handshake and seeds the genesis share. It is computed from the text
//! `network\0name\0password\0block_time\0min_diff\0pplns_window\0uncle_penalty\0` (decimal
//! numbers); the network is always `mainnet` because Kryptokrona addresses carry none. The three
//! built-in configurations have hard-coded ids; any other text is hashed with
//! CryptoNight-Turtle-Lite v2.

use xkr_crypto::cn_turtle_lite_slow_hash_v2;
use xkr_types::Hash;

use crate::difficulty::Difficulty;

/// The network word of the consensus text (always mainnet, see the module docs).
const NETWORK: &str = "mainnet";
/// `MIN_DIFFICULTY` (`src/side_chain.cpp`).
pub const MIN_DIFFICULTY: u64 = 100_000;
/// Upper bound of `min_diff` on a mainnet-typed sidechain.
pub const MAX_MIN_DIFFICULTY: u64 = 1_000_000_000;
/// Main-chain block time in seconds (`coin::MAIN_CHAIN_BLOCK_TIME`, used as
/// `MONERO_BLOCK_TIME`).
pub const MAIN_CHAIN_BLOCK_TIME: u64 = 90;
/// `MAX_PPLNS_WINDOW_HOURS`.
pub const MAX_PPLNS_WINDOW_HOURS: u64 = 30;
/// `UNCLE_BLOCK_DEPTH`: how far back an uncle may be.
pub const UNCLE_BLOCK_DEPTH: u64 = 3;

/// `default_consensus_id`: the live Kryptokrona sidechain.
pub const DEFAULT_CONSENSUS_ID: [u8; 32] = [
    34, 175, 126, 231, 181, 11, 104, 146, 227, 153, 218, 107, 44, 108, 68, 39, 178, 81, 4, 212,
    169, 4, 142, 0, 177, 110, 157, 240, 68, 7, 249, 24,
];
/// `mini_consensus_id`.
pub const MINI_CONSENSUS_ID: [u8; 32] = [
    57, 130, 201, 26, 149, 174, 199, 250, 66, 80, 189, 18, 108, 216, 194, 220, 136, 23, 63, 24, 64,
    113, 221, 44, 219, 86, 39, 163, 53, 24, 126, 196,
];
/// `nano_consensus_id`.
pub const NANO_CONSENSUS_ID: [u8; 32] = [
    171, 248, 206, 148, 210, 226, 114, 99, 250, 145, 221, 96, 13, 216, 23, 63, 104, 53, 129, 168,
    244, 80, 141, 138, 157, 250, 50, 54, 37, 189, 5, 89,
];

/// Default P2P port of the default sidechain (`DEFAULT_P2P_PORT`, `src/p2p_server.h`).
pub const DEFAULT_P2P_PORT: u16 = 37889;
/// Default P2P port of the mini sidechain.
pub const DEFAULT_P2P_PORT_MINI: u16 = 37888;
/// Default P2P port of the nano sidechain.
pub const DEFAULT_P2P_PORT_NANO: u16 = 37890;

/// Why a configuration was refused (the messages of `SideChain::check_config` /
/// `load_config`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The JSON text does not parse or is not an object.
    #[error("invalid sidechain config JSON: {0}")]
    Json(String),
    /// `name` is empty.
    #[error("name can't be empty")]
    EmptyName,
    /// `name` is longer than 128 bytes.
    #[error("name is too long (must be 128 characters max)")]
    NameTooLong,
    /// `password` is longer than 128 bytes.
    #[error("password is too long (must be 128 characters max)")]
    PasswordTooLong,
    /// `block_time` outside `1..=90`.
    #[error("block_time is invalid (must be between 1 and {MAIN_CHAIN_BLOCK_TIME})")]
    BlockTime,
    /// `min_diff` outside `100000..=1000000000`.
    #[error("min_diff is invalid (must be between {MIN_DIFFICULTY} and {MAX_MIN_DIFFICULTY})")]
    MinDiff,
    /// `pplns_window` outside `60..=2160`.
    #[error("pplns_window is invalid (must be between 60 and 2160)")]
    PplnsWindow,
    /// `uncle_penalty` outside `1..=99`.
    #[error("uncle_penalty is invalid (must be between 1 and 99)")]
    UnclePenalty,
    /// `pplns_window * block_time` above 30 hours.
    #[error("PPLNS window is too large (more than {MAX_PPLNS_WINDOW_HOURS} hours)")]
    WindowTooLong,
}

/// A sidechain configuration (`m_poolName`, `m_poolPassword`, `m_targetBlockTime`,
/// `m_minDifficulty`, `m_chainWindowSize`, `m_unclePenalty`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidechainConfig {
    /// Sidechain name.
    pub name: String,
    /// Password (part of the consensus id only).
    pub password: String,
    /// Target share time `T` in seconds.
    pub block_time: u64,
    /// Lowest share difficulty.
    pub min_diff: u64,
    /// PPLNS window `W` in shares.
    pub pplns_window: u64,
    /// Uncle penalty in percent.
    pub uncle_penalty: u64,
}

impl Default for SidechainConfig {
    fn default() -> Self {
        Self::preset("default")
    }
}

impl SidechainConfig {
    /// The C++ constructor defaults for `pool_name`: 10 s, 100 000, 2 160, 20 %; `nano` uses 30 s
    /// and 10 %.
    fn preset(name: &str) -> Self {
        let nano = name == "nano";
        Self {
            name: name.to_owned(),
            password: String::new(),
            block_time: if nano { 30 } else { 10 },
            min_diff: MIN_DIFFICULTY,
            pplns_window: 2160,
            uncle_penalty: if nano { 10 } else { 20 },
        }
    }

    /// The default sidechain (`p2pool` without options; the live Kryptokrona sidechain).
    #[must_use]
    pub fn main() -> Self {
        Self::preset("default")
    }

    /// `--mini`.
    #[must_use]
    pub fn mini() -> Self {
        Self::preset("mini")
    }

    /// `--nano`.
    #[must_use]
    pub fn nano() -> Self {
        Self::preset("nano")
    }

    /// C++ `load_config` on top of the default: the keys `name`, `password`, `block_time`,
    /// `min_diff` (0 keeps the default), `pplns_window` and `uncle_penalty`, each taken only when
    /// it has the right JSON type; `//` and `/* */` comments are allowed. The result is checked
    /// ([`Self::check`]).
    pub fn from_json(text: &str) -> Result<Self, ConfigError> {
        let doc: serde_json::Value = serde_json::from_str(&strip_json_comments(text))
            .map_err(|e| ConfigError::Json(e.to_string()))?;
        let obj = doc
            .as_object()
            .ok_or_else(|| ConfigError::Json("top level is not an object".into()))?;
        let mut c = Self::main();
        if let Some(s) = obj.get("name").and_then(serde_json::Value::as_str) {
            s.clone_into(&mut c.name);
        }
        if let Some(s) = obj.get("password").and_then(serde_json::Value::as_str) {
            s.clone_into(&mut c.password);
        }
        let number = |key: &str| obj.get(key).and_then(serde_json::Value::as_u64);
        if let Some(v) = number("block_time") {
            c.block_time = v;
        }
        if let Some(v) = number("min_diff").filter(|v| *v != 0) {
            c.min_diff = v;
        }
        if let Some(v) = number("pplns_window") {
            c.pplns_window = v;
        }
        if let Some(v) = number("uncle_penalty") {
            c.uncle_penalty = v;
        }
        c.check()?;
        Ok(c)
    }

    /// C++ `check_config` (mainnet-typed, so the `min_diff` bounds apply).
    pub fn check(&self) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::EmptyName);
        }
        if self.name.len() > 128 {
            return Err(ConfigError::NameTooLong);
        }
        if self.password.len() > 128 {
            return Err(ConfigError::PasswordTooLong);
        }
        if !(1..=MAIN_CHAIN_BLOCK_TIME).contains(&self.block_time) {
            return Err(ConfigError::BlockTime);
        }
        if !(MIN_DIFFICULTY..=MAX_MIN_DIFFICULTY).contains(&self.min_diff) {
            return Err(ConfigError::MinDiff);
        }
        if !(60..=2160).contains(&self.pplns_window) {
            return Err(ConfigError::PplnsWindow);
        }
        if !(1..=99).contains(&self.uncle_penalty) {
            return Err(ConfigError::UnclePenalty);
        }
        if self.pplns_window.saturating_mul(self.block_time) > MAX_PPLNS_WINDOW_HOURS * 3600 {
            return Err(ConfigError::WindowTooLong);
        }
        Ok(())
    }

    /// `m_minDifficulty`.
    #[must_use]
    pub const fn min_difficulty(&self) -> Difficulty {
        Difficulty::from_u64(self.min_diff)
    }

    /// The text the consensus id is computed from.
    #[must_use]
    pub fn consensus_text(&self) -> Vec<u8> {
        let mut s = Vec::with_capacity(64 + self.name.len() + self.password.len());
        for part in [
            NETWORK,
            &self.name,
            &self.password,
            &self.block_time.to_string(),
            &self.min_diff.to_string(),
            &self.pplns_window.to_string(),
            &self.uncle_penalty.to_string(),
        ] {
            s.extend_from_slice(part.as_bytes());
            s.push(0);
        }
        s
    }

    /// The consensus id (`m_consensusId`, also `m_consensusHash`): the hard-coded id of the
    /// default, mini or nano text, otherwise `cn_turtle_lite_slow_hash_v2(text)`.
    #[must_use]
    pub fn consensus_id(&self) -> Hash {
        let text = self.consensus_text();
        for (preset, id) in [
            (Self::main(), DEFAULT_CONSENSUS_ID),
            (Self::mini(), MINI_CONSENSUS_ID),
            (Self::nano(), NANO_CONSENSUS_ID),
        ] {
            if text == preset.consensus_text() {
                return Hash(id);
            }
        }
        cn_turtle_lite_slow_hash_v2(&text)
    }

    /// C++ `is_default()`: the consensus id is the default one (same text).
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.consensus_text() == Self::main().consensus_text()
    }

    /// C++ `is_mini()`.
    #[must_use]
    pub fn is_mini(&self) -> bool {
        self.consensus_text() == Self::mini().consensus_text()
    }

    /// C++ `is_nano()`.
    #[must_use]
    pub fn is_nano(&self) -> bool {
        self.consensus_text() == Self::nano().consensus_text()
    }

    /// Default P2P port: `37888` on mini, `37890` on nano, otherwise `37889`.
    #[must_use]
    pub fn default_p2p_port(&self) -> u16 {
        if self.is_mini() {
            DEFAULT_P2P_PORT_MINI
        } else if self.is_nano() {
            DEFAULT_P2P_PORT_NANO
        } else {
            DEFAULT_P2P_PORT
        }
    }

    /// `(W - 1) * 2 + U`: deeper shares are accepted without checks (pruning horizon).
    #[must_use]
    pub const fn unchecked_depth(&self) -> u64 {
        (self.pplns_window - 1) * 2 + UNCLE_BLOCK_DEPTH
    }

    /// `monero_headers_required()`: main-chain headers kept, `W·4·T / 90`.
    #[must_use]
    pub const fn main_chain_headers_required(&self) -> u64 {
        self.pplns_window * 4 * self.block_time / MAIN_CHAIN_BLOCK_TIME
    }
}

/// The consensus id shown on screen and in logs: the first and last 8 hex digits
/// (`22af7ee7…4407f918`).
#[must_use]
pub fn short_id(id: &Hash) -> String {
    let hex = id.to_hex();
    format!("{}…{}", &hex[..8], &hex[56..])
}

/// Removes `//` line comments and `/* */` block comments outside JSON strings (rapidjson's
/// `kParseCommentsFlag`).
fn strip_json_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = '\0';
                for n in chars.by_ref() {
                    if last == '*' && n == '/' {
                        break;
                    }
                    last = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_consensus_ids() {
        assert_eq!(
            SidechainConfig::main().consensus_id().to_hex(),
            "22af7ee7b50b6892e399da6b2c6c4427b25104d4a9048e00b16e9df04407f918"
        );
        assert_eq!(
            SidechainConfig::mini().consensus_id().to_hex(),
            "3982c91a95aec7fa4250bd126cd8c2dc88173f184071dd2cdb5627a335187ec4"
        );
        assert_eq!(
            SidechainConfig::nano().consensus_id().to_hex(),
            "abf8ce94d2e27263fa91dd600dd8173f683581a8f4508d8a9dfa323625bd0559"
        );
        assert_eq!(
            SidechainConfig::main().consensus_text(),
            b"mainnet\x00default\x00\x0010\x00100000\x002160\x0020\x00".to_vec()
        );
        assert_eq!(short_id(&Hash(DEFAULT_CONSENSUS_ID)), "22af7ee7…4407f918");
        for c in [
            SidechainConfig::main(),
            SidechainConfig::mini(),
            SidechainConfig::nano(),
        ] {
            assert_eq!(c.check(), Ok(()));
        }
        assert_eq!(SidechainConfig::nano().default_p2p_port(), 37890);
        assert_eq!(SidechainConfig::main().main_chain_headers_required(), 960);
        assert_eq!(SidechainConfig::main().unchecked_depth(), 4321);
    }

    /// `p2pool/sidechain-testnet.json` of the testnet suite: the C++ node logs `9b8712b7…53248686`.
    #[test]
    fn testnet_suite_config() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../p2pool/sidechain-testnet.json"
        );
        let text = std::fs::read_to_string(path).unwrap();
        let c = SidechainConfig::from_json(&text).unwrap();
        assert_eq!(c.name, "xkr-testnet-suite");
        assert_eq!(c.pplns_window, 60);
        let id = c.consensus_id().to_hex();
        assert!(
            id.starts_with("9b8712b7") && id.ends_with("53248686"),
            "{id}"
        );
    }

    #[test]
    fn json_rules() {
        let c = SidechainConfig::from_json(
            "/* a comment */ { \"name\": \"x // not a comment\", // line\n \"min_diff\": 0, \
             \"block_time\": \"20\", \"pplns_window\": 100 }",
        )
        .unwrap();
        assert_eq!(c.name, "x // not a comment");
        assert_eq!(c.min_diff, MIN_DIFFICULTY);
        assert_eq!(c.block_time, 10);
        assert_eq!(c.pplns_window, 100);
        assert!(matches!(
            SidechainConfig::from_json("[]"),
            Err(ConfigError::Json(_))
        ));
        assert_eq!(
            SidechainConfig::from_json("{\"name\": \"\"}"),
            Err(ConfigError::EmptyName)
        );
        assert_eq!(
            SidechainConfig::from_json("{\"min_diff\": 99999}"),
            Err(ConfigError::MinDiff)
        );
        assert_eq!(
            SidechainConfig::from_json("{\"block_time\": 91}"),
            Err(ConfigError::BlockTime)
        );
        assert_eq!(
            SidechainConfig::from_json("{\"pplns_window\": 59}"),
            Err(ConfigError::PplnsWindow)
        );
        assert_eq!(
            SidechainConfig::from_json("{\"uncle_penalty\": 100}"),
            Err(ConfigError::UnclePenalty)
        );
        assert_eq!(
            SidechainConfig::from_json("{\"block_time\": 60, \"pplns_window\": 2160}"),
            Err(ConfigError::WindowTooLong)
        );
        // A JSON text equal to a preset gets the preset's id.
        assert_eq!(
            SidechainConfig::from_json("{\"name\": \"mini\"}")
                .unwrap()
                .consensus_id(),
            Hash(MINI_CONSENSUS_ID)
        );
    }
}
