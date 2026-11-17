//! `xkrd bridge-check`: starts one Bare worker of the Hyperswarm bridge, waits for its `Hello`
//! and stops it again. The worker only answers once `src/worker.js` and every module it imports
//! loaded, so a pass means that libbare-kit, the bridge directory and its native `.bare` addons
//! work on this machine. The DHT gets a bootstrap address nothing answers on, so the check never
//! joins the network.

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use xkr_p2p_hyper::{BareConfig, Bridge, BridgeConfig};

/// The DHT's only bootstrap node: the discard port on loopback.
const NO_BOOTSTRAP: &str = "127.0.0.1:9";
/// Time the worker gets to leave after the `Hello`.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// Time bare-kit gets to tear down at the end.
const TEARDOWN: Duration = Duration::from_secs(2);

/// Runs the check; returns the bridge directory it used.
pub fn run(bridge_dir: Option<PathBuf>) -> Result<PathBuf, Box<dyn Error>> {
    let config = BridgeConfig {
        bridge_dir,
        bootstrap: vec![NO_BOOTSTRAP.to_owned()],
        ..BridgeConfig::default()
    };
    let dir = config.resolve_bridge_dir()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let started = runtime.block_on(async {
        let (bridge, mut workers) =
            Bridge::start_in_process(config, &BareConfig::default()).await?;
        bridge.close();
        workers.stop(STOP_GRACE).await;
        Ok::<_, Box<dyn Error>>(())
    });
    drop(runtime);
    if !xkr_p2p_hyper::bare::teardown(TEARDOWN) {
        tracing::warn!("bare-kit did not finish tearing down in time");
    }
    started.map(|()| dir)
}
