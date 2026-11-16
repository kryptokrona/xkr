//! xkr-wallet-cli — the terminal wallet `xkrwallet`, a drop-in for the C++ `xkrwallet`
//! (zedwallet, `src/zedwallet`) and `xkrwallet-beta` (zedwallet++, `src/zedwallet++`) on the
//! `xkr-wallet` engine.
//!
//! Same flags (`-r/--remote-daemon`, `-w/--wallet-file`, `-p/--password`, `-h`, `-v`,
//! `--debug`, plus `--network`), same start menu, node-down menu, commands, numbers,
//! view-wallet permissions, prompts and messages; both wallet file formats; `.addressBook.json`
//! in the form it was written; `transactions.csv`. Added: `status` shows the transaction
//! versions the node accepts and its next fork height, and three subwallet commands.
//!
//! | module | C++ |
//! |---|---|
//! | [`args`] | `parse_arguments.cpp` |
//! | [`commands`] | `commands.cpp`, `menu.h` `parseCommand`, the command implementations |
//! | [`input`] | `get_input.cpp` `getInput`, `password_container.cpp`, `coloured_msg.h` |
//! | [`session`] | `zed_wallet.cpp`, `menu.cpp`, `command_dispatcher.cpp` |
//! | `open` | `open.cpp` |
//! | `sync` | `sync.cpp` |
//! | `monitor` | `transaction_monitor.cpp` |
//! | `csv` | `command_implementations.cpp` `saveCSV` |
//! | `shutdown` | `zed_wallet.cpp` `shutdown` / `cleanup` (closing and saving) |
//! | `big` | the balance in big digits, as xkrd's live panel |
//! | `steps` | (the step-by-step output of closing, the wallet block after opening) |
//!
//! Without a terminal the wallet reads commands and answers line by line from stdin and
//! prints no colours, so it can be scripted; see [`input`].
#![forbid(unsafe_code)]

pub mod args;
mod ask;
mod big;
pub mod commands;
mod csv;
mod format;
pub mod input;
mod monitor;
mod open;
pub mod session;
mod shutdown;
mod steps;
mod sync;

pub use args::Options;
pub use input::Console;
pub use session::{HttpNodes, NodeFactory, Session, SessionConfig, Timing};
pub use shutdown::{ShutdownHandle, SignalOutcome};
