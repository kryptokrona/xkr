//! `hyper_probe`: first end-to-end check of the hyper transport.
//!
//! Starts one Bare worker in this process, joins a topic, prints the peers it finds and every
//! connection it gets. With `--echo` it dials each peer it finds, sends one line and prints the
//! reply; incoming connections get every line echoed back. Without `--echo` it never writes
//! (safe on Hugin topics, where hugin-node bans peers that send garbage) and prints the first
//! bytes each peer sends.
//!
//! ```text
//! cargo run -p xkr-p2p-hyper --example hyper_probe -- --topic xkr/probe/v1 --echo
//! cargo run -p xkr-p2p-hyper --example hyper_probe -- --hugin-mesh --mode client
//! ```
//!
//! Options:
//!   --topic <name>        XKR-style topic: `cn_fast_hash(name)`     (default xkr/probe/v1)
//!   --topic-hex <64 hex>  raw topic
//!   --hugin-mesh          hugin-node node mesh topic (with mid)
//!   --hugin-public        hugin public-node topic (with mid)
//!   --hugin-key <key>     any Hugin key string (with mid)
//!   --mode server|client|both                                     (default both)
//!   --echo                dial found peers and exchange a line
//!   --duration <secs>     run time, 0 = until Ctrl-C                (default 60)
//!   --max <n>             max peers per lookup                      (default 32)
//!   --bootstrap <host:port>  repeatable; default hyperdht's public bootstrap nodes
//!   --keypair <file>      persist the DHT key pair
//!   --udp-port <port>     fixed UDP port
//!   --bridge-dir <dir>    directory containing src/worker.js
//!
//! Needs libbare-kit (`scripts/build-bare-kit.sh`) and the installed `bridge/` dependencies.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use xkr_p2p_hyper::{
    AnnounceMode, BareConfig, Bridge, BridgeConfig, HuginMid, HuginTopic, HyperConn, OpenFlags,
    topic,
};

struct Args {
    topic: [u8; 32],
    topic_label: String,
    hugin: Option<HuginMid>,
    mode: AnnounceMode,
    echo: bool,
    duration: u64,
    max: u16,
    config: BridgeConfig,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        topic: topic::xkr_topic("xkr/probe/v1"),
        topic_label: "xkr/probe/v1".into(),
        hugin: None,
        mode: AnnounceMode::Both,
        echo: false,
        duration: 60,
        max: 32,
        config: BridgeConfig::default(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("missing value for {arg}"));
        match arg.as_str() {
            "--topic" => {
                let name = val()?;
                a.topic = topic::xkr_topic(&name);
                a.topic_label = name;
            }
            "--topic-hex" => {
                let h = val()?;
                a.topic = hex::decode(&h)
                    .ok()
                    .and_then(|v| v.try_into().ok())
                    .ok_or("topic-hex must be 64 hex chars")?;
                a.topic_label = h;
            }
            "--hugin-mesh" => set_hugin(&mut a, &HuginTopic::node_mesh(), "hugin node mesh"),
            "--hugin-public" => {
                set_hugin(&mut a, &HuginTopic::public_nodes(), "hugin public nodes");
            }
            "--hugin-key" => {
                let k = val()?;
                set_hugin(&mut a, &HuginTopic::from_key(&k), &format!("hugin key {k}"));
            }
            "--mode" => {
                a.mode = match val()?.as_str() {
                    "server" => AnnounceMode::Server,
                    "client" => AnnounceMode::Client,
                    "both" => AnnounceMode::Both,
                    m => return Err(format!("unknown mode {m}")),
                }
            }
            "--echo" => a.echo = true,
            "--duration" => a.duration = val()?.parse().map_err(|e| format!("--duration: {e}"))?,
            "--max" => a.max = val()?.parse().map_err(|e| format!("--max: {e}"))?,
            "--bootstrap" => a.config.bootstrap.push(val()?),
            "--keypair" => a.config.keypair_file = Some(PathBuf::from(val()?)),
            "--udp-port" => {
                a.config.udp_port = Some(val()?.parse().map_err(|e| format!("--udp-port: {e}"))?);
            }
            "--bridge-dir" => a.config.bridge_dir = Some(PathBuf::from(val()?)),
            "-h" | "--help" => return Err("see the doc comment of examples/hyper_probe.rs".into()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn set_hugin(a: &mut Args, t: &HuginTopic, label: &str) {
    a.topic = t.topic;
    label.clone_into(&mut a.topic_label);
    a.hugin = Some(t.mid());
}

fn short(k: &[u8; 32]) -> String {
    hex::encode(&k[..8])
}

/// Prints what a peer sends; in echo mode answers every line with `echo: <line>`.
async fn serve(conn: HyperConn, echo: bool) {
    let info = conn.info().clone();
    let who = short(&info.remote_public_key);
    let (rd, mut wr) = tokio::io::split(conn);
    if echo {
        let mut lines = BufReader::new(rd).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    println!("[{who}] < {line}");
                    if wr
                        .write_all(format!("echo: {line}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    println!("[{who}] read error: {e}");
                    break;
                }
            }
        }
    } else {
        let mut rd = rd;
        let mut buf = vec![0u8; 4096];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n.min(200)]).replace('\n', "\\n");
                    println!("[{who}] < {n} bytes: {text}");
                }
                Err(e) => {
                    println!("[{who}] read error: {e}");
                    break;
                }
            }
        }
    }
    println!("[{who}] closed");
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let started = Bridge::start_in_process(args.config.clone(), &BareConfig::default()).await;
    let (bridge, mut workers) = match started {
        Ok(v) => v,
        Err(e) => {
            eprintln!("cannot start the bridge: {e}");
            std::process::exit(1);
        }
    };
    let me = bridge.node_public_key().unwrap_or([0; 32]);
    println!("node public key {}", hex::encode(me));
    println!("topic {} = {}", args.topic_label, hex::encode(args.topic));

    let ack = match bridge
        .announce(args.topic, args.mode, args.hugin.clone())
        .await
    {
        Ok(a) => a,
        Err(e) => {
            eprintln!("announce failed: {e}");
            std::process::exit(1);
        }
    };
    let local = ack.local_public_key().unwrap_or(me);
    println!("joined as {} ({:?})", hex::encode(local), args.mode);

    let connected: Arc<Mutex<HashSet<[u8; 32]>>> = Arc::default();
    let mut incoming = bridge.incoming().expect("first call");
    let echo = args.echo;
    let conns_in = connected.clone();
    tokio::spawn(async move {
        while let Some(item) = incoming.next().await {
            match item {
                Ok(conn) => {
                    let i = conn.info();
                    println!(
                        "connection {} initiator={} topic={} addr={:?}",
                        hex::encode(i.remote_public_key),
                        i.initiator,
                        i.topic.map_or_default(|t| short(&t)),
                        i.addr
                    );
                    conns_in.lock().unwrap().insert(i.remote_public_key);
                    // Hyperswarm dials peers on its own after a join; the initiator side of
                    // such a connection sends the probe line so that bytes really flow.
                    if echo && i.initiator {
                        tokio::spawn(dial_echo(conn, local));
                    } else {
                        tokio::spawn(serve(conn, echo));
                    }
                }
                Err(e) => {
                    println!("bridge failed: {e}");
                    break;
                }
            }
        }
    });

    let probe = {
        let bridge = bridge.clone();
        let args_topic = args.topic;
        let max = args.max;
        let connected = connected.clone();
        async move {
            let mut seen = HashSet::new();
            loop {
                match bridge.lookup(args_topic, max).await {
                    Ok(peers) => {
                        println!("lookup: {} peer(s)", peers.len());
                        for p in peers {
                            if seen.insert(p.public_key) {
                                println!("  peer {} via {:?}", hex::encode(p.public_key), p.relay);
                            }
                            if !echo
                                || p.public_key == local
                                || connected.lock().unwrap().contains(&p.public_key)
                            {
                                continue;
                            }
                            connected.lock().unwrap().insert(p.public_key);
                            let bridge = bridge.clone();
                            tokio::spawn(async move {
                                match bridge
                                    .open(p.public_key, Some(args_topic), OpenFlags::NONE)
                                    .await
                                {
                                    Ok(conn) => dial_echo(conn, local).await,
                                    Err(e) => println!("open {} failed: {e}", short(&p.public_key)),
                                }
                            });
                        }
                    }
                    Err(e) => println!("lookup failed: {e}"),
                }
                if bridge.error().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    };

    let run = async {
        tokio::select! {
            () = probe => {}
            e = bridge.wait_closed() => println!("bridge closed: {e}"),
        }
    };
    if args.duration == 0 {
        tokio::select! {
            () = run => {}
            _ = tokio::signal::ctrl_c() => println!("interrupted"),
        }
    } else {
        tokio::select! {
            () = run => {}
            () = tokio::time::sleep(Duration::from_secs(args.duration)) => println!("done after {} s", args.duration),
            _ = tokio::signal::ctrl_c() => println!("interrupted"),
        }
    }
    bridge.close();
    workers.stop(Duration::from_secs(5)).await;
    drop(workers);
    let _ = xkr_p2p_hyper::bare::teardown(Duration::from_secs(5));
}

async fn dial_echo(conn: HyperConn, local: [u8; 32]) {
    let who = short(&conn.info().remote_public_key);
    println!(
        "dialed {who} (handshake {})",
        hex::encode(&conn.info().handshake_hash[..8])
    );
    let (rd, mut wr) = tokio::io::split(conn);
    let line = format!("hello from {}\n", hex::encode(local));
    if let Err(e) = wr.write_all(line.as_bytes()).await {
        println!("[{who}] write failed: {e}");
        return;
    }
    let mut lines = BufReader::new(rd).lines();
    match tokio::time::timeout(Duration::from_secs(20), lines.next_line()).await {
        Ok(Ok(Some(reply))) => println!("[{who}] reply: {reply}"),
        Ok(Ok(None)) => println!("[{who}] closed without reply"),
        Ok(Err(e)) => println!("[{who}] read error: {e}"),
        Err(_) => println!("[{who}] no reply within 20 s"),
    }
    // keep the connection a moment so the peer sees our close cleanly
    tokio::time::sleep(Duration::from_millis(200)).await;
}
