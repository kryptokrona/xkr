//! Test support: an in-process xkrd (the `xkr-rpc` router on a fresh testnet chain), blocks
//! mined like `xkr-testwallet mine`, a node that never answers, and [`Run`]: a terminal
//! wallet session without a terminal, fed line by line through a pipe, its output collected.
#![allow(dead_code)] // each test uses a different subset

use std::io::{BufReader, PipeWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;
use xkr_chain::{Chain, ChainConfig, set_merge_mining_tag};
use xkr_consensus::Network;
use xkr_daemon_client::{
    DaemonClient, HttpRequest, HttpResponse, Method, Transport, TransportError,
};
use xkr_rpc::{MiningStatus, NetStatus, NodeInfo, RpcState};
use xkr_serialization::{decode_block, encode_block};
use xkr_txpool::{PoolConfig, TxPool};
use xkr_wallet::{Node, SyncConfig, WalletError};
use xkr_wallet_cli::args::{self, Command};
use xkr_wallet_cli::{Console, NodeFactory, Session, SessionConfig, ShutdownHandle, Timing};
use xkr_wallet_core::{ADDRESS_PREFIX_ALT, Address};

// fixed key material of `testnet/fixtures/lib-wallet.sh`
pub const SEED_A: &str = "skulls woozy ouch summon gifts huts waffle ourselves obtains hexagon tadpoles hacksaw dormant hence abort listen history atom cadets stylishly snout vegan girth guest history";
pub const A: &str = "SEKReX4MYn2HAJFoHvcqVBPyHYom2ynKeK6dpYptbp8gQNzdzE73ZDkNmNurqfhhcMSUXpS1ZGEJKiKJUcPCyw7vYaCc35yBZCf";
pub const A_XKR: &str = "XkrezgVVv8PTPBBHRdFJMnFBi5GV4d8BV9PHLgKDVMzraC2YLfkjyBX6N2PoUXXyN7Dyt3prMAq1p1QXd8nRaxZy8N9RGwvT4C";
pub const A_SPEND: &str = "d61a57a59318d70ff77cc7f8ad7f62887c828da1d5d3f3b0d2f7d3fa596c2904";
pub const A_VIEW: &str = "688e5229df6463ec4c27f6ee11c3f1d3d4b4d2480c0aabe64fb807182cfdc801";
pub const K: &str = "SEKReZCo6myNqvP1x4MuTVFxqVydgF2PBatbBKdER2LP6uH56q3s4EbEaCRGvRc8uXEeoz5PaR5EgDZd1FTbCeVeYFqjboZpBZn";
pub const K_SPEND: &str = "55e0aa4ca65c0ae016c7364eec313f56fc162901ead0e38a9f846686ac78560f";
pub const K_VIEW: &str = "1f3f6c220dd9f97619dbf44d967f79f3041b9b1c63da2c895f980f1411d5d704";
pub const DUMMY_ADDR: &str = "SEKReTRGXxc41SGrKyw5ucHKoM4nhMYazTwseWGB181H9zRB68oSApmjSaUS8yPjgEGEnH4WesJEW1zaCmEB5ykSLQZvfs7CFTU";
pub const PID1: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// `address` under the `Xkr` prefix.
pub fn xkr_form(address: &str) -> String {
    let parsed = Address::parse(address).expect("valid address");
    Address {
        prefix: ADDRESS_PREFIX_ALT,
        ..parsed
    }
    .encode()
}

/// A runtime for the engine and the node.
pub fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

// ---------------------------------------------------------------------------------------------
// the node

/// xkrd's RPC on a fresh testnet chain (genesis only), in process.
pub struct TestNode {
    pub state: Arc<RpcState>,
    pub router: Router,
    _dir: tempfile::TempDir,
}

impl Default for TestNode {
    fn default() -> Self {
        Self::new()
    }
}

impl TestNode {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = xkr_storage::Store::open(dir.path().join("chain.redb")).expect("store");
        let pool = Arc::new(TxPool::new(PoolConfig::default()));
        let chain = Chain::open(store, ChainConfig::new(Network::Testnet), pool).expect("chain");
        let (_net_tx, net) = watch::channel(Arc::new(NetStatus::default()));
        let (_mining_tx, mining) = watch::channel(Arc::new(MiningStatus::default()));
        // no receiver: submitted blocks go straight to the chain
        let (commands, _commands_rx) = mpsc::unbounded_channel();
        let state = Arc::new(RpcState {
            chain: Arc::new(chain),
            net,
            commands,
            mining,
            hugin: None,
            p2pool: None,
            info: NodeInfo {
                version: "xkr-wallet-cli-tests".into(),
                start_time: 0,
                data_dir: dir.path().to_path_buf(),
                db_path: dir.path().join("chain.redb"),
                fee_address: String::new(),
                fee_amount: 0,
            },
        });
        let router = xkr_rpc::router(Arc::clone(&state), &[]);
        Self {
            state,
            router,
            _dir: dir,
        }
    }

    /// Index of the top block.
    pub fn top(&self) -> u64 {
        self.state.chain.tip().height
    }

    /// A JSON-RPC call; the `result`.
    pub async fn json_rpc(&self, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": 0, "method": method, "params": params });
        let req = Request::builder()
            .method("POST")
            .uri("/json_rpc")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");
        let Ok(resp) = self.router.clone().oneshot(req).await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let answer: Value = serde_json::from_slice(&bytes).expect("json answer");
        assert!(answer.get("error").is_none(), "{method}: {answer}");
        answer["result"].clone()
    }

    /// One block paying `address`, with what the pool offers.
    pub async fn mine_block(&self, address: &str) {
        let template = self
            .json_rpc(
                "getblocktemplate",
                json!({ "wallet_address": address, "reserve_size": 0 }),
            )
            .await;
        let blob =
            hex::decode(template["blocktemplate_blob"].as_str().expect("blob")).expect("hex");
        let mut block = decode_block(&blob).expect("template decodes");
        set_merge_mining_tag(&mut block).expect("merge-mining tag");
        let blob = encode_block(&block).expect("block encodes");
        let result = self
            .json_rpc("submitblock", json!([hex::encode(blob)]))
            .await;
        assert_eq!(result["status"], "OK", "submitblock: {result}");
    }

    /// Blocks paying `address` until the top block index is `top`.
    pub async fn mine_to(&self, address: &str, top: u64) {
        while self.top() < top {
            self.mine_block(address).await;
        }
    }

    /// Transactions waiting in the pool.
    pub fn pool_len(&self) -> usize {
        self.state.chain.pool().len()
    }
}

/// A fresh node on `rt`.
pub fn node(rt: &Runtime) -> TestNode {
    rt.block_on(async { TestNode::new() })
}

/// Hands daemon requests straight to the router.
#[derive(Debug, Clone)]
pub struct RouterTransport(pub Router);

impl Transport for RouterTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = match request.method {
            Method::Get => "GET",
            Method::Post => "POST",
        };
        let req = Request::builder()
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

/// Every `host:port` reaches the in-process node.
#[derive(Debug, Clone)]
pub struct RouterNodes(pub Router);

impl NodeFactory for RouterNodes {
    type Transport = RouterTransport;

    fn connect(&self, host: &str, port: u16) -> Result<Node<RouterTransport>, WalletError> {
        Ok(Node::new(
            DaemonClient::new(RouterTransport(self.0.clone())),
            host,
            port,
        ))
    }
}

/// A daemon that never answers.
#[derive(Debug, Clone)]
pub struct Unreachable;

impl Transport for Unreachable {
    fn send(
        &self,
        _request: HttpRequest,
    ) -> impl std::future::Future<Output = Result<HttpResponse, TransportError>> + Send {
        std::future::ready(Err(TransportError::Connection("connection refused".into())))
    }
}

/// Every `host:port` is down.
#[derive(Debug, Clone)]
pub struct OfflineNodes;

impl NodeFactory for OfflineNodes {
    type Transport = Unreachable;

    fn connect(&self, host: &str, port: u16) -> Result<Node<Unreachable>, WalletError> {
        Ok(Node::new(DaemonClient::new(Unreachable), host, port))
    }
}

// ---------------------------------------------------------------------------------------------
// sessions

/// Fast sync and short waits, files in `dir`.
pub fn config(dir: &Path) -> SessionConfig {
    let mut config = SessionConfig::new(Network::Testnet);
    config.wallet.sync = SyncConfig {
        poll_interval: Duration::from_millis(20),
        info_interval: Duration::from_millis(20),
        block_count: 100,
        queue_blocks: 1000,
    };
    config.timing = Timing {
        sync_poll: Duration::from_millis(30),
        wait: Duration::from_millis(100),
    };
    config.dir = dir.to_path_buf();
    config
}

/// The session's output.
#[derive(Debug, Clone, Default)]
pub struct Output(Arc<Mutex<Vec<u8>>>);

impl Output {
    pub fn text(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// How long [`Run::wait_for`] waits.
const WAIT: Duration = Duration::from_secs(120);

/// A session on its own thread, without a terminal.
pub struct Run {
    /// What Ctrl+C and SIGTERM call.
    pub shutdown: ShutdownHandle,
    input: Option<PipeWriter>,
    out: Output,
    seen: usize,
    thread: Option<JoinHandle<u8>>,
}

impl Run {
    /// `xkrwallet --network testnet -r 127.0.0.1:24899 <args>` with files in `dir`.
    pub fn start<N: NodeFactory>(nodes: N, rt: &Runtime, dir: &Path, extra: &[&str]) -> Self {
        let (reader, writer) = std::io::pipe().expect("pipe");
        let out = Output::default();
        let mut argv: Vec<String> = ["xkrwallet", "--network", "testnet", "-r", "127.0.0.1:24899"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        argv.extend(extra.iter().copied().map(str::to_owned));
        let Command::Run(options) = args::parse(&argv) else {
            panic!("arguments {argv:?}");
        };
        let handle = rt.handle().clone();
        let config = config(dir);
        let sink = out.clone();
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let console = Console::lines(Box::new(BufReader::new(reader)), Box::new(sink), false);
            let session = Session::new(options, nodes, console, handle, config);
            handle_tx
                .send(session.shutdown_handle())
                .expect("test waits for the handle");
            session.run()
        });
        let shutdown = handle_rx.recv().expect("session started");
        Self {
            shutdown,
            input: Some(writer),
            out,
            seen: 0,
            thread: Some(thread),
        }
    }

    /// Types `line` and enter.
    pub fn send(&mut self, line: &str) {
        let input = self.input.as_mut().expect("input open");
        writeln!(input, "{line}").expect("session reads");
        input.flush().expect("flush");
    }

    /// Types each line.
    pub fn send_all(&mut self, lines: &[&str]) {
        for line in lines {
            self.send(line);
        }
    }

    /// Everything printed so far.
    pub fn output(&self) -> String {
        self.out.text()
    }

    /// Where the next [`Run::wait_for`] starts looking.
    pub fn seen(&self) -> usize {
        self.seen
    }

    /// Whether `needle` was printed after the last match.
    pub fn printed(&self, needle: &str) -> bool {
        self.out.text()[self.seen..].contains(needle)
    }

    /// Waits until `needle` is printed after the last match; the output up to and including
    /// it.
    pub fn wait_for(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let text = self.out.text();
            if let Some(i) = text[self.seen..].find(needle) {
                let end = self.seen + i + needle.len();
                let chunk = text[self.seen..end].to_owned();
                self.seen = end;
                return chunk;
            }
            let ended = self.thread.as_ref().is_none_or(JoinHandle::is_finished);
            assert!(
                !ended && Instant::now() < deadline,
                "{needle:?} was not printed{}; output after the last match:\n{}",
                if ended { " (the session ended)" } else { "" },
                &text[self.seen..]
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Closes the input and waits for the session; its exit code and the whole output.
    pub fn finish(mut self) -> (u8, String) {
        drop(self.input.take());
        let code = self
            .thread
            .take()
            .expect("running")
            .join()
            .expect("session thread");
        (code, self.out.text())
    }
}

/// The wallet height a `status` output shows.
fn wallet_height(status: &str) -> Option<u64> {
    let rest = status.split("Wallet blockchain height: ").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Asks `status` until the wallet has processed block `height`.
pub fn wait_height(run: &mut Run, height: u64) {
    let deadline = Instant::now() + WAIT;
    loop {
        run.send("status");
        let status = run.wait_for("Peers: ");
        if wallet_height(&status).is_some_and(|h| h >= height) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the wallet did not reach {height}: {status}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
