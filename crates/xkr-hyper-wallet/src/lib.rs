//! Wallets reach a node over Hyperswarm (`docs/hyper-transport.md`, "Wallets topic").
//!
//! [`WalletSwarm::start`] starts one Bare worker of the Hyperswarm bridge (a fresh DHT key per
//! start, nothing announced). [`SwarmSource`], the [`ClientSource`] over it, opens connections
//! with service `wallet`:
//!
//! * **public:** a lookup of `xkr/<network>/wallets/v1`, then the nodes found, in turn from a
//!   rotating start, until one answers the hello as synchronized and with the state hash of
//!   the built-in list (`xkr_consensus::state_hashes`) for the span it names;
//! * **private:** the node of an invite (`xkrinv2:…`), on its private clients topic.
//!
//! The wallet tools pair it with `xkr_daemon_client::SwarmTransport`, which opens a new
//! connection whenever the old one is closed.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use tokio::sync::Mutex;
use xkr_consensus::Network;
use xkr_daemon_client::{ClientSource, TransportError};
use xkr_hyper_rpc::{Client, Hello, HelloReply, Service};
use xkr_p2p_hyper::topic::{self, HuginTopic, XkrNetwork};
use xkr_p2p_hyper::{BareConfig, BareWorkers, Bridge, BridgeConfig, Invite, OpenFlags};

/// Nodes asked for in one lookup.
const LOOKUP_MAX: u16 = 32;
/// Nodes tried per connection request before giving up.
const TRIES: usize = 8;
/// Time for one dial and hello.
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
/// Time the Bare worker gets to leave the DHT at the stop.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// What [`WalletSwarm::start`] needs.
#[derive(Debug, Clone)]
pub struct SwarmOptions {
    /// The network of the wallet.
    pub network: Network,
    /// DHT bootstrap nodes (`host:port`); empty: hyperdht's public nodes.
    pub bootstrap: Vec<String>,
    /// The bridge directory (default: next to the executable, see `BridgeConfig`).
    pub bridge_dir: Option<PathBuf>,
    /// A private node's invite: only that node is used.
    pub invite: Option<Invite>,
    /// The client name sent in the hello (`xkrwallet/1.0.0`, ...).
    pub client: String,
}

impl SwarmOptions {
    /// Options for `network` with the DHT bootstrap nodes of `XKR_HYPER_BOOTSTRAP`
    /// (comma-separated `host:port`, for a local test DHT) and the bridge directory of
    /// `XKR_BRIDGE_DIR` (else next to the executable).
    #[must_use]
    pub fn from_env(network: Network, invite: Option<Invite>, client: &str) -> Self {
        let bootstrap = std::env::var("XKR_HYPER_BOOTSTRAP").map_or_else(
            |_| Vec::new(),
            |v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            },
        );
        Self {
            network,
            bootstrap,
            bridge_dir: None,
            invite,
            client: client.to_owned(),
        }
    }
}

/// The Hyperswarm side of a wallet: one Bare worker and where to find a node.
pub struct WalletSwarm {
    bridge: Bridge,
    workers: Mutex<Option<BareWorkers>>,
    network: Network,
    target: Target,
    client: String,
    next: AtomicUsize,
}

impl std::fmt::Debug for WalletSwarm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletSwarm")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

/// The nodes a wallet may use.
enum Target {
    /// Any node on the wallets topic.
    Public(HuginTopic),
    /// The private node of an invite: its key and its private clients topic.
    Private { key: [u8; 32], topic: HuginTopic },
}

/// Why no node was reached.
fn no_node(why: impl std::fmt::Display) -> TransportError {
    TransportError::Connection(format!("no node over Hyperswarm: {why}"))
}

/// `XkrNetwork` of `network`.
#[must_use]
pub const fn xkr_network(network: Network) -> XkrNetwork {
    match network {
        Network::Mainnet => XkrNetwork::Mainnet,
        Network::Testnet => XkrNetwork::Testnet,
    }
}

impl WalletSwarm {
    /// Starts one Bare worker of the bridge.
    ///
    /// # Errors
    /// The invite has no clients key, or the bridge does not start (libbare-kit or the bridge
    /// directory missing, the DHT unreachable).
    pub async fn start(options: SwarmOptions) -> Result<Arc<Self>, String> {
        let net = xkr_network(options.network);
        let target = match &options.invite {
            Some(invite) => Target::Private {
                key: invite.clients_key.ok_or(
                    "the invite has no clients key (an xkrinv1 invite; ask for a new one)",
                )?,
                topic: invite.topics(net).clients,
            },
            None => Target::Public(topic::xkr_wallets_topic(net)),
        };
        let config = BridgeConfig {
            bridge_dir: options.bridge_dir.clone(),
            bootstrap: options.bootstrap.clone(),
            ..BridgeConfig::default()
        };
        let bare = BareConfig {
            workers: 1,
            ..BareConfig::default()
        };
        let (bridge, workers) = Bridge::start_in_process(config, &bare)
            .await
            .map_err(|e| format!("Hyperswarm: {e}"))?;
        Ok(Arc::new(Self {
            bridge,
            workers: Mutex::new(Some(workers)),
            network: options.network,
            target,
            client: options.client,
            next: AtomicUsize::new(0),
        }))
    }

    /// Stops the Bare worker (later connection requests fail).
    pub async fn stop(&self) {
        if let Some(mut w) = self.workers.lock().await.take() {
            w.stop(STOP_GRACE).await;
        }
    }

    /// Dials `key` on `topic` and says hello.
    async fn hello(&self, key: [u8; 32], topic: &HuginTopic) -> Result<Client, String> {
        let mid = topic.mid();
        let open = self
            .bridge
            .open_hugin(key, topic.topic, OpenFlags::NONE, &mid);
        let conn = tokio::time::timeout(DIAL_TIMEOUT, open)
            .await
            .map_err(|_| "no answer".to_owned())?
            .map_err(|e| e.to_string())?;
        let hello = Hello::new(
            xkr_network(self.network).as_str(),
            Service::Wallet,
            &self.client,
        );
        tokio::time::timeout(DIAL_TIMEOUT, Client::connect(conn, hello))
            .await
            .map_err(|_| "no hello".to_owned())?
            .map_err(|e| e.to_string())
    }

    async fn connect_public(&self, topic: &HuginTopic) -> Result<(Client, String), TransportError> {
        let peers = self
            .bridge
            .lookup(topic.topic, LOOKUP_MAX)
            .await
            .map_err(no_node)?;
        if peers.is_empty() {
            return Err(no_node("no node on the wallets topic yet"));
        }
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        let mut last = String::new();
        for i in 0..peers.len().min(TRIES) {
            let key = peers[(start + i) % peers.len()].public_key;
            let name = short(&key);
            match self.hello(key, topic).await {
                Ok(client) => match usable(self.network, client.hello()) {
                    Ok(()) => return Ok((client, name)),
                    Err(why) => {
                        tracing::debug!(node = %name, %why, "wallet node skipped");
                        last = why;
                    }
                },
                Err(e) => {
                    tracing::debug!(node = %name, error = %e, "wallet node not reached");
                    last = e;
                }
            }
        }
        Err(no_node(last))
    }
}

/// [`WalletSwarm`] behind an `Arc`, as the transport holds it.
#[derive(Debug, Clone)]
pub struct SwarmSource(pub Arc<WalletSwarm>);

impl ClientSource for SwarmSource {
    fn connect(&self) -> BoxFuture<'static, Result<(Client, String), TransportError>> {
        let swarm = self.0.clone();
        async move {
            match &swarm.target {
                Target::Public(topic) => swarm.connect_public(topic).await,
                Target::Private { key, topic } => {
                    let client = swarm.hello(*key, topic).await.map_err(no_node)?;
                    usable(swarm.network, client.hello()).map_err(no_node)?;
                    Ok((client, format!("private node {}", short(key))))
                }
            }
        }
        .boxed()
    }
}

/// A [`ClientSource`] that starts its [`WalletSwarm`] on the first connection request (the
/// wallet tools create one at start; the Bare worker runs only once a wallet uses Hyperswarm).
/// Clones share the swarm.
#[derive(Debug, Clone)]
pub struct LazySwarm {
    inner: Arc<LazyInner>,
}

#[derive(Debug)]
struct LazyInner {
    options: SwarmOptions,
    swarm: tokio::sync::OnceCell<Arc<WalletSwarm>>,
}

impl LazySwarm {
    /// A source that starts a swarm with `options` when first asked.
    #[must_use]
    pub fn new(options: SwarmOptions) -> Self {
        Self {
            inner: Arc::new(LazyInner {
                options,
                swarm: tokio::sync::OnceCell::new(),
            }),
        }
    }

    /// Stops the swarm if it was started.
    pub async fn stop(&self) {
        if let Some(s) = self.inner.swarm.get() {
            s.stop().await;
        }
    }
}

impl ClientSource for LazySwarm {
    fn connect(&self) -> BoxFuture<'static, Result<(Client, String), TransportError>> {
        let inner = self.inner.clone();
        async move {
            let swarm = inner
                .swarm
                .get_or_try_init(|| WalletSwarm::start(inner.options.clone()))
                .await
                .map_err(no_node)?
                .clone();
            SwarmSource(swarm).connect().await
        }
        .boxed()
    }
}

/// Whether a node's hello makes it usable for a wallet: synchronized, and its newest stable
/// state hash equal to the built-in one for that span (when the list has the span).
///
/// # Errors
/// The reason the node is not used.
pub fn usable(network: Network, reply: &HelloReply) -> Result<(), String> {
    if !reply.synced {
        return Err("the node is still synchronizing".to_owned());
    }
    if let (Some(span), Some(hash)) = (reply.state_span, reply.state_hash.as_deref())
        && let Ok(span) = u32::try_from(span)
        && let Some(want) = xkr_consensus::state_hashes::builtin(network, span)
        && want != hash
    {
        return Err(format!(
            "state hash of span {span} differs from the built-in one"
        ));
    }
    Ok(())
}

/// The first 8 hex digits of a key.
fn short(key: &[u8; 32]) -> String {
    hex::encode(&key[..4])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(synced: bool, span: Option<u64>, hash: Option<&str>) -> HelloReply {
        HelloReply {
            v: 1,
            height: 10,
            hash: String::new(),
            synced,
            services: vec!["wallet".into()],
            state_span: span,
            state_hash: hash.map(str::to_owned),
            pruning_seed: 0,
        }
    }

    #[test]
    fn a_node_is_usable_when_synced_and_its_state_hash_matches() {
        let (span0, hash0) = xkr_consensus::state_hashes::MAINNET_STATE_HASHES[0];
        let span0 = u64::from(span0);
        assert!(usable(Network::Mainnet, &reply(true, Some(span0), Some(hash0))).is_ok());
        assert!(usable(Network::Mainnet, &reply(true, None, None)).is_ok());
        assert!(usable(Network::Mainnet, &reply(false, Some(span0), Some(hash0))).is_err());
        let other = "00".repeat(32);
        assert!(usable(Network::Mainnet, &reply(true, Some(span0), Some(&other))).is_err());
        // a span above the built-in list is taken as it is
        assert!(
            usable(
                Network::Mainnet,
                &reply(true, Some(u64::from(u32::MAX)), Some(&other))
            )
            .is_ok()
        );
    }
}
