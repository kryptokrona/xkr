//! Private node (`xkrd --private`): the secret file, what derives from it and the invite.
//!
//! A private node accepts Hyperswarm connections only on identities of its own (block sync,
//! Hugin apps, clients), and only from peers that hold its invite. Each identity uses the Hugin
//! `mid` machinery of the bridge
//! (`bridge/src/mid.js`): each is announced like a Hugin topic, with a topic key and an identity
//! seed, so the bridge gives it its own DHT key pair, signs that key as its `mid` and refuses
//! every handshake whose `mid` does not verify against the topic base key.
//!
//! # The secret file
//!
//! `<data dir>/hyper-private.json` ([`PRIVATE_FILE`]), JSON
//! `{ "secret": "<64 hex>", "identity": "<64 hex>" }`, mode 0600 on Unix, created with fresh
//! random bytes from the operating system on the first `--private` start
//! ([`PrivateSecret::load_or_create`]) and replaced by [`PrivateSecret::renew`]. `secret` is
//! shared through the invite; `identity` stays on the node.
//!
//! # Derivation
//!
//! All hashes are `cn_fast_hash` (Keccak-256, original padding) of an ASCII domain followed by
//! 32 bytes:
//!
//! | value | derivation |
//! |---|---|
//! | sync topic key | `cn_fast_hash("xkr/private/sync/v1" \|\| secret)` |
//! | Hugin topic key | `cn_fast_hash("xkr/private/hugin/v1" \|\| secret)` |
//! | clients topic key | `cn_fast_hash("xkr/private/clients/v1" \|\| secret)` |
//! | sync identity seed | `cn_fast_hash("xkr/private/sync-identity/v1" \|\| identity)` |
//! | Hugin identity seed | `cn_fast_hash("xkr/private/hugin-identity/v1" \|\| identity)` |
//! | clients identity seed | `cn_fast_hash("xkr/private/clients-identity/v1" \|\| identity)` |
//!
//! A topic key is used exactly like a Hugin topic key: `base = crypto_sign_seed_keypair(topic
//! key)`, the DHT topic is `hugin_topic_on(network, base.public_key)` (the testnet domain on
//! the testnet) and every `mid` is a signature by `base` ([`crate::topic`]). Each topic key has
//! its own base because the bridge ties one topic to one base key. An identity seed is the seed
//! of the DHT key pair the node listens with on that topic; its public key is in the invite.
//!
//! # Invite
//!
//! One line: [`INVITE_PREFIX`] followed by 256 hex characters, `sync key || Hugin key ||
//! clients key || secret` (the public keys of the three identities and the secret, 32 bytes
//! each). A peer that holds it derives the topic keys, signs its own `mid` with them and dials
//! the keys directly. A new secret (and identity) makes every earlier invite useless.

use std::fmt;
use std::io::{self, Write as _};
use std::path::Path;
use std::str::FromStr;

use rand_core::RngCore as _;

use crate::bridge::HuginMid;
use crate::topic::{self, HuginTopic, XkrNetwork};

/// The secret file in the data directory.
pub const PRIVATE_FILE: &str = "hyper-private.json";

/// Start of every invite string.
pub const INVITE_PREFIX: &str = "xkr:";

/// Hex characters after [`INVITE_PREFIX`]: four 32-byte values.
pub const INVITE_HEX_LEN: usize = 4 * 64;

/// Length of the XKR address at the start of a Hugin app invite.
pub const APP_INVITE_ADDRESS_LEN: usize = 99;

/// Length of a Hugin app invite: the address, then two 32-byte values in hex.
pub const APP_INVITE_LEN: usize = APP_INVITE_ADDRESS_LEN + 2 * 64;

/// Domain of the sync topic key.
pub const SYNC_TOPIC_DOMAIN: &str = "xkr/private/sync/v1";
/// Domain of the Hugin topic key.
pub const HUGIN_TOPIC_DOMAIN: &str = "xkr/private/hugin/v1";
/// Domain of the clients topic key.
pub const CLIENTS_TOPIC_DOMAIN: &str = "xkr/private/clients/v1";
/// Domain of the sync identity seed.
pub const SYNC_IDENTITY_DOMAIN: &str = "xkr/private/sync-identity/v1";
/// Domain of the Hugin identity seed.
pub const HUGIN_IDENTITY_DOMAIN: &str = "xkr/private/hugin-identity/v1";
/// Domain of the clients identity seed.
pub const CLIENTS_IDENTITY_DOMAIN: &str = "xkr/private/clients-identity/v1";

/// `cn_fast_hash(domain || value)`.
fn derive(domain: &str, value: &[u8; 32]) -> [u8; 32] {
    let mut data = Vec::with_capacity(domain.len() + 32);
    data.extend_from_slice(domain.as_bytes());
    data.extend_from_slice(value);
    xkr_crypto::cn_fast_hash(&data).0
}

/// The Hugin-style topic of a topic key on `network`.
fn topic_of(topic_key: [u8; 32], network: XkrNetwork) -> HuginTopic {
    let base_public_key = topic::ed25519_public_from_seed(&topic_key);
    HuginTopic {
        key: hex::encode(topic_key),
        topic_key,
        base_public_key,
        topic: topic::hugin_topic_on(network, &base_public_key),
    }
}

/// The topics of a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateTopics {
    /// Block sync (Levin) connections.
    pub sync: HuginTopic,
    /// Hugin app connections.
    pub hugin: HuginTopic,
    /// Clients of the node's services (`xkr-hyper-rpc`), the private form of the clients topic.
    pub clients: HuginTopic,
}

impl PrivateTopics {
    /// The topics of `secret` on `network`.
    #[must_use]
    pub fn of(secret: &[u8; 32], network: XkrNetwork) -> Self {
        Self {
            sync: topic_of(derive(SYNC_TOPIC_DOMAIN, secret), network),
            hugin: topic_of(derive(HUGIN_TOPIC_DOMAIN, secret), network),
            clients: topic_of(derive(CLIENTS_TOPIC_DOMAIN, secret), network),
        }
    }
}

/// The content of the secret file.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateSecret {
    /// Shared through the invite; the topic keys derive from it.
    pub secret: [u8; 32],
    /// Kept on the node; the identity seeds derive from it.
    pub identity: [u8; 32],
}

impl fmt::Debug for PrivateSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateSecret").finish_non_exhaustive()
    }
}

impl PrivateSecret {
    /// Fresh random values from the operating system.
    #[must_use]
    pub fn generate() -> Self {
        let mut rng = xkr_crypto::os_rng();
        let mut secret = [0u8; 32];
        let mut identity = [0u8; 32];
        rng.fill_bytes(&mut secret);
        rng.fill_bytes(&mut identity);
        Self { secret, identity }
    }

    /// Reads the secret file; a missing file is created with [`Self::generate`] (mode 0600 on
    /// Unix).
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse_file(&text).map_err(|e| file_error(path, &e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let s = Self::generate();
                match write_file(path, &s, true) {
                    Ok(()) => {
                        tracing::info!(file = %path.display(), "created the private node file");
                        Ok(s)
                    }
                    // Created by someone else in the meantime: use that one.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        Self::load_or_create(path)
                    }
                    Err(e) => Err(file_error(path, &e.to_string())),
                }
            }
            Err(e) => Err(file_error(path, &e.to_string())),
        }
    }

    /// Replaces the secret file with fresh values ([`Self::generate`]) and returns them. The
    /// new file is written next to the old one and renamed over it.
    pub fn renew(path: &Path) -> Result<Self, String> {
        let s = Self::generate();
        let tmp = path.with_extension("json.new");
        write_file(&tmp, &s, false)
            .and_then(|()| std::fs::rename(&tmp, path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                file_error(path, &e.to_string())
            })?;
        tracing::info!(file = %path.display(), "renewed the private node file");
        Ok(s)
    }

    fn parse_file(text: &str) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let field = |name: &str| -> Result<[u8; 32], String> {
            let hex_text = v
                .get(name)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("no \"{name}\" string"))?;
            let bytes = hex::decode(hex_text).map_err(|e| format!("{name}: {e}"))?;
            <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| format!("{name} must be 32 bytes"))
        };
        Ok(Self {
            secret: field("secret")?,
            identity: field("identity")?,
        })
    }

    /// The topics of this secret on `network`.
    #[must_use]
    pub fn topics(&self, network: XkrNetwork) -> PrivateTopics {
        PrivateTopics::of(&self.secret, network)
    }

    /// Seed of the DHT key pair of the sync identity.
    #[must_use]
    pub fn sync_identity_seed(&self) -> [u8; 32] {
        derive(SYNC_IDENTITY_DOMAIN, &self.identity)
    }

    /// Seed of the DHT key pair of the Hugin identity.
    #[must_use]
    pub fn hugin_identity_seed(&self) -> [u8; 32] {
        derive(HUGIN_IDENTITY_DOMAIN, &self.identity)
    }

    /// Seed of the DHT key pair of the clients identity.
    #[must_use]
    pub fn clients_identity_seed(&self) -> [u8; 32] {
        derive(CLIENTS_IDENTITY_DOMAIN, &self.identity)
    }

    /// What to announce the sync topic with: its topic key and the sync identity seed.
    #[must_use]
    pub fn sync_mid(&self, topics: &PrivateTopics) -> HuginMid {
        HuginMid {
            identity_seed: Some(self.sync_identity_seed()),
            ..topics.sync.mid()
        }
    }

    /// What to announce the Hugin topic with: its topic key and the Hugin identity seed.
    #[must_use]
    pub fn hugin_mid(&self, topics: &PrivateTopics) -> HuginMid {
        HuginMid {
            identity_seed: Some(self.hugin_identity_seed()),
            ..topics.hugin.mid()
        }
    }

    /// What to announce the clients topic with: its topic key and the clients identity seed.
    #[must_use]
    pub fn clients_mid(&self, topics: &PrivateTopics) -> HuginMid {
        HuginMid {
            identity_seed: Some(self.clients_identity_seed()),
            ..topics.clients.mid()
        }
    }

    /// The invite of this secret.
    #[must_use]
    pub fn invite(&self) -> Invite {
        Invite {
            sync_key: topic::ed25519_public_from_seed(&self.sync_identity_seed()),
            hugin_key: topic::ed25519_public_from_seed(&self.hugin_identity_seed()),
            clients_key: topic::ed25519_public_from_seed(&self.clients_identity_seed()),
            secret: self.secret,
        }
    }

    /// The invite for Hugin apps ([`APP_INVITE_LEN`] characters): the relay's `address`, the
    /// Hugin topic key and the Hugin identity's public key, both in hex. An app pays the relay at
    /// the address, signs its `mid` with the topic key and admits only that key. The invite
    /// reaches the Hugin identity only: the secret and the sync topic key are not in it.
    #[must_use]
    pub fn hugin_app_invite(&self, address: &str) -> String {
        format!(
            "{address}{}{}",
            hex::encode(derive(HUGIN_TOPIC_DOMAIN, &self.secret)),
            hex::encode(topic::ed25519_public_from_seed(&self.hugin_identity_seed()))
        )
    }
}

fn file_error(path: &Path, what: &str) -> String {
    format!("private node file {}: {what}", path.display())
}

/// Writes `s` to `path` (mode 0600 on Unix); `create_new`: fail if the file exists.
fn write_file(path: &Path, s: &PrivateSecret, create_new: bool) -> io::Result<()> {
    let body = format!(
        "{}\n",
        serde_json::json!({
            "secret": hex::encode(s.secret),
            "identity": hex::encode(s.identity),
        })
    );
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut f = options.open(path)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()
}

/// Why an invite string was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InviteError {
    #[error("an invite starts with xkr:")]
    Prefix,
    #[error("an invite has 256 hex characters after xkr:, not {0}")]
    Length(usize),
    #[error("an invite is hex after its prefix")]
    Hex,
}

/// A private node's invite ([`INVITE_PREFIX`] + hex of the four values).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Invite {
    /// Public key of the node's sync identity.
    pub sync_key: [u8; 32],
    /// Public key of the node's Hugin identity.
    pub hugin_key: [u8; 32],
    /// Public key of the node's clients identity.
    pub clients_key: [u8; 32],
    /// The node's secret.
    pub secret: [u8; 32],
}

impl fmt::Debug for Invite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Invite")
            .field("sync_key", &hex::encode(self.sync_key))
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Invite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{INVITE_PREFIX}{}{}{}{}",
            hex::encode(self.sync_key),
            hex::encode(self.hugin_key),
            hex::encode(self.clients_key),
            hex::encode(self.secret)
        )
    }
}

impl FromStr for Invite {
    type Err = InviteError;

    /// Surrounding white space is ignored; hex digits may be upper or lower case.
    fn from_str(s: &str) -> Result<Self, InviteError> {
        let rest = s
            .trim()
            .strip_prefix(INVITE_PREFIX)
            .ok_or(InviteError::Prefix)?;
        if rest.len() != INVITE_HEX_LEN {
            return Err(InviteError::Length(rest.len()));
        }
        let bytes = hex::decode(rest).map_err(|_| InviteError::Hex)?;
        let part = |i: usize| -> [u8; 32] {
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes[i * 32..(i + 1) * 32]);
            out
        };
        Ok(Self {
            sync_key: part(0),
            hugin_key: part(1),
            clients_key: part(2),
            secret: part(3),
        })
    }
}

impl Invite {
    /// The topics of the invite's secret on `network`.
    #[must_use]
    pub fn topics(&self, network: XkrNetwork) -> PrivateTopics {
        PrivateTopics::of(&self.secret, network)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PrivateSecret {
        PrivateSecret {
            secret: [0x11; 32],
            identity: [0x22; 32],
        }
    }

    #[test]
    fn invite_round_trip() {
        let invite = sample().invite();
        let text = invite.to_string();
        assert!(text.starts_with("xkr:"));
        assert_eq!(text.len(), INVITE_PREFIX.len() + INVITE_HEX_LEN);
        assert!(text.ends_with(&"11".repeat(32)), "the secret comes last");
        assert_eq!(text.parse::<Invite>(), Ok(invite));
        let upper = format!(
            "{INVITE_PREFIX}{}",
            text[INVITE_PREFIX.len()..].to_uppercase()
        );
        assert_eq!(format!("  {upper}\n").parse::<Invite>(), Ok(invite));
        // the secret does not show in Debug
        assert!(!format!("{invite:?}").contains(&"11".repeat(32)));
        assert!(!format!("{:?}", sample()).contains("1111"));
    }

    #[test]
    fn hugin_app_invite() {
        let s = sample();
        let address = format!("SEKR{}", "a".repeat(APP_INVITE_ADDRESS_LEN - 4));
        let text = s.hugin_app_invite(&address);
        assert_eq!(text.len(), APP_INVITE_LEN);
        let (addr, rest) = text.split_at(APP_INVITE_ADDRESS_LEN);
        let (topic_key, key) = rest.split_at(64);
        assert_eq!(addr, address);
        let t = s.topics(XkrNetwork::Mainnet);
        assert_eq!(topic_key, hex::encode(t.hugin.topic_key));
        // the same topic key and DHT topic as hugin-network's huginTopicFromKey of those bytes
        assert_eq!(
            topic_key,
            "1757f41fcc392193a50601446066a6b19ddff9ab2422ea8558289ae425070483"
        );
        assert_eq!(
            hex::encode(t.hugin.topic),
            "f1e836184d1e8d39ded91d16044881d9560dbd9ab09b1dba8f8cc6f8e537ea8c"
        );
        assert_eq!(key, hex::encode(s.invite().hugin_key));
        assert!(!text.contains(&hex::encode(s.secret)));
        assert!(!text.contains(&hex::encode(t.sync.topic_key)));
    }

    #[test]
    fn bad_invites_are_refused() {
        let good = sample().invite().to_string();
        let hex_part = &good[INVITE_PREFIX.len()..];
        assert_eq!(
            format!("xkrinv:{hex_part}").parse::<Invite>(),
            Err(InviteError::Prefix)
        );
        assert_eq!(hex_part.parse::<Invite>(), Err(InviteError::Prefix));
        assert_eq!("".parse::<Invite>(), Err(InviteError::Prefix));
        assert_eq!(
            good[..good.len() - 2].parse::<Invite>(),
            Err(InviteError::Length(INVITE_HEX_LEN - 2))
        );
        assert_eq!(
            format!("{good}00").parse::<Invite>(),
            Err(InviteError::Length(INVITE_HEX_LEN + 2))
        );
        assert_eq!(
            format!("{}zz", &good[..good.len() - 2]).parse::<Invite>(),
            Err(InviteError::Hex)
        );
        assert_eq!("xkr:".parse::<Invite>(), Err(InviteError::Length(0)));
    }

    /// The clients key is the third value of the invite.
    #[test]
    fn the_invite_carries_the_clients_key() {
        let s = sample();
        let invite = s.invite();
        assert_eq!(
            invite.clients_key,
            topic::ed25519_public_from_seed(&s.clients_identity_seed())
        );
        let text = invite.to_string();
        assert_eq!(
            &text[INVITE_PREFIX.len() + 128..INVITE_PREFIX.len() + 192],
            hex::encode(invite.clients_key)
        );
    }

    /// The derivation spelled out: topic keys from the secret, identity seeds from the
    /// identity, each under its own domain; the topics are Hugin topics of their base keys.
    #[test]
    fn derivation() {
        let s = sample();
        let hash = |domain: &str, v: [u8; 32]| {
            let mut data = domain.as_bytes().to_vec();
            data.extend_from_slice(&v);
            xkr_crypto::cn_fast_hash(&data).0
        };
        for network in [XkrNetwork::Mainnet, XkrNetwork::Testnet] {
            let t = s.topics(network);
            assert_eq!(t.sync.topic_key, hash("xkr/private/sync/v1", s.secret));
            assert_eq!(t.hugin.topic_key, hash("xkr/private/hugin/v1", s.secret));
            assert_eq!(
                t.clients.topic_key,
                hash("xkr/private/clients/v1", s.secret)
            );
            for h in [&t.sync, &t.hugin, &t.clients] {
                assert_eq!(
                    h.base_public_key,
                    topic::ed25519_public_from_seed(&h.topic_key)
                );
                assert_eq!(h.topic, topic::hugin_topic_on(network, &h.base_public_key));
                assert_eq!(h.key, hex::encode(h.topic_key));
            }
            assert_ne!(t.sync.topic, t.hugin.topic);
            assert_ne!(t.clients.topic, t.sync.topic);
            assert_ne!(t.clients.topic, t.hugin.topic);
            assert_ne!(
                t.clients.topic,
                topic::xkr_clients_topic(network).topic,
                "the private clients topic is not the public one"
            );
            let clients = s.clients_mid(&t);
            assert_eq!(clients.topic_key, Some(t.clients.topic_key));
            assert_eq!(clients.identity_seed, Some(s.clients_identity_seed()));
            assert_eq!(
                t,
                s.invite().topics(network),
                "the invite gives the same topics"
            );
            let sync = s.sync_mid(&t);
            assert_eq!(sync.topic_key, Some(t.sync.topic_key));
            assert_eq!(sync.identity_seed, Some(s.sync_identity_seed()));
            assert_eq!(sync.mid, None);
            let hugin = s.hugin_mid(&t);
            assert_eq!(hugin.topic_key, Some(t.hugin.topic_key));
            assert_eq!(hugin.identity_seed, Some(s.hugin_identity_seed()));
        }
        assert_ne!(
            s.topics(XkrNetwork::Mainnet).sync.topic,
            s.topics(XkrNetwork::Testnet).sync.topic
        );
        assert_eq!(
            s.sync_identity_seed(),
            hash("xkr/private/sync-identity/v1", s.identity)
        );
        assert_eq!(
            s.hugin_identity_seed(),
            hash("xkr/private/hugin-identity/v1", s.identity)
        );
        assert_eq!(
            s.clients_identity_seed(),
            hash("xkr/private/clients-identity/v1", s.identity)
        );
        let invite = s.invite();
        assert_eq!(
            invite.sync_key,
            topic::ed25519_public_from_seed(&s.sync_identity_seed())
        );
        assert_eq!(
            invite.hugin_key,
            topic::ed25519_public_from_seed(&s.hugin_identity_seed())
        );
        assert_eq!(invite.secret, s.secret);
        // the identity changes the keys, not the topics
        let other = PrivateSecret {
            identity: [0x33; 32],
            ..sample()
        };
        assert_eq!(
            other.topics(XkrNetwork::Mainnet),
            s.topics(XkrNetwork::Mainnet)
        );
        assert_ne!(other.invite().sync_key, invite.sync_key);
    }

    #[test]
    fn the_file_is_created_once_read_back_and_renewed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join(PRIVATE_FILE);
        let s = PrivateSecret::load_or_create(&file).expect("create");
        assert_ne!(s.secret, s.identity);
        assert_eq!(PrivateSecret::load_or_create(&file).expect("read"), s);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let text = std::fs::read_to_string(&file).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["secret"], hex::encode(s.secret));
        assert_eq!(v["identity"], hex::encode(s.identity));

        let renewed = PrivateSecret::renew(&file).expect("renew");
        assert_ne!(renewed.secret, s.secret);
        assert_ne!(renewed.identity, s.identity);
        assert_eq!(PrivateSecret::load_or_create(&file).expect("read"), renewed);
        assert!(!file.with_extension("json.new").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let no_identity = format!("{{\"secret\": \"{}\"}}", "11".repeat(32));
        for bad in [
            "not json",
            "{}",
            "{\"secret\": \"1234\", \"identity\": \"1234\"}",
            no_identity.as_str(),
        ] {
            std::fs::write(&file, bad).unwrap();
            let e = PrivateSecret::load_or_create(&file).unwrap_err();
            assert!(e.starts_with("private node file "), "{e}");
        }
    }
}
