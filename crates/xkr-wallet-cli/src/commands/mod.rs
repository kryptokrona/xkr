//! The command tables and the command prompt.
//!
//! The commands of the C++ terminal wallets (`src/zedwallet++/commands.cpp`) with shorter
//! names, grouped for `help`. Every C++ name still works as an alias (`transfer` is `send`,
//! `ab_list` is `contacts`, …). Each command carries its C++ name ([`Command::key`]), which is
//! what the session dispatches on, and the view-wallet permission of `commands.cpp`: a view
//! wallet does not know the commands it may not use. The start menu and the node-down menu
//! also take numbers (`0` is `exit`).

mod address_book;
mod advanced;
mod basic;
mod status;
mod transfer;

use std::fmt::Write as _;

use crate::input::Painter;

/// Where `help` lists a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// Balance, address, sending, backup.
    Wallet,
    /// The transaction lists and the CSV export.
    History,
    /// `.addressBook.json`.
    AddressBook,
    /// Everything else.
    Tools,
    /// Typed but not listed (`help`, `exit`, `advanced`) or a menu entry.
    Hidden,
}

impl Group {
    /// The label `help` shows.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Wallet => "Wallet",
            Self::History => "History",
            Self::AddressBook => "Address book",
            Self::Tools => "Tools",
            Self::Hidden => "",
        }
    }
}

/// A command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    /// What `help` shows and what the user types.
    pub name: &'static str,
    /// The C++ command name, which the session dispatches on.
    pub key: &'static str,
    /// Other names that work too (the C++ names among them).
    pub aliases: &'static [&'static str],
    /// One line for `help <command>` and the menus.
    pub description: &'static str,
    /// `viewWalletSupport`.
    pub view_wallet: bool,
    /// Where `help` lists it.
    pub group: Group,
    /// The number of a menu entry.
    pub number: Option<u8>,
}

const fn cmd(
    name: &'static str,
    key: &'static str,
    aliases: &'static [&'static str],
    description: &'static str,
    view_wallet: bool,
    group: Group,
) -> Command {
    Command {
        name,
        key,
        aliases,
        description,
        view_wallet,
        group,
        number: None,
    }
}

const fn entry(
    number: u8,
    name: &'static str,
    key: &'static str,
    aliases: &'static [&'static str],
    description: &'static str,
) -> Command {
    Command {
        name,
        key,
        aliases,
        description,
        view_wallet: true,
        group: Group::Hidden,
        number: Some(number),
    }
}

/// `startupCommands`.
pub const STARTUP: &[Command] = &[
    entry(1, "open", "open", &[], "Open a wallet on this computer"),
    entry(2, "create", "create", &[], "Create a new wallet"),
    entry(
        3,
        "restore",
        "seed_restore",
        &["seed_restore"],
        "Restore from a 25-word seed",
    ),
    entry(
        4,
        "keys",
        "key_restore",
        &["key_restore"],
        "Restore from private keys",
    ),
    entry(
        5,
        "view",
        "view_wallet",
        &["view_wallet"],
        "Import a view-only wallet",
    ),
    entry(0, "exit", "exit", &["quit"], ""),
];

/// `nodeDownCommands`.
pub const NODE_DOWN: &[Command] = &[
    entry(
        1,
        "retry",
        "try_again",
        &["try_again"],
        "Ask the node again",
    ),
    entry(
        2,
        "continue",
        "continue",
        &[],
        "Go on without the node for now",
    ),
    entry(3, "node", "swap_node", &["swap_node"], "Use another node"),
    entry(0, "exit", "exit", &["quit"], ""),
];

/// The wallet's commands (`allCommands`, plus the subwallet commands).
pub const ALL: &[Command] = &[
    cmd(
        "balance",
        "balance",
        &["bal", "money", "account"],
        "Show how much XKR you have",
        true,
        Group::Wallet,
    ),
    cmd(
        "address",
        "address",
        &["addr"],
        "Show your address, also in the other prefix",
        true,
        Group::Wallet,
    ),
    cmd(
        "send",
        "transfer",
        &["s", "transfer"],
        "Send XKR to someone",
        false,
        Group::Wallet,
    ),
    cmd(
        "seed",
        "seed",
        &["mnemonic"],
        "Show your 25-word seed (asks your password)",
        true,
        Group::Wallet,
    ),
    cmd(
        "keys",
        "keys",
        &["private_keys"],
        "Show your private keys (asks your password)",
        true,
        Group::Wallet,
    ),
    cmd(
        "back",
        "back",
        &[],
        "Close this wallet and go back to the start menu",
        true,
        Group::Wallet,
    ),
    cmd(
        "txs",
        "txs",
        &["transactions"],
        "Your transactions, newest first; txs in, txs out",
        true,
        Group::History,
    ),
    cmd(
        "incoming",
        "incoming_transfers",
        &["incoming_transfers"],
        "List incoming transfers",
        true,
        Group::History,
    ),
    cmd(
        "outgoing",
        "outgoing_transfers",
        &["outgoing_transfers"],
        "List outgoing transfers",
        false,
        Group::History,
    ),
    cmd(
        "all",
        "list_transfers",
        &["list_transfers", "history"],
        "List all transfers",
        false,
        Group::History,
    ),
    cmd(
        "export",
        "save_csv",
        &["save_csv"],
        "Save all transactions to transactions.csv",
        true,
        Group::History,
    ),
    cmd(
        "contacts",
        "ab_list",
        &["ab_list"],
        "List your address book",
        true,
        Group::AddressBook,
    ),
    cmd(
        "add",
        "ab_add",
        &["ab_add"],
        "Add someone to your address book",
        true,
        Group::AddressBook,
    ),
    cmd(
        "remove",
        "ab_delete",
        &["ab_delete"],
        "Remove someone from your address book",
        true,
        Group::AddressBook,
    ),
    cmd(
        "send-to",
        "ab_send",
        &["ab_send"],
        "Send XKR to someone in your address book",
        false,
        Group::AddressBook,
    ),
    cmd(
        "optimize",
        "optimize",
        &[],
        "Merge small amounts so large sends fit",
        false,
        Group::Tools,
    ),
    cmd(
        "send-all",
        "send_all",
        &["send_all"],
        "Send your whole balance to someone",
        false,
        Group::Tools,
    ),
    cmd(
        "integrated",
        "make_integrated_address",
        &["make_integrated_address"],
        "Make an address with a payment ID in it",
        true,
        Group::Tools,
    ),
    cmd(
        "reset",
        "reset",
        &[],
        "Scan the chain again from a height",
        true,
        Group::Tools,
    ),
    cmd(
        "node",
        "swap_node",
        &["swap_node"],
        "Use another node",
        true,
        Group::Tools,
    ),
    cmd(
        "password",
        "change_password",
        &["change_password"],
        "Change your wallet password",
        true,
        Group::Tools,
    ),
    cmd(
        "status",
        "status",
        &[],
        "Show sync status, node and network",
        true,
        Group::Tools,
    ),
    cmd(
        "save",
        "save",
        &[],
        "Save your wallet now",
        true,
        Group::Tools,
    ),
    cmd(
        "tx-key",
        "get_tx_private_key",
        &["get_tx_private_key"],
        "Show the private key of a sent transaction",
        true,
        Group::Tools,
    ),
    cmd(
        "subwallets",
        "subwallets",
        &[],
        "List the addresses in your wallet",
        true,
        Group::Tools,
    ),
    cmd(
        "new-subwallet",
        "create_subwallet",
        &["create_subwallet"],
        "Add a new address to your wallet",
        false,
        Group::Tools,
    ),
    cmd(
        "import-subwallet",
        "import_subwallet",
        &["import_subwallet"],
        "Add an address from its spend key",
        true,
        Group::Tools,
    ),
    cmd(
        "backup",
        "backup",
        &[],
        "Show your private keys and seed (asks your password)",
        true,
        Group::Hidden,
    ),
    cmd(
        "help",
        "help",
        &["?"],
        "List the commands, or explain one",
        true,
        Group::Hidden,
    ),
    cmd(
        "advanced",
        "advanced",
        &[],
        "List the commands",
        true,
        Group::Hidden,
    ),
    cmd(
        "exit",
        "exit",
        &["quit"],
        "Save and close the wallet",
        true,
        Group::Hidden,
    ),
];

/// The commands a wallet knows: a view wallet only those it may use.
#[must_use]
pub fn available(view_wallet: bool) -> Vec<Command> {
    ALL.iter()
        .filter(|c| c.view_wallet || !view_wallet)
        .copied()
        .collect()
}

/// Every name that works in `commands` (for Tab completion).
#[must_use]
pub fn names(commands: &[Command]) -> Vec<&'static str> {
    commands
        .iter()
        .flat_map(|c| std::iter::once(c.name).chain(c.aliases.iter().copied()))
        .collect()
}

/// The command `word` names (its name or an alias, any case).
#[must_use]
pub fn find<'a>(word: &str, commands: &'a [Command]) -> Option<&'a Command> {
    let word = word.to_lowercase();
    commands
        .iter()
        .find(|c| c.name == word || c.aliases.contains(&word.as_str()))
}

/// What a line typed at a command prompt means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Nothing typed: ask again.
    Empty,
    /// A command (its [`Command::key`]) and what followed its name.
    Command(&'static str, String),
    /// A number that is no menu entry.
    BadNumber,
    /// A word that is no command (lower case).
    Unknown(String),
}

/// A leading integer as `std::stoi` reads one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Number {
    /// No digits at the start.
    None,
    /// The value.
    Value(i64),
    /// Too large for an `int`.
    OutOfRange,
}

/// `std::stoi`: optional leading whitespace, an optional sign and at least one digit; what
/// follows the digits is ignored.
#[must_use]
pub fn leading_int(text: &str) -> Number {
    let s = text.trim_start();
    let (negative, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let end = rest
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(rest.len());
    let digits = &rest[..end];
    if digits.is_empty() {
        return Number::None;
    }
    let Ok(value) = digits.parse::<i64>() else {
        return Number::OutOfRange;
    };
    let value = if negative { -value } else { value };
    if i32::try_from(value).is_err() {
        return Number::OutOfRange;
    }
    Number::Value(value)
}

/// The first word names a command of `available` (or, all digits, a menu number); the rest of
/// the line is its argument.
#[must_use]
pub fn select(input: &str, available: &[Command]) -> Selection {
    let line = input.trim();
    let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    if word.is_empty() {
        return Selection::Empty;
    }
    let rest = rest.trim().to_owned();
    if word.bytes().all(|b| b.is_ascii_digit()) {
        return word
            .parse::<u8>()
            .ok()
            .and_then(|n| available.iter().find(|c| c.number == Some(n)))
            .map_or(Selection::BadNumber, |c| Selection::Command(c.key, rest));
    }
    match find(word, available) {
        Some(c) => Selection::Command(c.key, rest),
        None => Selection::Unknown(word.to_lowercase()),
    }
}

/// Column the command lists of `help` start at (`  Address book   `).
const GROUP_COLUMN: usize = 17;
/// Lines of `help` fit in this many columns.
const WIDTH: usize = 60;

/// A menu: `  1  open      Open a wallet on this computer`, `0  exit` last.
#[must_use]
pub fn menu(paint: Painter, title: &str, items: &[Command]) -> String {
    let width = items.iter().map(|c| c.name.len()).max().unwrap_or(0).max(8);
    let mut text = String::new();
    if !title.is_empty() {
        let _ = write!(text, "\n  {}\n", paint.bold(title));
    }
    text.push('\n');
    for c in items {
        let number = c.number.map_or_default(|n| n.to_string());
        if c.description.is_empty() {
            let _ = writeln!(text, "  {number}  {}", paint.bold(c.name));
        } else {
            let pad = " ".repeat(width - c.name.len());
            let _ = writeln!(
                text,
                "  {number}  {}{pad}  {}",
                paint.bold(c.name),
                paint.dim(c.description)
            );
        }
    }
    text.push('\n');
    text
}

/// `help`: the groups with their commands, wrapped at [`WIDTH`] columns.
#[must_use]
pub fn groups(paint: Painter, available: &[Command]) -> String {
    let mut text = String::from("\n");
    for group in [
        Group::Wallet,
        Group::History,
        Group::AddressBook,
        Group::Tools,
    ] {
        let names: Vec<&str> = available
            .iter()
            .filter(|c| c.group == group)
            .map(|c| c.name)
            .collect();
        if names.is_empty() {
            continue;
        }
        let label = group.label();
        let _ = write!(
            text,
            "  {}{}",
            paint.dim(label),
            " ".repeat(GROUP_COLUMN - 2 - label.len())
        );
        let mut column = GROUP_COLUMN;
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                if column + 2 + name.len() > WIDTH {
                    let _ = write!(text, "\n{}", " ".repeat(GROUP_COLUMN));
                    column = GROUP_COLUMN;
                } else {
                    text.push_str("  ");
                    column += 2;
                }
            }
            text.push_str(&paint.bold(name));
            column += name.len();
        }
        text.push('\n');
    }
    let _ = write!(
        text,
        "\n  {}\n\n",
        paint.dim("Type a command, help <command> for details, exit to close.")
    );
    text
}

/// `help <command>`: its description and the other names it has.
#[must_use]
pub fn explain(paint: Painter, command: &Command) -> String {
    let mut text = format!(
        "\n  {}  {}\n",
        paint.bold(command.name),
        paint.dim(command.description)
    );
    if !command.aliases.is_empty() {
        let _ = writeln!(
            text,
            "  {}",
            paint.dim(&format!("also: {}", command.aliases.join(", ")))
        );
    }
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(commands: &[Command]) -> Vec<&'static str> {
        commands.iter().map(|c| c.key).collect()
    }

    #[test]
    fn every_cpp_command_is_there() {
        // `commands.cpp` `allCommands`, in its order
        let cpp = [
            "advanced",
            "address",
            "balance",
            "backup",
            "exit",
            "help",
            "transfer",
            "ab_add",
            "ab_delete",
            "ab_list",
            "ab_send",
            "change_password",
            "get_tx_private_key",
            "make_integrated_address",
            "incoming_transfers",
            "list_transfers",
            "optimize",
            "outgoing_transfers",
            "reset",
            "save",
            "save_csv",
            "send_all",
            "status",
            "swap_node",
        ];
        let all = available(false);
        for name in cpp {
            assert_eq!(
                find(name, &all).map(|c| c.key),
                Some(name),
                "{name} does not work"
            );
        }
        // what a view wallet may not use (`commands.cpp`), plus creating a subwallet
        let spend_only: Vec<&str> = ALL
            .iter()
            .filter(|c| !c.view_wallet)
            .map(|c| c.key)
            .collect();
        assert_eq!(
            spend_only,
            [
                "transfer",
                "outgoing_transfers",
                "list_transfers",
                "ab_send",
                "optimize",
                "send_all",
                "create_subwallet",
            ]
        );
        assert_eq!(
            keys(STARTUP),
            [
                "open",
                "create",
                "seed_restore",
                "key_restore",
                "view_wallet",
                "exit"
            ]
        );
        assert_eq!(
            keys(NODE_DOWN),
            ["try_again", "continue", "swap_node", "exit"]
        );
    }

    #[test]
    fn selection() {
        let spend = available(false);
        let view = available(true);
        let none = String::new();
        assert_eq!(select("", &spend), Selection::Empty);
        assert_eq!(select("   ", &spend), Selection::Empty);
        assert_eq!(
            select("send", &spend),
            Selection::Command("transfer", none.clone())
        );
        assert_eq!(
            select("TRANSFER", &spend),
            Selection::Command("transfer", none.clone())
        );
        assert_eq!(
            select(" status ", &view),
            Selection::Command("status", none.clone())
        );
        assert_eq!(
            select("help send", &spend),
            Selection::Command("help", "send".to_owned())
        );
        assert_eq!(select("send", &view), Selection::Unknown("send".to_owned()));
        assert_eq!(
            select("transfer", &view),
            Selection::Unknown("transfer".to_owned())
        );
        assert_eq!(
            select("Nope", &spend),
            Selection::Unknown("nope".to_owned())
        );
        // numbers only in the menus
        assert_eq!(select("3", &spend), Selection::BadNumber);
        assert_eq!(
            select("3", STARTUP),
            Selection::Command("seed_restore", none.clone())
        );
        assert_eq!(
            select("0", STARTUP),
            Selection::Command("exit", none.clone())
        );
        assert_eq!(select("9", STARTUP), Selection::BadNumber);
        assert_eq!(
            select("restore", STARTUP),
            Selection::Command("seed_restore", none.clone())
        );
        assert_eq!(
            select("key_restore", STARTUP),
            Selection::Command("key_restore", none.clone())
        );
        assert_eq!(
            select("try_again", NODE_DOWN),
            Selection::Command("try_again", none)
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(leading_int("12"), Number::Value(12));
        assert_eq!(leading_int(" +3x"), Number::Value(3));
        assert_eq!(leading_int("-2"), Number::Value(-2));
        assert_eq!(leading_int("abc"), Number::None);
        assert_eq!(leading_int("-"), Number::None);
        assert_eq!(leading_int("99999999999"), Number::OutOfRange);
    }

    #[test]
    fn layout() {
        let p = Painter::new(false);
        assert_eq!(
            menu(p, "Welcome to your XKR wallet", STARTUP),
            "\n  Welcome to your XKR wallet\n\n  1  open      Open a wallet on this computer\n  2  \
             create    Create a new wallet\n  3  restore   Restore from a 25-word seed\n  4  \
             keys      Restore from private keys\n  5  view      Import a view-only wallet\n  \
             0  exit\n\n"
        );
        let help = groups(p, &available(false));
        assert!(
            help.starts_with(
                "\n  Wallet         balance  address  send  seed  keys  back\n  History        txs  \
                 incoming  outgoing  all  export\n  Address book   contacts  add  remove  send-to\n  \
                 Tools          optimize  send-all  integrated  reset  node\n"
            ),
            "{help}"
        );
        for line in help.lines() {
            assert!(line.chars().count() <= 60, "{line:?}");
        }
        assert!(
            help.ends_with("\n  Type a command, help <command> for details, exit to close.\n\n")
        );
        let view_help = groups(p, &available(true));
        assert!(!view_help.contains("send"), "{view_help}");
        assert_eq!(
            explain(p, find("send", &available(false)).unwrap()),
            "\n  send  Send XKR to someone\n  also: s, transfer\n\n"
        );
    }
}
