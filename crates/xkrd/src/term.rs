//! The node's terminal output: the shared [`xkr_term::term`] pieces (colours, the bottom
//! region, number formats, big digits) with the "kryptokrona node" header.

#[cfg(test)]
pub use xkr_term::term::visible_width;
pub use xkr_term::term::{
    Color, SPINNER, Style, Term, Word, big_number, bytes, columns, duration_min_sec, eta, fit,
    group, hashrate, sync_percentage, uptime, wrap,
};

/// The start-up header with "node" under "kryptokrona" (`xkr_term::term::header`).
#[must_use]
pub fn header(style: Style) -> String {
    xkr_term::term::header(style, Word::Node)
}
