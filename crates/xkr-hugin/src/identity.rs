//! The node's Hugin identity (hugin-node `id.js`, `config.json`).
//!
//! hugin-node generates an XKR address on first start and stores
//! `nodeId = <address (99 chars)><private view key (64 hex)>` in `config.json`; it sends the
//! address to every client (`{ address, version }`), uses it as the private-node topic key and
//! hashes the view key into the DHT seed of its client-facing swarms. It keeps only the address
//! and the view key. We keep the same `nodeId` format, so a hugin-node `config.json` can be
//! imported, and also store the spend key.
//!
//! The file also holds the node operator's fee address (the address the node's shares are
//! submitted for) and whether the operator was asked for one: xkrd reads and writes these
//! through its fee address handle.
//!
//! File: `<data-dir>/hugin/identity.json`, mode 0600:
//! `{ "nodeId": "...", "spendSecretKey": "<hex>", "feeAddress": "<optional>", "feeAsked": true }`.
//! A hugin-node `payoutAddress` is read as the fee address when there is no `feeAddress` (unless
//! it is the node's own address without its spend key, hugin-node's default).

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use xkr_types::SecretKey;
use xkr_wallet_core::{Address, WalletKeys};

/// File name inside the Hugin data directory.
pub const IDENTITY_FILE: &str = "identity.json";
/// Length of a standard XKR address.
pub const ADDRESS_LEN: usize = 99;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("identity file is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid identity: {0}")]
    Invalid(&'static str),
}

/// The node identity.
#[derive(Clone)]
pub struct NodeIdentity {
    /// Standard `SEKR…` address sent to clients.
    pub address: String,
    pub view_secret: SecretKey,
    /// `None` for an imported hugin-node `config.json`.
    pub spend_secret: Option<SecretKey>,
    /// The operator's fee address (`feeAddress`, or an imported hugin-node `payoutAddress`).
    pub fee_address: Option<String>,
    /// The operator answered the fee address question (`feeAsked`).
    pub fee_asked: bool,
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("address", &self.address)
            .field("spend_secret", &self.spend_secret.is_some())
            .field("fee_address", &self.fee_address)
            .field("fee_asked", &self.fee_asked)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct IdentityFile {
    #[serde(rename = "nodeId")]
    node_id: String,
    #[serde(
        rename = "spendSecretKey",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    spend_secret_key: Option<String>,
    /// hugin-node `config.json`: read, never written.
    #[serde(rename = "payoutAddress", default, skip_serializing)]
    payout_address: Option<String>,
    #[serde(
        rename = "feeAddress",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    fee_address: Option<String>,
    #[serde(rename = "feeAsked", default, skip_serializing_if = "is_false")]
    fee_asked: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// A standard XKR address (`SEKR…` or `Xkr…`, not integrated).
#[must_use]
pub fn is_standard_address(text: &str) -> bool {
    Address::parse(text).is_ok_and(|a| a.payment_id.is_none())
}

impl NodeIdentity {
    /// A fresh deterministic wallet (view key derived from the spend key).
    #[must_use]
    pub fn generate() -> Self {
        let keys = WalletKeys::generate(&mut xkr_crypto::os_rng());
        Self {
            address: keys.address().encode(),
            view_secret: keys.view_secret,
            spend_secret: Some(keys.spend_secret),
            fee_address: None,
            fee_asked: false,
        }
    }

    /// hugin-node's `nodeId`: address followed by the private view key in hex.
    #[must_use]
    pub fn node_id(&self) -> String {
        format!(
            "{}{}",
            self.address,
            hex::encode(self.view_secret.as_bytes())
        )
    }

    /// Private view key as lowercase hex (hugin-node `NodeId.viewkey`).
    #[must_use]
    pub fn view_key_hex(&self) -> String {
        hex::encode(self.view_secret.as_bytes())
    }

    /// Parses an identity file (ours or a hugin-node `config.json`).
    pub fn from_json(text: &str) -> Result<Self, IdentityError> {
        let file: IdentityFile = serde_json::from_str(text)?;
        let raw = file.node_id.trim();
        if raw.len() < ADDRESS_LEN + 64 || !raw.is_ascii() {
            return Err(IdentityError::Invalid("nodeId is not <address><view key>"));
        }
        let (address, view) = raw.split_at(ADDRESS_LEN);
        let parsed = Address::parse(address).map_err(|_| IdentityError::Invalid("bad address"))?;
        let view_secret: SecretKey = view[..64]
            .parse()
            .map_err(|_| IdentityError::Invalid("bad view key"))?;
        let mut spend_secret = None;
        if let Some(s) = &file.spend_secret_key {
            let spend: SecretKey = s
                .parse()
                .map_err(|_| IdentityError::Invalid("bad spend key"))?;
            let keys = WalletKeys::from_secrets(&spend, &view_secret)
                .ok_or(IdentityError::Invalid("bad spend key"))?;
            if keys.spend_public != parsed.spend || keys.view_public != parsed.view {
                return Err(IdentityError::Invalid("keys do not match the address"));
            }
            spend_secret = Some(spend);
        }
        let clean = |a: Option<String>| a.map(|p| p.trim().to_owned()).filter(|p| !p.is_empty());
        let fee_address = match clean(file.fee_address) {
            Some(fee) => Some(fee),
            // hugin-node's default payout address is its own address, whose spend key it
            // does not keep: not a fee address.
            None => {
                clean(file.payout_address).filter(|p| !(p == address && spend_secret.is_none()))
            }
        };
        if let Some(p) = &fee_address
            && !is_standard_address(p)
        {
            return Err(IdentityError::Invalid("bad feeAddress"));
        }
        Ok(Self {
            address: address.to_owned(),
            view_secret,
            spend_secret,
            fee_address,
            fee_asked: file.fee_asked,
        })
    }

    /// The file content.
    #[must_use]
    pub fn to_json(&self) -> String {
        let file = IdentityFile {
            node_id: self.node_id(),
            spend_secret_key: self.spend_secret.map(|k| hex::encode(k.as_bytes())),
            payout_address: None,
            fee_address: self.fee_address.clone(),
            fee_asked: self.fee_asked,
        };
        serde_json::to_string_pretty(&file).unwrap_or_default()
    }

    /// Loads `path`, or creates a new identity there (directories included, mode 0600).
    pub fn load_or_create(path: &Path) -> Result<Self, IdentityError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::from_json(&text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let id = Self::generate();
                id.save(path)?;
                Ok(id)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Writes the identity atomically with mode 0600.
    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        {
            let mut f = opts.open(&tmp)?;
            f.write_all(self.to_json().as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_hugin_node_import() {
        let id = NodeIdentity::generate();
        assert_eq!(id.address.len(), ADDRESS_LEN);
        assert!(id.address.starts_with("SEKR"));
        assert_eq!(id.node_id().len(), ADDRESS_LEN + 64);
        let back = NodeIdentity::from_json(&id.to_json()).unwrap();
        assert_eq!(back.node_id(), id.node_id());
        assert_eq!(back.spend_secret, id.spend_secret);
        assert_eq!(back.fee_address, None);
        assert!(!back.fee_asked);

        // a hugin-node config.json: nodeId only, plus its other keys
        let cfg = format!(
            r#"{{ "payoutAddress": "{}", "private": false, "nodeId": "{}" }}"#,
            id.address,
            id.node_id()
        );
        let imported = NodeIdentity::from_json(&cfg).unwrap();
        assert!(imported.spend_secret.is_none());
        assert_eq!(imported.view_key_hex(), id.view_key_hex());
        // hugin-node's default payout address (its own address, spend key thrown away) is
        // not taken as the fee address
        assert_eq!(imported.fee_address, None);
        // an operator's payout address is
        let operator = NodeIdentity::generate();
        let cfg = format!(
            r#"{{ "payoutAddress": " {} ", "private": false, "nodeId": "{}" }}"#,
            operator.address,
            id.node_id()
        );
        let imported = NodeIdentity::from_json(&cfg).unwrap();
        assert_eq!(
            imported.fee_address.as_deref(),
            Some(operator.address.as_str())
        );
        assert!(!imported.fee_asked);
        // written back as feeAddress, never as payoutAddress
        let json = imported.to_json();
        assert!(json.contains("\"feeAddress\""), "{json}");
        assert!(!json.contains("payoutAddress"), "{json}");
        assert_eq!(
            NodeIdentity::from_json(&json).unwrap().fee_address,
            imported.fee_address
        );

        // a spend key that does not belong to the address is refused
        let other = NodeIdentity::generate();
        let bad = format!(
            r#"{{ "nodeId": "{}", "spendSecretKey": "{}" }}"#,
            id.node_id(),
            hex::encode(other.spend_secret.unwrap().as_bytes())
        );
        assert!(NodeIdentity::from_json(&bad).is_err());
        assert!(NodeIdentity::from_json(r#"{ "nodeId": "short" }"#).is_err());
    }

    /// `feeAddress` and `feeAsked` survive a save and a load; `feeAddress` wins over a
    /// hugin-node `payoutAddress`; an integrated or broken fee address is refused.
    #[test]
    fn fee_address_and_question_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hugin").join(IDENTITY_FILE);
        let mut id = NodeIdentity::load_or_create(&path).unwrap();
        assert_eq!(id.fee_address, None);
        assert!(!id.fee_asked);
        let fee = NodeIdentity::generate().address;
        id.fee_address = Some(fee.clone());
        id.fee_asked = true;
        id.save(&path).unwrap();
        let back = NodeIdentity::load_or_create(&path).unwrap();
        assert_eq!(back.fee_address.as_deref(), Some(fee.as_str()));
        assert!(back.fee_asked);
        assert_eq!(back.node_id(), id.node_id());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"feeAsked\": true"), "{text}");

        // asked, but kept the default: no feeAddress in the file
        id.fee_address = None;
        id.save(&path).unwrap();
        let back = NodeIdentity::load_or_create(&path).unwrap();
        assert_eq!(back.fee_address, None);
        assert!(back.fee_asked);

        let other = NodeIdentity::generate().address;
        let both = format!(
            r#"{{ "nodeId": "{}", "payoutAddress": "{other}", "feeAddress": "{fee}" }}"#,
            id.node_id()
        );
        assert_eq!(
            NodeIdentity::from_json(&both)
                .unwrap()
                .fee_address
                .as_deref(),
            Some(fee.as_str())
        );
        for bad in ["SEKRnope", "x"] {
            let f = format!(
                r#"{{ "nodeId": "{}", "feeAddress": "{bad}" }}"#,
                id.node_id()
            );
            assert!(NodeIdentity::from_json(&f).is_err(), "{bad}");
        }
        assert!(is_standard_address(&fee));
        assert!(!is_standard_address(""));
    }

    #[test]
    fn persisted_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hugin").join(IDENTITY_FILE);
        let a = NodeIdentity::load_or_create(&path).unwrap();
        let b = NodeIdentity::load_or_create(&path).unwrap();
        assert_eq!(a.node_id(), b.node_id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
