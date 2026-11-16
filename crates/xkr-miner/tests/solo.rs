//! Solo mining against xkrd's RPC router in process (no socket): a testnet node with only the
//! genesis block (fixed difficulty 1), `getblocktemplate` -> hash -> `submitblock` until
//! `--limit` blocks are on the chain.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use xkr_chain::{Chain, ChainConfig};
use xkr_consensus::Network;
use xkr_daemon_client::{
    DaemonClient, HttpRequest, HttpResponse, Method, Transport, TransportError,
};
use xkr_miner::{
    AesImpl, Event, MinerOptions, SoloConfig, SoloSource, SubmitResult, Timestamps, run,
};
use xkr_rpc::{MiningStatus, NetStatus, NodeInfo, RpcState};
use xkr_txpool::{PoolConfig, TxPool};
use xkr_wallet_core::Address;

/// Sends requests straight into an axum router.
#[derive(Debug, Clone)]
struct RouterTransport(Router);

impl Transport for RouterTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = match request.method {
            Method::Get => "GET",
            Method::Post => "POST",
        };
        let req = axum::http::Request::builder()
            .method(method)
            .uri(&request.path)
            .header("content-type", "application/json")
            .body(Body::from(request.body))
            .map_err(|e| TransportError::Connection(e.to_string()))?;
        let Ok(resp) = self.0.clone().oneshot(req).await;
        let status = resp.status().as_u16();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .map_err(|e| TransportError::Connection(e.to_string()))?;
        Ok(HttpResponse {
            status,
            body: body.to_vec(),
        })
    }
}

/// A database file under the temp dir, removed on drop.
struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "xkr-miner-solo-{}-{}.redb",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A testnet node over `db` (genesis only). Without a P2P layer `submitblock` adds blocks to
/// the chain directly.
fn testnet_state(db: &TempDb) -> Arc<RpcState> {
    let store = xkr_storage::Store::open(&db.0).expect("store");
    let pool = Arc::new(TxPool::new(PoolConfig::default()));
    let chain = Chain::open(store, ChainConfig::new(Network::Testnet), pool).expect("chain");
    let (_net_tx, net) = watch::channel(Arc::new(NetStatus::default()));
    let (_mining_tx, mining) = watch::channel(Arc::new(MiningStatus::default()));
    let (commands, _commands_rx) = mpsc::unbounded_channel();
    Arc::new(RpcState {
        chain: Arc::new(chain),
        net,
        commands,
        mining,
        hugin: None,
        p2pool: None,
        info: NodeInfo {
            version: "xkr-miner-test".into(),
            start_time: 0,
            data_dir: std::env::temp_dir(),
            db_path: db.0.clone(),
            fee_address: String::new(),
            fee_amount: 0,
        },
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn solo_mines_testnet_blocks() {
    let db = TempDb::new();
    let state = testnet_state(&db);
    assert_eq!(state.chain.tip().height, 0);
    let client = DaemonClient::new(RouterTransport(xkr_rpc::router(state.clone(), &[])));
    let mut rng = xkr_crypto::os_rng();
    let (spend, _) = xkr_crypto::generate_keys(&mut rng);
    let (view, _) = xkr_crypto::generate_keys(&mut rng);
    let mut source = SoloSource::new(
        client,
        SoloConfig {
            address: Address::new(spend, view),
            scan_time: Duration::from_millis(200),
            timestamps: Timestamps::default(),
        },
    );
    let opts = MinerOptions {
        threads: 1,
        aes: AesImpl::detect(),
        report_interval: Duration::from_secs(60),
        limit: 3,
        thread_name: "test-solo".into(),
    };
    let mut jobs = Vec::new();
    let mut blocks = Vec::new();
    let mut observer = |e: Event<'_>| match e {
        Event::Job(job) => jobs.push((job.height, job.major, job.target.difficulty())),
        Event::Submitted { result, .. } => blocks.push(result.clone()),
        _ => {}
    };
    let totals = tokio::time::timeout(
        Duration::from_secs(120),
        run(&mut source, &opts, &mut observer, &CancellationToken::new()),
    )
    .await
    .expect("three blocks within 120 s")
    .expect("hashing threads");

    assert_eq!(totals.accepted, 3, "{blocks:?}");
    assert_eq!((totals.rejected, totals.errors), (0, 0), "{blocks:?}");
    let tip = state.chain.tip();
    assert_eq!(tip.height, 3);
    // The last accepted block is the top block.
    assert_eq!(totals.last_block_hash, Some(tip.hash.to_string()));
    for r in &blocks {
        assert!(
            matches!(
                r,
                SubmitResult::Accepted {
                    block_hash: Some(_)
                }
            ),
            "{r:?}"
        );
    }
    // One template per height (1, 2, 3), each a v5 block at difficulty 1.
    assert_eq!(&jobs[..3], &[(1, 5, 1), (2, 5, 1), (3, 5, 1)]);
}

/// The miner's merge-mining tag is the node's (`xkr_chain::set_merge_mining_tag`, C++
/// `adjustMergeMiningTag`).
#[test]
fn merge_mining_tag_matches_the_node() {
    use xkr_types::{Block, BlockHeader, Hash, ParentBlock, Transaction};
    for major in 1..=7u8 {
        let mut block = Block {
            header: BlockHeader {
                major_version: major,
                minor_version: 1,
                timestamp: 1_700_000_123,
                prev_hash: Hash([9; 32]),
                nonce: 77,
            },
            parent_block: (major >= 2).then(|| ParentBlock {
                major_version: 1,
                transaction_count: 1,
                ..ParentBlock::default()
            }),
            miner_tx: Transaction::default(),
            tx_hashes: vec![Hash([1; 32])],
        };
        block.miner_tx.prefix.version = 1;
        let mut ours = block.clone();
        xkr_miner::set_merge_mining_tag(&mut ours).unwrap();
        xkr_chain::set_merge_mining_tag(&mut block).unwrap();
        assert_eq!(ours, block, "major {major}");
    }
}
