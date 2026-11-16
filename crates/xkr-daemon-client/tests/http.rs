//! [`HttpTransport`] over a loopback socket: the xkr-rpc router served by axum, one kept-alive
//! connection for several requests, the request timeout and a refused connection.

mod common;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{GenesisDb, genesis_node};
use tokio::net::{TcpListener, TcpStream};
use xkr_daemon_client::{DaemonClient, DaemonError, HttpConfig, TransportError};

/// A listener that counts the connections it accepts.
struct Counting {
    inner: TcpListener,
    accepted: Arc<AtomicUsize>,
}

impl axum::serve::Listener for Counting {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let conn = axum::serve::Listener::accept(&mut self.inner).await;
        self.accepted.fetch_add(1, Ordering::SeqCst);
        conn
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_share_one_kept_alive_connection() {
    let db = GenesisDb::new();
    let state = genesis_node(&db, ("", 0));
    let app = xkr_rpc::router(state, &[]);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    let accepted = Arc::new(AtomicUsize::new(0));
    let counting = Counting {
        inner: listener,
        accepted: accepted.clone(),
    };
    let server = tokio::spawn(async move { axum::serve(counting, app).await });

    // The pool takes a connection back once its answer has been read; the pauses let that
    // happen before the next request.
    let pause = || tokio::time::sleep(Duration::from_millis(50));
    let client = DaemonClient::connect(&format!("http://{addr}")).expect("client");
    let info = client.info().await.expect("info");
    assert_eq!(info.height, 1);
    assert!(
        info.tx_versions.is_some(),
        "xkrd states its transaction versions"
    );
    pause().await;
    let fee = client.fee().await.expect("fee");
    assert_eq!(fee.payable(), None);
    pause().await;
    assert_eq!(client.block_count().await.expect("count"), 1);
    pause().await;
    let genesis = client.last_block_header().await.expect("genesis header");
    assert_eq!(genesis.height, 0);
    pause().await;
    let again = client.block_header_by_height(0).await.expect("header 0");
    assert_eq!(again, genesis);
    // A bare host:port means plain HTTP, like the C++ --daemon-address/--daemon-port pair.
    let bare = DaemonClient::connect(&addr.to_string()).expect("client");
    assert_eq!(bare.info().await.expect("info").height, 1);

    assert_eq!(
        accepted.load(Ordering::SeqCst),
        2,
        "one connection per client, kept open between requests"
    );
    server.abort();
}

#[tokio::test]
async fn a_silent_daemon_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    // Accepts and never answers.
    let silent = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((conn, _)) = listener.accept().await {
            open.push(conn);
        }
    });
    let config = HttpConfig {
        request_timeout: Duration::from_millis(300),
        ..HttpConfig::default()
    };
    let client = DaemonClient::connect_with(&format!("http://{addr}"), &config).expect("client");
    let e = client.info().await.unwrap_err();
    let timed_out = matches!(
        e,
        DaemonError::Transport(TransportError::Timeout(d)) if d == Duration::from_millis(300)
    );
    assert!(timed_out, "{e}");
    assert!(e.is_unreachable());
    silent.abort();
}

#[tokio::test]
async fn a_closed_port_is_unreachable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    drop(listener);
    let client = DaemonClient::connect(&format!("http://{addr}")).expect("client");
    let e = client.info().await.unwrap_err();
    assert!(
        matches!(e, DaemonError::Transport(TransportError::Connection(_))),
        "{e}"
    );
}

#[tokio::test]
async fn bad_addresses_are_refused() {
    for bad in ["ftp://127.0.0.1:1", "http://", "http://node:1/?a=b"] {
        let e = DaemonClient::connect(bad).unwrap_err();
        assert!(
            matches!(e, DaemonError::Transport(TransportError::Address(_))),
            "{bad}: {e}"
        );
    }
}

#[tokio::test]
async fn answers_above_the_limit_are_refused() {
    let db = GenesisDb::new();
    let app = xkr_rpc::router(genesis_node(&db, ("", 0)), &[]);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let config = HttpConfig {
        max_answer_bytes: 16,
        ..HttpConfig::default()
    };
    let client = DaemonClient::connect_with(&format!("http://{addr}"), &config).expect("client");
    let e = client.info().await.unwrap_err();
    assert!(
        matches!(e, DaemonError::Transport(TransportError::TooLarge(16))),
        "{e}"
    );
    server.abort();
}
