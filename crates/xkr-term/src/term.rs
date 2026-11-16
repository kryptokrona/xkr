//! Terminal output of the Kryptokrona programs (xkrd, xkr-miner, later xkrwallet): the
//! Kryptokrona header, colours, number formats, big digits and a stdout writer that keeps a
//! region of lines (a live panel or a progress line) at the bottom of a terminal (the C++
//! programs print their header from `src/config/cli_header.h` and colour their log lines,
//! `utilities/colours`).
//!
//! Everything goes to stdout, the log lines (through [`Term`] as the tracing writer) and the
//! program's own lines alike, so the bottom region can be cleared before a log line and drawn
//! again after it ([`Screen`]). Colours only when stdout is a terminal and `NO_COLOR` is unset
//! or empty (<https://no-color.org>) and `TERM` is not `dumb`; with `TERM=dumb` nothing is
//! redrawn in place either.

use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex, PoisonError};

use tracing_subscriber::fmt::MakeWriter;

/// Back to the start of the line, clear it.
const CR_CLEAR: &[u8] = b"\r\x1b[2K";
/// Cursor up one line, clear it.
const UP_CLEAR: &[u8] = b"\x1b[1A\x1b[2K";
/// Save / restore the cursor position (DECSC / DECRC).
const SAVE_CURSOR: &[u8] = b"\x1b7";
const RESTORE_CURSOR: &[u8] = b"\x1b8";
/// Terminal width when neither `COLUMNS` nor the terminal tell.
const DEFAULT_COLUMNS: usize = 80;

/// The big digits 0-9, three rows of three columns each.
const BIG_DIGITS: [[&str; 3]; 10] = [
    ["┏━┓", "┃ ┃", "┗━┛"],
    [" ┓ ", " ┃ ", " ┻ "],
    ["╺━┓", "┏━┛", "┗━╸"],
    ["╺━┓", " ━┫", "╺━┛"],
    ["╻ ╻", "┗━┫", "  ╹"],
    ["┏━╸", "┗━┓", "╺━┛"],
    ["┏━╸", "┣━┓", "┗━┛"],
    ["╺━┓", "  ┃", "  ╹"],
    ["┏━┓", "┣━┫", "┗━┛"],
    ["┏━┓", "┗━┫", "╺━┛"],
];

/// The XKR logo of the header, in the same small block font as the name under it.
const ASCII_ART: &str = r"
▀▄ ▄▀ █ ▄▀ █▀▀▄
 ▄▀▄  █▀▄  █▀█
▀   ▀ ▀  ▀ ▀  ▀
";

/// "kryptokrona" in a small block font, under the XKR logo (the fourth row holds the
/// descenders of y and p).
const KRYPTOKRONA_ART: &str = r"
█  ▄ ▄ ▄▄ ▄  ▄ ▄▄▄  ▄█▄   ▄▄  █  ▄ ▄ ▄▄  ▄▄  ▄▄▄   ▄▄▄
█▄█  █▀   █  █ █  █  █   █  █ █▄█  █▀   █  █ █  █ █  █
▀  ▀ ▀     ▀▀█ █▀▀    ▀▀  ▀▀  ▀  ▀ ▀     ▀▀  ▀  ▀  ▀▀▀▀
           ▀▀  ▀
";

/// "node" in the same font.
const NODE_ART: &str = r"
▄▄▄   ▄▄   ▄▄█  ▄▄
█  █ █  █ █  █ █▀▀▀
▀  ▀  ▀▀   ▀▀▀  ▀▀▀
";

/// "wallet" in the same font.
const WALLET_ART: &str = r"
▄   ▄  ▄▄▄  █ █  ▄▄  ▄█▄
█ █ █ █  █  █ █ █▀▀▀  █
 ▀ ▀   ▀▀▀▀ ▀ ▀  ▀▀▀   ▀▀
";

/// "miner" in the same font: m like n with a middle stem, i as a dot over a stem.
const MINER_ART: &str = r"
▄▄▄▄▄ ▀ ▄▄▄   ▄▄  ▄ ▄▄
█ █ █ █ █  █ █▀▀▀ █▀
▀ ▀ ▀ ▀ ▀  ▀  ▀▀▀ ▀
";

/// The word under "kryptokrona" in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Word {
    Node,
    Wallet,
    Miner,
}

impl Word {
    /// Its block-font lines.
    #[must_use]
    pub fn art(self) -> Vec<&'static str> {
        let art = match self {
            Self::Node => NODE_ART,
            Self::Wallet => WALLET_ART,
            Self::Miner => MINER_ART,
        };
        art.trim_matches('\n').lines().collect()
    }
}

/// Spinner frames (the panels and the steps).
pub const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Terminal colours used by the node's own lines (C++ `logging::BRIGHT_YELLOW` & co.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Green,
    Yellow,
    Red,
    Cyan,
    Bold,
    Dim,
}

impl Color {
    const fn code(self) -> &'static str {
        match self {
            Self::Green => "\x1b[1;32m",
            Self::Yellow => "\x1b[1;33m",
            Self::Red => "\x1b[1;31m",
            Self::Cyan => "\x1b[1;36m",
            Self::Bold => "\x1b[1m",
            Self::Dim => "\x1b[2m",
        }
    }
}

/// Whether colour codes are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    color: bool,
}

impl Style {
    /// Colours on or off.
    #[must_use]
    pub const fn new(color: bool) -> Self {
        Self { color }
    }

    /// `text` in `color` (unchanged without colours).
    #[must_use]
    pub fn paint(self, color: Color, text: &str) -> String {
        if self.color {
            format!("{}{text}\x1b[0m", color.code())
        } else {
            text.to_owned()
        }
    }
}

/// `TERM=dumb`: no colours and no cursor movement.
fn dumb_terminal() -> bool {
    std::env::var_os("TERM").is_some_and(|t| t == "dumb")
}

/// Whether the environment allows colours (`NO_COLOR`, `TERM=dumb`).
fn colours_allowed() -> bool {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    !no_color && !dumb_terminal()
}

/// The bottom region of a terminal and the bytes that keep it there. The region's lines end
/// with a newline each, so the cursor rests at the start of the line below them (where typed
/// console commands are echoed).
#[derive(Debug)]
pub struct Screen {
    /// The lines kept at the bottom (panel or progress line).
    region: Vec<String>,
    /// Region lines on screen now (the cursor is on the line below them).
    drawn: usize,
    /// Whether the last output ended a line (the region is only drawn at a line start).
    at_line_start: bool,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            region: Vec::new(),
            drawn: 0,
            at_line_start: true,
        }
    }
}

impl Screen {
    /// Removes the drawn region and `extra` lines under it: clears the cursor's line, then
    /// cursor-up + clear-line once per line. The cursor ends at the region's first line.
    fn clear(&mut self, out: &mut Vec<u8>, extra: usize) {
        if self.drawn == 0 {
            return;
        }
        out.extend_from_slice(CR_CLEAR);
        for _ in 0..self.drawn + extra {
            out.extend_from_slice(UP_CLEAR);
        }
        self.drawn = 0;
    }

    fn draw(&mut self, out: &mut Vec<u8>) {
        for line in &self.region {
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        self.drawn = self.region.len();
    }

    /// Other output (log lines, the node's lines, console replies) above the region.
    pub fn write(&mut self, buf: &[u8], out: &mut Vec<u8>) {
        if buf.is_empty() {
            return;
        }
        self.clear(out, 0);
        out.extend_from_slice(buf);
        self.at_line_start = buf.ends_with(b"\n");
        if self.at_line_start {
            self.draw(out);
        }
    }

    /// Replaces the region. With as many lines as on screen it is rewritten in place between
    /// a saved and restored cursor, so text being typed on the line below stays; otherwise it
    /// is cleared and drawn again.
    pub fn set_region(&mut self, lines: Vec<String>, out: &mut Vec<u8>) {
        if self.drawn > 0 && self.drawn == lines.len() {
            if lines == self.region {
                return;
            }
            out.extend_from_slice(SAVE_CURSOR);
            out.extend_from_slice(format!("\x1b[{}A", self.drawn).as_bytes());
            for line in &lines {
                out.extend_from_slice(CR_CLEAR);
                out.extend_from_slice(line.as_bytes());
                out.push(b'\n');
            }
            out.extend_from_slice(RESTORE_CURSOR);
            self.region = lines;
        } else {
            self.clear(out, 0);
            self.region = lines;
            if self.at_line_start {
                self.draw(out);
            }
        }
    }

    /// Removes the region for good.
    pub fn clear_region(&mut self, out: &mut Vec<u8>) {
        self.clear(out, 0);
        self.region.clear();
    }

    /// A console line was entered: the terminal's echo of Enter moved the cursor one line
    /// down. The region and the echoed line are cleared, the command is written again above
    /// the region (nothing for an empty line) and the region drawn under it.
    pub fn entered(&mut self, text: &str, out: &mut Vec<u8>) {
        if self.drawn == 0 {
            return;
        }
        self.clear(out, 1);
        let text = text.trim_end();
        if !text.trim().is_empty() {
            out.extend_from_slice(text.as_bytes());
            out.push(b'\n');
        }
        self.draw(out);
    }
}

#[derive(Debug)]
struct Inner {
    tty: bool,
    style: Style,
    screen: Mutex<Screen>,
    /// The terminal width, read once ([`Term::columns`]).
    width: std::sync::OnceLock<usize>,
}

/// Stdout of the node (cheap to clone). Also the tracing writer of the text logs, see the
/// module docs.
#[derive(Debug, Clone)]
pub struct Term(Arc<Inner>);

impl Term {
    /// Stdout, with colours when it is a terminal and the environment allows them.
    #[must_use]
    pub fn stdout() -> Self {
        let tty = io::stdout().is_terminal() && !dumb_terminal();
        Self::new(tty, tty && colours_allowed())
    }

    /// A writer with explicit settings (`tty`: keep the bottom region in place).
    #[must_use]
    pub fn new(tty: bool, color: bool) -> Self {
        Self(Arc::new(Inner {
            tty,
            style: Style::new(color),
            screen: Mutex::new(Screen::default()),
            width: std::sync::OnceLock::new(),
        }))
    }

    /// Whether stdout is a terminal that takes cursor movement (not `TERM=dumb`).
    #[must_use]
    pub fn tty(&self) -> bool {
        self.0.tty
    }

    /// The colour setting.
    #[must_use]
    pub fn style(&self) -> Style {
        self.0.style
    }

    /// The terminal width ([`columns`]), read on first use and kept.
    #[must_use]
    pub fn columns(&self) -> usize {
        *self.0.width.get_or_init(columns)
    }

    /// Whether colour codes are written (also for the log lines).
    #[must_use]
    pub fn color(&self) -> bool {
        self.0.style.color
    }

    /// Prints `text` as whole lines (a missing final newline is added).
    pub fn print(&self, text: &str) {
        if text.ends_with('\n') {
            self.write_out(text.as_bytes());
        } else {
            self.write_out(format!("{text}\n").as_bytes());
        }
    }

    /// Shows the progress line: in place on a terminal, else as a normal line.
    pub fn progress(&self, text: &str) {
        if self.0.tty {
            self.panel(vec![text.to_owned()]);
        } else {
            self.print(text);
        }
    }

    /// Shows `lines` as the bottom region of a terminal, replacing the previous one (nothing
    /// without a terminal).
    pub fn panel(&self, lines: Vec<String>) {
        if self.0.tty {
            self.with_screen(|screen, out| screen.set_region(lines, out));
        }
    }

    /// Removes the progress line or panel.
    pub fn clear_progress(&self) {
        self.with_screen(Screen::clear_region);
    }

    /// The console read `line` (see [`Screen::entered`]).
    pub fn entered(&self, line: &str) {
        if self.0.tty {
            self.with_screen(|screen, out| screen.entered(line, out));
        }
    }

    /// Runs `f` on the screen state and writes what it produced in one go. Write errors are
    /// dropped (a closed stdout must not stop the node).
    fn with_screen(&self, f: impl FnOnce(&mut Screen, &mut Vec<u8>)) {
        let mut screen = self.0.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let mut buf = Vec::new();
        f(&mut screen, &mut buf);
        if !buf.is_empty() {
            let mut out = io::stdout().lock();
            let _ = out.write_all(&buf);
            let _ = out.flush();
        }
    }

    /// A writer for [`Term::print`]-like output in pieces (the console's replies).
    #[must_use]
    pub fn writer(&self) -> TermWriter<'_> {
        TermWriter(self)
    }

    /// Writes `buf` above the bottom region on a terminal ([`Screen::write`]), as it is
    /// otherwise.
    fn write_out(&self, buf: &[u8]) {
        if self.0.tty {
            self.with_screen(|screen, out| screen.write(buf, out));
        } else {
            self.with_screen(|_, out| out.extend_from_slice(buf));
        }
    }
}

/// [`Term`] as an [`io::Write`].
#[derive(Debug)]
pub struct TermWriter<'a>(&'a Term);

impl Write for TermWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_out(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Term {
    type Writer = TermWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        TermWriter(self)
    }
}

/// The start-up header: the XKR logo, "kryptokrona" and `word` in the same block font, all
/// indented by two columns (the C++ `cryptonote::getProjectCLIHeader`,
/// `src/config/cli_header.h`, prints its art and licence lines here; the version is in
/// `--version`).
#[must_use]
pub fn header(style: Style, word: Word) -> String {
    let mut out = String::from("\n");
    for line in ASCII_ART.trim_matches('\n').lines() {
        out.push_str("  ");
        out.push_str(&style.paint(Color::Yellow, line));
        out.push('\n');
    }
    out.push('\n');
    let name = KRYPTOKRONA_ART.trim_matches('\n').lines().chain(word.art());
    for line in name {
        out.push_str("  ");
        out.push_str(&style.paint(Color::Yellow, line));
        out.push('\n');
    }
    out.push('\n');
    out
}

/// The XKR logo lines, indented like the header's.
#[must_use]
pub fn logo_lines(style: Style) -> Vec<String> {
    ASCII_ART
        .trim_matches('\n')
        .lines()
        .map(|l| format!("  {}", style.paint(Color::Yellow, l)))
        .collect()
}

/// A duration with seconds: `42 s`, `3 min 12 s`, `1 h 5 min`.
#[must_use]
pub fn duration_min_sec(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3_600 => format!("{} min {} s", secs / 60, secs % 60),
        _ => format!("{} h {} min", secs / 3_600, secs / 60 % 60),
    }
}

/// A duration in words for the final line: `42 s`, `12 min`, `3 h 12 min`, `2 d 4 h`.
#[must_use]
pub fn duration_words(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3_600 => format!("{} min", secs / 60),
        3_600..86_400 => format!("{} h {} min", secs / 3_600, secs / 60 % 60),
        _ => format!("{} d {} h", secs / 86_400, secs / 3_600 % 24),
    }
}

/// `text` broken at spaces into lines of at most `width` characters (a longer word gets a line
/// of its own).
#[must_use]
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let len = line.chars().count();
        if len > 0 && len + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// `n` with a space between groups of three digits (`2 620 300`).
#[must_use]
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Terminal width: `COLUMNS`, else the size `stty` reports for the controlling terminal, else
/// 80.
#[must_use]
pub fn columns() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&c| c > 0)
        .or_else(tty_columns)
        .unwrap_or(DEFAULT_COLUMNS)
}

/// `stty size` on `/dev/tty` (`rows cols`).
#[cfg(unix)]
fn tty_columns() -> Option<usize> {
    let tty = std::fs::File::open("/dev/tty").ok()?;
    let out = std::process::Command::new("stty")
        .arg("size")
        .stdin(tty)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    text.split_whitespace()
        .nth(1)?
        .parse::<usize>()
        .ok()
        .filter(|&c| c > 0)
}

#[cfg(not(unix))]
fn tty_columns() -> Option<usize> {
    None
}

/// `n` in big digits ([`BIG_DIGITS`]): three rows, one space between digits and three between
/// groups of three (as in [`group`]).
#[must_use]
pub fn big_number(n: u64) -> [String; 3] {
    let digits = n.to_string();
    let len = digits.len();
    let mut rows = [String::new(), String::new(), String::new()];
    for (i, c) in digits.bytes().enumerate() {
        if i > 0 {
            let gap = if (len - i).is_multiple_of(3) {
                "   "
            } else {
                " "
            };
            for row in &mut rows {
                row.push_str(gap);
            }
        }
        let digit = &BIG_DIGITS[usize::from(c - b'0')];
        for (row, part) in rows.iter_mut().zip(digit) {
            row.push_str(part);
        }
    }
    rows
}

/// Columns `s` takes on a terminal: characters without the colour escapes (every character
/// used here is one column wide).
#[must_use]
pub fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut escape = false;
    for c in s.chars() {
        if escape {
            escape = !c.is_ascii_alphabetic();
        } else if c == '\x1b' {
            escape = true;
        } else {
            width += 1;
        }
    }
    width
}

/// `s` cut to `width` visible columns, colour escapes kept (and reset after a cut).
#[must_use]
pub fn fit(s: &str, width: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut visible = 0;
    let mut escape = false;
    let mut cut = false;
    for c in s.chars() {
        if escape {
            escape = !c.is_ascii_alphabetic();
        } else if c == '\x1b' {
            escape = true;
        } else if visible == width {
            cut = true;
            break;
        } else {
            visible += 1;
        }
        out.push(c);
    }
    if cut && out.contains('\x1b') {
        out.push_str("\x1b[0m");
    }
    out
}

/// C++ `utilities::get_mining_speed` (`src/utilities/format_tools.cpp`): two decimals and
/// `H/s`, `KH/s`, `MH/s` or `GH/s`.
#[must_use]
pub fn hashrate(hashes_per_second: f64) -> String {
    let h = hashes_per_second;
    if h > 1e9 {
        format!("{:.2} GH/s", h / 1e9)
    } else if h > 1e6 {
        format!("{:.2} MH/s", h / 1e6)
    } else if h > 1e3 {
        format!("{:.2} KH/s", h / 1e3)
    } else {
        format!("{h:.2} H/s")
    }
}

/// C++ `utilities::get_sync_percentage` (`src/utilities/format_tools.cpp`): two decimals,
/// never above 100 and never 100.00 before the target.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn sync_percentage(height: u64, target: u64) -> String {
    if height == 0 || target == 0 {
        return "0.00".to_owned();
    }
    let height = height.min(target);
    let mut percent = 100.0 * height as f64 / target as f64;
    if height < target && percent > 99.99 {
        percent = 99.99;
    }
    format!("{percent:.2}")
}

/// Uptime as C++ `get_status_string` prints it: `0d 2h 13m 5s`.
#[must_use]
pub fn uptime(secs: u64) -> String {
    format!(
        "{}d {}h {}m {}s",
        secs / 86_400,
        secs / 3_600 % 24,
        secs / 60 % 60,
        secs % 60
    )
}

/// A remaining time for the sync progress: `45s`, `12m 05s`, `3h 07m`, `2d 04h`.
#[must_use]
pub fn eta(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m {:02}s", secs / 60, secs % 60),
        3_600..86_400 => format!("{}h {:02}m", secs / 3_600, secs / 60 % 60),
        _ => format!("{}d {:02}h", secs / 86_400, secs / 3_600 % 24),
    }
}

/// A byte count in B, KB, MB or GB (powers of 1 000, two decimals).
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn bytes(n: u64) -> String {
    let b = n as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.2} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.2} KB", b / 1e3)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_grouped() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1_000), "1 000");
        assert_eq!(group(2_620_300), "2 620 300");
        assert_eq!(group(12_345_678), "12 345 678");
    }

    #[test]
    fn formats_match_cpp() {
        assert_eq!(hashrate(0.0), "0.00 H/s");
        assert_eq!(hashrate(1_000.0), "1000.00 H/s");
        assert_eq!(hashrate(41_150.0), "41.15 KH/s");
        assert_eq!(hashrate(2_500_000.0), "2.50 MH/s");
        assert_eq!(sync_percentage(0, 10), "0.00");
        assert_eq!(sync_percentage(5, 10), "50.00");
        assert_eq!(sync_percentage(12, 10), "100.00");
        assert_eq!(sync_percentage(999_999, 1_000_000), "99.99");
        assert_eq!(sync_percentage(10, 10), "100.00");
        assert_eq!(uptime(0), "0d 0h 0m 0s");
        assert_eq!(uptime(2 * 86_400 + 3 * 3_600 + 4 * 60 + 5), "2d 3h 4m 5s");
        assert_eq!(eta(45), "45s");
        assert_eq!(eta(725), "12m 05s");
        assert_eq!(eta(3 * 3_600 + 7 * 60), "3h 07m");
        assert_eq!(eta(2 * 86_400 + 4 * 3_600), "2d 04h");
        assert_eq!(duration_min_sec(42), "42 s");
        assert_eq!(duration_min_sec(192), "3 min 12 s");
        assert_eq!(duration_min_sec(3_900), "1 h 5 min");
        assert_eq!(duration_words(42), "42 s");
        assert_eq!(duration_words(12 * 60 + 5), "12 min");
        assert_eq!(duration_words(3 * 3_600 + 12 * 60), "3 h 12 min");
        assert_eq!(duration_words(2 * 86_400 + 4 * 3_600), "2 d 4 h");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(10_210_000_000), "10.21 GB");
    }

    #[test]
    fn paint_respects_the_colour_setting() {
        assert_eq!(Style::new(false).paint(Color::Green, "x"), "x");
        assert_eq!(
            Style::new(true).paint(Color::Green, "x"),
            "\x1b[1;32mx\x1b[0m"
        );
    }

    #[test]
    fn header_has_the_logo_and_name_left_aligned() {
        let h = header(Style::new(false), Word::Node);
        // logo and name share the two-column indent
        assert!(h.starts_with("\n  ▀▄ ▄▀ █ ▄▀ █▀▀▄\n   ▄▀▄  █▀▄  █▀█\n"));
        assert!(h.contains("\n  █  ▄ ▄ ▄▄ ▄  ▄ ▄▄▄"));
        assert!(h.contains("  ▄▄▄   ▄▄   ▄▄█  ▄▄\n"));
        // no version or licence line any more
        assert!(!h.contains("xkrd v"));
        assert!(!h.contains("GPL"));
        assert!(!h.contains('\x1b'));
        assert!(h.ends_with("▀▀▀\n\n"));
        assert!(ASCII_ART.lines().all(|l| l.chars().count() <= 15));
    }

    #[test]
    fn words_in_the_block_font() {
        assert_eq!(
            Word::Miner.art(),
            [
                "▄▄▄▄▄ ▀ ▄▄▄   ▄▄  ▄ ▄▄",
                "█ █ █ █ █  █ █▀▀▀ █▀",
                "▀ ▀ ▀ ▀ ▀  ▀  ▀▀▀ ▀",
            ]
        );
        for word in [Word::Node, Word::Wallet, Word::Miner] {
            let art = word.art();
            // three rows, block characters and spaces only, no trailing spaces
            assert_eq!(art.len(), 3, "{word:?}");
            assert!(
                art.iter().all(|l| l.chars().all(|c| " ▄▀█".contains(c))),
                "{word:?}"
            );
            assert!(art.iter().all(|l| !l.ends_with(' ')), "{word:?}");
            // under "kryptokrona", left-aligned with it
            let h = header(Style::new(false), word);
            assert!(
                h.contains(&format!("           ▀▀  ▀\n  {}\n", art[0])),
                "{word:?}"
            );
            assert!(h.ends_with(&format!("  {}\n\n", art[2])), "{word:?}");
        }
        assert!(header(Style::new(false), Word::Wallet).contains("  ▄   ▄  ▄▄▄  █ █  ▄▄  ▄█▄\n"));
        assert!(header(Style::new(true), Word::Miner).contains("\x1b[1;33m▄▄▄▄▄ ▀ ▄▄▄"));
    }

    #[test]
    fn big_digits() {
        let rows = |n| big_number(n).map(|r| r.clone());
        assert_eq!(rows(0), ["┏━┓", "┃ ┃", "┗━┛"]);
        assert_eq!(rows(1), [" ┓ ", " ┃ ", " ┻ "]);
        for (d, glyph) in BIG_DIGITS.iter().enumerate() {
            let r = big_number(d as u64);
            assert_eq!(r, glyph.map(str::to_owned), "{d}");
            assert!(glyph.iter().all(|g| g.chars().count() == 3), "{d}");
        }
        // one space inside a group of three, three between groups
        assert_eq!(
            big_number(1_234),
            [
                " ┓    ╺━┓ ╺━┓ ╻ ╻".to_owned(),
                " ┃    ┏━┛  ━┫ ┗━┫".to_owned(),
                " ┻    ┗━╸ ╺━┛   ╹".to_owned(),
            ]
        );
        let all = big_number(1_234_567_890);
        assert!(
            all.iter()
                .all(|r| r.chars().count() == all[0].chars().count())
        );
        // 10 digits in 4 groups: 30 + 6 single gaps + 3 triple gaps
        assert_eq!(all[0].chars().count(), 30 + 6 + 9);
        // a mainnet height fits 40 columns with an indent of 2
        assert_eq!(big_number(2_620_300)[0].chars().count(), 31);
    }

    #[test]
    fn widths_skip_colour_codes() {
        let s = Style::new(true).paint(Color::Green, "abcdef");
        assert_eq!(visible_width(&s), 6);
        assert_eq!(fit(&s, 10), s);
        assert_eq!(fit(&s, 3), "\x1b[1;32mabc\x1b[0m");
        assert_eq!(fit("⠋ waiting", 4), "⠋ wa");
        assert_eq!(visible_width("┏━┓ x"), 5);
    }

    /// The bottom region on a terminal: drawn under the output, rewritten in place, cleared
    /// line by line (cursor-up + clear-line) before other output and drawn again after it.
    #[test]
    fn screen_keeps_the_region_at_the_bottom() {
        let mut s = Screen::default();
        let mut out = Vec::new();
        let region = |a: &str, b: &str| vec![a.to_owned(), b.to_owned()];

        s.set_region(region("A1", "A2"), &mut out);
        assert_eq!(out, b"A1\nA2\n");

        // same size: in place between save and restore
        out.clear();
        s.set_region(region("B1", "B2"), &mut out);
        assert_eq!(out, b"\x1b7\x1b[2A\r\x1b[2KB1\n\r\x1b[2KB2\n\x1b8");
        // unchanged: nothing
        out.clear();
        s.set_region(region("B1", "B2"), &mut out);
        assert!(out.is_empty());

        // a log line: clear both lines, write it, draw the region again
        out.clear();
        s.write(b"log\n", &mut out);
        assert_eq!(out, b"\r\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2Klog\nB1\nB2\n");

        // a partial line: the region waits for its end
        out.clear();
        s.write(b"part", &mut out);
        assert_eq!(out, b"\r\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2Kpart");
        out.clear();
        s.set_region(region("C1", "C2"), &mut out);
        assert!(out.is_empty());
        s.write(b"ial\n", &mut out);
        assert_eq!(out, b"ial\nC1\nC2\n");

        // a different size: cleared and drawn again
        out.clear();
        s.set_region(vec!["D".to_owned()], &mut out);
        assert_eq!(out, b"\r\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2KD\n");

        // Enter in the console: the echoed line goes too, the command is written back
        out.clear();
        s.entered("status\n", &mut out);
        assert_eq!(out, b"\r\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2Kstatus\nD\n");
        out.clear();
        s.entered("\n", &mut out);
        assert_eq!(out, b"\r\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2KD\n");

        out.clear();
        s.clear_region(&mut out);
        assert_eq!(out, b"\r\x1b[2K\x1b[1A\x1b[2K");
        out.clear();
        s.write(b"after\n", &mut out);
        assert_eq!(out, b"after\n");
        // nothing drawn: Enter changes nothing
        out.clear();
        s.entered("x\n", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn wrap_breaks_at_spaces() {
        assert_eq!(wrap("", 10), [""]);
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("a verylongword b", 5), ["a", "verylongword", "b"]);
        let text = "This reads the whole 9.30 GB file and can take a few minutes";
        assert!(wrap(text, 26).iter().all(|l| l.chars().count() <= 26));
        assert_eq!(wrap(text, 26).join(" "), text);
    }
}
