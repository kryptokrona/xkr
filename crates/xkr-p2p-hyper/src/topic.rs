//! Topic derivation for the HyperDHT.
//!
//! # XKR topics
//!
//! Our own topics are the raw 32-byte Keccak-256 (`cn_fast_hash`, original Keccak padding) of a
//! versioned ASCII name, e.g. `xkr/mainnet/blocks/v1`. Keccak via `xkr-crypto` was chosen over
//! SHA3-256 so the crate needs no extra hash dependency for it and the value is reproducible with
//! any CryptoNote toolkit (`cn_fast_hash`). We do *not* use Hugin's hex-fill step for our topics:
//! it would throw away half of the entropy for no interoperability gain, since no Hugin client
//! joins XKR topics.
//!
//! Stripe topics ([`xkr_stripe_topic`]) follow the same rule: `xkr/<network>/stripe/<n>/<k>/v1`
//! with `n` = stripes and `k` = stripe in `1..=n` as ASCII decimals (the two bytes of a pruning
//! seed `n << 8 | k`). A node holding stripe `k` of `n` in full announces that topic; a full node
//! announces every stripe of the default count; a syncing node that lacks a stripe looks it up
//! (`docs/hyper-transport.md`, "Stripe topics").
//!
//! # Hugin topics (domain-separated derivation of hugin-p2p 3.0)
//!
//! Hugin swarms (hugin-node, hugin-desktop, hugin-native on `hugin-p2p`) are derived from a
//! string `key`. Since the move off hyperswarm-hugin only clients with the topic and `mid`
//! derivation of hugin-p2p 3.0 can reach each other: both the DHT topic and the `mid` are
//! domain-separated ([`HUGIN_TOPIC_DOMAIN`], [`HUGIN_MID_DOMAIN`]), so a client using the earlier
//! derivation neither finds an updated peer on the DHT nor passes its `mid` check (and vice
//! versa). This is a one-time breaking change: there is no legacy mode and no fallback.
//!
//! 1. `topic_key = fill32(key)`: the UTF-8 bytes of `key`, repeated or truncated to 32 bytes
//!    (`Buffer.alloc(32).fill(key)`, [`fill32`]). For a 64-char hex key this is its first 32
//!    *characters*.
//! 2. `base = crypto_sign_seed_keypair(topic_key)`: the ed25519 topic base key pair.
//! 3. `topic = cn_fast_hash(HUGIN_TOPIC_DOMAIN || base.public_key)`: Keccak-256 with the original
//!    padding over the 11 ASCII bytes `hugin/topic` followed by the 32-byte base public key
//!    (43 bytes), [`hugin_topic`]. (The earlier derivation used `fill32(hex(base.public_key))`.)
//! 4. Each peer's `mid` is the ed25519 detached signature by `base.secret_key` over
//!    `HUGIN_MID_DOMAIN || dht_public_key` (the 9 ASCII bytes `hugin/mid` followed by its own
//!    32-byte DHT public key, 41 bytes, [`hugin_mid_message`]); a peer's `mid` is accepted only if
//!    it verifies against `base.public_key` over exactly that message. (The earlier derivation
//!    signed the bare DHT public key.) The `mid` travels in the Noise handshake payload unchanged
//!    (flag 128, 64 bytes after the upstream fields). The bridge signs and verifies it
//!    (`bridge/src/mid.js`); Rust only hands it `topic_key`.
//!
//! The keys hugin-node uses (unchanged by the new derivation):
//!
//! | swarm | key | constructor |
//! |---|---|---|
//! | node mesh (nodes find each other) | `hex(cn_fast_hash(NETWORK_ADDRESS))` | [`HuginTopic::node_mesh`] |
//! | public nodes (clients find any node) | `NETWORK_ADDRESS` | [`HuginTopic::public_nodes`] |
//! | private node (clients with an invite) | the node's XKR address | [`HuginTopic::private_node`] |
//!
//! where `NETWORK_ADDRESS` = [`HUGIN_NETWORK_ADDRESS`]. The same constants live in
//! `bridge/src/mid.js` (Bare workers) and `hugin-p2p` (apps); `docs/test-vectors/hugin-relay.json`
//! (`hugin_derivation`) pins the derived values for all three node keys.

use sha2::{Digest, Sha512};
use xkr_crypto::EdwardsPoint;

use crate::bridge::HuginMid;

/// The common Hugin network address hugin-node and the Hugin apps derive topics from.
pub const HUGIN_NETWORK_ADDRESS: &str =
    "xkr96c0f8a36e951b399681d447922f6c54c28c6ef3cad1c65d3568008151337";

/// Domain separator of a Hugin topic:
/// `topic = cn_fast_hash(HUGIN_TOPIC_DOMAIN || base_public_key)`.
pub const HUGIN_TOPIC_DOMAIN: &str = "hugin/topic";

/// Domain separator of a Hugin topic on the XKR testnet, used instead of [`HUGIN_TOPIC_DOMAIN`]
/// (`topic = cn_fast_hash(HUGIN_TESTNET_TOPIC_DOMAIN || base_public_key)`), so testnet relays
/// and apps never meet mainnet ones on the DHT. Keys, base key pairs and `mid`s are the same on
/// both networks.
pub const HUGIN_TESTNET_TOPIC_DOMAIN: &str = "hugin/testnet/topic";

/// Domain separator of the message a Hugin `mid` signs: `HUGIN_MID_DOMAIN || dht_public_key`.
pub const HUGIN_MID_DOMAIN: &str = "hugin/mid";

/// Length of the message a `mid` signs: [`HUGIN_MID_DOMAIN`] plus a 32-byte DHT public key.
pub const HUGIN_MID_MESSAGE_LEN: usize = HUGIN_MID_DOMAIN.len() + 32;

/// Which XKR network a topic belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum XkrNetwork {
    Mainnet,
    Testnet,
}

impl XkrNetwork {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            XkrNetwork::Mainnet => "mainnet",
            XkrNetwork::Testnet => "testnet",
        }
    }
}

/// `cn_fast_hash(name)` as a DHT topic.
#[must_use]
pub fn xkr_topic(name: &str) -> [u8; 32] {
    xkr_crypto::cn_fast_hash(name.as_bytes()).0
}

/// The block/tx gossip topic of a network: `xkr/<network>/blocks/v1`.
#[must_use]
pub fn xkr_blocks_topic(network: XkrNetwork) -> [u8; 32] {
    xkr_topic(&format!("xkr/{}/blocks/v1", network.as_str()))
}

/// The topic of stripe `stripe` of `stripes` (stripe pruning):
/// `xkr/<network>/stripe/<stripes>/<stripe>/v1`. Peers on it hold that stripe of the chain in
/// full (full nodes announce all of them).
#[must_use]
pub fn xkr_stripe_topic(network: XkrNetwork, stripes: u8, stripe: u8) -> [u8; 32] {
    xkr_topic(&format!(
        "xkr/{}/stripe/{stripes}/{stripe}/v1",
        network.as_str()
    ))
}

/// The topics of stripes `1..=stripes`, in order (what a full node announces).
#[must_use]
pub fn xkr_stripe_topics(network: XkrNetwork, stripes: u8) -> Vec<[u8; 32]> {
    (1..=stripes)
        .map(|k| xkr_stripe_topic(network, stripes, k))
        .collect()
}

/// `Buffer.alloc(32).fill(bytes)`: `bytes` repeated cyclically (all zeros if empty).
#[must_use]
pub fn fill32(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    if !bytes.is_empty() {
        for (i, b) in out.iter_mut().enumerate() {
            *b = bytes[i % bytes.len()];
        }
    }
    out
}

/// Hugin's `hash(text)`: lowercase hex of `cn_fast_hash(text)`.
///
/// Hugin hashes `toHex(text)` (each UTF-16 code unit as unpadded hex) through
/// kryptokrona-utils; for ASCII text from U+0010 upwards (all Hugin keys: addresses, hex) that is
/// exactly the UTF-8 bytes hashed here. Other input would diverge from Hugin.
#[must_use]
pub fn hugin_hash_key(text: &str) -> String {
    hex::encode(xkr_crypto::cn_fast_hash(text.as_bytes()).0)
}

/// ed25519 public key for a 32-byte seed (`crypto_sign_seed_keypair`): `SHA-512(seed)[..32]`
/// clamped, times the base point.
#[must_use]
pub fn ed25519_public_from_seed(seed: &[u8; 32]) -> [u8; 32] {
    let h = Sha512::digest(seed);
    let mut scalar = [0u8; 32];
    scalar.copy_from_slice(&h[..32]);
    EdwardsPoint::mul_base_clamped(scalar).compress().to_bytes()
}

/// The DHT topic of a topic base public key:
/// `cn_fast_hash(HUGIN_TOPIC_DOMAIN || base_public_key)` (43 bytes hashed).
#[must_use]
pub fn hugin_topic(base_public_key: &[u8; 32]) -> [u8; 32] {
    let mut data = Vec::with_capacity(HUGIN_TOPIC_DOMAIN.len() + 32);
    data.extend_from_slice(HUGIN_TOPIC_DOMAIN.as_bytes());
    data.extend_from_slice(base_public_key);
    xkr_crypto::cn_fast_hash(&data).0
}

/// The DHT topic of a topic base public key on `network`: [`hugin_topic`] on mainnet,
/// `cn_fast_hash(HUGIN_TESTNET_TOPIC_DOMAIN || base_public_key)` on the testnet.
#[must_use]
pub fn hugin_topic_on(network: XkrNetwork, base_public_key: &[u8; 32]) -> [u8; 32] {
    match network {
        XkrNetwork::Mainnet => hugin_topic(base_public_key),
        XkrNetwork::Testnet => {
            let mut data = Vec::with_capacity(HUGIN_TESTNET_TOPIC_DOMAIN.len() + 32);
            data.extend_from_slice(HUGIN_TESTNET_TOPIC_DOMAIN.as_bytes());
            data.extend_from_slice(base_public_key);
            xkr_crypto::cn_fast_hash(&data).0
        }
    }
}

/// The message a Hugin `mid` signs for a peer with DHT public key `dht_public_key`:
/// `HUGIN_MID_DOMAIN || dht_public_key` (41 bytes). The `mid` is the ed25519 detached signature
/// of it by the topic base secret key; the bridge signs and verifies it.
#[must_use]
pub fn hugin_mid_message(dht_public_key: &[u8; 32]) -> [u8; HUGIN_MID_MESSAGE_LEN] {
    let mut out = [0u8; HUGIN_MID_MESSAGE_LEN];
    let (domain, key) = out.split_at_mut(HUGIN_MID_DOMAIN.len());
    domain.copy_from_slice(HUGIN_MID_DOMAIN.as_bytes());
    key.copy_from_slice(dht_public_key);
    out
}

/// A Hugin swarm topic and the material the bridge needs to sign/verify `mid`s for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuginTopic {
    /// The string key the topic was derived from.
    pub key: String,
    /// `fill32(key)`: seed of the topic base key pair (TLV `0x11`).
    pub topic_key: [u8; 32],
    /// Topic base public key; every `mid` verifies against it.
    pub base_public_key: [u8; 32],
    /// The DHT topic, [`hugin_topic`]`(base_public_key)`.
    pub topic: [u8; 32],
}

impl HuginTopic {
    /// Derives everything from a Hugin key string (already hashed where Hugin hashes).
    #[must_use]
    pub fn from_key(key: &str) -> Self {
        let topic_key = fill32(key.as_bytes());
        let base_public_key = ed25519_public_from_seed(&topic_key);
        let topic = hugin_topic(&base_public_key);
        Self {
            key: key.to_owned(),
            topic_key,
            base_public_key,
            topic,
        }
    }

    /// Node-to-node mesh: key `hex(cn_fast_hash(HUGIN_NETWORK_ADDRESS))`, joined as server and
    /// client by every hugin-node.
    #[must_use]
    pub fn node_mesh() -> Self {
        Self::from_key(&hugin_hash_key(HUGIN_NETWORK_ADDRESS))
    }

    /// Public node discovery: key `HUGIN_NETWORK_ADDRESS`; public hugin-nodes join as server,
    /// Hugin apps as client.
    #[must_use]
    pub fn public_nodes() -> Self {
        Self::from_key(HUGIN_NETWORK_ADDRESS)
    }

    /// A private node: key = the node's XKR address (clients get it in the node invite).
    #[must_use]
    pub fn private_node(xkr_address: &str) -> Self {
        Self::from_key(xkr_address)
    }

    /// The same swarm on `network`: the topic of [`hugin_topic_on`] (unchanged on mainnet); key,
    /// topic key and base key stay.
    #[must_use]
    pub fn on_network(mut self, network: XkrNetwork) -> Self {
        self.topic = hugin_topic_on(network, &self.base_public_key);
        self
    }

    /// What to pass to [`crate::Bridge::announce`] / [`crate::Bridge::open_hugin`].
    #[must_use]
    pub fn mid(&self) -> HuginMid {
        HuginMid::from_topic_key(self.topic_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> [u8; 32] {
        hex::decode(s).unwrap().try_into().unwrap()
    }

    const PRIVATE_ADDRESS: &str = "SEKReYU57DLLvUjNzmjVhaK7jqc8SdZZ3cyKJS5f4gWXK4NQQYChzKUUwzCGhgqUPkWQypeR94rqpgMPjXWG9ijnZKNw2LWXnZU";

    /// `cn_fast_hash(b"hugin/topic" || base_public_key)`, spelled out here instead of calling
    /// the code under test.
    fn expected_topic(base_public_key: &[u8; 32]) -> [u8; 32] {
        let mut data = b"hugin/topic".to_vec();
        data.extend_from_slice(base_public_key);
        assert_eq!(data.len(), 43);
        xkr_crypto::cn_fast_hash(&data).0
    }

    /// The earlier (hyperswarm-hugin) topic: `Buffer.alloc(32).fill(hex(base_public_key))`,
    /// i.e. the first 32 hex characters of the base public key as ASCII.
    fn legacy_topic(base_public_key: &[u8; 32]) -> [u8; 32] {
        let text = hex::encode(base_public_key);
        text.as_bytes()[..32].try_into().unwrap()
    }

    struct NodeVector {
        topic: HuginTopic,
        topic_key: [u8; 32],
        base_public_key: [u8; 32],
        legacy_topic: [u8; 32],
    }

    fn v(topic: HuginTopic, topic_key: &str, base_public_key: &str, legacy: &str) -> NodeVector {
        NodeVector {
            topic,
            topic_key: h(topic_key),
            base_public_key: h(base_public_key),
            legacy_topic: h(legacy),
        }
    }

    /// The three node topics with their inputs (topic key, base public key; unchanged by the new
    /// derivation, computed with hugin-node's code path: kryptokrona-utils
    /// `cn_fast_hash(toHex(s))`, `Buffer.alloc(32).fill(key)`, `crypto_sign_seed_keypair`) and
    /// the hex-fill topic each of them had under the earlier derivation.
    fn node_topics() -> [NodeVector; 4] {
        [
            v(
                HuginTopic::node_mesh(),
                "6339623464383437666661393761316434396337346438303835623935323963",
                "a341b41cb1f327f229cc1a71596094e1bd05f0256d836ed135b6fe377fb874f3",
                "6133343162343163623166333237663232396363316137313539363039346531",
            ),
            v(
                HuginTopic::public_nodes(),
                "786b723936633066386133366539353162333939363831643434373932326636",
                "5f70ed4fd39ca7305ba72377d815dade9083749f3fd9d18b99c618cf81146112",
                "3566373065643466643339636137333035626137323337376438313564616465",
            ),
            v(
                HuginTopic::private_node(PRIVATE_ADDRESS),
                "53454b526559553537444c4c76556a4e7a6d6a5668614b376a71633853645a5a",
                "4137128773a971b710878df51973b42358b37fb016b6ee3bd837cdd356c42b95",
                "3431333731323837373361393731623731303837386466353139373362343233",
            ),
            v(
                HuginTopic::from_key("abc"),
                "6162636162636162636162636162636162636162636162636162636162636162",
                "204aaed5e86cd99a149c0d51d033261996cdbd67a2becb24dfaa43a4828181e8",
                "3230346161656435653836636439396131343963306435316430333332363139",
            ),
        ]
    }

    /// Testnet topics hash their own domain; mainnet topics are exactly the hugin-p2p 3.0 ones.
    #[test]
    fn hugin_testnet_topics_differ_from_mainnet() {
        let address = "SEKReYU57DLLvUjNzmjVhaK7jqc8SdZZ3cyKJS5f4gWXK4NQQYChzKUUwzCGhgqUPkWQypeR94rqpgMPjXWG9ijnZKNw2LWXnZU";
        for t in [
            HuginTopic::node_mesh(),
            HuginTopic::public_nodes(),
            HuginTopic::private_node(address),
        ] {
            let main = t.clone().on_network(XkrNetwork::Mainnet);
            assert_eq!(main, t, "mainnet is unchanged");
            assert_eq!(main.topic, hugin_topic(&t.base_public_key));
            let test = t.clone().on_network(XkrNetwork::Testnet);
            assert_ne!(test.topic, t.topic, "{}", t.key);
            assert_eq!(test.base_public_key, t.base_public_key);
            assert_eq!(test.topic_key, t.topic_key);
            assert_eq!(test.mid(), t.mid());
            let mut data = b"hugin/testnet/topic".to_vec();
            data.extend_from_slice(&t.base_public_key);
            assert_eq!(test.topic, xkr_crypto::cn_fast_hash(&data).0);
        }
        assert_eq!(HUGIN_TESTNET_TOPIC_DOMAIN, "hugin/testnet/topic");
    }

    #[test]
    fn hugin_keys_and_base_keys_match_hugin_node() {
        assert_eq!(
            hugin_hash_key(HUGIN_NETWORK_ADDRESS),
            "c9b4d847ffa97a1d49c74d8085b9529c191080704e325cef9229717a8f769614"
        );
        assert_eq!(
            HuginTopic::node_mesh().key,
            hugin_hash_key(HUGIN_NETWORK_ADDRESS)
        );
        assert_eq!(HuginTopic::public_nodes().key, HUGIN_NETWORK_ADDRESS);
        for n in node_topics() {
            let t = &n.topic;
            assert_eq!(t.topic_key, n.topic_key, "{}", t.key);
            assert_eq!(t.base_public_key, n.base_public_key, "{}", t.key);
            assert_eq!(t.mid().topic_key, Some(n.topic_key), "{}", t.key);
        }
    }

    #[test]
    fn hugin_topics_hash_the_domain_and_base_key() {
        for n in node_topics() {
            let t = &n.topic;
            assert_eq!(t.topic, expected_topic(&n.base_public_key), "{}", t.key);
            assert_eq!(hugin_topic(&n.base_public_key), t.topic, "{}", t.key);
        }
        let all: std::collections::HashSet<_> =
            node_topics().iter().map(|n| n.topic.topic).collect();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn hugin_topics_are_not_the_legacy_hex_fill_topics() {
        for n in node_topics() {
            let t = &n.topic;
            // the recorded earlier vector is what the old rule gives ...
            assert_eq!(
                legacy_topic(&n.base_public_key),
                n.legacy_topic,
                "{}",
                t.key
            );
            assert_eq!(
                fill32(hex::encode(n.base_public_key).as_bytes()),
                n.legacy_topic
            );
            // ... and is not what we join any more
            assert_ne!(t.topic, n.legacy_topic, "{}", t.key);
        }
    }

    #[test]
    fn hugin_domain_constants() {
        assert_eq!(HUGIN_TOPIC_DOMAIN, "hugin/topic");
        assert_eq!(HUGIN_MID_DOMAIN, "hugin/mid");
        assert_eq!(HUGIN_TOPIC_DOMAIN.len(), 11);
        assert_eq!(HUGIN_MID_MESSAGE_LEN, 41);
    }

    #[test]
    fn hugin_mid_message_is_domain_then_key() {
        let key = [0xabu8; 32];
        let msg = hugin_mid_message(&key);
        assert_eq!(&msg[..9], b"hugin/mid");
        assert_eq!(&msg[9..], &key);
        // the earlier mid signed the bare DHT public key
        assert_ne!(&msg[..], &key[..]);
    }

    #[test]
    fn xkr_topics() {
        // cn_fast_hash of the ASCII names, cross-checked with kryptokrona-utils.
        assert_eq!(
            xkr_blocks_topic(XkrNetwork::Mainnet),
            h("5a7d447e3718667d6b1b8b997b7007e438e5418ba70f57fbfac201c50750ae5b")
        );
        assert_eq!(
            xkr_blocks_topic(XkrNetwork::Testnet),
            h("094b227a2d677ccd034c1c1547bbe2bcce7bcb41f1c8438947b604dcce609ad2")
        );
        assert_eq!(
            xkr_topic(""),
            h("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
        );
    }

    #[test]
    fn stripe_topics() {
        // cn_fast_hash of the ASCII names (original Keccak-256 padding).
        assert_eq!(
            xkr_stripe_topic(XkrNetwork::Mainnet, 4, 1),
            h("140a9b48574ce42d940df9f4d615647b7b97e8df327d711d3147cdf2f5cee3e0")
        );
        assert_eq!(
            xkr_stripe_topic(XkrNetwork::Mainnet, 4, 4),
            h("f5d49c32ee12b4bfa77af460b9830fe6b60930ac30280971ea193cb9506d62eb")
        );
        assert_eq!(
            xkr_stripe_topic(XkrNetwork::Testnet, 4, 2),
            h("f8ad926c99e9368c86fe6876566febc3e9ded8e8814a573247aea2ee2dc0a439")
        );
        assert_eq!(
            xkr_stripe_topic(XkrNetwork::Mainnet, 6, 6),
            h("a1258da07baa317817528df50dd001eb4268fe17c34d92979b34db6bc56ff672")
        );
        assert_eq!(
            xkr_stripe_topic(XkrNetwork::Mainnet, 4, 3),
            xkr_topic("xkr/mainnet/stripe/4/3/v1")
        );

        let all = xkr_stripe_topics(XkrNetwork::Mainnet, 4);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0], xkr_stripe_topic(XkrNetwork::Mainnet, 4, 1));
        let distinct: std::collections::HashSet<_> = all
            .iter()
            .chain(&xkr_stripe_topics(XkrNetwork::Testnet, 4))
            .chain(&xkr_stripe_topics(XkrNetwork::Mainnet, 6))
            .copied()
            .chain([
                xkr_blocks_topic(XkrNetwork::Mainnet),
                xkr_blocks_topic(XkrNetwork::Testnet),
            ])
            .collect();
        assert_eq!(distinct.len(), 4 + 4 + 6 + 2);
        assert_eq!(xkr_stripe_topics(XkrNetwork::Mainnet, 0).len(), 0);
    }

    #[test]
    fn fill32_semantics() {
        assert_eq!(fill32(b""), [0u8; 32]);
        assert_eq!(fill32(&[1u8; 40]), [1u8; 32]);
        assert_eq!(&fill32(b"ab")[..4], b"abab");
    }
}
