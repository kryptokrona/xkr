//! The console: coloured output and the three kinds of input of the terminal wallets
//! (`src/zedwallet++/get_input.cpp` `getInput`, `src/zedwallet++/password_container.cpp`,
//! `src/utilities/coloured_msg.h`).
//!
//! On a terminal, commands are read with a line editor (`rustyline`) that has an in-memory
//! history of the commands only and completes command names with Tab; every other answer is
//! read without history, and passwords without echo (`rpassword`). Colours follow
//! `coloured_msg.h` (green, bright yellow, bright red) unless `NO_COLOR` is set.
//!
//! Without a terminal (stdin or stdout redirected) the wallet reads one line per answer, as the
//! C++ wallets do with `linenoise` and `PasswordContainer::read_from_file`: command prompts and
//! password prompts are not printed, other prompts are, nothing is coloured, and a password is
//! asked once even where a terminal asks for it twice. The end of the input answers every
//! question with "cancel" and the command prompt with `exit`.

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::{Arc, Mutex, PoisonError};

use rustyline::ExternalPrinter as _;
use rustyline::history::DefaultHistory;
use rustyline::{Config, Editor};
use zeroize::Zeroizing;

/// Where the console writes; shared with the transaction monitor.
pub type Out = Arc<Mutex<Box<dyn Write + Send>>>;

/// Prints text above the prompt (the transaction monitor's output).
pub type Printer = Box<dyn FnMut(&str) + Send>;

/// `coloured_msg.h`: `SuccessMsg`, `InformationMsg`, `WarningMsg`, or no colour at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Painter {
    colour: bool,
}

/// `common::console::Color::Green`.
const GREEN: &str = "\x1b[0;32m";
/// `common::console::Color::BrightYellow`.
const YELLOW: &str = "\x1b[1;33m";
/// `common::console::Color::BrightRed`.
const RED: &str = "\x1b[1;31m";
/// `common::console::Color::Default`.
const RESET: &str = "\x1b[0m";

impl Painter {
    /// Colours on or off.
    #[must_use]
    pub const fn new(colour: bool) -> Self {
        Self { colour }
    }

    /// Whether colours are on.
    #[must_use]
    pub const fn colour(self) -> bool {
        self.colour
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.colour && !text.is_empty() {
            format!("{code}{text}{RESET}")
        } else {
            text.to_owned()
        }
    }

    /// `SuccessMsg` (green).
    #[must_use]
    pub fn ok(self, text: &str) -> String {
        self.paint(GREEN, text)
    }

    /// `InformationMsg` (bright yellow).
    #[must_use]
    pub fn info(self, text: &str) -> String {
        self.paint(YELLOW, text)
    }

    /// `WarningMsg` (bright red).
    #[must_use]
    pub fn warn(self, text: &str) -> String {
        self.paint(RED, text)
    }

    /// Bold (command names, titles).
    #[must_use]
    pub fn bold(self, text: &str) -> String {
        self.paint("\x1b[1m", text)
    }

    /// Dim (descriptions, labels, times).
    #[must_use]
    pub fn dim(self, text: &str) -> String {
        self.paint("\x1b[2m", text)
    }

    /// Bold green (✔, "Wallet saved"), as the node's steps.
    #[must_use]
    pub fn good(self, text: &str) -> String {
        self.paint("\x1b[1;32m", text)
    }

    /// Bold cyan (the spinner), as the node's steps.
    #[must_use]
    pub fn cyan(self, text: &str) -> String {
        self.paint("\x1b[1;36m", text)
    }

    /// The colour of a [`Tone`].
    #[must_use]
    pub fn tone(self, tone: Tone, text: &str) -> String {
        match tone {
            Tone::Plain => text.to_owned(),
            Tone::Info => self.info(text),
            Tone::Ok => self.ok(text),
            Tone::Warn => self.warn(text),
            Tone::Bold => self.bold(text),
        }
    }
}

/// The colour of a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// No colour.
    Plain,
    /// `InformationMsg`.
    Info,
    /// `SuccessMsg`.
    Ok,
    /// `WarningMsg`.
    Warn,
    /// Bold: the compact prompts (`  Amount ›`).
    Bold,
}

/// Writes and flushes; output errors are ignored (as `std::cout` does).
pub fn write_to(out: &Out, text: &str) {
    let mut guard = out.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = guard.write_all(text.as_bytes());
    let _ = guard.flush();
}

/// Tab completion of command names (`linenoise::SetCompletionCallback` in `getInput`).
#[derive(Debug, Default)]
struct Completion {
    names: Vec<&'static str>,
}

impl rustyline::completion::Completer for Completion {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let typed = line.get(..pos).unwrap_or(line).to_lowercase();
        if typed.contains(char::is_whitespace) {
            return Ok((pos, Vec::new()));
        }
        let found = self
            .names
            .iter()
            .copied()
            .filter(|n| n.starts_with(typed.as_str()))
            .map(str::to_owned)
            .collect();
        Ok((0, found))
    }
}

impl rustyline::hint::Hinter for Completion {
    type Hint = String;
}

impl rustyline::highlight::Highlighter for Completion {}

impl rustyline::validate::Validator for Completion {}

impl rustyline::Helper for Completion {}

/// `linenoise::SetHistoryMaxLen(256)`.
const HISTORY_SIZE: usize = 256;

enum Input {
    /// A terminal: the line editor.
    Terminal(Box<Editor<Completion, DefaultHistory>>),
    /// Lines from a pipe, a file or a test.
    Lines(Box<dyn BufRead + Send>),
}

/// The console of a session.
pub struct Console {
    out: Out,
    input: Input,
    paint: Painter,
    closed: bool,
}

impl std::fmt::Debug for Console {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Console")
            .field("terminal", &self.is_terminal())
            .field("paint", &self.paint)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

/// Whether `NO_COLOR` asks for no colours (set and not empty) or the terminal is `dumb`.
fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
        || std::env::var_os("TERM").is_some_and(|t| t == "dumb")
}

impl Console {
    /// The terminal: line editor on stdin, stdout, colours unless `NO_COLOR`. `None` if the
    /// line editor cannot be set up.
    #[must_use]
    pub fn terminal() -> Option<Self> {
        let config = Config::builder()
            .max_history_size(HISTORY_SIZE)
            .ok()?
            .auto_add_history(false)
            .history_ignore_space(false)
            .build();
        let mut editor = Editor::<Completion, DefaultHistory>::with_config(config).ok()?;
        editor.set_helper(Some(Completion::default()));
        let stdout: Box<dyn Write + Send> = Box::new(io::stdout());
        Some(Self {
            out: Arc::new(Mutex::new(stdout)),
            input: Input::Terminal(Box::new(editor)),
            paint: Painter::new(!no_color()),
            closed: false,
        })
    }

    /// Lines from `input`, output to `output` (scripts and tests).
    #[must_use]
    pub fn lines(
        input: Box<dyn BufRead + Send>,
        output: Box<dyn Write + Send>,
        colour: bool,
    ) -> Self {
        Self {
            out: Arc::new(Mutex::new(output)),
            input: Input::Lines(input),
            paint: Painter::new(colour),
            closed: false,
        }
    }

    /// The terminal when stdin and stdout are both terminals, else lines from stdin.
    #[must_use]
    pub fn stdio() -> Self {
        if io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && let Some(console) = Self::terminal()
        {
            return console;
        }
        Self::lines(
            Box::new(io::BufReader::new(io::stdin())),
            Box::new(io::stdout()),
            false,
        )
    }

    /// Whether this is the interactive terminal.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self.input, Input::Terminal(_))
    }

    /// The colours.
    #[must_use]
    pub const fn paint(&self) -> Painter {
        self.paint
    }

    /// The output, for other threads.
    #[must_use]
    pub fn out(&self) -> Out {
        Arc::clone(&self.out)
    }

    /// Writes `text`.
    pub fn write(&self, text: &str) {
        write_to(&self.out, text);
    }

    /// A printer for text that arrives while the wallet waits for input: above the prompt on
    /// a terminal, else straight to the output.
    pub fn printer(&mut self) -> Printer {
        if let Input::Terminal(editor) = &mut self.input
            && let Ok(mut printer) = editor.create_external_printer()
        {
            return Box::new(move |text: &str| {
                let _ = printer.print(text.to_owned());
            });
        }
        let out = Arc::clone(&self.out);
        Box::new(move |text: &str| write_to(&out, text))
    }

    /// One line from the input without its line end; `None` at the end of the input.
    fn next_line(reader: &mut dyn BufRead) -> Option<Zeroizing<String>> {
        let mut bytes = Zeroizing::new(Vec::new());
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                while bytes.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    bytes.pop();
                }
                Some(Zeroizing::new(String::from_utf8_lossy(&bytes).into_owned()))
            }
        }
    }

    /// Reads with the line editor; `prompt` may span lines (only the last one is the editor's
    /// prompt).
    fn edit(
        editor: &mut Editor<Completion, DefaultHistory>,
        out: &Out,
        paint: Painter,
        prompt: &str,
        tone: Tone,
    ) -> Option<String> {
        let (head, tail) = match prompt.rfind('\n') {
            Some(i) => (&prompt[..=i], &prompt[i + 1..]),
            None => ("", prompt),
        };
        if !head.is_empty() {
            write_to(out, &paint.tone(tone, head));
        }
        let styled = paint.tone(tone, tail);
        editor.readline(&(tail, styled.as_str())).ok()
    }

    /// `getInput`: a command. On a terminal: the prompt in yellow, Tab completes `names`, the
    /// line goes to the history. Without a terminal the prompt is not printed.
    pub fn read_command(&mut self, prompt: &str, names: &[&'static str]) -> Option<String> {
        if self.closed {
            return None;
        }
        let line = match &mut self.input {
            Input::Terminal(editor) => {
                if let Some(helper) = editor.helper_mut() {
                    helper.names = names.to_vec();
                }
                let line = Self::edit(editor, &self.out, self.paint, prompt, Tone::Bold);
                if let Some(helper) = editor.helper_mut() {
                    helper.names.clear();
                }
                if let Some(line) = &line {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let _ = editor.add_history_entry(trimmed);
                    }
                }
                line
            }
            Input::Lines(reader) => Self::next_line(reader.as_mut()).map(|l| l.as_str().to_owned()),
        };
        if line.is_none() {
            self.closed = true;
        }
        line
    }

    /// `std::getline(std::cin, …)` after printing `prompt` in `tone`. Never kept in the
    /// history.
    pub fn read_line(&mut self, prompt: &str, tone: Tone) -> Option<String> {
        if self.closed {
            return None;
        }
        let line = match &mut self.input {
            Input::Terminal(editor) => Self::edit(editor, &self.out, self.paint, prompt, tone),
            Input::Lines(reader) => {
                write_to(&self.out, &self.paint.tone(tone, prompt));
                Self::next_line(reader.as_mut()).map(|l| l.as_str().to_owned())
            }
        };
        if line.is_none() {
            self.closed = true;
        }
        line
    }

    /// `PasswordContainer::read_password(false, prompt)`: without echo on a terminal; one
    /// line without a prompt otherwise.
    pub fn read_password(&mut self, prompt: &str) -> Option<Zeroizing<String>> {
        if self.closed {
            return None;
        }
        let password = match &mut self.input {
            Input::Terminal(_) => rpassword::prompt_password(self.paint.bold(prompt))
                .ok()
                .map(Zeroizing::new),
            Input::Lines(reader) => Self::next_line(reader.as_mut()),
        };
        if password.is_none() {
            self.closed = true;
        }
        password
    }

    /// `PasswordContainer::read_password(true, prompt)`: on a terminal the password is asked
    /// twice until both agree; without a terminal it is read once.
    pub fn read_new_password(&mut self, prompt: &str) -> Option<Zeroizing<String>> {
        if !self.is_terminal() {
            return self.read_password(prompt);
        }
        loop {
            let first = self.read_password(prompt)?;
            let second = self.read_password("  Confirm password › ")?;
            if *first == *second {
                return Some(first);
            }
            self.write(&format!(
                "  {}\n",
                self.paint.warn("✗ The passwords do not match, try again")
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::completion::Completer as _;

    #[test]
    fn painter() {
        assert_eq!(Painter::new(false).ok("x"), "x");
        assert_eq!(Painter::new(true).ok("x"), "\x1b[0;32mx\x1b[0m");
        assert_eq!(Painter::new(true).warn(""), "");
        assert_eq!(Painter::new(true).tone(Tone::Plain, "p"), "p");
    }

    #[test]
    fn lines_end_and_line_ends() {
        let input = io::Cursor::new(b"one\r\ntwo\nlast".to_vec());
        let mut c = Console::lines(Box::new(input), Box::new(io::sink()), false);
        assert_eq!(c.read_command("> ", &[]).as_deref(), Some("one"));
        assert_eq!(
            c.read_password("pw: ").as_deref().map(String::as_str),
            Some("two")
        );
        assert_eq!(c.read_line("x: ", Tone::Info).as_deref(), Some("last"));
        assert_eq!(c.read_line("x: ", Tone::Info), None);
        assert_eq!(c.read_command("> ", &[]), None);
    }

    #[test]
    fn completes_command_names() {
        let helper = Completion {
            names: vec!["ab_add", "ab_list", "address", "balance"],
        };
        let history = DefaultHistory::new();
        let ctx = rustyline::Context::new(&history);
        let (start, found) = helper.complete("ab", 2, &ctx).unwrap();
        assert_eq!(start, 0);
        assert_eq!(found, vec!["ab_add".to_owned(), "ab_list".to_owned()]);
        assert!(helper.complete("x y", 3, &ctx).unwrap().1.is_empty());
    }
}
