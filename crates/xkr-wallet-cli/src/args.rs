//! The command line (`src/zedwallet++/parse_arguments.cpp` `parseArguments` and
//! `src/zedwallet/parse_arguments.cpp`, parsed the way cxxopts does).
//!
//! | flag | default |
//! |---|---|
//! | `-h`, `--help` | |
//! | `-v`, `--version` | |
//! | `--debug` | off (zedwallet: write the engine's log to `xkrwallet.log`) |
//! | `-r`, `--remote-daemon <host:port>` | `127.0.0.1:` + the network's RPC port when a node answers there, else a node found over Hyperswarm (`hyperswarm`, or a private node's invite `xkr:…`, picks Hyperswarm) |
//! | `-w`, `--wallet-file <file>` | none: the start menu asks |
//! | `-p`, `--password <pass>` | none: asked when the wallet is opened |
//! | `--network <mainnet\|testnet>` | `mainnet` |
//!
//! Values come as `--name value`, `--name=value`, `-r value` or `-rvalue`; boolean flags also
//! take `=true` / `=false`; words that are not options are ignored. An unknown option, a
//! missing value or a value that does not parse prints an error and the help and exits with 1.
//! A `--remote-daemon` that does not parse prints "There was an error parsing the
//! --remote-daemon you specified" and exits with 1.

use xkr_consensus::Network;
use zeroize::Zeroizing;

/// The options of a run.
pub struct Options {
    /// Daemon host (`--remote-daemon`); a full URL (`https://node.example`) is kept whole.
    pub daemon_host: String,
    /// Daemon port (`--remote-daemon`).
    pub daemon_port: u16,
    /// Whether `--remote-daemon` was given (else the local node, or Hyperswarm without one).
    pub daemon_given: bool,
    /// A private node's invite given as `--remote-daemon xkr:…` (the host is then
    /// [`xkr_wallet::SWARM_HOST`]).
    pub invite: Option<String>,
    /// `--wallet-file`.
    pub wallet_file: Option<String>,
    /// `--password` (an empty password counts as given).
    pub password: Option<Zeroizing<String>>,
    /// `--network`.
    pub network: Network,
    /// `--debug`.
    pub debug: bool,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("daemon_host", &self.daemon_host)
            .field("daemon_port", &self.daemon_port)
            .field("daemon_given", &self.daemon_given)
            .field("invite_given", &self.invite.is_some())
            .field("wallet_file", &self.wallet_file)
            .field("password_given", &self.password.is_some())
            .field("network", &self.network)
            .field("debug", &self.debug)
            .finish()
    }
}

/// What the command line asks for.
#[derive(Debug)]
pub enum Command {
    /// Run the wallet.
    Run(Options),
    /// Print this text and exit with 0 (`--help`, `--version`).
    Print(String),
    /// Print this text and exit with 1.
    Fail(String),
}

/// The XKR logo of the header, in the same small block font as the name under it.
const ASCII_ART: &str = r"
▀▄ ▄▀ █ ▄▀ █▀▀▄
 ▄▀▄  █▀▄  █▀█
▀   ▀ ▀  ▀ ▀  ▀
";

/// "kryptokrona" and "wallet" in a small block font, under the XKR logo.
const NAME_ART: &str = r"
█  ▄ ▄ ▄▄ ▄  ▄ ▄▄▄  ▄█▄   ▄▄  █  ▄ ▄ ▄▄  ▄▄  ▄▄▄   ▄▄▄
█▄█  █▀   █  █ █  █  █   █  █ █▄█  █▀   █  █ █  █ █  █
▀  ▀ ▀     ▀▀█ █▀▀    ▀▀  ▀▀  ▀  ▀ ▀     ▀▀  ▀  ▀  ▀▀▀▀
           ▀▀  ▀
▄   ▄  ▄▄▄  █ █  ▄▄  ▄█▄
█ █ █ █  █  █ █ █▀▀▀  █
 ▀ ▀   ▀▀▀▀ ▀ ▀  ▀▀▀   ▀▀
";

/// The program header: the XKR logo and "kryptokrona wallet" under it, both indented by two
/// spaces (the version is in `--version` and `status`).
#[must_use]
pub fn header() -> String {
    let mut text = String::from("\n");
    for line in logo_lines() {
        text.push_str("  ");
        text.push_str(line);
        text.push('\n');
    }
    text.push('\n');
    for line in NAME_ART.trim_matches('\n').lines() {
        text.push_str("  ");
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// The lines of the XKR logo (also drawn when the wallet closes).
#[must_use]
pub fn logo_lines() -> Vec<&'static str> {
    ASCII_ART.trim_matches('\n').lines().collect()
}

/// `xkrwallet v… · GPL-3.0 · <licence>` (`--version`, `status`).
#[must_use]
pub fn version_line() -> String {
    format!(
        "xkrwallet v{} · GPL-3.0 · https://github.com/kryptokrona/kryptokrona/blob/master/LICENSE",
        env!("CARGO_PKG_VERSION")
    )
}

/// `127.0.0.1:<port>`, the default `--remote-daemon`.
fn default_daemon(network: Network) -> String {
    format!("127.0.0.1:{}", network.params().rpc_port)
}

/// The help text (cxxopts `Options::help`).
#[must_use]
pub fn help(program: &str) -> String {
    format!(
        "{header}\nUsage:\n  {program} [OPTION...]\n\n \
         Core options:\n  \
         -h, --help                    Display this help message\n  \
         -v, --version                 Output software version information\n      \
         --debug                   Write the wallet's log to xkrwallet.log\n      \
         --network arg             The network: mainnet or testnet; sets the default daemon \
         port (default: mainnet)\n\n \
         Daemon options:\n  \
         -r, --remote-daemon <host:port>\n                                \
         The daemon <host:port> combination to use for node operations. (default: \
         {daemon} when a node answers there, else a node found over Hyperswarm; \
         \"hyperswarm\" or a private node's invite xkr:... use Hyperswarm)\n\n \
         Wallet options:\n  \
         -w, --wallet-file <file>      Open the wallet <file>\n  \
         -p, --password <pass>         Use the password <pass> to open the wallet\n",
        header = header(),
        daemon = default_daemon(Network::Mainnet),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opt {
    Help,
    Version,
    Debug,
    Network,
    RemoteDaemon,
    WalletFile,
    Password,
}

impl Opt {
    fn long(name: &str) -> Option<Self> {
        Some(match name {
            "help" => Self::Help,
            "version" => Self::Version,
            "debug" => Self::Debug,
            "network" => Self::Network,
            "remote-daemon" => Self::RemoteDaemon,
            "wallet-file" => Self::WalletFile,
            "password" => Self::Password,
            _ => return None,
        })
    }

    const fn short(c: char) -> Option<Self> {
        match c {
            'h' => Some(Self::Help),
            'v' => Some(Self::Version),
            'r' => Some(Self::RemoteDaemon),
            'w' => Some(Self::WalletFile),
            'p' => Some(Self::Password),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::Version => "version",
            Self::Debug => "debug",
            Self::Network => "network",
            Self::RemoteDaemon => "remote-daemon",
            Self::WalletFile => "wallet-file",
            Self::Password => "password",
        }
    }

    const fn is_flag(self) -> bool {
        matches!(self, Self::Help | Self::Version | Self::Debug)
    }
}

#[derive(Default)]
struct Raw {
    help: bool,
    version: bool,
    debug: bool,
    network: Option<String>,
    remote_daemon: Option<String>,
    wallet_file: Option<String>,
    password: Option<Zeroizing<String>>,
}

impl Raw {
    fn set(&mut self, opt: Opt, value: String) -> Result<(), String> {
        match opt {
            Opt::Help => self.help = flag(&value)?,
            Opt::Version => self.version = flag(&value)?,
            Opt::Debug => self.debug = flag(&value)?,
            Opt::Network => self.network = Some(value),
            Opt::RemoteDaemon => self.remote_daemon = Some(value),
            Opt::WalletFile => self.wallet_file = Some(value),
            Opt::Password => self.password = Some(Zeroizing::new(value)),
        }
        Ok(())
    }
}

/// cxxopts boolean values.
fn flag(value: &str) -> Result<bool, String> {
    match value {
        "true" | "t" | "yes" | "y" | "1" | "" => Ok(true),
        "false" | "f" | "no" | "n" | "0" => Ok(false),
        other => Err(failed_to_parse(other)),
    }
}

fn failed_to_parse(value: &str) -> String {
    format!("Argument \u{2018}{value}\u{2019} failed to parse")
}

fn network(value: &str) -> Result<Network, String> {
    match value {
        "mainnet" => Ok(Network::Mainnet),
        "testnet" => Ok(Network::Testnet),
        other => Err(failed_to_parse(other)),
    }
}

fn missing(opt: Opt) -> String {
    format!(
        "Option \u{2018}{}\u{2019} is missing an argument",
        opt.name()
    )
}

fn scan(args: &[String]) -> Result<Raw, String> {
    let mut raw = Raw::default();
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" {
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (long, None),
            };
            let opt = Opt::long(name)
                .ok_or_else(|| format!("Option \u{2018}{name}\u{2019} does not exist"))?;
            let value = match inline {
                Some(v) => v,
                None if opt.is_flag() => String::new(),
                None => {
                    let v = args.get(i).cloned().ok_or_else(|| missing(opt))?;
                    i += 1;
                    v
                }
            };
            raw.set(opt, value)?;
        } else if let Some(shorts) = arg.strip_prefix('-').filter(|s| !s.is_empty()) {
            for (pos, c) in shorts.char_indices() {
                let opt = Opt::short(c)
                    .ok_or_else(|| format!("Option \u{2018}{c}\u{2019} does not exist"))?;
                if opt.is_flag() {
                    raw.set(opt, String::new())?;
                    continue;
                }
                let rest = &shorts[pos + c.len_utf8()..];
                let value = if rest.is_empty() {
                    let v = args.get(i).cloned().ok_or_else(|| missing(opt))?;
                    i += 1;
                    v
                } else {
                    rest.to_owned()
                };
                raw.set(opt, value)?;
                break;
            }
        }
    }
    Ok(raw)
}

/// The leading decimal digits of `text` as a port (`std::stoi` on the part after the colon);
/// `None` if there are none or the number is not a port.
fn leading_port(text: &str) -> Option<u16> {
    let digits: &str = {
        let end = text
            .char_indices()
            .find(|(_, c)| !c.is_ascii_digit())
            .map_or(text.len(), |(i, _)| i);
        &text[..end]
    };
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// `zed_utilities::parseDaemonAddressFromString`: `host:port`, or `host` with the default
/// port. A URL with a scheme (`https://node.example:443/x`) is used whole.
#[must_use]
pub fn parse_daemon_address(text: &str, default_port: u16) -> Option<(String, u16)> {
    if text.contains("://") {
        return Some((text.to_owned(), default_port));
    }
    let mut parts: Vec<&str> = text.split(':').collect();
    // `std::getline` splitting yields no trailing empty field
    if parts.last() == Some(&"") {
        parts.pop();
    }
    match parts.as_slice() {
        [] => None,
        [host] => Some(((*host).to_owned(), default_port)),
        [host, port, ..] => Some(((*host).to_owned(), leading_port(port)?)),
    }
}

/// Parses `args` (with the program name first).
#[must_use]
pub fn parse(args: &[String]) -> Command {
    let program = args.first().map_or("xkrwallet", String::as_str);
    let fail = |message: &str| {
        Command::Fail(format!(
            "Error: Unable to parse command line argument options: {message}\n\n{}",
            help(program)
        ))
    };
    let raw = match scan(args) {
        Ok(raw) => raw,
        Err(e) => return fail(&e),
    };
    let network = match raw.network.as_deref().map(network).transpose() {
        Ok(n) => n.unwrap_or(Network::Mainnet),
        Err(e) => return fail(&e),
    };
    if raw.help {
        return Command::Print(help(program));
    }
    if raw.version {
        return Command::Print(format!("{}\n  {}\n", header(), version_line()));
    }
    let default_port = network.params().rpc_port;
    let invite = raw
        .remote_daemon
        .as_deref()
        .filter(|d| d.starts_with("xkr:"))
        .map(str::to_owned);
    let daemon = match (&invite, &raw.remote_daemon) {
        (Some(_), _) => xkr_wallet::SWARM_HOST.to_owned(),
        (None, Some(d)) => d.clone(),
        (None, None) => default_daemon(network),
    };
    let Some((daemon_host, daemon_port)) = parse_daemon_address(&daemon, default_port) else {
        return Command::Fail(
            "There was an error parsing the --remote-daemon you specified".to_owned(),
        );
    };
    Command::Run(Options {
        daemon_host,
        daemon_port,
        daemon_given: raw.remote_daemon.is_some(),
        invite,
        wallet_file: raw.wallet_file,
        password: raw.password,
        network,
        debug: raw.debug,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("xkrwallet")
            .chain(list.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    fn run(list: &[&str]) -> Options {
        match parse(&args(list)) {
            Command::Run(o) => o,
            other => panic!("not a run: {other:?}"),
        }
    }

    #[test]
    fn defaults() {
        let o = run(&[]);
        assert_eq!(
            (o.daemon_host.as_str(), o.daemon_port),
            ("127.0.0.1", 11898)
        );
        assert!(o.wallet_file.is_none());
        assert!(o.password.is_none());
        assert_eq!(o.network, Network::Mainnet);
        assert!(!o.debug);
        let t = run(&["--network", "testnet"]);
        assert_eq!(t.daemon_port, Network::Testnet.params().rpc_port);
    }

    #[test]
    fn forms() {
        let o = run(&[
            "-r",
            "node.example:11800",
            "--wallet-file=my.wallet",
            "-pse=cret",
            "--debug",
        ]);
        assert_eq!(
            (o.daemon_host.as_str(), o.daemon_port),
            ("node.example", 11800)
        );
        assert_eq!(o.wallet_file.as_deref(), Some("my.wallet"));
        assert_eq!(o.password.as_deref().map(String::as_str), Some("se=cret"));
        assert!(o.debug);
        let o = run(&["--remote-daemon", "localhost", "-w", "a", "-p", ""]);
        assert_eq!(
            (o.daemon_host.as_str(), o.daemon_port),
            ("localhost", 11898)
        );
        assert_eq!(o.password.as_deref().map(String::as_str), Some(""));
        let o = run(&["-r", "https://node.example/rpc"]);
        assert_eq!(o.daemon_host, "https://node.example/rpc");
    }

    #[test]
    fn daemon_addresses() {
        assert_eq!(parse_daemon_address("h:1", 5), Some(("h".to_owned(), 1)));
        assert_eq!(parse_daemon_address("h:", 5), Some(("h".to_owned(), 5)));
        assert_eq!(parse_daemon_address("h", 5), Some(("h".to_owned(), 5)));
        assert_eq!(
            parse_daemon_address("h:12ab", 5),
            Some(("h".to_owned(), 12))
        );
        assert_eq!(parse_daemon_address("", 5), None);
        assert_eq!(parse_daemon_address("h:x", 5), None);
        assert_eq!(parse_daemon_address("h:99999", 5), None);
    }

    #[test]
    fn help_version_and_errors() {
        assert!(matches!(parse(&args(&["-h"])), Command::Print(t) if t.contains("Usage:")));
        assert!(
            matches!(parse(&args(&["--help"])), Command::Print(t) if t.contains("--remote-daemon"))
        );
        assert!(matches!(parse(&args(&["-v"])), Command::Print(t) if t.contains("xkrwallet v")));
        assert!(matches!(
            parse(&args(&["-r", "h:abc"])),
            Command::Fail(t) if t == "There was an error parsing the --remote-daemon you specified"
        ));
        for bad in [
            &["--nope"][..],
            &["-w"],
            &["--network", "devnet"],
            &["-x"],
            &["--debug=maybe"],
        ] {
            assert!(
                matches!(parse(&args(bad)), Command::Fail(t) if t.starts_with("Error: Unable to parse command line argument options: ")),
                "{bad:?}"
            );
        }
    }
}
