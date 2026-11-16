//! The balance in big box-drawing digits, as xkrd shows the block height in its live panel
//! (`crates/xkrd/src/term.rs` `big_number`, `BIG_DIGITS`, `columns`): bold cyan, one space
//! between digits and three between groups of three. The whole XKR are big; the decimals
//! follow at normal size on the last row:
//!
//! ```text
//!    ┓    ╺━┓ ╺━┓ ╻ ╻
//!    ┃    ┏━┛  ━┫ ┗━┫
//!    ┻    ┗━╸ ╺━┛   ╹ .56 XKR
//! ```
//!
//! Without a colour terminal, or on one narrower than [`NARROW`] columns (or too narrow for
//! the number), it is one normal line (`  1 234.56 XKR`).

use std::fmt::Write as _;

use xkr_wallet_core::ATOMIC_UNITS_PER_COIN;

use crate::format::amount_short;
use crate::input::Painter;

/// The big digits 0-9, three rows of three columns each (xkrd's `BIG_DIGITS`).
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

/// Below this many columns the balance is normal text (xkrd's panel limit).
pub const NARROW: usize = 40;

/// Terminal width when neither `COLUMNS` nor the terminal tell.
const DEFAULT_COLUMNS: usize = 80;

/// `n` in big digits: three rows, one space between digits and three between groups of
/// three (xkrd `big_number`).
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

/// Terminal width: `COLUMNS`, else the size `stty` reports for the controlling terminal, else
/// 80 (xkrd `columns`).
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

/// The available balance, big when `width` (the terminal's columns) is given and wide
/// enough, then a dim `locked …` line if anything is locked.
#[must_use]
pub fn balance_block(paint: Painter, available: u64, locked: u64, width: Option<usize>) -> String {
    let short = amount_short(available);
    let decimals = short.split_once('.').map_or("00", |(_, d)| d);
    let rows = big_number(available / ATOMIC_UNITS_PER_COIN);
    let tail = format!(" .{decimals} XKR");
    let fits = width
        .is_some_and(|w| w >= NARROW && 2 + rows[2].chars().count() + tail.chars().count() < w);
    let mut text = String::new();
    if fits {
        let _ = writeln!(text, "  {}", paint.cyan(&rows[0]));
        let _ = writeln!(text, "  {}", paint.cyan(&rows[1]));
        let _ = writeln!(
            text,
            "  {} {} {}",
            paint.cyan(&rows[2]),
            paint.bold(&format!(".{decimals}")),
            paint.dim("XKR")
        );
    } else {
        let _ = writeln!(text, "  {} {}", paint.bold(&short), paint.dim("XKR"));
    }
    if locked != 0 {
        let _ = writeln!(
            text,
            "  {}",
            paint.dim(&format!("locked {} XKR", amount_short(locked)))
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_and_groups_like_the_node() {
        assert_eq!(big_number(0), ["┏━┓", "┃ ┃", "┗━┛"]);
        assert_eq!(
            big_number(1_234),
            [
                " ┓    ╺━┓ ╺━┓ ╻ ╻".to_owned(),
                " ┃    ┏━┛  ━┫ ┗━┫".to_owned(),
                " ┻    ┗━╸ ╺━┛   ╹".to_owned(),
            ]
        );
    }

    #[test]
    fn balance_big_narrow_and_plain() {
        let plain = Painter::new(false);
        assert_eq!(
            balance_block(plain, 123_456_000, 0, Some(80)),
            "   ┓    ╺━┓ ╺━┓ ╻ ╻\n   ┃    ┏━┛  ━┫ ┗━┫\n   ┻    ┗━╸ ╺━┛   ╹ .56 XKR\n"
        );
        // narrow terminal and plain mode: one line
        assert_eq!(
            balance_block(plain, 123_456_000, 0, Some(39)),
            "  1 234.56 XKR\n"
        );
        assert_eq!(
            balance_block(plain, 123_456_000, 0, None),
            "  1 234.56 XKR\n"
        );
        assert_eq!(
            balance_block(plain, 0, 1_234_000, None),
            "  0.00 XKR\n  locked 12.34 XKR\n"
        );
        // colours as the node's panel: bold cyan digits
        let coloured = balance_block(Painter::new(true), 100_000, 0, Some(80));
        assert!(
            coloured.starts_with("  \x1b[1;36m ┓ \x1b[0m\n"),
            "{coloured:?}"
        );
        assert!(
            coloured.ends_with(" \x1b[1m.00\x1b[0m \x1b[2mXKR\x1b[0m\n"),
            "{coloured:?}"
        );
    }
}
