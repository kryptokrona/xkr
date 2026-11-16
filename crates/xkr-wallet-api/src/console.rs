//! The console on standard input (`src/wallet_api/wallet_api.cpp` `main`).
//!
//! Lines are read until `exit` or `quit` (the whole line, only the line feed removed) or the
//! end of input; then the server saves the wallet and stops. `help` prints
//! "Type exit to save and shutdown.".

use std::io::{BufRead, Write};

use tokio::sync::oneshot;

/// The hint printed on start and for `help`.
pub const HINT: &str = "Type exit to save and shutdown.";

/// Reads `input` until `exit`, `quit`, the end or a read error; answers `help` on `out`.
pub fn read_until_exit(mut input: impl BufRead, out: &mut impl Write) {
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = line.strip_suffix('\n').unwrap_or(&line);
        match text {
            "exit" | "quit" => return,
            "help" => {
                let _ = writeln!(out, "{HINT}");
                let _ = out.flush();
            }
            _ => {}
        }
    }
}

/// Reads standard input on its own thread; the receiver completes when the console asks to
/// stop (or the thread cannot start).
#[must_use]
pub fn spawn() -> oneshot::Receiver<()> {
    let (tx, rx) = oneshot::channel();
    let started = std::thread::Builder::new()
        .name("console".into())
        .spawn(move || {
            read_until_exit(std::io::stdin().lock(), &mut std::io::stdout());
            let _ = tx.send(());
        });
    if let Err(e) = started {
        tracing::warn!(error = %e, "cannot read standard input");
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn session(input: &str) -> (String, usize) {
        let mut cursor = Cursor::new(input.as_bytes().to_vec());
        let mut out = Vec::new();
        read_until_exit(&mut cursor, &mut out);
        (String::from_utf8(out).unwrap(), cursor.position() as usize)
    }

    #[test]
    fn exit_quit_help_and_end_of_input() {
        let (out, pos) = session("help\nstatus\nexit\nhelp\n");
        assert_eq!(out, format!("{HINT}\n"));
        assert_eq!(pos, "help\nstatus\nexit\n".len());
        let (_, pos) = session("quit\nmore\n");
        assert_eq!(pos, 5);
        let (out, _) = session("help");
        assert_eq!(out, format!("{HINT}\n"));
        // only the line feed is removed: "exit\r" is not a command
        let (_, pos) = session("exit\r\nexit\n");
        assert_eq!(pos, "exit\r\nexit\n".len());
        let (out, pos) = session("");
        assert_eq!((out.as_str(), pos), ("", 0));
    }
}
