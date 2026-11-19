//! Server and client over an in-memory stream: hello negotiation, calls, pushes, limits and
//! protocol errors.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use xkr_hyper_rpc::server::Refused;
use xkr_hyper_rpc::{
    Accepted, Client, ClientError, Handler, Hello, HelloReply, MAX_FRAME, Push, RefuseReason,
    RpcError, ServeEnd, ServerLimits, Service, Tip, code, read_frame, serve, write_frame,
};

const NETWORK: &str = "testnet";

fn reply(services: &[&str]) -> HelloReply {
    HelloReply {
        v: 1,
        height: 7,
        hash: "11".repeat(32),
        synced: true,
        services: services.iter().map(|s| (*s).to_owned()).collect(),
        state_span: Some(0),
        state_hash: Some("22".repeat(32)),
        pruning_seed: 0,
    }
}

/// Echoes its call; `sleep` waits `params.ms` first, `hold` waits for the gate, `big` answers
/// more than a frame, `fail` answers an error.
struct Echo {
    gate: Arc<Notify>,
    calls: AtomicUsize,
}

impl Handler for Echo {
    fn call(&self, method: String, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let gate = self.gate.clone();
        async move {
            match method.as_str() {
                "sleep" => {
                    let ms = params["ms"].as_u64().unwrap_or(0);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                "hold" => gate.notified().await,
                "big" => return Ok(Value::String("x".repeat(MAX_FRAME))),
                "fail" => return Err(RpcError::new(-2, "no such block")),
                _ => {}
            }
            Ok(json!({ "method": method, "params": params }))
        }
        .boxed()
    }
}

struct Server {
    task: JoinHandle<ServeEnd>,
    gate: Arc<Notify>,
    pushes: mpsc::UnboundedSender<Push>,
}

/// A server on one end of a duplex stream that serves `node` (and refuses everything else);
/// returns the client end.
fn start(limits: ServerLimits) -> (DuplexStream, Server) {
    let (client_end, server_end) = tokio::io::duplex(4 * MAX_FRAME);
    let gate = Arc::new(Notify::new());
    let (push_tx, push_rx) = mpsc::unbounded_channel::<Push>();
    let handler = Arc::new(Echo {
        gate: gate.clone(),
        calls: AtomicUsize::new(0),
    });
    let task =
        tokio::spawn(serve(server_end, limits, move |hello: &Hello| {
            if hello.network != NETWORK {
                return Err(Refused::new(RefuseReason::Network, "testnet node"));
            }
            if Service::parse(&hello.service) != Some(Service::Node) {
                return Err(Refused::new(RefuseReason::Service, "node only"));
            }
            let pushes = futures::stream::unfold(push_rx, |mut rx| async move {
                rx.recv().await.map(|p| (p, rx))
            })
            .boxed();
            Ok(Accepted {
                reply: reply(&["node"]),
                handler,
                pushes: Some(pushes),
            })
        }));
    (
        client_end,
        Server {
            task,
            gate,
            pushes: push_tx,
        },
    )
}

fn hello(service: Service) -> Hello {
    Hello::new(NETWORK, service, "test/1")
}

async fn end_of(server: Server) -> ServeEnd {
    tokio::time::timeout(Duration::from_secs(60), server.task)
        .await
        .expect("the server ends")
        .expect("no panic")
}

#[tokio::test]
async fn hello_is_answered_with_the_node_state() {
    let (stream, _server) = start(ServerLimits::default());
    let client = Client::connect(stream, hello(Service::Node)).await.unwrap();
    assert_eq!(client.hello(), &reply(&["node"]));
    assert!(client.hello().serves(Service::Node));
    assert_eq!(
        *client.tips().borrow(),
        Tip {
            height: 7,
            hash: "11".repeat(32),
            synced: true
        }
    );
    assert!(!client.is_closed());
}

#[tokio::test]
async fn refused_hellos_name_the_reason_and_close() {
    for (h, reason) in [
        (hello(Service::Stratum), RefuseReason::Service),
        (hello(Service::Wallet), RefuseReason::Service),
        (
            Hello::new("mainnet", Service::Node, "test/1"),
            RefuseReason::Network,
        ),
        (
            Hello {
                v: 2,
                ..hello(Service::Node)
            },
            RefuseReason::Version,
        ),
        (
            Hello {
                service: "nodes".into(),
                ..hello(Service::Node)
            },
            RefuseReason::Service,
        ),
    ] {
        let (stream, server) = start(ServerLimits::default());
        match Client::connect(stream, h.clone()).await {
            Err(ClientError::Refused { reason: r, .. }) => assert_eq!(r, reason.as_str(), "{h:?}"),
            other => panic!("{h:?}: {other:?}"),
        }
        assert!(
            matches!(end_of(server).await, ServeEnd::Refused(r) if r == reason),
            "{h:?}"
        );
    }
}

#[tokio::test]
async fn a_first_frame_that_is_not_a_hello_is_refused() {
    let (mut stream, server) = start(ServerLimits::default());
    write_frame(&mut stream, br#"{"id":1,"method":"getinfo"}"#, MAX_FRAME)
        .await
        .unwrap();
    let body = read_frame(&mut stream, MAX_FRAME).await.unwrap().unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"], "hello");
    assert!(read_frame(&mut stream, MAX_FRAME).await.unwrap().is_none());
    assert!(end_of(server).await.is_misbehaviour());
}

#[tokio::test(start_paused = true)]
async fn no_hello_in_time_ends_the_connection() {
    let (_stream, server) = start(ServerLimits::default());
    assert!(matches!(end_of(server).await, ServeEnd::HelloTimeout));
}

#[tokio::test]
async fn calls_are_answered() {
    let (stream, _server) = start(ServerLimits::default());
    let client = Client::connect(stream, hello(Service::Node)).await.unwrap();
    let v = client.call("getinfo", json!({ "a": 1 })).await.unwrap();
    assert_eq!(v, json!({ "method": "getinfo", "params": { "a": 1 } }));
    let v = client
        .call("getblockheaderbyheight", json!([5]))
        .await
        .unwrap();
    assert_eq!(v["params"], json!([5]));
    match client.call("fail", Value::Null).await {
        Err(ClientError::Rpc(e)) => assert_eq!(e, RpcError::new(-2, "no such block")),
        other => panic!("{other:?}"),
    }
    match client.call("big", Value::Null).await {
        Err(ClientError::Rpc(e)) => assert_eq!(e.code, code::TOO_LARGE),
        other => panic!("{other:?}"),
    }
    // the connection survives error answers
    assert!(client.call("ping", Value::Null).await.is_ok());
}

/// Answers come back in the order they finish, each to its own call.
#[tokio::test]
async fn concurrent_calls_get_their_own_answers() {
    let (stream, _server) = start(ServerLimits {
        max_in_flight: 16,
        ..ServerLimits::default()
    });
    let client = Client::connect(stream, hello(Service::Node)).await.unwrap();
    let calls = (0..16u64).map(|i| {
        let client = client.clone();
        async move {
            let ms = (16 - i) * 10;
            let v = client
                .call("sleep", json!({ "ms": ms, "i": i }))
                .await
                .unwrap();
            assert_eq!(v["params"]["i"], i);
            i
        }
    });
    let done: Vec<u64> = futures::future::join_all(calls).await;
    assert_eq!(done, (0..16).collect::<Vec<_>>());
}

#[tokio::test]
async fn pushes_reach_the_tip_channel() {
    let (stream, server) = start(ServerLimits::default());
    let client = Client::connect(stream, hello(Service::Node)).await.unwrap();
    let mut tips = client.tips();
    let mut pushes = client.pushes();
    let tip = Tip {
        height: 8,
        hash: "33".repeat(32),
        synced: true,
    };
    server.pushes.send(Push::tip(&tip)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), tips.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*tips.borrow_and_update(), tip);
    assert_eq!(pushes.recv().await.unwrap().as_tip(), Some(tip));
    // other pushes only go to `pushes`
    server
        .pushes
        .send(Push {
            method: "news".into(),
            params: json!(1),
        })
        .unwrap();
    assert_eq!(pushes.recv().await.unwrap().method, "news");
    assert!(!tips.has_changed().unwrap());
    // calls still work while pushes arrive
    assert!(client.call("ping", Value::Null).await.is_ok());
}

#[tokio::test]
async fn calls_over_the_in_flight_cap_are_busy() {
    let (stream, server) = start(ServerLimits {
        max_in_flight: 2,
        ..ServerLimits::default()
    });
    let client = Client::connect(stream, hello(Service::Node)).await.unwrap();
    let held: Vec<_> = (0..2)
        .map(|_| {
            let c = client.clone();
            tokio::spawn(async move { c.call("hold", Value::Null).await })
        })
        .collect();
    // let both reach the server
    tokio::time::sleep(Duration::from_millis(100)).await;
    match client.call("ping", Value::Null).await {
        Err(ClientError::Rpc(e)) => assert_eq!(e.code, code::BUSY),
        other => panic!("{other:?}"),
    }
    server.gate.notify_waiters();
    for h in held {
        assert!(h.await.unwrap().is_ok());
    }
    assert!(client.call("ping", Value::Null).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn calls_over_the_rate_are_refused_and_many_refusals_close() {
    let (stream, server) = start(ServerLimits {
        rate_per_sec: 1,
        burst: 2,
        max_refused_in_a_row: 3,
        ..ServerLimits::default()
    });
    let client = Client::connect(stream, hello(Service::Node)).await.unwrap();
    assert!(client.call("a", Value::Null).await.is_ok());
    assert!(client.call("b", Value::Null).await.is_ok());
    match client.call("c", Value::Null).await {
        Err(ClientError::Rpc(e)) => assert_eq!(e.code, code::RATE_LIMITED),
        other => panic!("{other:?}"),
    }
    tokio::time::advance(Duration::from_millis(1_100)).await;
    assert!(client.call("d", Value::Null).await.is_ok(), "refilled");
    for _ in 0..3 {
        assert!(matches!(
            client.call("e", Value::Null).await,
            Err(ClientError::Rpc(_))
        ));
    }
    // the fourth refusal in a row closes the connection
    assert!(matches!(
        client.call("f", Value::Null).await,
        Err(ClientError::Closed)
    ));
    assert!(matches!(end_of(server).await, ServeEnd::TooManyRefused));
    client.closed().await;
    assert!(client.is_closed());
}

#[tokio::test]
async fn malformed_json_closes_the_connection() {
    let (stream, server) = start(ServerLimits::default());
    let (mut rd, mut wr) = tokio::io::split(stream);
    write_frame(
        &mut wr,
        &serde_json::to_vec(&hello(Service::Node)).unwrap(),
        MAX_FRAME,
    )
    .await
    .unwrap();
    assert!(read_frame(&mut rd, MAX_FRAME).await.unwrap().is_some());
    write_frame(&mut wr, b"{\"id\":1,\"method\":", MAX_FRAME)
        .await
        .unwrap();
    assert!(
        read_frame(&mut rd, MAX_FRAME).await.unwrap().is_none(),
        "closed"
    );
    assert!(matches!(end_of(server).await, ServeEnd::Protocol(_)));
}

#[tokio::test]
async fn a_request_without_an_integer_id_closes_the_connection() {
    let (stream, server) = start(ServerLimits::default());
    let (mut rd, mut wr) = tokio::io::split(stream);
    write_frame(
        &mut wr,
        &serde_json::to_vec(&hello(Service::Node)).unwrap(),
        MAX_FRAME,
    )
    .await
    .unwrap();
    assert!(read_frame(&mut rd, MAX_FRAME).await.unwrap().is_some());
    write_frame(&mut wr, br#"{"id":"x","method":"getinfo"}"#, MAX_FRAME)
        .await
        .unwrap();
    assert!(read_frame(&mut rd, MAX_FRAME).await.unwrap().is_none());
    assert!(end_of(server).await.is_misbehaviour());
}

#[tokio::test]
async fn an_oversize_frame_closes_the_connection() {
    let (stream, server) = start(ServerLimits {
        max_frame: 1_024,
        ..ServerLimits::default()
    });
    let (mut rd, mut wr) = tokio::io::split(stream);
    write_frame(
        &mut wr,
        &serde_json::to_vec(&hello(Service::Node)).unwrap(),
        MAX_FRAME,
    )
    .await
    .unwrap();
    assert!(read_frame(&mut rd, MAX_FRAME).await.unwrap().is_some());
    // only the length: the server must not wait for the body
    wr.write_all(&2_000u32.to_le_bytes()).await.unwrap();
    let mut rest = Vec::new();
    rd.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
    assert!(matches!(end_of(server).await, ServeEnd::Protocol(_)));
}

#[tokio::test]
async fn a_closed_server_fails_waiting_calls() {
    let (stream, server) = start(ServerLimits::default());
    let client = Client::connect(stream, hello(Service::Node)).await.unwrap();
    let waiting = {
        let c = client.clone();
        tokio::spawn(async move { c.call("hold", Value::Null).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.task.abort();
    assert!(matches!(waiting.await.unwrap(), Err(ClientError::Closed)));
    let mut tips = client.tips();
    assert!(tips.changed().await.is_err(), "the tip channel ends");
    assert!(matches!(
        client.call("ping", Value::Null).await,
        Err(ClientError::Closed)
    ));
}

#[tokio::test]
async fn the_client_ends_the_session_by_dropping() {
    let (stream, server) = start(ServerLimits::default());
    let client = Client::connect(stream, hello(Service::Node)).await.unwrap();
    assert!(client.call("ping", Value::Null).await.is_ok());
    drop(client);
    assert!(matches!(end_of(server).await, ServeEnd::Closed));
}
