//! The command line (`src/wallet_api/parse_arguments.cpp` `parseArguments`, parsed the way
//! cxxopts does).
//!
//! | flag | default |
//! |---|---|
//! | `-h`, `--help` | |
//! | `-v`, `--version` | |
//! | `-p`, `--port <port>` | 8070 (`SERVICE_DEFAULT_PORT`) |
//! | `--rpc-bind-ip <ip>` | `127.0.0.1` |
//! | `--enable-cors <domain>` | none |
//! | `-r`, `--rpc-password <password>` | required unless `-h` or `-v` |
//! | `--network <mainnet\|testnet>` | `mainnet` |
//!
//! Values come as `--name value`, `--name=value`, `-p value` or `-pvalue`; boolean flags also
//! take `=true` / `=false`; words that are not options are ignored. An unknown option, a
//! missing value or a value that does not parse prints an error and the help and exits with 1;
//! a missing `--rpc-password` prints "You must specify an rpc-password!" and the help and exits
//! with 1.

use xkr_consensus::Network;
use xkr_consensus::params::SERVICE_DEFAULT_PORT;
use zeroize::Zeroizing;

/// The options of a run.
pub struct Options {
    /// `--port`.
    pub port: u16,
    /// `--rpc-bind-ip`.
    pub rpc_bind_ip: String,
    /// `--enable-cors`, `None` when unset or empty.
    pub enable_cors: Option<String>,
    /// `--rpc-password`.
    pub rpc_password: Zeroizing<String>,
    /// `--network`.
    pub network: Network,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("port", &self.port)
            .field("rpc_bind_ip", &self.rpc_bind_ip)
            .field("enable_cors", &self.enable_cors)
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

/// What the command line asks for.
#[derive(Debug)]
pub enum Command {
    /// Run the server.
    Run(Options),
    /// Print this text and exit with 0 (`--help`, `--version`).
    Print(String),
    /// Print this text and exit with 1.
    Fail(String),
}

/// The program header (`cryptonote::getProjectCLIHeader`).
#[must_use]
pub fn header() -> String {
    format!(
        "\n xkr-wallet-api v{} (wallet-api)\n This software is distributed under the General \
         Public License v3.0\n\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// The help text (cxxopts `Options::help`).
#[must_use]
pub fn help(program: &str) -> String {
    format!(
        "{header}\nUsage:\n  {program} [OPTION...]\n\n \
         Core options:\n  \
         -h, --help                     Display this help message\n  \
         -v, --version                  Output software version information\n\n \
         Network options:\n  \
         -p, --port <port>              The port to listen on for http requests (default: \
         {port})\n      \
         --rpc-bind-ip arg          Interface IP address for the RPC service (default: \
         127.0.0.1)\n      \
         --network arg              The network: mainnet or testnet; sets the default \
         daemon port (default: mainnet)\n\n \
         RPC options:\n      \
         --enable-cors <domain>     Adds header 'Access-Control-Allow-Origin' to the RPC \
         responses. Uses the value specified as the domain. Use * for all.\n  \
         -r, --rpc-password <password>  Specify the <password> to access the RPC server.\n",
        header = header(),
        port = SERVICE_DEFAULT_PORT,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opt {
    Help,
    Version,
    Port,
    BindIp,
    Cors,
    Password,
    Network,
}

impl Opt {
    fn long(name: &str) -> Option<Self> {
        Some(match name {
            "help" => Self::Help,
            "version" => Self::Version,
            "port" => Self::Port,
            "rpc-bind-ip" => Self::BindIp,
            "enable-cors" => Self::Cors,
            "rpc-password" => Self::Password,
            "network" => Self::Network,
            _ => return None,
        })
    }

    const fn short(c: char) -> Option<Self> {
        match c {
            'h' => Some(Self::Help),
            'v' => Some(Self::Version),
            'p' => Some(Self::Port),
            'r' => Some(Self::Password),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::Version => "version",
            Self::Port => "port",
            Self::BindIp => "rpc-bind-ip",
            Self::Cors => "enable-cors",
            Self::Password => "rpc-password",
            Self::Network => "network",
        }
    }

    const fn is_flag(self) -> bool {
        matches!(self, Self::Help | Self::Version)
    }
}

#[derive(Default)]
struct Raw {
    help: bool,
    version: bool,
    port: Option<String>,
    bind_ip: Option<String>,
    cors: Option<String>,
    password: Option<Zeroizing<String>>,
    network: Option<String>,
}

impl Raw {
    fn set(&mut self, opt: Opt, value: String) -> Result<(), String> {
        match opt {
            Opt::Help => self.help = flag(&value)?,
            Opt::Version => self.version = flag(&value)?,
            Opt::Port => self.port = Some(value),
            Opt::BindIp => self.bind_ip = Some(value),
            Opt::Cors => self.cors = Some(value),
            Opt::Password => self.password = Some(Zeroizing::new(value)),
            Opt::Network => self.network = Some(value),
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

/// A decimal or `0x` hexadecimal port.
fn port(value: &str) -> Result<u16, String> {
    let parsed = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u16::from_str_radix(hex, 16).ok(),
        None => value.parse::<u16>().ok(),
    };
    parsed.ok_or_else(|| failed_to_parse(value))
}

fn network(value: &str) -> Result<Network, String> {
    match value {
        "mainnet" => Ok(Network::Mainnet),
        "testnet" => Ok(Network::Testnet),
        other => Err(failed_to_parse(other)),
    }
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

fn missing(opt: Opt) -> String {
    format!(
        "Option \u{2018}{}\u{2019} is missing an argument",
        opt.name()
    )
}

/// Parses `args` (with the program name first).
#[must_use]
pub fn parse(args: &[String]) -> Command {
    let program = args.first().map_or("wallet-api", String::as_str);
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
    let port = match raw.port.as_deref().map(port).transpose() {
        Ok(p) => p.unwrap_or(SERVICE_DEFAULT_PORT),
        Err(e) => return fail(&e),
    };
    let network = match raw.network.as_deref().map(network).transpose() {
        Ok(n) => n.unwrap_or(Network::Mainnet),
        Err(e) => return fail(&e),
    };
    let Some(rpc_password) = raw.password.filter(|_| !(raw.help || raw.version)) else {
        if raw.help {
            return Command::Print(help(program));
        }
        if raw.version {
            return Command::Print(header());
        }
        return Command::Fail(format!(
            "You must specify an rpc-password!\n{}",
            help(program)
        ));
    };
    Command::Run(Options {
        port,
        rpc_bind_ip: raw.bind_ip.unwrap_or_else(|| "127.0.0.1".to_owned()),
        enable_cors: raw.cors.filter(|c| !c.is_empty()),
        rpc_password,
        network,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("wallet-api")
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
        let o = run(&["-r", "pw"]);
        assert_eq!(o.port, 8070);
        assert_eq!(o.rpc_bind_ip, "127.0.0.1");
        assert_eq!(o.enable_cors, None);
        assert_eq!(o.rpc_password.as_str(), "pw");
        assert_eq!(o.network, Network::Mainnet);
    }

    #[test]
    fn forms() {
        let o = run(&[
            "--port",
            "24070",
            "--rpc-bind-ip=0.0.0.0",
            "--enable-cors",
            "*",
            "--rpc-password=a=b",
            "--network",
            "testnet",
        ]);
        assert_eq!(o.port, 24070);
        assert_eq!(o.rpc_bind_ip, "0.0.0.0");
        assert_eq!(o.enable_cors.as_deref(), Some("*"));
        assert_eq!(o.rpc_password.as_str(), "a=b");
        assert_eq!(o.network, Network::Testnet);
        let o = run(&["-p8071", "-rsecret", "extra-word"]);
        assert_eq!((o.port, o.rpc_password.as_str()), (8071, "secret"));
        assert_eq!(run(&["-p", "0x10", "-r", ""]).port, 16);
        assert_eq!(run(&["--enable-cors", "", "-r", "x"]).enable_cors, None);
    }

    #[test]
    fn help_version_and_errors() {
        assert!(matches!(parse(&args(&["-h"])), Command::Print(t) if t.contains("Usage:")));
        assert!(
            matches!(parse(&args(&["--help", "-r", "x"])), Command::Print(t) if t.contains("--rpc-password"))
        );
        assert!(
            matches!(parse(&args(&["-v"])), Command::Print(t) if t.contains("xkr-wallet-api v"))
        );
        assert!(matches!(
            parse(&args(&["--version=false", "-r", "x"])),
            Command::Run(_)
        ));
        assert!(
            matches!(parse(&args(&[])), Command::Fail(t) if t.starts_with("You must specify an rpc-password!"))
        );
        for bad in [
            &["--nope", "-r", "x"][..],
            &["-r"],
            &["-p", "70000", "-r", "x"],
            &["--port", "abc", "-r", "x"],
            &["--network", "devnet", "-r", "x"],
            &["-x"],
        ] {
            assert!(
                matches!(parse(&args(bad)), Command::Fail(t) if t.starts_with("Error: Unable to parse command line argument options: ")),
                "{bad:?}"
            );
        }
    }
}
