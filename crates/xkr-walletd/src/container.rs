//! `--generate-container` (`src/wallet_service/wallet_service.cpp` `generateNewWallet`): a new
//! walletd container from fresh keys, a mnemonic seed or a spend and view key, written without
//! talking to a node (C++ uses `NodeFactory::createNodeStub`).
//!
//! As in C++, a bad seed or key is logged and the program ends with exit code 0; only a file
//! that cannot be created ends it with an error.

use xkr_consensus::Network;
use xkr_daemon_client::{DaemonClient, HttpRequest, HttpResponse, Transport, TransportError};
use xkr_types::SecretKey;
use xkr_wallet::{ErrorCode, FileFormat, Node, SyncConfig, Wallet, WalletConfig};
use xkr_wallet_core::secret_from_mnemonic;

use crate::config::Config;
use crate::errors::AppError;
use crate::model;

/// A daemon that is never there (`NodeRpcStub`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDaemon;

impl Transport for NoDaemon {
    fn send(
        &self,
        _request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send {
        std::future::ready(Err(TransportError::Connection(
            "no daemon while generating".into(),
        )))
    }
}

/// What `--generate-container` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Generated {
    /// The container was written; its address.
    Created(String),
    /// Nothing was written; the message C++ logs (and the program still exits with 0).
    Refused(String),
}

/// The engine settings for a new container: format B, no sync.
fn new_container_config(network: Network) -> WalletConfig {
    WalletConfig {
        network,
        format: FileFormat::WalletGreen,
        sync: SyncConfig::default(),
        start_sync: false,
    }
}

/// `generateNewWallet`. `Err` is a fatal error (`Fatal error: …`, exit code 1).
pub async fn generate(config: &Config) -> Result<Generated, String> {
    let node = || {
        Node::new(
            DaemonClient::new(NoDaemon),
            config.daemon_address.clone(),
            0,
        )
    };
    let settings = new_container_config(config.network);
    let path = &config.container_file;
    let password = &config.container_password;
    let fatal = |e: &xkr_wallet::WalletError| -> String {
        if e.code() == ErrorCode::WalletFileAlreadyExists {
            "MemoryMappedFile::create: File exists".to_owned()
        } else {
            AppError::from(e).message
        }
    };

    let wallet = if config.spend_key.is_empty()
        && config.view_key.is_empty()
        && config.mnemonic_seed.is_empty()
    {
        tracing::info!("Generating new wallet");
        Wallet::create(path, password, node(), settings)
            .await
            .map_err(|e| fatal(&e))?
    } else if !config.mnemonic_seed.is_empty() {
        tracing::info!("Attempting to import wallet from mnemonic seed");
        if let Err(e) = secret_from_mnemonic(&config.mnemonic_seed) {
            let message = xkr_wallet::WalletError::from(e).message().to_owned();
            tracing::error!("{message}");
            return Ok(Generated::Refused(message));
        }
        Wallet::import_seed(
            path,
            password,
            &config.mnemonic_seed,
            config.scan_height,
            node(),
            settings,
        )
        .await
        .map_err(|e| fatal(&e))?
    } else {
        if config.spend_key.is_empty() || config.view_key.is_empty() {
            let message = "Need both secret spend key and secret view key.".to_owned();
            tracing::error!("{message}");
            return Ok(Generated::Refused(message));
        }
        tracing::info!("Attemping to import wallet from keys");
        let Some(spend) = model::hex32(&config.spend_key).map(SecretKey) else {
            tracing::error!("Invalid spend key");
            return Ok(Generated::Refused("Invalid spend key".to_owned()));
        };
        let Some(view) = model::hex32(&config.view_key).map(SecretKey) else {
            tracing::error!("Invalid view key");
            return Ok(Generated::Refused("Invalid view key".to_owned()));
        };
        match Wallet::import_keys(
            path,
            password,
            spend,
            view,
            config.scan_height,
            node(),
            settings,
        )
        .await
        {
            Ok(w) => w,
            Err(e) if e.code() == ErrorCode::InvalidKeyFormat => {
                tracing::error!("Invalid spend key");
                return Ok(Generated::Refused("Invalid spend key".to_owned()));
            }
            Err(e) => return Err(fatal(&e)),
        }
    };
    let address = wallet.primary_address();
    tracing::info!("New wallet is generated. Address: {address}");
    wallet.close().await.map_err(|e| fatal(&e))?;
    tracing::info!("Wallet is saved");
    Ok(Generated::Created(address))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seed and keys of the C++ fixtures (`generate/seed-a.txt`, `keys-k-*.txt`).
    const SEED_A: &str = "skulls woozy ouch summon gifts huts waffle ourselves obtains hexagon \
                          tadpoles hacksaw dormant hence abort listen history atom cadets \
                          stylishly snout vegan girth guest history";
    const ADDRESS_A: &str = "SEKReX4MYn2HAJFoHvcqVBPyHYom2ynKeK6dpYptbp8gQNzdzE73ZDkNmNurqfhhcMSUXpS1ZGEJKiKJUcPCyw7vYaCc35yBZCf";

    fn config(dir: &std::path::Path, name: &str) -> Config {
        let mut c = Config::defaults(Network::Testnet);
        c.container_file = dir.join(name).to_str().unwrap().to_owned();
        c.container_password = "walletd-pass".into();
        c.generate = true;
        c
    }

    #[tokio::test]
    async fn generate_paths() {
        let dir = tempfile::tempdir().unwrap();

        // from the seed: the C++ address
        let mut c = config(dir.path(), "a.wallet");
        c.mnemonic_seed = SEED_A.into();
        assert_eq!(
            generate(&c).await.unwrap(),
            Generated::Created(ADDRESS_A.to_owned())
        );
        // the container opens with the engine and holds the same address
        let opened = Wallet::open(
            &c.container_file,
            "walletd-pass",
            Node::new(DaemonClient::new(NoDaemon), "x", 0),
            new_container_config(Network::Testnet),
        )
        .await
        .unwrap();
        assert_eq!(opened.addresses(), vec![ADDRESS_A.to_owned()]);
        assert_eq!(opened.format(), FileFormat::WalletGreen);
        opened.close().await.unwrap();

        // the file exists: fatal, as `MemoryMappedFile::create`
        assert_eq!(
            generate(&c).await,
            Err("MemoryMappedFile::create: File exists".to_owned())
        );

        // a bad checksum word: logged, nothing written
        let mut c = config(dir.path(), "bad-seed.wallet");
        c.mnemonic_seed = SEED_A.replace("guest history", "guest girth");
        assert!(matches!(generate(&c).await, Ok(Generated::Refused(_))));
        assert!(!std::path::Path::new(&c.container_file).exists());

        // keys: both needed, both hex
        let mut c = config(dir.path(), "keys.wallet");
        c.view_key = "11".repeat(32);
        assert_eq!(
            generate(&c).await.unwrap(),
            Generated::Refused("Need both secret spend key and secret view key.".to_owned())
        );
        c.spend_key = "zz".into();
        assert_eq!(
            generate(&c).await.unwrap(),
            Generated::Refused("Invalid spend key".to_owned())
        );
        let keys = xkr_wallet_core::WalletKeys::generate(&mut xkr_crypto::os_rng());
        c.spend_key = keys.spend_secret.to_hex();
        c.view_key = keys.view_secret.to_hex();
        assert_eq!(
            generate(&c).await.unwrap(),
            Generated::Created(keys.address().to_string())
        );

        // a new random wallet, with an empty password
        let mut c = config(dir.path(), "new.wallet");
        c.container_password = String::new();
        assert!(matches!(generate(&c).await, Ok(Generated::Created(_))));
    }
}
