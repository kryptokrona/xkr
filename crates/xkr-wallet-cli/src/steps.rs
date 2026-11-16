//! Step-by-step output in the node's style (`crates/xkrd/src/stopping.rs`): the steps of
//! opening and of closing the wallet, and the closing line with the logo.
//!
//! On a colour terminal a running step is a cyan braille spinner with its time
//! (`  ⠋ Node  0.4 s`, the line of the node's start-up steps, a frame every 100 ms) that turns
//! into a green `✔` with what was done and the time (`  ✔ Saving the wallet file · a.wallet,
//! wallet file  412 ms`), or a red `✗` with the reason. While the wallet closes
//! ([`Steps::closing`]) the first word of the title ("Closing") and the running lines, spinner
//! and label, are red. Without a terminal, with `NO_COLOR` or with `TERM=dumb` each step is
//! one plain line once it is done
//! (`Saving the wallet file … done 412 ms (a.wallet, wallet file)`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::args;
use crate::format::elapsed;
use crate::input::{Out, Painter, write_to};

/// The spinner.
const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Time between two spinner frames.
const FRAME_INTERVAL: Duration = Duration::from_millis(100);
/// Time between the logo lines of the closing effect.
const LOGO_LINE: Duration = Duration::from_millis(60);
/// Back to the start of the line and clear it.
const CLEAR_LINE: &str = "\r\x1b[2K";

/// How steps are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStyle {
    /// Spinner, then `✔` / `✗` in colour on the same line.
    Live,
    /// One plain line per finished step.
    Plain,
}

/// Writes steps to an output.
#[derive(Clone)]
pub struct Steps {
    out: Out,
    style: StepStyle,
    paint: Painter,
    closing: bool,
}

impl std::fmt::Debug for Steps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Steps")
            .field("style", &self.style)
            .field("paint", &self.paint)
            .field("closing", &self.closing)
            .finish_non_exhaustive()
    }
}

impl Steps {
    /// Steps written to `out`.
    #[must_use]
    pub const fn new(out: Out, style: StepStyle, paint: Painter) -> Self {
        Self {
            out,
            style,
            paint,
            closing: false,
        }
    }

    /// The same steps in the closing colours: the title's first word and the running lines
    /// in red.
    #[must_use]
    pub fn closing(mut self) -> Self {
        self.closing = true;
        self
    }

    /// The style for a console: live only on a colour terminal.
    #[must_use]
    pub const fn style_for(terminal: bool, paint: Painter) -> StepStyle {
        if terminal && paint.colour() {
            StepStyle::Live
        } else {
            StepStyle::Plain
        }
    }

    /// The heading above the steps (`  Opening the wallet`, bold on a terminal; while
    /// closing its first word red).
    pub fn title(&self, text: &str) {
        let p = self.paint;
        let line = match self.style {
            StepStyle::Live if self.closing => match text.split_once(' ') {
                Some((first, rest)) => format!("\n  {} {}\n", p.warn(first), p.bold(rest)),
                None => format!("\n  {}\n", p.warn(text)),
            },
            StepStyle::Live => format!("\n  {}\n", p.bold(text)),
            StepStyle::Plain => format!("\n{text}\n"),
        };
        write_to(&self.out, &line);
    }

    /// Runs `work` as the step `label`, with a spinner on a terminal; `outcome` tells from
    /// the result what was done (`Ok`, may be empty) or why it failed (`Err`).
    pub fn run<T>(
        &self,
        label: &str,
        work: impl FnOnce() -> T,
        outcome: impl FnOnce(&T) -> Result<String, String>,
    ) -> T {
        let started = Instant::now();
        let value = match self.style {
            StepStyle::Plain => work(),
            StepStyle::Live => {
                let done = AtomicBool::new(false);
                std::thread::scope(|scope| {
                    scope.spawn(|| {
                        let mut frame = 0usize;
                        while !done.load(Ordering::SeqCst) {
                            write_to(&self.out, &self.running(label, started.elapsed(), frame));
                            frame += 1;
                            std::thread::sleep(FRAME_INTERVAL);
                        }
                    });
                    let value = work();
                    done.store(true, Ordering::SeqCst);
                    value
                })
            }
        };
        let result = outcome(&value);
        write_to(&self.out, &self.finished(label, &result, started.elapsed()));
        value
    }

    /// [`Steps::run`] for work that says itself what it did; true if it succeeded.
    pub fn step(&self, label: &str, work: impl FnOnce() -> Result<String, String>) -> bool {
        self.run(label, work, Clone::clone).is_ok()
    }

    /// `  ⠋ label  0.4 s` (without a line end: the next frame replaces it); spinner and
    /// label red while closing.
    fn running(&self, label: &str, time: Duration, frame: usize) -> String {
        let p = self.paint;
        let spinner = FRAMES[frame % FRAMES.len()];
        let time = p.dim(&elapsed(time));
        if self.closing {
            format!(
                "{CLEAR_LINE}  {} {}  {time}",
                p.warn(spinner),
                p.warn(label)
            )
        } else {
            format!("{CLEAR_LINE}  {} {label}  {time}", p.cyan(spinner))
        }
    }

    /// The line a finished step leaves.
    fn finished(&self, label: &str, result: &Result<String, String>, time: Duration) -> String {
        let p = self.paint;
        let took = elapsed(time);
        match (self.style, result) {
            (StepStyle::Live, Ok(detail)) if detail.is_empty() => {
                format!("{CLEAR_LINE}  {} {label}  {}\n", p.good("✔"), p.dim(&took))
            }
            (StepStyle::Live, Ok(detail)) => format!(
                "{CLEAR_LINE}  {} {label} · {detail}  {}\n",
                p.good("✔"),
                p.dim(&took)
            ),
            (StepStyle::Live, Err(reason)) => format!(
                "{CLEAR_LINE}  {} {label} · {}  {}\n",
                p.warn("✗"),
                p.warn(reason),
                p.dim(&took)
            ),
            (StepStyle::Plain, Ok(detail)) if detail.is_empty() => {
                format!("{label} … done {took}\n")
            }
            (StepStyle::Plain, Ok(detail)) => format!("{label} … done {took} ({detail})\n"),
            (StepStyle::Plain, Err(reason)) => {
                format!("{label} … failed after {took}: {reason}\n")
            }
        }
    }

    /// The closing line: `Wallet saved · goodbye` in green, then on a terminal the logo drawn
    /// line by line; or `Wallet not saved · <reason>` in red.
    pub fn end(&self, saved: bool, reason: &str) {
        let p = self.paint;
        match (self.style, saved) {
            (StepStyle::Live, true) => {
                write_to(
                    &self.out,
                    &format!("\n  {} · goodbye\n\n", p.good("Wallet saved")),
                );
                for line in args::logo_lines() {
                    std::thread::sleep(LOGO_LINE);
                    write_to(&self.out, &format!("  {}\n", p.info(line)));
                }
                write_to(&self.out, "\n");
            }
            (StepStyle::Live, false) => write_to(
                &self.out,
                &format!("\n  {} · {reason}\n\n", p.warn("Wallet not saved")),
            ),
            (StepStyle::Plain, true) => write_to(&self.out, "Wallet saved · goodbye\n"),
            (StepStyle::Plain, false) => {
                write_to(&self.out, &format!("Wallet not saved · {reason}\n"));
            }
        }
    }
}

/// Rows of `label  value` as the node's info block: two spaces in, the labels dim and padded
/// to the longest one plus two.
#[must_use]
pub fn info_block(paint: Painter, rows: &[(&str, String)]) -> String {
    let width = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    let mut out = String::new();
    for (k, v) in rows {
        out.push_str("  ");
        out.push_str(&paint.dim(k));
        out.push_str(&" ".repeat(width - k.chars().count()));
        out.push_str(v);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::io::Write;
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
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

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(
                self.0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone(),
            )
            .expect("utf-8")
        }
    }

    fn steps(style: StepStyle, colour: bool) -> (Steps, Buffer) {
        let buffer = Buffer::default();
        let sink: Box<dyn Write + Send> = Box::new(buffer.clone());
        let out: Out = Arc::new(Mutex::new(sink));
        (Steps::new(out, style, Painter::new(colour)), buffer)
    }

    /// The times (`12 ms`, `0.3 s`) replaced by `T`, line by line.
    fn untimed(text: &str) -> String {
        text.split('\n')
            .map(untimed_line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn untimed_line(text: &str) -> String {
        let mut out = String::new();
        let words: Vec<&str> = text.split(' ').collect();
        let mut i = 0;
        while i < words.len() {
            let unit = words.get(i + 1).map(|w| {
                w.trim_end_matches(|c: char| !c.is_ascii_alphabetic())
                    .trim_start_matches(|c: char| !c.is_ascii_alphabetic())
            });
            if words[i].parse::<f64>().is_ok() && matches!(unit, Some("ms" | "s")) {
                let next = words[i + 1];
                let unit = unit.unwrap_or_default();
                let at = next.find(unit).unwrap_or(0);
                let _ = write!(out, "T{} ", &next[at + unit.len()..]);
                i += 2;
                continue;
            }
            out.push_str(words[i]);
            out.push(' ');
            i += 1;
        }
        out.pop();
        out
    }

    #[test]
    fn plain_is_one_line_per_step() {
        let (s, buffer) = steps(StepStyle::Plain, false);
        s.title("Closing the wallet");
        assert!(s.step("Stopping sync", || Ok("at height 81".to_owned())));
        assert!(s.step("Closing the node connection", || Ok(String::new())));
        assert!(!s.step("Saving the wallet file", || Err("disk full".to_owned())));
        assert_eq!(s.run("Node", || 7, |n| Ok(format!("height {n}"))), 7);
        s.end(true, "");
        assert_eq!(
            untimed(&buffer.text()),
            "\nClosing the wallet\nStopping sync … done T (at height 81)\nClosing the node \
             connection … done T\nSaving the wallet file … failed after T: disk full\nNode … \
             done T (height 7)\nWallet saved · goodbye\n"
        );
    }

    #[test]
    fn live_spins_then_ticks_or_crosses() {
        let (s, buffer) = steps(StepStyle::Live, true);
        s.title("Closing the wallet");
        assert!(s.step("Stopping sync", || {
            std::thread::sleep(Duration::from_millis(250));
            Ok("at height 81".to_owned())
        }));
        assert!(!s.step("Saving the wallet file", || Err("disk full".to_owned())));
        let text = buffer.text();
        assert!(
            text.starts_with("\n  \x1b[1mClosing the wallet\x1b[0m\n"),
            "{text:?}"
        );
        assert!(
            text.contains(&format!(
                "{CLEAR_LINE}  \x1b[1;36m⠋\x1b[0m Stopping sync  \x1b[2m"
            )),
            "{text:?}"
        );
        assert!(
            text.contains("\x1b[1;36m⠙\x1b[0m Stopping sync"),
            "{text:?}"
        );
        assert!(
            text.contains(&format!(
                "{CLEAR_LINE}  \x1b[1;32m✔\x1b[0m Stopping sync · at height 81  \x1b[2m"
            )),
            "{text:?}"
        );
        assert!(
            text.contains(&format!(
                "{CLEAR_LINE}  \x1b[1;31m✗\x1b[0m Saving the wallet file · \x1b[1;31mdisk \
                 full\x1b[0m  \x1b[2m"
            )),
            "{text:?}"
        );
        let (s, buffer) = steps(StepStyle::Live, true);
        s.end(true, "");
        let text = buffer.text();
        assert!(
            text.starts_with("\n  \x1b[1;32mWallet saved\x1b[0m · goodbye\n\n  \x1b[1;33m"),
            "{text:?}"
        );
        assert_eq!(text.lines().count(), 7, "{text:?}");
    }

    #[test]
    fn closing_runs_in_red_and_ends_in_green() {
        let (s, buffer) = steps(StepStyle::Live, true);
        let s = s.closing();
        s.title("Closing the wallet");
        assert!(s.step("Saving the wallet file", || {
            std::thread::sleep(Duration::from_millis(150));
            Ok("a.wallet, wallet file".to_owned())
        }));
        let text = buffer.text();
        assert!(
            text.starts_with("\n  \x1b[1;31mClosing\x1b[0m \x1b[1mthe wallet\x1b[0m\n"),
            "{text:?}"
        );
        assert!(
            text.contains(&format!(
                "{CLEAR_LINE}  \x1b[1;31m⠋\x1b[0m \x1b[1;31mSaving the wallet file\x1b[0m  \x1b[2m"
            )),
            "{text:?}"
        );
        assert!(
            text.contains(&format!(
                "{CLEAR_LINE}  \x1b[1;32m✔\x1b[0m Saving the wallet file · a.wallet, wallet \
                 file  \x1b[2m"
            )),
            "{text:?}"
        );
        // plain mode does not change
        let (s, buffer) = steps(StepStyle::Plain, false);
        let s = s.closing();
        s.title("Closing the wallet");
        assert!(s.step("Stopping sync", || Ok(String::new())));
        assert!(
            buffer
                .text()
                .starts_with("\nClosing the wallet\nStopping sync … done ")
        );
    }

    #[test]
    fn styles_and_blocks() {
        assert_eq!(Steps::style_for(true, Painter::new(true)), StepStyle::Live);
        assert_eq!(
            Steps::style_for(true, Painter::new(false)),
            StepStyle::Plain
        );
        assert_eq!(
            Steps::style_for(false, Painter::new(true)),
            StepStyle::Plain
        );
    }
}
