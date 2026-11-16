//! The node's own status lines in a terminal: one line per new top block, a progress line while
//! syncing and "✔ Synced · height N" when the node first reaches the network's top (C++
//! `kryptokronad` reports these from `CryptoNoteProtocolHandler`,
//! `src/cryptonote_protocol/cryptonote_protocol_handler.cpp`: `request_missing_objects`,
//! `on_connection_synchronized`, `updateObservedHeight`).
//!
//! [`SyncDisplay`] decides what to print from what the chain and the P2P status show once a
//! second; [`run`] feeds it and prints through [`Term`]. It also says once, in yellow, when the
//! Hyperswarm DHT finds the node behind a strict NAT without `--port`
//! ([`crate::hyper::PORT_HINT`]), and when Hyperswarm stays off ([`crate::hyper::off_line`]).
//! The lines do not go through tracing, so
//! they appear at every `--log-level` (not with `--log-json`).
//!
//! On a colour terminal the per-block lines and the sync line give way to a live panel at the
//! bottom ([`render_panel`]): the top height in big digits and a status line with a spinner
//! ("waiting for block N+1 · last block 42s ago · …", for 2 s "new block · …" after a new top
//! block, a progress bar while syncing). Log lines print above it. Without colours, without a
//! terminal and with `--log-json` the plain lines stay as they are (log files and the testnet
//! suite read them).

use std::fmt::Write as _;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use xkr_chain::Chain;
use xkr_rpc::{MiningStatus, NetStatus};
use xkr_storage::BlockInfo;
use xkr_types::Hash;

use crate::term::{self, Color, SPINNER, Style, Term};

/// Progress line interval on a terminal (redrawn in place).
const PROGRESS_TTY: Duration = Duration::from_secs(3);
/// Progress line interval when stdout is a file or a pipe (one line each time).
const PROGRESS_LOG: Duration = Duration::from_secs(30);
/// A synced node that falls this many blocks behind while downloading shows progress again.
const RESYNC_GAP: u64 = 10;
/// At most this many new-block lines per update; older ones are summarised.
const MAX_BLOCK_LINES: u64 = 20;
/// Panel redraw interval (the spinner's frame rate, 8 fps).
const PANEL_TICK: Duration = Duration::from_millis(125);
/// Panel redraws per second (the chain and the P2P status are read once a second).
const PANEL_TICKS_PER_SECOND: u64 = 8;
/// The terminal width is read again this often (in panel ticks, 10 s).
const PANEL_WIDTH_TICKS: u64 = 80;
/// How long "new block" / "switched chain" stays on the panel's status line.
const PANEL_FLASH: Duration = Duration::from_secs(2);
/// Below this many columns the panel shows the height in normal digits.
const PANEL_NARROW: usize = 40;
/// Cells of the sync progress bar.
const PANEL_BAR: usize = 20;
/// Without any peer for this long, say so once.
const NO_PEERS_NOTICE: Duration = Duration::from_secs(30);
/// Width of the `--port` hint lines (the node's terminal lines stay within 60 columns).
const HINT_COLUMNS: usize = 60;
/// A node at the top of its first peers' chains says "synchronized" after this long with peers
/// (their heights arrive with the handshakes).
const PEER_SETTLE: Duration = Duration::from_secs(5);

/// What the node looks like at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    /// Index of the top block.
    pub top: u64,
    /// Its hash.
    pub top_hash: Hash,
    /// Network height (block count, at least `top + 1`).
    pub network_height: u64,
    /// `NetStatus::synced`.
    pub synced: bool,
    /// `NetStatus::downloading`.
    pub downloading: bool,
    /// Connected peers (legacy and Hyperswarm).
    pub peers: usize,
    /// The Hyperswarm DHT suggests `--port` ([`crate::hyper::wants_port_hint`]).
    pub port_hint: bool,
    /// Why Hyperswarm stays off ([`crate::hyper::off_reason`]).
    pub hyper_off: Option<&'static str>,
}

/// One thing to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Show (or replace) the progress line.
    Progress(String),
    /// Remove the progress line.
    ClearProgress,
    /// Print a line.
    Line(String),
    /// Print the new-block lines of these block indexes.
    NewBlocks(RangeInclusive<u64>),
    /// The main chain switched; the new top block index.
    Reorg(u64),
    /// The node reached the network's top (at this top block index).
    Synced(u64),
}

/// Where the display is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Started; no peer has told us where the top is yet.
    Waiting,
    /// Behind the network: progress lines.
    Syncing,
    /// At the top: one line per new block.
    Synced,
}

/// The status-line state machine.
#[derive(Debug)]
pub struct SyncDisplay {
    style: Style,
    interval: Duration,
    phase: Phase,
    started: Instant,
    no_peers_said: bool,
    /// The `--port` hint was printed.
    port_hint_said: bool,
    /// The "Hyperswarm off" line was printed.
    hyper_off_said: bool,
    /// Since when peers are connected (while waiting).
    peers_since: Option<Instant>,
    /// Top block index and hash already reported.
    last_top: (u64, Hash),
    /// Time and top of the last progress line.
    last_progress: Option<(Instant, u64)>,
    /// Smoothed sync rate, blocks per second.
    rate: Option<f64>,
}

impl SyncDisplay {
    /// A display that started at `now`; `tty` picks the progress interval.
    #[must_use]
    pub fn new(now: Instant, style: Style, tty: bool) -> Self {
        Self {
            style,
            interval: if tty { PROGRESS_TTY } else { PROGRESS_LOG },
            phase: Phase::Waiting,
            started: now,
            no_peers_said: false,
            port_hint_said: false,
            hyper_off_said: false,
            peers_since: None,
            last_top: (0, Hash::default()),
            last_progress: None,
            rate: None,
        }
    }

    /// The current phase.
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The smoothed sync rate, blocks per second (once two progress samples exist).
    #[must_use]
    pub fn rate(&self) -> Option<f64> {
        self.rate
    }

    /// What to print for the observation `obs` made at `now`.
    pub fn update(&mut self, now: Instant, obs: &Observed) -> Vec<Action> {
        let mut out = Vec::new();
        if obs.port_hint && !self.port_hint_said {
            self.port_hint_said = true;
            for line in term::wrap(crate::hyper::PORT_HINT, HINT_COLUMNS) {
                out.push(Action::Line(self.style.paint(Color::Yellow, &line)));
            }
        }
        if let Some(reason) = obs.hyper_off
            && !self.hyper_off_said
        {
            self.hyper_off_said = true;
            for line in term::wrap(&crate::hyper::off_line(reason), HINT_COLUMNS) {
                out.push(Action::Line(self.style.paint(Color::Yellow, &line)));
            }
        }
        let behind = obs.network_height.saturating_sub(obs.top + 1);
        match self.phase {
            Phase::Waiting => {
                if obs.peers == 0 {
                    self.peers_since = None;
                    if !self.no_peers_said && now.duration_since(self.started) >= NO_PEERS_NOTICE {
                        self.no_peers_said = true;
                        out.push(Action::Line(
                            self.style
                                .paint(Color::Yellow, "No peers connected yet, still trying."),
                        ));
                    }
                } else if behind > 0 || obs.downloading {
                    self.start_sync(now, obs, &mut out);
                } else if obs.synced
                    && now.duration_since(*self.peers_since.get_or_insert(now)) >= PEER_SETTLE
                {
                    self.synchronized(obs, &mut out);
                }
            }
            Phase::Syncing => {
                if obs.synced && obs.peers > 0 {
                    out.push(Action::ClearProgress);
                    self.synchronized(obs, &mut out);
                } else if self
                    .last_progress
                    .is_none_or(|(at, _)| now.duration_since(at) >= self.interval)
                {
                    out.push(Action::Progress(self.progress_line(now, obs)));
                }
            }
            Phase::Synced => {
                if obs.downloading && behind > RESYNC_GAP {
                    self.start_sync(now, obs, &mut out);
                } else {
                    self.new_blocks(obs, &mut out);
                }
            }
        }
        out
    }

    fn start_sync(&mut self, now: Instant, obs: &Observed, out: &mut Vec<Action>) {
        self.phase = Phase::Syncing;
        self.last_progress = None;
        self.rate = None;
        out.push(Action::Progress(self.progress_line(now, obs)));
    }

    fn synchronized(&mut self, obs: &Observed, out: &mut Vec<Action>) {
        self.phase = Phase::Synced;
        self.last_top = (obs.top, obs.top_hash);
        out.push(Action::Synced(obs.top));
    }

    fn new_blocks(&mut self, obs: &Observed, out: &mut Vec<Action>) {
        let (last, last_hash) = self.last_top;
        if obs.top > last {
            let first = (last + 1).max(obs.top.saturating_sub(MAX_BLOCK_LINES - 1));
            if first > last + 1 {
                out.push(Action::Line(format!(
                    "{} blocks added ({} to {})",
                    term::group(first - last - 1),
                    term::group(last + 1),
                    term::group(first - 1)
                )));
            }
            out.push(Action::NewBlocks(first..=obs.top));
        } else if obs.top_hash != last_hash {
            out.push(Action::Reorg(obs.top));
        }
        self.last_top = (obs.top, obs.top_hash);
    }

    /// `Syncing 1 234 567 / 2 620 300 (47.12%) · 850 blocks/s · ETA 27m 10s`.
    #[allow(clippy::cast_precision_loss)]
    fn progress_line(&mut self, now: Instant, obs: &Observed) -> String {
        if let Some((at, top)) = self.last_progress {
            let dt = now.duration_since(at).as_secs_f64();
            if dt > 0.0 {
                let r = obs.top.saturating_sub(top) as f64 / dt;
                self.rate = Some(self.rate.map_or(r, |old| 0.3 * r + 0.7 * old));
            }
        }
        self.last_progress = Some((now, obs.top));
        let target = obs.network_height.saturating_sub(1).max(obs.top);
        let mut line = format!(
            "{} {} / {} ({}%)",
            self.style.paint(Color::Cyan, "Syncing"),
            term::group(obs.top),
            term::group(target),
            term::sync_percentage(obs.top + 1, obs.network_height)
        );
        if let Some(rate) = self.rate {
            let _ = write!(line, " · {rate:.0} blocks/s");
            let left = target - obs.top;
            if rate >= 0.5 && left > 0 {
                let _ = write!(line, " · ETA {}", term::eta((left as f64 / rate) as u64));
            }
        }
        line
    }
}

/// `New block 2 620 300 · diff 3 703 125 · 2 txs` (+ `· mined here`).
#[must_use]
pub fn new_block_line(style: Style, height: u64, info: &BlockInfo, mined_here: bool) -> String {
    let txs = match info.tx_count {
        1 => "1 tx".to_owned(),
        n => format!("{} txs", term::group(u64::from(n))),
    };
    let mut line = format!(
        "{} {} · diff {} · {txs}",
        style.paint(Color::Green, "New block"),
        style.paint(Color::Bold, &term::group(height)),
        term::group(info.difficulty)
    );
    if mined_here {
        line.push_str(" · ");
        line.push_str(&style.paint(Color::Green, "mined here"));
    }
    line
}

/// `  ✔ Synced · height 2 620 300` on a colour terminal, `xkrd synced · height 2 620 300`
/// otherwise.
#[must_use]
pub fn synced_line(style: Style, fancy: bool, top: u64) -> String {
    let height = format!("height {}", term::group(top));
    if fancy {
        format!(
            "  {} {}",
            style.paint(Color::Green, "✔ Synced"),
            style.paint(Color::Dim, &format!("· {height}"))
        )
    } else {
        format!("xkrd synced · {height}")
    }
}

/// `Switched to another chain, top block 2 620 300`.
#[must_use]
pub fn reorg_line(style: Style, top: u64) -> String {
    style.paint(
        Color::Yellow,
        &format!("Switched to another chain, top block {}", term::group(top)),
    )
}

/// What the panel's status line shows.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelStatus {
    /// No peer yet, or waiting for their heights.
    Connecting { peers: usize },
    /// At the top, waiting for the next block.
    Waiting {
        /// Seconds since the top block's timestamp.
        last_block_secs: u64,
        /// The top block's difficulty.
        difficulty: u64,
        peers: usize,
    },
    /// A new top block arrived (shown for [`PANEL_FLASH`]).
    NewBlock {
        txs: u32,
        difficulty: u64,
        mined_here: bool,
    },
    /// The main chain switched (shown for [`PANEL_FLASH`]).
    Switched,
    /// Behind the network.
    Syncing {
        /// Network height (block count).
        network_height: u64,
        /// Blocks per second, once known.
        rate: Option<f64>,
    },
}

fn txs_text(n: u32) -> String {
    if n == 1 {
        "1 tx".to_owned()
    } else {
        format!("{} txs", term::group(u64::from(n)))
    }
}

/// Status line plus label rows under the height.
const PANEL_STATUS_LINES: usize = 4;

/// The panel for top block index `top`: the height in big digits (normal digits below
/// [`PANEL_NARROW`] columns or when they do not fit) and the status line with spinner frame
/// `frame`, every line cut to `width - 1` columns (the last column stays free, so no line
/// wraps).
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn render_panel(
    top: u64,
    status: &PanelStatus,
    frame: usize,
    width: usize,
    style: Style,
) -> Vec<String> {
    let usable = width.saturating_sub(1).max(1);
    let mut lines = Vec::with_capacity(8);
    let big = term::big_number(top);
    if width >= PANEL_NARROW && big[0].chars().count() + 2 <= usable {
        for row in &big {
            lines.push(format!("  {}", style.paint(Color::Cyan, row)));
        }
    } else {
        lines.push(term::fit(
            &format!("  {}", style.paint(Color::Bold, &term::group(top))),
            usable,
        ));
    }
    let big_rows = lines.len();
    let spin = style.paint(Color::Cyan, &SPINNER[frame % SPINNER.len()].to_string());
    // A status line, then aligned label/value rows; always PANEL_STATUS_LINES lines, so the
    // panel keeps its height when the state changes.
    let mut details: Vec<(&str, String)> = Vec::new();
    let text = match status {
        PanelStatus::Connecting { peers: 0 } => format!("{spin} waiting for peers"),
        PanelStatus::Connecting { peers } => {
            details.push(("peers", peers.to_string()));
            format!("{spin} looking for the top of the chain")
        }
        PanelStatus::Waiting {
            last_block_secs,
            difficulty,
            peers,
        } => {
            details.push(("last block", format!("{} ago", term::eta(*last_block_secs))));
            details.push(("difficulty", term::group(*difficulty)));
            details.push(("peers", peers.to_string()));
            format!("{spin} waiting for block {}", term::group(top + 1))
        }
        PanelStatus::NewBlock {
            txs,
            difficulty,
            mined_here,
        } => {
            details.push(("difficulty", term::group(*difficulty)));
            let mut t = format!("✔ new block · {}", txs_text(*txs));
            if *mined_here {
                t.push_str(" · mined here");
            }
            style.paint(Color::Green, &t)
        }
        PanelStatus::Switched => style.paint(
            Color::Yellow,
            &format!("↺ switched chain · top block {}", term::group(top)),
        ),
        PanelStatus::Syncing {
            network_height,
            rate,
        } => {
            let done = (top + 1).min(*network_height);
            let filled = if *network_height == 0 {
                0
            } else {
                (PANEL_BAR as u64 * done / network_height) as usize
            };
            let bar = format!(
                "{}{}",
                style.paint(Color::Green, &"█".repeat(filled)),
                style.paint(Color::Dim, &"░".repeat(PANEL_BAR - filled))
            );
            let target = network_height.saturating_sub(1).max(top);
            if let Some(rate) = rate {
                details.push(("speed", format!("{rate:.0} blocks/s")));
                let left = target - top;
                if *rate >= 0.5 && left > 0 {
                    details.push(("time left", term::eta((left as f64 / rate) as u64)));
                }
            }
            details.push(("top", term::group(target)));
            format!(
                "{spin} syncing {bar} {}%",
                term::sync_percentage(top + 1, *network_height)
            )
        }
    };
    lines.push(term::fit(&format!("  {text}"), usable));
    for (label, value) in details.iter().take(PANEL_STATUS_LINES - 1) {
        let label = style.paint(Color::Dim, &format!("{label:<12}"));
        lines.push(term::fit(&format!("    {label} {value}"), usable));
    }
    while lines.len() < big_rows + PANEL_STATUS_LINES {
        lines.push(String::new());
    }
    lines
}

/// Whether the live panel is used: a terminal that takes colours and cursor movement.
#[must_use]
pub fn panel_mode(term: &Term) -> bool {
    term.tty() && term.color()
}

/// Reads the chain and the P2P status.
fn observe(chain: &Chain, net: &watch::Receiver<Arc<NetStatus>>) -> (Observed, xkr_chain::TipInfo) {
    let tip = chain.tip();
    let status = net.borrow().clone();
    let obs = Observed {
        top: tip.height,
        top_hash: tip.hash,
        network_height: status.network_height.max(tip.height + 1),
        synced: status.synced,
        downloading: status.downloading,
        peers: status.outgoing + status.incoming + status.hyper,
        port_hint: status.hyper_net.is_some_and(crate::hyper::wants_port_hint),
        hyper_off: crate::hyper::off_reason(),
    };
    (obs, tip)
}

/// The info of the last block of `range` and whether the built-in miner found it.
fn last_block(
    chain: &Chain,
    mining: &watch::Receiver<Arc<MiningStatus>>,
    height: u64,
) -> Option<(BlockInfo, bool)> {
    let info = chain.reader().ok()?.block_info(height).ok()??;
    let mined = mining
        .borrow()
        .last_block_hash
        .as_deref()
        .is_some_and(|h| h == Hash(info.hash).to_string());
    Some((info, mined))
}

/// Prints the status lines (or keeps the panel) until `shutdown`.
pub async fn run(
    term: Term,
    chain: Arc<Chain>,
    net: watch::Receiver<Arc<NetStatus>>,
    mining: watch::Receiver<Arc<MiningStatus>>,
    shutdown: CancellationToken,
) {
    if panel_mode(&term) {
        run_panel(&term, &chain, &net, &mining, &shutdown).await;
    } else {
        run_lines(&term, &chain, &net, &mining, &shutdown).await;
    }
    term.clear_progress();
}

/// One line per block and the sync line, once a second.
async fn run_lines(
    term: &Term,
    chain: &Chain,
    net: &watch::Receiver<Arc<NetStatus>>,
    mining: &watch::Receiver<Arc<MiningStatus>>,
    shutdown: &CancellationToken,
) {
    let style = term.style();
    let mut display = SyncDisplay::new(Instant::now(), style, term.tty());
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            _ = tick.tick() => {}
        }
        let (obs, _) = observe(chain, net);
        for action in display.update(Instant::now(), &obs) {
            match action {
                Action::Progress(text) => term.progress(&text),
                Action::ClearProgress => term.clear_progress(),
                Action::Line(text) => term.print(&text),
                Action::Reorg(top) => term.print(&reorg_line(style, top)),
                Action::Synced(top) => term.print(&synced_line(style, false, top)),
                Action::NewBlocks(range) => {
                    for height in range {
                        if let Some((info, here)) = last_block(chain, mining, height) {
                            term.print(&new_block_line(style, height, &info, here));
                        }
                    }
                }
            }
        }
    }
}

/// The live panel: redrawn 8 times a second, the chain read once a second.
async fn run_panel(
    term: &Term,
    chain: &Chain,
    net: &watch::Receiver<Arc<NetStatus>>,
    mining: &watch::Receiver<Arc<MiningStatus>>,
    shutdown: &CancellationToken,
) {
    let style = term.style();
    let mut display = SyncDisplay::new(Instant::now(), style, true);
    let mut tick = tokio::time::interval(PANEL_TICK);
    let mut width = tokio::task::spawn_blocking(term::columns)
        .await
        .unwrap_or(80);
    let mut base = PanelStatus::Connecting { peers: 0 };
    let mut flash: Option<(PanelStatus, Instant)> = None;
    let mut top = chain.tip().height;
    let mut n: u64 = 0;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            _ = tick.tick() => {}
        }
        let now = Instant::now();
        if n.is_multiple_of(PANEL_TICKS_PER_SECOND) {
            let (obs, tip) = observe(chain, net);
            top = obs.top;
            for action in display.update(now, &obs) {
                match action {
                    Action::Progress(_) | Action::ClearProgress => {}
                    Action::Line(text) => term.print(&text),
                    Action::Synced(top) => term.print(&synced_line(style, true, top)),
                    Action::Reorg(_) => flash = Some((PanelStatus::Switched, now + PANEL_FLASH)),
                    Action::NewBlocks(range) => {
                        if let Some((info, mined_here)) = last_block(chain, mining, *range.end()) {
                            let status = PanelStatus::NewBlock {
                                txs: info.tx_count,
                                difficulty: info.difficulty,
                                mined_here,
                            };
                            flash = Some((status, now + PANEL_FLASH));
                        }
                    }
                }
            }
            base = match display.phase() {
                Phase::Waiting => PanelStatus::Connecting { peers: obs.peers },
                Phase::Syncing => PanelStatus::Syncing {
                    network_height: obs.network_height,
                    rate: display.rate(),
                },
                Phase::Synced => PanelStatus::Waiting {
                    last_block_secs: xkr_chain::unix_now().saturating_sub(tip.timestamp),
                    difficulty: tip.difficulty,
                    peers: obs.peers,
                },
            };
        }
        if n.is_multiple_of(PANEL_WIDTH_TICKS) && n > 0 {
            width = tokio::task::spawn_blocking(term::columns)
                .await
                .unwrap_or(width);
        }
        if flash.as_ref().is_some_and(|(_, until)| now >= *until) {
            flash = None;
        }
        let status = flash.as_ref().map_or(&base, |(s, _)| s);
        term.panel(render_panel(top, status, n as usize, width, style));
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(top: u64, network_height: u64, peers: usize) -> Observed {
        Observed {
            top,
            top_hash: Hash([top as u8; 32]),
            network_height,
            synced: top + 1 >= network_height,
            downloading: top + 1 < network_height,
            peers,
            port_hint: false,
            hyper_off: None,
        }
    }

    fn lines(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Line(l) | Action::Progress(l) => Some(l.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn waits_for_peers_then_syncs_then_reports_blocks() {
        let t0 = Instant::now();
        let mut d = SyncDisplay::new(t0, Style::new(false), true);
        // no peers: nothing, then one notice
        assert_eq!(d.update(t0, &obs(10, 11, 0)), vec![]);
        let notice = d.update(t0 + NO_PEERS_NOTICE, &obs(10, 11, 0));
        assert_eq!(lines(&notice).len(), 1);
        assert!(
            d.update(t0 + NO_PEERS_NOTICE * 2, &obs(10, 11, 0))
                .is_empty()
        );

        // a peer far ahead: the progress line at once, without a rate yet
        let t1 = t0 + Duration::from_secs(100);
        let a = d.update(t1, &obs(1_000, 101_001, 3));
        assert_eq!(
            a,
            vec![Action::Progress("Syncing 1 000 / 101 000 (0.99%)".into())]
        );
        // not again within the interval
        assert!(
            d.update(t1 + Duration::from_secs(1), &obs(1_500, 101_001, 3))
                .is_empty()
        );
        // 3 s later: 3 000 blocks -> 1 000 blocks/s, 97 000 left
        let a = d.update(t1 + PROGRESS_TTY, &obs(4_000, 101_001, 3));
        assert_eq!(
            lines(&a),
            ["Syncing 4 000 / 101 000 (3.96%) · 1000 blocks/s · ETA 1m 37s"]
        );

        // at the top: the progress line goes, "synced" comes
        let t2 = t1 + Duration::from_secs(200);
        let a = d.update(t2, &obs(101_000, 101_001, 3));
        assert_eq!(a, vec![Action::ClearProgress, Action::Synced(101_000)]);
        // no change: nothing
        assert!(d.update(t2, &obs(101_000, 101_001, 3)).is_empty());
        // two new blocks
        assert_eq!(
            d.update(t2, &obs(101_002, 101_003, 3)),
            vec![Action::NewBlocks(101_001..=101_002)]
        );
        // one block behind for a moment is not a resync
        let mut o = obs(101_002, 101_004, 3);
        o.downloading = true;
        o.synced = false;
        assert!(d.update(t2, &o).is_empty());
        // many blocks at once: a summary and the last MAX_BLOCK_LINES
        let a = d.update(t2, &obs(101_102, 101_103, 3));
        assert_eq!(lines(&a), ["80 blocks added (101 003 to 101 082)"]);
        assert_eq!(a[1], Action::NewBlocks(101_083..=101_102));
        // same height, other hash: a switch
        let mut o = obs(101_102, 101_103, 3);
        o.top_hash = Hash([1; 32]);
        assert_eq!(d.update(t2, &o), vec![Action::Reorg(101_102)]);
        assert_eq!(
            reorg_line(Style::new(false), 101_102),
            "Switched to another chain, top block 101 102"
        );
        // far behind again while downloading: progress again
        let a = d.update(t2, &obs(101_102, 101_200, 3));
        assert!(matches!(&a[0], Action::Progress(l) if l.starts_with("Syncing 101 102 / 101 199")));
    }

    #[test]
    fn a_node_at_the_top_says_synchronized_once_peers_agree() {
        let t0 = Instant::now();
        let mut d = SyncDisplay::new(t0, Style::new(false), false);
        // the first peers' heights may not be in yet
        assert!(d.update(t0, &obs(50, 51, 2)).is_empty());
        assert!(
            d.update(t0 + Duration::from_secs(1), &obs(50, 51, 2))
                .is_empty()
        );
        let a = d.update(t0 + PEER_SETTLE, &obs(50, 51, 2));
        assert_eq!(a, vec![Action::Synced(50)]);
        assert_eq!(
            synced_line(Style::new(false), true, 50),
            "  ✔ Synced · height 50"
        );
        assert_eq!(
            synced_line(Style::new(false), false, 2_620_300),
            "xkrd synced · height 2 620 300"
        );
        assert_eq!(
            synced_line(Style::new(true), true, 7),
            "  \x1b[1;32m✔ Synced\x1b[0m \x1b[2m· height 7\x1b[0m"
        );
        assert!(d.update(t0 + PEER_SETTLE, &obs(50, 51, 2)).is_empty());
    }

    /// The `--port` hint comes once, in yellow, within 60 columns, whatever the phase.
    #[test]
    fn port_hint_is_said_once() {
        let t0 = Instant::now();
        let mut d = SyncDisplay::new(t0, Style::new(false), false);
        let hinted = Observed {
            port_hint: true,
            ..obs(50, 51, 2)
        };
        let a = d.update(t0, &hinted);
        let text = lines(&a).join(" ");
        assert_eq!(text, crate::hyper::PORT_HINT);
        assert!(lines(&a).iter().all(|l| l.chars().count() <= HINT_COLUMNS));
        assert!(lines(&d.update(t0, &hinted)).is_empty(), "only once");

        let mut c = SyncDisplay::new(t0, Style::new(true), false);
        let coloured = lines(&c.update(t0, &hinted));
        assert!(coloured[0].starts_with("\x1b[1;33m"), "{coloured:?}");
        assert!(lines(&c.update(t0, &obs(50, 51, 2))).is_empty());
    }

    /// "Hyperswarm off" comes once, in yellow, within 60 columns.
    #[test]
    fn hyperswarm_off_is_said_once() {
        let t0 = Instant::now();
        let mut d = SyncDisplay::new(t0, Style::new(false), false);
        let off = Observed {
            hyper_off: Some(xkr_p2p_hyper::bare::CXX_RUNTIME_MISSING),
            ..obs(50, 51, 2)
        };
        let a = d.update(t0, &off);
        assert_eq!(
            lines(&a).join(" "),
            "Hyperswarm off: libstdc++.so.6 not found (needed by the Hyperswarm addons). Install \
             it, or on NixOS add gcc's lib directory to LD_LIBRARY_PATH."
        );
        assert!(lines(&a).iter().all(|l| l.chars().count() <= HINT_COLUMNS));
        assert!(lines(&d.update(t0, &off)).is_empty(), "only once");

        let mut c = SyncDisplay::new(t0, Style::new(true), false);
        let coloured = lines(&c.update(t0, &off));
        assert!(coloured[0].starts_with("\x1b[1;33m"), "{coloured:?}");
    }

    #[test]
    fn block_lines() {
        let info = BlockInfo {
            difficulty: 3_703_125,
            tx_count: 2,
            ..zero_info()
        };
        assert_eq!(
            new_block_line(Style::new(false), 2_620_300, &info, false),
            "New block 2 620 300 · diff 3 703 125 · 2 txs"
        );
        let one = BlockInfo {
            tx_count: 1,
            ..info
        };
        assert_eq!(
            new_block_line(Style::new(false), 7, &one, true),
            "New block 7 · diff 3 703 125 · 1 tx · mined here"
        );
    }

    #[test]
    fn panel_layout() {
        let plain = Style::new(false);
        let big = term::big_number(2_620_300).map(|r| format!("  {r}"));
        let waiting = PanelStatus::Waiting {
            last_block_secs: 74,
            difficulty: 2_609_115_515,
            peers: 8,
        };
        let p = render_panel(2_620_300, &waiting, 0, 120, plain);
        assert_eq!(p.len(), 7);
        assert_eq!(p[..3], big);
        assert_eq!(
            p[3..],
            [
                "  ⠋ waiting for block 2 620 301",
                "    last block   1m 14s ago",
                "    difficulty   2 609 115 515",
                "    peers        8",
            ]
        );
        // the spinner turns with the frame
        assert!(render_panel(2_620_300, &waiting, 1, 120, plain)[3].starts_with("  ⠙ "));
        assert!(render_panel(2_620_300, &waiting, 10, 120, plain)[3].starts_with("  ⠋ "));

        let new_block = PanelStatus::NewBlock {
            txs: 2,
            difficulty: 3_703_125,
            mined_here: true,
        };
        let p = render_panel(2_620_300, &new_block, 3, 120, plain);
        assert_eq!(p.len(), 7, "the panel keeps its height");
        assert_eq!(p[..3], big);
        assert_eq!(
            p[3..],
            [
                "  ✔ new block · 2 txs · mined here",
                "    difficulty   3 703 125",
                "",
                "",
            ]
        );
        assert_eq!(
            render_panel(2_620_300, &PanelStatus::Switched, 0, 120, plain)[3],
            "  ↺ switched chain · top block 2 620 300"
        );
        assert_eq!(
            render_panel(5, &PanelStatus::Connecting { peers: 0 }, 0, 80, plain)[3],
            "  ⠋ waiting for peers"
        );
        assert_eq!(
            render_panel(5, &PanelStatus::Connecting { peers: 1 }, 0, 80, plain)[3..5],
            ["  ⠋ looking for the top of the chain", "    peers        1"]
        );

        // syncing: block 999 of 2 000 blocks, 100 blocks/s, 1 000 left
        let syncing = PanelStatus::Syncing {
            network_height: 2_000,
            rate: Some(100.0),
        };
        let p = render_panel(999, &syncing, 1, 120, plain);
        assert_eq!(p[..3], term::big_number(999).map(|r| format!("  {r}")));
        assert_eq!(
            p[3..],
            [
                "  ⠙ syncing ██████████░░░░░░░░░░ 50.00%",
                "    speed        100 blocks/s",
                "    time left    10s",
                "    top          1 999",
            ]
        );
        let early = PanelStatus::Syncing {
            network_height: 2_000,
            rate: None,
        };
        assert_eq!(
            render_panel(0, &early, 0, 120, plain)[3..5],
            [
                "  ⠋ syncing ░░░░░░░░░░░░░░░░░░░░ 0.05%",
                "    top          1 999"
            ]
        );

        // narrow: normal digits, every line within the width (last column free)
        let p = render_panel(2_620_300, &waiting, 0, 39, plain);
        assert_eq!(p.len(), 5);
        assert_eq!(p[0], "  2 620 300");
        assert!(p.iter().all(|l| term::visible_width(l) <= 38));
        // coloured: labels dimmed, every line within the width
        let coloured = Style::new(true);
        let p = render_panel(2_620_300, &waiting, 0, 50, coloured);
        assert_eq!(p.len(), 7);
        assert!(p.iter().all(|l| term::visible_width(l) <= 49));
        assert!(p[4].contains("\x1b["));
    }

    fn zero_info() -> BlockInfo {
        BlockInfo {
            hash: [0; 32],
            timestamp: 0,
            difficulty: 0,
            cumulative_difficulty_lo: 0,
            cumulative_difficulty_hi: 0,
            already_generated_coins: 0,
            cumulative_tx_count: 0,
            block_size: 0,
            cumulative_size: 0,
            tx_count: 0,
            flags: 0,
        }
    }
}
