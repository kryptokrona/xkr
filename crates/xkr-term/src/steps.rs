//! Start-up and shutdown, step by step (xkrd and xkr-miner; [`Subject`] names the program).
//! While the program starts, `main` reports each part it brings up (a [`Kind::Start`]
//! reporter, "Starting the node"); after Ctrl-C, SIGTERM or the console's `exit` every part
//! reports when it starts and finishes stopping ([`step`], the global [`Kind::Stop`]
//! reporter), in the order `main` stops them. On a colour terminal a
//! running step has a spinner and its elapsed time at the bottom and turns into a green ✔ (or a
//! red ✗ with the reason) with its time; otherwise one plain line per step (`starting Storage …
//! done 2.1 s (…)`, `stopping RPC server … done 12 ms`); with `--log-json` one `startup step` /
//! `shutdown step` event per step. Start-up ends with "Node running · started in …", shutdown
//! with "Node stopped at height … · uptime …".
//!
//! The stop reporter is global ([`install`]) so the components report without new
//! parameters; with none installed (tests, `xkrd db`) a step does nothing.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::term::{self, Color, SPINNER, Style, Term};

/// Spinner frame interval of running steps.
const FRAME: Duration = Duration::from_millis(100);
/// Plain and JSON output repeat a step's status at most this often (the terminal shows it live).
const STATUS_EVERY: Duration = Duration::from_secs(5);
/// Delay between the logo lines of the closing effect (5 lines, 0.3 s).
const LOGO_LINE: Duration = Duration::from_millis(60);

/// How steps are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Colour terminal: spinners, ✔ / ✗, the closing logo.
    Fancy,
    /// One plain line per step (files, pipes, `NO_COLOR`, `TERM=dumb`).
    Plain,
    /// Tracing events only (`--log-json`).
    Json,
}

/// Start-up or shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Start,
    Stop,
}

/// The program the steps are about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subject {
    /// In titles: "Starting the node", "the node stops after …".
    pub noun: &'static str,
    /// At the start of the final lines: "Node running", "Node stopped".
    pub capitalised: &'static str,
    /// In plain lines: "starting xkrd", "xkrd running".
    pub program: &'static str,
}

impl Subject {
    /// xkrd.
    pub const NODE: Self = Self {
        noun: "node",
        capitalised: "Node",
        program: "xkrd",
    };
    /// xkr-miner.
    pub const MINER: Self = Self {
        noun: "miner",
        capitalised: "Miner",
        program: "xkr-miner",
    };
}

impl Kind {
    fn title(self, subject: Subject) -> String {
        match self {
            Self::Start => format!("Starting the {}", subject.noun),
            Self::Stop => format!("Stopping the {}", subject.noun),
        }
    }

    /// The verb of the plain lines.
    const fn verb(self) -> &'static str {
        match self {
            Self::Start => "starting",
            Self::Stop => "stopping",
        }
    }

    /// The message of the `--log-json` events.
    const fn event(self) -> &'static str {
        match self {
            Self::Start => "startup step",
            Self::Stop => "shutdown step",
        }
    }
}

/// How a step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Done, with an optional detail (`8 peers disconnected`).
    Done(Option<String>),
    /// Failed, with the reason.
    Failed(String),
    /// Given up after this long.
    TimedOut(Duration),
}

/// Where the reporter writes: whole lines above the bottom region, and that region.
pub trait Sink: Send + Sync {
    /// A finished line.
    fn line(&self, text: &str);
    /// The running steps (empty: none).
    fn region(&self, lines: Vec<String>);
    /// Columns of the region's lines, if known (longer ones are cut or wrapped, so none wraps
    /// on the terminal and the region keeps its height).
    fn width(&self) -> Option<usize> {
        None
    }
}

impl Sink for Term {
    fn line(&self, text: &str) {
        self.print(text);
    }

    fn region(&self, lines: Vec<String>) {
        if lines.is_empty() {
            self.clear_progress();
        } else {
            self.panel(lines);
        }
    }

    fn width(&self) -> Option<usize> {
        Some(self.columns())
    }
}

/// A line of the start-up block ([`Kind::Start`] on a colour terminal).
#[derive(Debug, Clone)]
enum BlockLine {
    /// A finished step (painted).
    Step(String),
    /// An explanation (plain text, wrapped to the width and painted dim when drawn).
    Note(String),
}

#[derive(Debug)]
struct Running {
    id: u64,
    name: String,
    started: Instant,
    /// What the step is doing now ([`StepHandle::status`]).
    status: Option<String>,
    /// When the status was last printed (plain / JSON).
    status_shown: Option<Instant>,
}

#[derive(Debug, Default)]
struct State {
    begun: bool,
    ended: bool,
    next_id: u64,
    frame: usize,
    running: Vec<Running>,
    /// The finished lines of the start-up block, kept in the region until it ends.
    block: Vec<BlockLine>,
    /// Shown dim after the start-up block's title (`· Mainnet`).
    subtitle: Option<String>,
}

/// The step reporter.
pub struct Reporter {
    kind: Kind,
    subject: Subject,
    mode: Mode,
    style: Style,
    sink: Arc<dyn Sink>,
    state: Mutex<State>,
}

impl std::fmt::Debug for Reporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reporter")
            .field("kind", &self.kind)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl Reporter {
    /// The shutdown reporter.
    #[must_use]
    pub fn new(mode: Mode, style: Style, sink: Arc<dyn Sink>) -> Arc<Self> {
        Self::with_kind(Kind::Stop, mode, style, sink)
    }

    /// A start-up or shutdown reporter.
    #[must_use]
    pub fn with_kind(kind: Kind, mode: Mode, style: Style, sink: Arc<dyn Sink>) -> Arc<Self> {
        Self::with_subject(kind, Subject::NODE, mode, style, sink)
    }

    /// A start-up or shutdown reporter about `subject`.
    #[must_use]
    pub fn with_subject(
        kind: Kind,
        subject: Subject,
        mode: Mode,
        style: Style,
        sink: Arc<dyn Sink>,
    ) -> Arc<Self> {
        Arc::new(Self {
            kind,
            subject,
            mode,
            style,
            sink,
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start-up on a colour terminal keeps its whole block (title, finished steps, notes,
    /// running steps) in the bottom region, so the title can turn into "Started" at the end.
    fn keeps_block(&self) -> bool {
        self.kind == Kind::Start && self.mode == Mode::Fancy
    }

    /// The bottom region: the running steps, or the whole start-up block.
    fn region_lines(&self, state: &State) -> Vec<String> {
        if !self.keeps_block() {
            return self.running_lines(state);
        }
        let width = self.sink.width().map(|w| w.saturating_sub(1).max(20));
        let title = self.with_subtitle(
            state,
            &self
                .style
                .paint(Color::Bold, &self.kind.title(self.subject)),
        );
        let mut lines = self.block_lines(state, &title, width);
        for line in self.running_lines(state) {
            lines.push(width.map_or_else(|| line.clone(), |w| term::fit(&line, w)));
        }
        lines
    }

    /// The start-up block under `title`: a blank line, the title, the finished lines; cut or
    /// wrapped to `width` columns when given.
    fn block_lines(&self, state: &State, title: &str, width: Option<usize>) -> Vec<String> {
        let mut lines = vec![String::new(), format!("  {title}")];
        for entry in &state.block {
            match entry {
                BlockLine::Step(line) => {
                    lines.push(width.map_or_else(|| line.clone(), |w| term::fit(line, w)));
                }
                BlockLine::Note(text) => {
                    let parts = match width {
                        Some(w) => term::wrap(text, w.saturating_sub(4).max(10)),
                        None => vec![text.clone()],
                    };
                    for part in parts {
                        lines.push(format!("    {}", self.style.paint(Color::Dim, &part)));
                    }
                }
            }
        }
        lines
    }

    /// Sets the text shown dim after the start-up block's title (`Mainnet` gives
    /// "Starting the node · Mainnet", later "Started · Mainnet").
    pub fn set_subtitle(&self, text: &str) {
        self.lock().subtitle = Some(text.to_owned());
    }

    /// `title` with the dim ` · <subtitle>` after it, when one is set.
    fn with_subtitle(&self, state: &State, title: &str) -> String {
        match &state.subtitle {
            Some(sub) => format!(
                "{title} {}",
                self.style.paint(Color::Dim, &format!("· {sub}"))
            ),
            None => title.to_owned(),
        }
    }

    /// Prints the start-up block for good under `title` (the region goes).
    fn print_block(&self, state: &State, title: &str) {
        self.sink.region(Vec::new());
        self.sink
            .line(&self.block_lines(state, title, None).join("\n"));
    }

    /// Asked to stop during start-up: said in the start-up block (the node stops after the
    /// step under way, then the stop steps follow).
    pub fn stop_requested(&self) {
        let mut state = self.lock();
        if state.ended {
            return;
        }
        let text = format!(
            "stop requested: the {} stops after the step under way",
            self.subject.noun
        );
        let text = text.as_str();
        match self.mode {
            Mode::Fancy if self.keeps_block() => {
                state.block.push(BlockLine::Step(format!(
                    "  {}",
                    self.style.paint(Color::Yellow, text)
                )));
                self.sink.region(self.region_lines(&state));
            }
            Mode::Fancy => self
                .sink
                .line(&format!("  {}", self.style.paint(Color::Yellow, text))),
            Mode::Plain => self.sink.line(text),
            Mode::Json => tracing::info!("{text}"),
        }
    }

    /// Shows the title once (and starts the spinner thread in [`Mode::Fancy`]).
    pub fn begin(self: &Arc<Self>) {
        let mut state = self.lock();
        if state.begun {
            return;
        }
        state.begun = true;
        match self.mode {
            Mode::Fancy => {
                if self.keeps_block() {
                    self.sink.region(self.region_lines(&state));
                } else {
                    self.sink.line(&format!(
                        "\n  {}",
                        self.style
                            .paint(Color::Bold, &self.kind.title(self.subject))
                    ));
                }
                let me = self.clone();
                let _ = std::thread::Builder::new()
                    .name("xkr-steps".into())
                    .spawn(move || {
                        while me.tick() {
                            std::thread::sleep(FRAME);
                        }
                    });
            }
            Mode::Plain => {
                self.sink
                    .line(&format!("{} {}", self.kind.verb(), self.subject.program));
            }
            Mode::Json => tracing::info!("{} {}", self.kind.verb(), self.subject.program),
        }
    }

    /// One spinner frame; `false` once [`Reporter::end`] ran.
    fn tick(&self) -> bool {
        let mut state = self.lock();
        if state.ended {
            return false;
        }
        state.frame += 1;
        if !state.running.is_empty() {
            self.sink.region(self.region_lines(&state));
        }
        true
    }

    fn running_lines(&self, state: &State) -> Vec<String> {
        state
            .running
            .iter()
            .map(|r| {
                render_running(
                    self.style,
                    &r.name,
                    r.status.as_deref(),
                    r.started.elapsed(),
                    state.frame,
                )
            })
            .collect()
    }

    /// Starts the step `name`.
    pub fn step(self: &Arc<Self>, name: &str) -> Step {
        self.begin();
        let mut state = self.lock();
        let id = state.next_id;
        state.next_id += 1;
        let started = Instant::now();
        state.running.push(Running {
            id,
            name: name.to_owned(),
            started,
            status: None,
            status_shown: None,
        });
        if self.mode == Mode::Fancy {
            self.sink.region(self.region_lines(&state));
        }
        Step {
            reporter: Some(self.clone()),
            id,
            name: name.to_owned(),
            started,
            open: true,
        }
    }

    /// Sets what the running step `id` is doing now: live on a colour terminal, at most every
    /// [`STATUS_EVERY`] (and whenever the text before its last two words changes) otherwise.
    fn status(&self, id: u64, text: &str) {
        let mut state = self.lock();
        let Some(i) = state.running.iter().position(|r| r.id == id) else {
            return;
        };
        let now = Instant::now();
        let r = &mut state.running[i];
        let changed_kind = r
            .status
            .as_deref()
            .is_none_or(|old| status_kind(old) != status_kind(text));
        let due = changed_kind
            || r.status_shown
                .is_none_or(|at| now.duration_since(at) >= STATUS_EVERY);
        text.clone_into(r.status.get_or_insert_with(String::new));
        let name = r.name.clone();
        if due {
            r.status_shown = Some(now);
        }
        match self.mode {
            Mode::Fancy => self.sink.region(self.region_lines(&state)),
            Mode::Plain if due => {
                self.sink
                    .line(&format!("{} {name} … {text}", self.kind.verb()));
            }
            Mode::Json if due => {
                tracing::info!(step = name.as_str(), status = text, "{}", self.kind.event());
            }
            Mode::Plain | Mode::Json => {}
        }
    }

    /// A line of explanation above the running step `id` (once; why it takes long): dim and
    /// indented on a colour terminal, a plain indented line otherwise.
    fn note(&self, id: u64, text: &str) {
        let mut state = self.lock();
        let Some(name) = state
            .running
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.name.clone())
        else {
            return;
        };
        match self.mode {
            Mode::Fancy if self.keeps_block() => {
                state.block.push(BlockLine::Note(text.to_owned()));
                self.sink.region(self.region_lines(&state));
            }
            Mode::Fancy => self
                .sink
                .line(&format!("    {}", self.style.paint(Color::Dim, text))),
            Mode::Plain => self.sink.line(&format!("  {text}")),
            Mode::Json => {
                tracing::info!(step = name.as_str(), note = text, "{}", self.kind.event());
            }
        }
    }

    /// Whether [`Reporter::end`] (or `ready`, `abort`, `forced`) ran.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.lock().ended
    }

    /// Every exit shows the stop: when nothing ended this reporter yet (a start-up error before
    /// anything was opened), the title and the final line still appear.
    pub fn ensure_ended(self: &Arc<Self>, uptime_secs: u64) {
        if !self.is_ended() {
            self.begin();
            self.end(None, uptime_secs);
        }
    }

    fn finish(&self, id: u64, name: &str, elapsed: Duration, outcome: &Outcome) {
        let mut state = self.lock();
        state.running.retain(|r| r.id != id);
        if state.ended && self.mode == Mode::Fancy {
            // Given up (`abort`) or ended: nothing is drawn any more.
            return;
        }
        match self.mode {
            Mode::Fancy if self.keeps_block() => {
                let line = render_finished(self.style, name, outcome, elapsed);
                state.block.push(BlockLine::Step(line));
                self.sink.region(self.region_lines(&state));
            }
            Mode::Fancy => {
                self.sink
                    .line(&render_finished(self.style, name, outcome, elapsed));
                self.sink.region(self.running_lines(&state));
            }
            Mode::Plain => {
                self.sink
                    .line(&render_plain(self.kind.verb(), name, outcome, elapsed));
            }
            Mode::Json => {
                let ms = elapsed.as_millis() as u64;
                let event = self.kind.event();
                match outcome {
                    Outcome::Done(detail) => tracing::info!(
                        step = name,
                        ms,
                        outcome = "done",
                        detail = detail.as_deref().unwrap_or(""),
                        "{event}"
                    ),
                    Outcome::Failed(reason) => tracing::warn!(
                        step = name,
                        ms,
                        outcome = "failed",
                        reason = reason.as_str(),
                        "{event}"
                    ),
                    Outcome::TimedOut(after) => tracing::warn!(
                        step = name,
                        ms,
                        outcome = "timed out",
                        after_ms = after.as_millis() as u64,
                        "{event}"
                    ),
                }
            }
        }
    }

    /// Start-up done: the spinner stops, the start-up block is printed for good under the
    /// title "Started", and "Node running · started in …" follows (the live panel takes over
    /// the bottom of the terminal).
    pub fn ready(&self, elapsed: Duration) {
        let mut state = self.lock();
        state.ended = true;
        let took = format!("started in {}", elapsed_text(elapsed));
        match self.mode {
            Mode::Fancy => {
                if self.keeps_block() {
                    let title =
                        self.with_subtitle(&state, &self.style.paint(Color::Green, "Started"));
                    self.print_block(&state, &title);
                }
                self.sink.region(Vec::new());
                self.sink.line(&format!(
                    "\n  {} {}\n",
                    self.style.paint(
                        Color::Green,
                        &format!("{} running", self.subject.capitalised)
                    ),
                    self.style.paint(Color::Dim, &format!("· {took}"))
                ));
            }
            Mode::Plain => {
                self.sink
                    .line(&format!("{} running · {took}", self.subject.program));
            }
            Mode::Json => tracing::info!(
                ms = elapsed.as_millis() as u64,
                "{} running",
                self.subject.program
            ),
        }
    }

    /// Start-up given up (the node stops before it was up): the spinner stops; the start-up
    /// block is printed for good under "Start stopped" (the stop steps follow), the region
    /// goes.
    pub fn abort(&self) {
        let mut state = self.lock();
        if !state.ended {
            state.ended = true;
            if self.keeps_block() {
                let title =
                    self.with_subtitle(&state, &self.style.paint(Color::Yellow, "Start stopped"));
                self.print_block(&state, &title);
            } else if self.mode == Mode::Fancy {
                self.sink.region(Vec::new());
            }
        }
    }

    /// The final line, then (on a colour terminal) the logo drawn line by line. `height` is
    /// `None` when the node stopped before its chain was open.
    pub fn end(&self, height: Option<u64>, uptime_secs: u64) {
        let uptime = term::duration_words(uptime_secs);
        let text = match height {
            Some(h) => format!("at height {} · uptime {uptime}", term::group(h)),
            None => format!("· uptime {uptime}"),
        };
        self.end_with(&text);
    }

    /// The final line "<Subject> stopped <text>", then (on a colour terminal) the logo drawn
    /// line by line.
    pub fn end_with(&self, text: &str) {
        let mut state = self.lock();
        state.ended = true;
        let stopped = format!("{} stopped", self.subject.capitalised);
        match self.mode {
            Mode::Fancy => {
                self.sink.region(Vec::new());
                self.sink.line(&format!(
                    "\n  {} {text}\n",
                    self.style.paint(Color::Green, &stopped)
                ));
                drop(state);
                for line in term::logo_lines(self.style) {
                    std::thread::sleep(LOGO_LINE);
                    self.sink.line(&line);
                }
                self.sink.line("");
            }
            Mode::Plain => self.sink.line(&format!("{stopped} {text}")),
            Mode::Json => {}
        }
    }

    /// A line between the steps: dim and indented on a colour terminal, plain otherwise, an
    /// info event with `--log-json`.
    pub fn say(&self, text: &str) {
        let _state = self.lock();
        match self.mode {
            Mode::Fancy => self
                .sink
                .line(&format!("  {}", self.style.paint(Color::Dim, text))),
            Mode::Plain => self.sink.line(text),
            Mode::Json => tracing::info!("{text}"),
        }
    }

    /// A second Ctrl-C: the process exits now.
    pub fn forced(&self) {
        let mut state = self.lock();
        state.ended = true;
        match self.mode {
            Mode::Fancy => {
                self.sink.region(Vec::new());
                self.sink.line(&format!(
                    "  {}",
                    self.style.paint(Color::Red, "✗ forced stop")
                ));
            }
            Mode::Plain => self.sink.line("forced stop"),
            Mode::Json => tracing::warn!("forced stop"),
        }
    }
}

/// A running step; report its end with one of the methods (dropped unfinished, it is reported
/// as failed).
#[derive(Debug)]
#[must_use = "a step is reported when it is finished"]
pub struct Step {
    reporter: Option<Arc<Reporter>>,
    id: u64,
    name: String,
    started: Instant,
    open: bool,
}

/// Reports the status of a running step from elsewhere (a callback); cheap to clone.
#[derive(Debug, Clone)]
pub struct StepHandle {
    reporter: Option<Arc<Reporter>>,
    id: u64,
}

impl StepHandle {
    /// What the step is doing now (`checking the database  37 %`).
    pub fn status(&self, text: &str) {
        if let Some(r) = &self.reporter {
            r.status(self.id, text);
        }
    }

    /// A line of explanation above the running step (why it takes long).
    pub fn note(&self, text: &str) {
        if let Some(r) = &self.reporter {
            r.note(self.id, text);
        }
    }
}

impl Step {
    /// A handle for status updates while the step runs.
    #[must_use]
    pub fn handle(&self) -> StepHandle {
        StepHandle {
            reporter: self.reporter.clone(),
            id: self.id,
        }
    }

    /// Finished.
    pub fn done(self) {
        self.close(&Outcome::Done(None));
    }

    /// Finished, with a detail.
    pub fn done_with(self, detail: impl Into<String>) {
        self.close(&Outcome::Done(Some(detail.into())));
    }

    /// Failed.
    pub fn fail(self, reason: impl Into<String>) {
        self.close(&Outcome::Failed(reason.into()));
    }

    /// Given up after `after`.
    pub fn timed_out(self, after: Duration) {
        self.close(&Outcome::TimedOut(after));
    }

    fn close(mut self, outcome: &Outcome) {
        self.open = false;
        if let Some(r) = &self.reporter {
            r.finish(self.id, &self.name, self.started.elapsed(), outcome);
        }
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        if self.open
            && let Some(r) = self.reporter.take()
        {
            let outcome = Outcome::Failed("stopped before it finished".into());
            r.finish(self.id, &self.name, self.started.elapsed(), &outcome);
        }
    }
}

/// What a stop signal (Ctrl-C, SIGTERM, SIGHUP) does, by how many have arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalAction {
    /// The first: the clean stop starts.
    Stop,
    /// The second: the stop goes on; [`STOPPING_NOTE`] is shown once.
    Note,
    /// Any later one: nothing (only SIGKILL ends the process before the stop is done).
    Ignore,
}

/// Shown once when another stop signal arrives while the program stops cleanly.
pub const STOPPING_NOTE: &str = "stopping cleanly — please wait, the database is being closed";

/// The action for the `n`-th stop signal (1-based): one signal stops cleanly, later ones never
/// cut the stop short, so the database is always closed.
#[must_use]
pub fn signal_action(n: usize) -> SignalAction {
    match n {
        0 | 1 => SignalAction::Stop,
        2 => SignalAction::Note,
        _ => SignalAction::Ignore,
    }
}

static REPORTER: OnceLock<Arc<Reporter>> = OnceLock::new();
static STARTUP: OnceLock<Arc<Reporter>> = OnceLock::new();

/// Installs the start-up reporter (once), so a stop requested during start-up is told there.
pub fn install_startup(reporter: Arc<Reporter>) {
    let _ = STARTUP.set(reporter);
}

/// Installs the node's reporter (once).
pub fn install(reporter: Arc<Reporter>) {
    let _ = REPORTER.set(reporter);
}

/// The installed reporter.
pub fn reporter() -> Option<&'static Arc<Reporter>> {
    REPORTER.get()
}

/// The node starts stopping: shows the stop title, or, while start-up still runs, a line in
/// the start-up block (the stop title follows once start-up has ended).
pub fn begin() {
    if let Some(s) = STARTUP.get()
        && !s.is_ended()
    {
        s.stop_requested();
        return;
    }
    if let Some(r) = REPORTER.get() {
        r.begin();
    }
}

/// Starts a step on the installed reporter (does nothing without one).
pub fn step(name: &str) -> Step {
    step_on(REPORTER.get(), name)
}

/// Starts a step on `reporter` (does nothing without one).
pub fn step_on(reporter: Option<&Arc<Reporter>>, name: &str) -> Step {
    match reporter {
        Some(r) => r.step(name),
        None => Step {
            reporter: None,
            id: 0,
            name: name.to_owned(),
            started: Instant::now(),
            open: false,
        },
    }
}

/// The Storage step's status while redb checks the file after an unclean stop. redb reports
/// its progress only at the phase boundaries (0, 0.3, 0.6, 0.9), so the status shows how
/// much of the file the process has read (`read_bytes`, from `/proc/self/io` on Linux) against
/// its size: `checking the database · ~38 % read`, or the bytes once the reads pass the size
/// (the check reads some pages twice), or no figure without one.
#[must_use]
pub fn check_status(read_bytes: Option<u64>, file_size: u64) -> String {
    match read_bytes {
        Some(read) if file_size > 0 && read < file_size => format!(
            "checking the database · ~{} % read",
            read.saturating_mul(100) / file_size
        ),
        Some(read) if read > 0 => {
            format!("checking the database · {} read", term::bytes(read))
        }
        _ => "checking the database".to_owned(),
    }
}

/// Bytes this process has read so far (`rchar` of `/proc/self/io`: page-cache hits count
/// too); `None` where the file does not exist (not Linux).
#[must_use]
pub fn process_read_bytes() -> Option<u64> {
    let io = std::fs::read_to_string("/proc/self/io").ok()?;
    io.lines()
        .find_map(|l| l.strip_prefix("rchar:"))
        .and_then(|v| v.trim().parse().ok())
}

/// `12 ms`, `1.4 s`, `1m 05s`.
#[must_use]
pub fn elapsed_text(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1_000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", d.as_secs_f64())
    } else {
        term::eta(d.as_secs())
    }
}

/// The part of a status that names what is going on: all but a trailing percentage
/// (`checking the database  37 %` → `checking the database`).
fn status_kind(text: &str) -> &str {
    // `checking the database · ~38 % read`: the part before the figures
    if let Some((kind, _)) = text.split_once(" · ") {
        return kind;
    }
    text.trim_end().strip_suffix('%').map_or(text, |t| {
        t.trim_end()
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .trim_end()
    })
}

/// `  ⠋ RPC server  0.4 s`, with a status `  ⠋ Storage · checking …  37 %  4.2 s`.
#[must_use]
pub fn render_running(
    style: Style,
    name: &str,
    status: Option<&str>,
    elapsed: Duration,
    frame: usize,
) -> String {
    let status = status.map_or_else(String::new, |s| format!(" · {s}"));
    format!(
        "  {} {name}{status}  {}",
        style.paint(Color::Cyan, &SPINNER[frame % SPINNER.len()].to_string()),
        style.paint(Color::Dim, &elapsed_text(elapsed))
    )
}

/// `  ✔ legacy P2P · 8 peers disconnected  12 ms`, `  ✗ … · reason  10.0 s`.
#[must_use]
pub fn render_finished(style: Style, name: &str, outcome: &Outcome, elapsed: Duration) -> String {
    let time = style.paint(Color::Dim, &elapsed_text(elapsed));
    match outcome {
        Outcome::Done(None) => format!("  {} {name}  {time}", style.paint(Color::Green, "✔")),
        Outcome::Done(Some(detail)) => format!(
            "  {} {name} · {detail}  {time}",
            style.paint(Color::Green, "✔")
        ),
        Outcome::Failed(reason) => format!(
            "  {} {name} · {}  {time}",
            style.paint(Color::Red, "✗"),
            style.paint(Color::Red, reason)
        ),
        Outcome::TimedOut(after) => format!(
            "  {} {name} · {}  {time}",
            style.paint(Color::Red, "✗"),
            style.paint(
                Color::Red,
                &format!("timed out after {}", elapsed_text(*after))
            )
        ),
    }
}

/// `stopping RPC server … done 12 ms` (`verb`: `starting` / `stopping`).
#[must_use]
pub fn render_plain(verb: &str, name: &str, outcome: &Outcome, elapsed: Duration) -> String {
    let time = elapsed_text(elapsed);
    match outcome {
        Outcome::Done(None) => format!("{verb} {name} … done {time}"),
        Outcome::Done(Some(detail)) => format!("{verb} {name} … done {time} ({detail})"),
        Outcome::Failed(reason) => format!("{verb} {name} … failed after {time}: {reason}"),
        Outcome::TimedOut(after) => {
            format!("{verb} {name} … timed out after {}", elapsed_text(*after))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what the reporter writes.
    #[derive(Debug, Default)]
    struct Recorder {
        out: Mutex<Vec<String>>,
        width: Option<usize>,
    }

    impl Sink for Recorder {
        fn line(&self, text: &str) {
            self.out.lock().unwrap().push(format!("line {text}"));
        }

        fn region(&self, lines: Vec<String>) {
            self.out
                .lock()
                .unwrap()
                .push(format!("region [{}]", lines.join(" | ")));
        }

        fn width(&self) -> Option<usize> {
            self.width
        }
    }

    fn reporter(mode: Mode) -> (Arc<Reporter>, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        (Reporter::new(mode, Style::new(false), rec.clone()), rec)
    }

    /// Replaces the varying times (`12 ms`, `0.1 s`) with `T` for comparison.
    fn untimed(s: &str) -> String {
        let words: Vec<&str> = s.split(' ').collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < words.len() {
            if let Some(next) = words.get(i + 1) {
                let unit = next.trim_end_matches(|c: char| !c.is_ascii_alphabetic());
                if words[i].parse::<f64>().is_ok() && (unit == "ms" || unit == "s") {
                    out.push(format!("T{}", &next[unit.len()..]));
                    i += 2;
                    continue;
                }
            }
            out.push(words[i].to_owned());
            i += 1;
        }
        out.join(" ")
    }

    #[test]
    fn plain_lines_in_order() {
        let (r, rec) = reporter(Mode::Plain);
        let p2p = r.step("legacy P2P");
        let rpc = r.step("RPC server");
        p2p.done_with("8 peers disconnected");
        rpc.done();
        r.step("Hyperswarm transport")
            .timed_out(Duration::from_secs(10));
        r.step("chain thread").fail("panicked");
        drop(r.step("miner"));
        r.end(Some(2_620_300), 3 * 3_600 + 12 * 60);
        let out: Vec<String> = rec.out.lock().unwrap().iter().map(|l| untimed(l)).collect();
        assert_eq!(
            out,
            [
                "line stopping xkrd",
                "line stopping legacy P2P … done T (8 peers disconnected)",
                "line stopping RPC server … done T",
                "line stopping Hyperswarm transport … timed out after T",
                "line stopping chain thread … failed after T: panicked",
                "line stopping miner … failed after T: stopped before it finished",
                "line Node stopped at height 2 620 300 · uptime 3 h 12 min",
            ]
        );
    }

    #[test]
    fn fancy_spinners_turn_into_marks() {
        let (r, rec) = reporter(Mode::Fancy);
        let p2p = r.step("legacy P2P");
        let rpc = r.step("RPC server");
        p2p.done_with("8 peers disconnected");
        rpc.fail("bind lost");
        r.step("Hyperswarm transport")
            .timed_out(Duration::from_secs(10));
        r.end(Some(5), 3 * 3_600 + 12 * 60);
        let out: Vec<String> = rec.out.lock().unwrap().iter().map(|l| untimed(l)).collect();
        // the spinner thread may add region redraws; compare without them
        let lines: Vec<&String> = out.iter().filter(|l| l.starts_with("line")).collect();
        assert_eq!(
            lines,
            [
                "line \n  Stopping the node",
                "line   ✔ legacy P2P · 8 peers disconnected  T",
                "line   ✗ RPC server · bind lost  T",
                "line   ✗ Hyperswarm transport · timed out after T  T",
                "line \n  Node stopped at height 5 · uptime 3 h 12 min\n",
                "line   ▀▄ ▄▀ █ ▄▀ █▀▀▄",
                "line    ▄▀▄  █▀▄  █▀█",
                "line   ▀   ▀ ▀  ▀ ▀  ▀",
                "line ",
            ]
        );
        // the running steps live in the region: both, then the one left, then none
        let regions: Vec<&String> = out.iter().filter(|l| l.starts_with("region")).collect();
        // (the spinner thread may redraw in between, with another frame)
        assert!(regions[0].contains("legacy P2P"));
        assert!(
            regions
                .iter()
                .any(|r| r.contains("legacy P2P") && r.contains("RPC server"))
        );
        let after_p2p = out.iter().position(|l| l.contains("✔ legacy P2P")).unwrap();
        assert!(out[after_p2p + 1].contains("RPC server"));
        assert!(!out[after_p2p + 1].contains("legacy P2P"));
        assert!(out.contains(&"region []".to_owned()));
    }

    #[test]
    fn renders() {
        let plain = Style::new(false);
        let ms = Duration::from_millis(12);
        assert_eq!(elapsed_text(ms), "12 ms");
        assert_eq!(elapsed_text(Duration::from_millis(1_440)), "1.4 s");
        assert_eq!(elapsed_text(Duration::from_secs(65)), "1m 05s");
        assert_eq!(
            render_running(plain, "RPC server", None, ms, 1),
            "  ⠙ RPC server  12 ms"
        );
        assert_eq!(
            render_finished(
                plain,
                "database",
                &Outcome::Done(Some("height 7".into())),
                ms
            ),
            "  ✔ database · height 7  12 ms"
        );
        assert_eq!(
            render_plain(
                "stopping",
                "database",
                &Outcome::Done(Some("height 7".into())),
                ms
            ),
            "stopping database … done 12 ms (height 7)"
        );
        let coloured = render_finished(Style::new(true), "x", &Outcome::Failed("no".into()), ms);
        assert!(coloured.contains("\x1b[1;31m✗\x1b[0m"));
        // a step without a reporter does nothing
        step("nothing").done();
    }

    #[test]
    fn startup_steps_plain_and_fancy() {
        let rec = Arc::new(Recorder::default());
        let r = Reporter::with_kind(Kind::Start, Mode::Plain, Style::new(false), rec.clone());
        r.step("Storage").done_with("opened 9.30 GB (full)");
        r.step("Chain").fail("database is newer than this xkrd");
        r.ready(Duration::from_millis(1_500));
        let out: Vec<String> = rec.out.lock().unwrap().iter().map(|l| untimed(l)).collect();
        assert_eq!(
            out,
            [
                "line starting xkrd",
                "line starting Storage … done T (opened 9.30 GB (full))",
                "line starting Chain … failed after T: database is newer than this xkrd",
                "line xkrd running · started in T",
            ]
        );

        let rec = Arc::new(Recorder::default());
        let r = Reporter::with_kind(Kind::Start, Mode::Fancy, Style::new(false), rec.clone());
        let storage = r.step("Storage");
        storage.done_with("opened 9.30 GB (pruned, stripe 2/4 (0x0402))");
        r.ready(Duration::from_secs(9));
        let out: Vec<String> = rec.out.lock().unwrap().iter().map(|l| untimed(l)).collect();
        // the block stays in the region under "Starting the node" ...
        assert!(
            out.iter().any(
                |l| l.starts_with("region [ |   Starting the node | ") && l.contains("Storage")
            )
        );
        // ... and is printed once for good under "Started"
        let lines: Vec<&String> = out.iter().filter(|l| l.starts_with("line")).collect();
        assert_eq!(
            lines,
            [
                "line \n  Started\n  ✔ Storage · opened 9.30 GB (pruned, stripe 2/4 (0x0402))  T",
                "line \n  Node running · started in T\n",
            ]
        );
        assert_eq!(
            out.last().unwrap(),
            "line \n  Node running · started in T\n"
        );
    }

    #[test]
    fn stopped_before_the_chain_was_open() {
        let (r, rec) = reporter(Mode::Plain);
        r.step("Storage").done_with("closed");
        r.end(None, 4);
        let out = rec.out.lock().unwrap().clone();
        assert_eq!(out.last().unwrap(), "line Node stopped · uptime 4 s");
        // a start-up given up prints its block under "Start stopped", once
        let rec = Arc::new(Recorder::default());
        let s = Reporter::with_kind(Kind::Start, Mode::Fancy, Style::new(false), rec.clone());
        s.begin();
        s.abort();
        s.abort();
        let out = rec.out.lock().unwrap().clone();
        assert_eq!(
            out,
            [
                "region [ |   Starting the node]",
                "region []",
                "line \n  Start stopped"
            ]
        );
    }

    #[test]
    fn repair_progress_in_the_storage_step() {
        let plain = Style::new(false);
        let check = "checking the database";
        // honest figures: the share of the file read, the bytes once past it, else none
        assert_eq!(check_status(None, 9_300_000_000), check);
        assert_eq!(
            check_status(Some(0), 9_300_000_000),
            format!("{check} · ~0 % read")
        );
        assert_eq!(
            check_status(Some(3_534_000_000), 9_300_000_000),
            format!("{check} · ~38 % read")
        );
        assert_eq!(
            check_status(Some(11_200_000_000), 9_300_000_000),
            format!("{check} · 11.20 GB read")
        );
        assert_eq!(check_status(Some(5), 0), format!("{check} · 5 B read"));
        assert_eq!(check_status(Some(0), 0), check);
        assert_eq!(status_kind(&check_status(Some(1), 10)), check);
        assert_eq!(status_kind("finishing a repack"), "finishing a repack");
        assert_eq!(status_kind("syncing  37 %"), "syncing");
        assert_eq!(
            render_running(
                plain,
                "Storage",
                Some(&check_status(Some(38), 100)),
                Duration::from_millis(4_200),
                0
            ),
            format!("  ⠋ Storage · {check} · ~38 % read  4.2 s")
        );
        // on Linux the process has read something already (at least its own binary's data)
        if std::path::Path::new("/proc/self/io").exists() {
            assert!(process_read_bytes().is_some());
        }

        // colour terminal: the running line follows the status live
        let rec = Arc::new(Recorder::default());
        let r = Reporter::with_kind(Kind::Start, Mode::Fancy, plain, rec.clone());
        let step = r.step("Storage");
        let handle = step.handle();
        handle.status(&check_status(Some(10), 100));
        handle.status(&check_status(Some(37), 100));
        handle.note("The node did not stop cleanly last time, so the database is being checked.");
        step.done_with("opened 9.30 GB (full, checked after an unclean stop · 3 min 12 s)");
        // a handle of a finished step changes nothing
        handle.status(&check_status(Some(90), 100));
        r.ready(Duration::from_secs(40));
        let out: Vec<String> = rec.out.lock().unwrap().iter().map(|l| untimed(l)).collect();
        assert!(
            out.iter()
                .any(|l| l.contains(&format!("Storage · {check} · ~37 % read")))
        );
        assert!(!out.iter().any(|l| l.contains("90 %")));
        // the note and the finished step are in the block printed at the end
        let block = out
            .iter()
            .find(|l| l.starts_with("line \n  Started"))
            .unwrap();
        assert!(block.contains(
            "\n    The node did not stop cleanly last time, so the database is being checked."
        ));
        assert!(block.contains(&untimed(
            "✔ Storage · opened 9.30 GB (full, checked after an unclean stop · 3 min 12 s)"
        )));

        // plain: the first status of a kind, then at most every few seconds
        let rec = Arc::new(Recorder::default());
        let r = Reporter::with_kind(Kind::Start, Mode::Plain, plain, rec.clone());
        let step = r.step("Storage");
        let handle = step.handle();
        handle.status(&check_status(Some(10), 100));
        handle.status(&check_status(Some(20), 100));
        handle.status("finishing an interrupted repack of outputs");
        step.done_with("opened 1.00 MB (full)");
        let out: Vec<String> = rec.out.lock().unwrap().iter().map(|l| untimed(l)).collect();
        assert_eq!(
            out,
            [
                "line starting xkrd".to_owned(),
                format!("line starting Storage … {check} · ~10 % read"),
                "line starting Storage … finishing an interrupted repack of outputs".to_owned(),
                "line starting Storage … done T (opened 1.00 MB (full))".to_owned(),
            ]
        );
    }

    #[test]
    fn later_signals_never_force() {
        assert_eq!(signal_action(1), SignalAction::Stop);
        assert_eq!(signal_action(2), SignalAction::Note);
        assert_eq!(signal_action(3), SignalAction::Ignore);
        assert_eq!(signal_action(50), SignalAction::Ignore);
        let (r, rec) = reporter(Mode::Plain);
        r.say(STOPPING_NOTE);
        assert_eq!(
            *rec.out.lock().unwrap(),
            ["line stopping cleanly — please wait, the database is being closed"]
        );
        let rec = Arc::new(Recorder::default());
        let r = Reporter::new(Mode::Fancy, Style::new(true), rec.clone());
        r.say("x");
        assert_eq!(*rec.out.lock().unwrap(), ["line   \x1b[2mx\x1b[0m"]);
    }

    #[test]
    fn notes_appear_above_a_running_step_only() {
        let (r, rec) = reporter(Mode::Plain);
        let step = r.step("Storage");
        let handle = step.handle();
        handle.note("The node did not stop cleanly last time, so the database is being checked.");
        step.done();
        handle.note("too late");
        let out = rec.out.lock().unwrap().clone();
        assert_eq!(
            out[1],
            "line   The node did not stop cleanly last time, so the database is being checked."
        );
        assert!(!out.iter().any(|l| l.contains("too late")));

        let rec = Arc::new(Recorder::default());
        let r = Reporter::with_kind(Kind::Start, Mode::Fancy, Style::new(true), rec.clone());
        let step = r.step("Storage");
        step.handle().note("why");
        step.done();
        r.ready(Duration::from_secs(1));
        assert!(
            rec.out
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("\n    \x1b[2mwhy\x1b[0m"))
        );
    }

    #[test]
    fn start_block_fits_the_width_and_tells_a_stop() {
        let rec = Arc::new(Recorder {
            width: Some(31),
            ..Recorder::default()
        });
        let r = Reporter::with_kind(Kind::Start, Mode::Fancy, Style::new(true), rec.clone());
        let step = r.step("Storage");
        step.handle().note(
            "This reads the whole 9.30 GB file and can take a few minutes; it does not need \
             the network.",
        );
        step.done_with("opened 9.30 GB (full, checked after an unclean stop · 3 min 12 s)");
        let chain = r.step("Chain");
        r.stop_requested();
        let last_region = rec
            .out
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|l| l.starts_with("region ["))
            .unwrap()
            .clone();
        let lines: Vec<&str> = last_region
            .trim_start_matches("region [")
            .trim_end_matches(']')
            .split(" | ")
            .collect();
        // every line within 30 columns; the note wrapped over several lines, not cut
        assert!(
            lines.iter().all(|l| term::visible_width(l) <= 30),
            "{lines:?}"
        );
        let note: Vec<&&str> = lines.iter().filter(|l| l.contains("\x1b[2m")).collect();
        assert!(note.len() >= 3);
        assert!(lines.iter().any(|l| l.contains("stop requested")));
        drop(chain);
        r.abort();
        let out = rec.out.lock().unwrap().clone();
        let block = out.last().unwrap();
        assert!(block.starts_with("line \n  \x1b[1;33mStart stopped\x1b[0m\n"));
        // printed for good: the note in one piece
        assert!(block.contains("it does not need the network."));

        // plain: one line
        let rec = Arc::new(Recorder::default());
        let r = Reporter::with_kind(Kind::Start, Mode::Plain, Style::new(false), rec.clone());
        r.begin();
        r.stop_requested();
        r.abort();
        r.stop_requested();
        assert_eq!(
            *rec.out.lock().unwrap(),
            [
                "line starting xkrd",
                "line stop requested: the node stops after the step under way"
            ]
        );
    }

    #[test]
    fn ensure_ended_always_shows_the_stop() {
        // nothing ran yet (an error before start-up): title and final line
        let (r, rec) = reporter(Mode::Plain);
        r.ensure_ended(2);
        assert_eq!(
            *rec.out.lock().unwrap(),
            ["line stopping xkrd", "line Node stopped · uptime 2 s"]
        );
        // already ended: nothing more
        r.ensure_ended(3);
        assert_eq!(rec.out.lock().unwrap().len(), 2);
        let (r, rec) = reporter(Mode::Plain);
        r.end(Some(7), 1);
        r.ensure_ended(1);
        assert_eq!(
            *rec.out.lock().unwrap(),
            ["line Node stopped at height 7 · uptime 1 s"]
        );
    }

    #[test]
    fn start_title_names_the_network() {
        let rec = Arc::new(Recorder::default());
        let r = Reporter::with_kind(Kind::Start, Mode::Fancy, Style::new(false), rec.clone());
        r.set_subtitle("Mainnet");
        r.begin();
        r.step("Storage").done();
        r.ready(Duration::from_secs(1));
        let out = rec.out.lock().unwrap().clone();
        assert!(out[0].starts_with("region [ |   Starting the node · Mainnet"));
        assert!(
            out.iter()
                .any(|l| l.starts_with("line \n  Started · Mainnet\n  ✔ Storage"))
        );

        let rec = Arc::new(Recorder::default());
        let r = Reporter::with_kind(Kind::Start, Mode::Fancy, Style::new(true), rec.clone());
        r.set_subtitle("Testnet");
        r.begin();
        r.abort();
        let out = rec.out.lock().unwrap().clone();
        assert_eq!(
            out.last().unwrap(),
            "line \n  \x1b[1;33mStart stopped\x1b[0m \x1b[2m· Testnet\x1b[0m"
        );
    }

    #[test]
    fn miner_subject() {
        let rec = Arc::new(Recorder::default());
        let r = Reporter::with_subject(
            Kind::Start,
            Subject::MINER,
            Mode::Plain,
            Style::new(false),
            rec.clone(),
        );
        r.step("Threads").done_with("4 threads, hardware AES");
        r.stop_requested();
        r.ready(Duration::from_millis(300));
        let out: Vec<String> = rec.out.lock().unwrap().iter().map(|l| untimed(l)).collect();
        assert_eq!(
            out,
            [
                "line starting xkr-miner",
                "line starting Threads … done T (4 threads, hardware AES)",
                "line stop requested: the miner stops after the step under way",
                "line xkr-miner running · started in T",
            ]
        );

        let rec = Arc::new(Recorder::default());
        let r = Reporter::with_subject(
            Kind::Stop,
            Subject::MINER,
            Mode::Fancy,
            Style::new(false),
            rec.clone(),
        );
        r.begin();
        r.end_with("· 12 accepted · 0 rejected · uptime 3 min");
        let out = rec.out.lock().unwrap().clone();
        let out: Vec<&String> = out.iter().filter(|l| l.starts_with("line")).collect();
        assert_eq!(out[0], "line \n  Stopping the miner");
        assert_eq!(
            out[1],
            "line \n  Miner stopped · 12 accepted · 0 rejected · uptime 3 min\n"
        );
    }
}
