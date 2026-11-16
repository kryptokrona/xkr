//! The miner's live panel on a colour terminal (the binary's; `xkr_term` draws it at the
//! bottom): the hashrate in big digits with its unit, and a status line with a spinner,
//! "mining block N · diff … · 12 accepted · 0 rejected · 4 threads · solo", that briefly shows
//! "✔ block found · height N", "✔ share accepted" or a refusal. The C++ miner prints a line per
//! event instead (`src/miner/miner_manager.cpp` `printHashRate`), which xkr-miner still does
//! without a colour terminal.

use std::time::{Duration, Instant};

use xkr_term::term::{self, Color, SPINNER, Style};

/// How long a found block or an accepted share stays on the status line.
pub const FLASH: Duration = Duration::from_secs(2);
/// Below this many columns the hashrate is shown in normal digits.
const NARROW: usize = 40;

/// A brief message on the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flash {
    /// Solo: the daemon took the block at this height.
    Block(u64),
    /// Pool: the share was accepted.
    Share,
    /// Refused, stale or not submitted, with the reason.
    Refused(String),
}

/// What the panel shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PanelState {
    /// Hashes per second.
    pub hashrate: f64,
    /// Height of the job (0: none yet, or a pool that does not say).
    pub height: u64,
    pub difficulty: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub threads: usize,
    /// `solo` or `pool <host>`.
    pub mode: String,
    /// Why hashing waits (no job, node not synced), if it does.
    pub paused: Option<String>,
    /// A message shown until the instant.
    pub flash: Option<(Flash, Instant)>,
}

/// The hashrate as a whole number for the big digits and its unit: H/s below 10 000, kH/s
/// below 10 000 000, MH/s above.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn hashrate_digits(hashes_per_second: f64) -> (u64, &'static str) {
    let h = hashes_per_second.max(0.0);
    if h < 10_000.0 {
        (h.round() as u64, "H/s")
    } else if h < 10_000_000.0 {
        ((h / 1e3).round() as u64, "kH/s")
    } else {
        ((h / 1e6).round() as u64, "MH/s")
    }
}

/// `pool <host>` from a pool URL (`stratum+tcp://host:port` gives `pool host:port`).
#[must_use]
pub fn pool_mode(url: &str) -> String {
    let host = url.split_once("://").map_or(url, |(_, rest)| rest);
    format!("pool {}", host.trim_end_matches('/'))
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The panel at `now` with spinner frame `frame`, every line cut to `width - 1` columns.
#[must_use]
pub fn render(
    p: &PanelState,
    now: Instant,
    frame: usize,
    width: usize,
    style: Style,
) -> Vec<String> {
    let usable = width.saturating_sub(1).max(1);
    let (value, unit) = hashrate_digits(p.hashrate);
    let big = term::big_number(value);
    let mut lines = Vec::with_capacity(4);
    if width >= NARROW && big[0].chars().count() + 4 + unit.len() <= usable {
        lines.push(format!("  {}", style.paint(Color::Cyan, &big[0])));
        lines.push(format!(
            "  {}  {}",
            style.paint(Color::Cyan, &big[1]),
            style.paint(Color::Bold, unit)
        ));
        lines.push(format!("  {}", style.paint(Color::Cyan, &big[2])));
    } else {
        lines.push(term::fit(
            &format!("  {} {unit}", style.paint(Color::Bold, &term::group(value))),
            usable,
        ));
    }
    let flash = p.flash.as_ref().filter(|(_, until)| now < *until);
    let status = match flash {
        Some((Flash::Block(height), _)) => style.paint(
            Color::Green,
            &format!("✔ block found · height {}", term::group(*height)),
        ),
        Some((Flash::Share, _)) => style.paint(
            Color::Green,
            &format!("✔ share accepted · {} accepted", p.accepted),
        ),
        Some((Flash::Refused(reason), _)) => {
            style.paint(Color::Red, &format!("✗ refused: {reason}"))
        }
        None => {
            let spin = style.paint(Color::Cyan, &SPINNER[frame % SPINNER.len()].to_string());
            let threads = count(p.threads, "thread", "threads");
            match &p.paused {
                Some(reason) => format!("{spin} paused: {reason} · {threads} · {}", p.mode),
                None if p.height == 0 && p.difficulty == 0 => {
                    format!("{spin} waiting for work · {threads} · {}", p.mode)
                }
                None => {
                    let job = if p.height == 0 {
                        "mining".to_owned()
                    } else {
                        format!("mining block {}", term::group(p.height))
                    };
                    format!(
                        "{spin} {job} · diff {} · {} accepted · {} rejected · {threads} · {}",
                        term::group(p.difficulty),
                        p.accepted,
                        p.rejected,
                        p.mode
                    )
                }
            }
        }
    };
    lines.push(term::fit(&format!("  {status}"), usable));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PanelState {
        PanelState {
            hashrate: 1_234.4,
            height: 2_620_300,
            difficulty: 3_703_125,
            accepted: 12,
            rejected: 0,
            threads: 4,
            mode: "solo".into(),
            ..PanelState::default()
        }
    }

    #[test]
    fn hashrate_units() {
        assert_eq!(hashrate_digits(0.0), (0, "H/s"));
        assert_eq!(hashrate_digits(-3.0), (0, "H/s"));
        assert_eq!(hashrate_digits(1_234.4), (1_234, "H/s"));
        assert_eq!(hashrate_digits(9_999.4), (9_999, "H/s"));
        assert_eq!(hashrate_digits(41_150.0), (41, "kH/s"));
        assert_eq!(hashrate_digits(12_500_000.0), (13, "MH/s"));
        assert_eq!(
            pool_mode("stratum+tcp://pool.example:3333"),
            "pool pool.example:3333"
        );
        assert_eq!(pool_mode("host:3333"), "pool host:3333");
    }

    #[test]
    fn panel_layout() {
        let plain = Style::new(false);
        let now = Instant::now();
        let big = term::big_number(1_234);
        let p = render(&state(), now, 0, 100, plain);
        assert_eq!(
            p,
            [
                format!("  {}", big[0]),
                format!("  {}  H/s", big[1]),
                format!("  {}", big[2]),
                "  ⠋ mining block 2 620 300 · diff 3 703 125 · 12 accepted · 0 rejected · \
                 4 threads · solo"
                    .to_owned(),
            ]
        );
        // the spinner turns
        assert!(render(&state(), now, 1, 100, plain)[3].starts_with("  ⠙ mining"));

        // a found block for two seconds, then the status again
        let mut s = state();
        s.flash = Some((Flash::Block(2_620_300), now + FLASH));
        assert_eq!(
            render(&s, now, 0, 100, plain)[3],
            "  ✔ block found · height 2 620 300"
        );
        assert!(render(&s, now + FLASH, 0, 100, plain)[3].starts_with("  ⠋ mining block"));
        s.flash = Some((Flash::Share, now + FLASH));
        assert_eq!(
            render(&s, now, 0, 100, plain)[3],
            "  ✔ share accepted · 12 accepted"
        );
        s.flash = Some((Flash::Refused("low difficulty".into()), now + FLASH));
        assert_eq!(
            render(&s, now, 0, 100, plain)[3],
            "  ✗ refused: low difficulty"
        );

        // pool without heights, paused, before the first job
        let mut s = state();
        s.height = 0;
        s.threads = 1;
        s.mode = pool_mode("stratum+tcp://p:3333");
        assert_eq!(
            render(&s, now, 0, 120, plain)[3],
            "  ⠋ mining · diff 3 703 125 · 12 accepted · 0 rejected · 1 thread · pool p:3333"
        );
        s.paused = Some("the node is not synchronized".into());
        assert_eq!(
            render(&s, now, 0, 120, plain)[3],
            "  ⠋ paused: the node is not synchronized · 1 thread · pool p:3333"
        );
        let fresh = PanelState {
            threads: 2,
            mode: "solo".into(),
            ..PanelState::default()
        };
        assert_eq!(
            render(&fresh, now, 0, 80, plain)[3],
            "  ⠋ waiting for work · 2 threads · solo"
        );

        // kH/s, coloured
        let mut s = state();
        s.hashrate = 41_150.0;
        let p = render(&s, now, 0, 100, Style::new(true));
        assert!(p[1].ends_with("\x1b[1mkH/s\x1b[0m"));

        // narrow: normal digits, lines cut to the width
        let p = render(&state(), now, 0, 30, plain);
        assert_eq!(p[0], "  1 234 H/s");
        assert_eq!(p.len(), 2);
        assert!(p.iter().all(|l| term::visible_width(l) <= 29));
    }
}
