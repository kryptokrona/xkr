//! Terminal output shared by the Kryptokrona programs (xkrd, xkr-miner; xkrwallet later):
//! colours that honour `NO_COLOR` and `TERM=dumb`, a stdout writer with a redrawable bottom
//! region (live panels, progress lines), the block-font header, big digits, number and time
//! formats ([`term`]) and the start-up / shutdown step reporter ([`steps`]).

// Windows only: the console calls in `term::win` need `unsafe`, nothing else may.
#![cfg_attr(not(windows), forbid(unsafe_code))]
#![cfg_attr(windows, deny(unsafe_code))]

pub mod steps;
pub mod term;
