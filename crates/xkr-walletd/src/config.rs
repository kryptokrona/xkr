//! Settings of walletd: command line, JSON config file, the old ini config format, `--dump-config`
//! and `--save-config` (`src/wallet_service/wallet_service_configuration.cpp` `handleSettings`,
//! `updateConfigFormat`, `asJSON`, `asFile`; `configuration_manager.cpp`
//! `ConfigurationManager::init`).
//!
//! The order is the C++ one: the command line, then the config file (an old `key = value` file
//! is first converted to JSON, keeping a copy as `<file>.ini.bak`), then the command line again
//! so it wins over the file.

use std::path::Path;

use clap::builder::BoolishValueParser;
use clap::{CommandFactory, Parser};
use serde_json::{Map, Value, json};
use xkr_consensus::Network;
use xkr_consensus::params::SERVICE_DEFAULT_PORT;

/// `logging::Level`: `FATAL` 0 … `TRACE` 5.
pub const LOG_TRACE: i32 = 5;
/// `logging::INFO`.
pub const LOG_INFO: i32 = 3;

/// `WalletServiceConfiguration` (its flags are separate `bool`s, as in C++).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `--daemon-address`.
    pub daemon_address: String,
    /// `--daemon-port`.
    pub daemon_port: i32,
    /// `--bind-address`.
    pub bind_address: String,
    /// `--bind-port`.
    pub bind_port: i32,
    /// `--rpc-password`.
    pub rpc_password: String,
    /// `-w, --container-file`.
    pub container_file: String,
    /// `-p, --container-password`.
    pub container_password: String,
    /// `--server-root`.
    pub server_root: String,
    /// `--enable-cors`.
    pub cors: String,
    /// `-l, --log-file`.
    pub log_file: String,
    /// `--log-level`.
    pub log_level: i32,
    /// `--init-timeout` (seconds).
    pub init_timeout: i32,
    /// `--rpc-legacy-security`.
    pub legacy_security: bool,
    /// `-h, --help`.
    pub help: bool,
    /// `-v, --version`.
    pub version: bool,
    /// `--dump-config`.
    pub dump_config: bool,
    /// `-c, --config`.
    pub config_file: String,
    /// `--save-config`.
    pub output_file: String,
    /// `--view-key`.
    pub view_key: String,
    /// `--spend-key`.
    pub spend_key: String,
    /// `--mnemonic-seed`.
    pub mnemonic_seed: String,
    /// `-g, --generate-container`.
    pub generate: bool,
    /// `--daemonize` (Windows service builds of C++).
    pub daemonize: bool,
    /// `--register-service` (Windows).
    pub register_service: bool,
    /// `--unregister-service` (Windows).
    pub unregister_service: bool,
    /// `--address`: print the addresses and exit.
    pub print_addresses: bool,
    /// `--SYNC_FROM_ZERO` (accepted; C++ walletd does not use it either).
    pub sync_from_zero: bool,
    /// `--scan-height`.
    pub scan_height: u64,
    /// `--network` (not in C++, which has separate testnet builds): default daemon port and
    /// consensus parameters.
    pub network: Network,
}

impl Config {
    /// The C++ defaults for `network`.
    #[must_use]
    pub fn defaults(network: Network) -> Self {
        Self {
            daemon_address: "127.0.0.1".into(),
            daemon_port: i32::from(network.params().rpc_port),
            bind_address: "127.0.0.1".into(),
            bind_port: i32::from(SERVICE_DEFAULT_PORT),
            rpc_password: String::new(),
            container_file: String::new(),
            container_password: String::new(),
            server_root: String::new(),
            cors: String::new(),
            log_file: "service.log".into(),
            log_level: LOG_INFO,
            init_timeout: 10,
            legacy_security: false,
            help: false,
            version: false,
            dump_config: false,
            config_file: String::new(),
            output_file: String::new(),
            view_key: String::new(),
            spend_key: String::new(),
            mnemonic_seed: String::new(),
            generate: false,
            daemonize: false,
            register_service: false,
            unregister_service: false,
            print_addresses: false,
            sync_from_zero: false,
            scan_height: 0,
            network,
        }
    }

    /// `asJSON`: the settings a config file holds.
    #[must_use]
    pub fn as_json(&self) -> Value {
        json!({
            "daemon-address": self.daemon_address,
            "daemon-port": self.daemon_port,
            "log-file": self.log_file,
            "log-level": self.log_level,
            "init-timeout": self.init_timeout,
            "container-file": self.container_file,
            "container-password": self.container_password,
            "bind-address": self.bind_address,
            "bind-port": self.bind_port,
            "enable-cors": self.cors,
            "rpc-legacy-security": self.legacy_security,
            "rpc-password": self.rpc_password,
            "server-root": self.server_root,
        })
    }

    /// `asString`: `json.dump(2)` (keys sorted).
    #[must_use]
    pub fn as_string(&self) -> String {
        serde_json::to_string_pretty(&self.as_json()).unwrap_or_default()
    }

    /// `asFile`: the JSON with a newline.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, format!("{}\n", self.as_string()))
    }
}

/// The command line (`cxxopts` options of `handleSettings`). Every option is optional so the
/// ones given can be applied over the config file.
#[derive(Debug, Clone, Default, Parser)]
#[command(
    name = "kryptokrona-service",
    disable_help_flag = true,
    disable_version_flag = true,
    about = "Kryptokrona wallet service (walletd): JSON-RPC on /json_rpc"
)]
pub struct Cli {
    /// Display this help message
    #[arg(short = 'h', long = "help", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", value_parser = BoolishValueParser::new())]
    pub help: Option<bool>,
    /// Output software version information
    #[arg(short = 'v', long = "version", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", value_parser = BoolishValueParser::new())]
    pub version: Option<bool>,

    /// The daemon host to use for node operations
    #[arg(long = "daemon-address", value_name = "ip")]
    pub daemon_address: Option<String>,
    /// The daemon RPC port to use for node operations
    #[arg(
        long = "daemon-port",
        value_name = "port",
        allow_negative_numbers = true
    )]
    pub daemon_port: Option<i32>,

    /// Specify the configuration <file> to use instead of CLI arguments
    #[arg(short = 'c', long = "config", value_name = "file")]
    pub config: Option<String>,
    /// Prints the current configuration to the screen
    #[arg(long = "dump-config", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", value_parser = BoolishValueParser::new())]
    pub dump_config: Option<bool>,
    /// Specify log <file> location
    #[arg(short = 'l', long = "log-file", value_name = "file")]
    pub log_file: Option<String>,
    /// Specify log level
    #[arg(long = "log-level", value_name = "#", allow_negative_numbers = true)]
    pub log_level: Option<i32>,
    /// The service will use this <path> as the working directory
    #[arg(long = "server-root", value_name = "path")]
    pub server_root: Option<String>,
    /// Save the configuration to the specified <file>
    #[arg(long = "save-config", value_name = "file")]
    pub save_config: Option<String>,
    /// Amount of time in seconds to wait for initial connection
    #[arg(
        long = "init-timeout",
        value_name = "seconds",
        allow_negative_numbers = true
    )]
    pub init_timeout: Option<i32>,

    /// Print the wallet addresses and then exit
    #[arg(long = "address", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", value_parser = BoolishValueParser::new())]
    pub address: Option<bool>,
    /// Wallet container <file>
    #[arg(short = 'w', long = "container-file", value_name = "file")]
    pub container_file: Option<String>,
    /// Wallet container <password>
    #[arg(short = 'p', long = "container-password", value_name = "password")]
    pub container_password: Option<String>,
    /// Generate a new wallet container
    #[arg(short = 'g', long = "generate-container", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", value_parser = BoolishValueParser::new())]
    pub generate_container: Option<bool>,
    /// Generate a wallet container with this secret view <key>
    #[arg(long = "view-key", value_name = "key")]
    pub view_key: Option<String>,
    /// Generate a wallet container with this secret spend <key>
    #[arg(long = "spend-key", value_name = "key")]
    pub spend_key: Option<String>,
    /// Generate a wallet container with this Mnemonic <seed>
    #[arg(long = "mnemonic-seed", value_name = "seed")]
    pub mnemonic_seed: Option<String>,
    /// Start scanning for transactions from this Blockchain height
    #[arg(long = "scan-height", value_name = "#")]
    pub scan_height: Option<u64>,
    /// Force the wallet to sync from 0
    #[arg(long = "SYNC_FROM_ZERO", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", value_parser = BoolishValueParser::new())]
    pub sync_from_zero: Option<bool>,

    /// Interface IP address for the RPC service
    #[arg(long = "bind-address", value_name = "ip")]
    pub bind_address: Option<String>,
    /// TCP port for the RPC service
    #[arg(long = "bind-port", value_name = "port", allow_negative_numbers = true)]
    pub bind_port: Option<i32>,

    /// Adds header 'Access-Control-Allow-Origin' to the RPC responses. Uses the value specified
    /// as the domain. Use * for all.
    #[arg(long = "enable-cors", value_name = "domain")]
    pub enable_cors: Option<String>,
    /// Enable legacy mode (no password for RPC). WARNING: INSECURE. USE ONLY AS A LAST RESORT.
    #[arg(long = "rpc-legacy-security", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", value_parser = BoolishValueParser::new())]
    pub rpc_legacy_security: Option<bool>,
    /// Specify the <password> to access the RPC server.
    #[arg(long = "rpc-password", value_name = "password")]
    pub rpc_password: Option<String>,

    /// Run the service as a daemon (Windows builds of the C++ service)
    #[arg(long = "daemonize", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", value_parser = BoolishValueParser::new())]
    pub daemonize: Option<bool>,
    /// Registers this program as a Windows service (Windows builds of the C++ service)
    #[arg(long = "register-service", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", value_parser = BoolishValueParser::new())]
    pub register_service: Option<bool>,
    /// Unregisters this program from being a Windows service (Windows builds of the C++
    /// service)
    #[arg(long = "unregister-service", num_args = 0..=1, require_equals = true,
          default_missing_value = "true", value_parser = BoolishValueParser::new())]
    pub unregister_service: Option<bool>,

    /// Network: mainnet or testnet (default daemon port and consensus parameters)
    #[arg(long = "network", value_name = "name")]
    pub network: Option<String>,
}

impl Cli {
    /// The help text.
    #[must_use]
    pub fn help_text() -> String {
        Self::command().render_help().to_string()
    }

    /// `handleSettings(argc, argv, config)`: the options given override `config`.
    pub fn apply(&self, config: &mut Config) {
        fn set<T: Clone>(field: &mut T, value: Option<&T>) {
            if let Some(v) = value {
                field.clone_from(v);
            }
        }
        set(&mut config.help, self.help.as_ref());
        set(&mut config.version, self.version.as_ref());
        set(&mut config.config_file, self.config.as_ref());
        set(&mut config.output_file, self.save_config.as_ref());
        set(&mut config.dump_config, self.dump_config.as_ref());
        set(&mut config.daemon_address, self.daemon_address.as_ref());
        set(&mut config.daemon_port, self.daemon_port.as_ref());
        set(&mut config.init_timeout, self.init_timeout.as_ref());
        set(&mut config.log_file, self.log_file.as_ref());
        set(&mut config.log_level, self.log_level.as_ref());
        set(&mut config.container_file, self.container_file.as_ref());
        set(
            &mut config.container_password,
            self.container_password.as_ref(),
        );
        set(&mut config.bind_address, self.bind_address.as_ref());
        set(&mut config.bind_port, self.bind_port.as_ref());
        set(&mut config.cors, self.enable_cors.as_ref());
        set(
            &mut config.legacy_security,
            self.rpc_legacy_security.as_ref(),
        );
        set(&mut config.rpc_password, self.rpc_password.as_ref());
        set(&mut config.server_root, self.server_root.as_ref());
        set(&mut config.view_key, self.view_key.as_ref());
        set(&mut config.spend_key, self.spend_key.as_ref());
        set(&mut config.mnemonic_seed, self.mnemonic_seed.as_ref());
        set(&mut config.generate, self.generate_container.as_ref());
        set(&mut config.daemonize, self.daemonize.as_ref());
        set(&mut config.register_service, self.register_service.as_ref());
        set(
            &mut config.unregister_service,
            self.unregister_service.as_ref(),
        );
        set(&mut config.print_addresses, self.address.as_ref());
        set(&mut config.sync_from_zero, self.sync_from_zero.as_ref());
        set(&mut config.scan_height, self.scan_height.as_ref());
    }

    /// The network named by `--network` (mainnet when not given).
    pub fn network(&self) -> Result<Network, String> {
        match self.network.as_deref() {
            None | Some("mainnet") => Ok(Network::Mainnet),
            Some("testnet") => Ok(Network::Testnet),
            Some(other) => Err(format!(
                "unknown network {other:?} (expected mainnet or testnet)"
            )),
        }
    }
}

/// `std::stoi`: optional leading whitespace and sign, then at least one digit; what follows
/// the digits is ignored.
fn stoi(text: &str) -> Result<i32, &'static str> {
    let s = text.trim_start();
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let end = digits
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return Err("stoi");
    }
    let value: i64 = digits[..end].parse().map_err(|_| "stoi")?;
    let value = if negative { -value } else { value };
    i32::try_from(value).map_err(|_| "stoi")
}

/// The `key = value` of an old config line, as the regex
/// `\s*(\S[^ \t=]*)\s*=\s*((\s?\S+)+)\s*$` of `updateConfigFormat` matches it: the key has no
/// blank or `=`, the value no run of two blanks.
fn ini_item(line: &str) -> Option<(&str, &str)> {
    let s = line.trim_start();
    let first = s.chars().next()?;
    let key_end = first.len_utf8()
        + s[first.len_utf8()..]
            .find([' ', '\t', '='])
            .unwrap_or(s.len() - first.len_utf8());
    let key = &s[..key_end];
    let rest = s[key_end..].trim_start();
    let value = rest.strip_prefix('=')?.trim();
    if value.is_empty() {
        return None;
    }
    let mut blank = false;
    for c in value.chars() {
        let is_blank = c.is_whitespace();
        if is_blank && blank {
            return None;
        }
        blank = is_blank;
    }
    Some((key, value))
}

/// `updateConfigFormat`: reads an old `key = value` config file into `config`. Returns whether
/// any setting was read; then a copy of the file is kept as `<file>.ini.bak` and the caller
/// rewrites the file as JSON.
pub fn read_ini(path: &str, config: &mut Config) -> Result<bool, String> {
    let text = std::fs::read(path).map_err(|_| {
        "The --config-file you specified does not exist, please check the filename and try \
         again."
            .to_owned()
    })?;
    let text = String::from_utf8_lossy(&text);
    let mut updated = false;
    for line in text.lines() {
        let Some((key, value)) = ini_item(line) else {
            continue;
        };
        let int = |v: &str| stoi(v).map_err(|e| format!("{e} - Invalid value for {key}"));
        match key {
            "daemon-address" => value.clone_into(&mut config.daemon_address),
            "daemon-port" => config.daemon_port = int(value)?,
            "init-timeout" => config.init_timeout = int(value)?,
            "log-file" => value.clone_into(&mut config.log_file),
            "log-level" => config.log_level = int(value)?,
            "container-file" => value.clone_into(&mut config.container_file),
            "container-password" => value.clone_into(&mut config.container_password),
            "bind-address" => value.clone_into(&mut config.bind_address),
            "bind-port" => config.bind_port = int(value)?,
            "enable-cors" => value.clone_into(&mut config.cors),
            "rpc-legacy-security" => config.legacy_security = value.starts_with('1'),
            "rpc-password" => value.clone_into(&mut config.rpc_password),
            "server-root" => value.clone_into(&mut config.server_root),
            _ if !key.is_ascii() => return Err("Bad/invalid config file".to_owned()),
            _ => return Err(format!("Unknown option: {key}")),
        }
        updated = true;
    }
    if updated {
        let _ = std::fs::copy(path, format!("{path}.ini.bak"));
    }
    Ok(updated)
}

/// The JSON type name nlohmann uses in its messages.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `j[key].get<std::string>()`.
fn json_string(v: &Value) -> Result<String, String> {
    v.as_str().map(str::to_owned).ok_or_else(|| {
        format!(
            "[json.exception.type_error.302] type must be string, but is {}",
            type_name(v)
        )
    })
}

/// `j[key].get<int>()`: numbers (truncated) and booleans.
fn json_int(v: &Value) -> Result<i32, String> {
    match v {
        Value::Number(n) => Ok(n
            .as_i64()
            .map(|i| i as i32)
            .or_else(|| n.as_u64().map(|u| u as i32))
            .or_else(|| n.as_f64().map(|f| f as i32))
            .unwrap_or_default()),
        Value::Bool(b) => Ok(i32::from(*b)),
        other => Err(format!(
            "[json.exception.type_error.302] type must be number, but is {}",
            type_name(other)
        )),
    }
}

/// `j[key].get<bool>()`.
fn json_bool(v: &Value) -> Result<bool, String> {
    v.as_bool().ok_or_else(|| {
        format!(
            "[json.exception.type_error.302] type must be boolean, but is {}",
            type_name(v)
        )
    })
}

/// `handleSettings(configFile, config)`: the JSON config file.
pub fn read_json(path: &str, config: &mut Config) -> Result<(), String> {
    let text = std::fs::read(path).map_err(|_| {
        "The --config-file you specified does not exist, please check the filename and try \
         again."
            .to_owned()
    })?;
    let j: Value = serde_json::from_slice(&text)
        .map_err(|e| format!("[json.exception.parse_error.101] parse error: {e}"))?;
    let empty = Map::new();
    let j = j.as_object().unwrap_or(&empty);
    let get = |k: &str| j.get(k);
    if let Some(v) = get("daemon-address") {
        config.daemon_address = json_string(v)?;
    }
    if let Some(v) = get("daemon-port") {
        config.daemon_port = json_int(v)?;
    }
    if let Some(v) = get("init-timeout") {
        config.init_timeout = json_int(v)?;
    }
    if let Some(v) = get("log-file") {
        config.log_file = json_string(v)?;
    }
    if let Some(v) = get("log-level") {
        config.log_level = json_int(v)?;
    }
    if let Some(v) = get("container-file") {
        config.container_file = json_string(v)?;
    }
    if let Some(v) = get("container-password") {
        config.container_password = json_string(v)?;
    }
    if let Some(v) = get("bind-address") {
        config.bind_address = json_string(v)?;
    }
    if let Some(v) = get("bind-port") {
        config.bind_port = json_int(v)?;
    }
    if let Some(v) = get("enable-cors") {
        config.cors = json_string(v)?;
    }
    if let Some(v) = get("rpc-legacy-security") {
        config.legacy_security = json_bool(v)?;
    }
    if let Some(v) = get("rpc-password") {
        config.rpc_password = json_string(v)?;
    }
    if let Some(v) = get("server-root") {
        config.server_root = json_string(v)?;
    }
    Ok(())
}

/// What `ConfigurationManager::init` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Startup {
    /// Print this text and exit with this code (`--help`, `--version`, `--dump-config`,
    /// `--save-config`, a bad config file).
    Exit {
        /// Standard output.
        stdout: String,
        /// Exit code.
        code: u8,
    },
    /// Carry on with these settings.
    Run(Box<Config>),
}

/// A message for an unusable setting (`throw std::runtime_error`, shown as `Fatal error: …`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fatal(pub String);

/// `ConfigurationManager::init`: command line, config file, command line again, then the
/// checks. `header` is the text C++ prints before `--dump-config` and `--save-config` output.
pub fn load(cli: &Cli, header: &str) -> Result<Startup, Fatal> {
    let network = cli.network().map_err(Fatal)?;
    let mut config = Config::defaults(network);
    cli.apply(&mut config);
    if config.help {
        return Ok(Startup::Exit {
            stdout: format!("{header}\n{}\n", Cli::help_text()),
            code: 0,
        });
    }
    if config.version {
        return Ok(Startup::Exit {
            stdout: format!("{header}\n"),
            code: 0,
        });
    }

    if !config.config_file.is_empty() {
        let file = config.config_file.clone();
        let parse_failure = |e: String| Startup::Exit {
            stdout: format!(
                "\nThere was an error parsing the specified configuration file. Please check \
                 the file and try again:\n{e}\n"
            ),
            code: 1,
        };
        let mut notes = String::new();
        match read_ini(&file, &mut config) {
            Ok(true) => {
                notes.push_str("\nUpdating configuration file format...\n");
                let _ = config.save(Path::new(&file));
            }
            Ok(false) => {}
            Err(e) => return Ok(parse_failure(e)),
        }
        if !notes.is_empty() {
            print!("{notes}");
        }
        if let Err(e) = read_json(&file, &mut config) {
            return Ok(Startup::Exit {
                stdout: format!(
                    "\nThere was an error parsing the specified configuration file. Please \
                     check the file and try again: \n{e}\n"
                ),
                code: 1,
            });
        }
    }

    cli.apply(&mut config);

    if config.dump_config {
        return Ok(Startup::Exit {
            stdout: format!("{header}{}\n", config.as_string()),
            code: 0,
        });
    }
    if !config.output_file.is_empty() {
        return Ok(match config.save(Path::new(&config.output_file)) {
            Ok(()) => Startup::Exit {
                stdout: format!("{header}Configuration saved to: {}\n", config.output_file),
                code: 0,
            },
            Err(e) => Startup::Exit {
                stdout: format!(
                    "{header}Could not save configuration to: {}\n{e}\n",
                    config.output_file
                ),
                code: 1,
            },
        });
    }

    check(&config)?;
    Ok(Startup::Run(Box::new(config)))
}

/// The checks of `ConfigurationManager::init` after the settings are read.
pub fn check(config: &Config) -> Result<(), Fatal> {
    let fatal = |m: &str| Err(Fatal(m.to_owned()));
    if config.register_service && config.unregister_service {
        return fatal(
            "It's impossible to use both --register-service and --unregister-service at the \
             same time",
        );
    }
    if config.log_level > LOG_TRACE {
        return Err(Fatal(format!("log-level must be between 0..{LOG_TRACE}")));
    }
    if config.container_file.is_empty() {
        return fatal("You must specify a wallet file to open!");
    }
    if !Path::new(&config.container_file).exists() && !config.generate {
        if Path::new(&format!("{}.wallet", config.container_file)).exists() {
            return Err(Fatal(format!(
                "The wallet file you specified does not exist. Did you mean: {}.wallet?",
                config.container_file
            )));
        }
        return fatal(
            "The wallet file you specified does not exist; please check your spelling and try \
             again.",
        );
    }
    if (!config.view_key.is_empty() || !config.spend_key.is_empty()) && !config.generate {
        return fatal("--generate-container is required");
    }
    if !config.mnemonic_seed.is_empty() && !config.generate {
        return fatal("--generate-container is required");
    }
    if !config.mnemonic_seed.is_empty()
        && (!config.view_key.is_empty() || !config.spend_key.is_empty())
    {
        return fatal("You cannot specify import from both Mnemonic seed and private keys");
    }
    if config.generate {
        return Ok(());
    }
    if config.rpc_password.is_empty() && !config.legacy_security {
        return fatal(
            "Please specify either an RPC password or use the --rpc-legacy-security flag",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("kryptokrona-service").chain(args.iter().copied()))
            .expect("parses")
    }

    #[test]
    fn command_line_flags() {
        let c = cli(&[
            "-w",
            "a.wallet",
            "-p",
            "pw",
            "--rpc-password",
            "x",
            "--bind-port",
            "9000",
            "--daemon-address=node",
            "--rpc-legacy-security",
            "-g",
            "--scan-height",
            "5",
            "--enable-cors",
            "*",
        ]);
        let mut config = Config::defaults(Network::Mainnet);
        c.apply(&mut config);
        assert_eq!(config.container_file, "a.wallet");
        assert_eq!(config.container_password, "pw");
        assert_eq!(config.rpc_password, "x");
        assert_eq!(config.bind_port, 9000);
        assert_eq!(config.daemon_address, "node");
        assert_eq!(config.daemon_port, 11898);
        assert!(config.legacy_security && config.generate);
        assert_eq!(config.scan_height, 5);
        assert_eq!(config.cors, "*");
        let c = cli(&["--rpc-legacy-security=false", "--network", "testnet"]);
        assert_eq!(c.rpc_legacy_security, Some(false));
        assert_eq!(Config::defaults(c.network().unwrap()).daemon_port, 11899);
        assert!(Cli::try_parse_from(["x", "--no-such-flag"]).is_err());
        assert_eq!(cli(&["--daemonize"]).daemonize, Some(true));
    }

    #[test]
    fn json_dump_is_sorted_and_indented() {
        let config = Config::defaults(Network::Mainnet);
        let text = config.as_string();
        assert!(text.starts_with("{\n  \"bind-address\": \"127.0.0.1\",\n  \"bind-port\": 8070,"));
        assert!(text.ends_with("\"server-root\": \"\"\n}"));
    }

    #[test]
    fn ini_lines_match_like_the_cpp_regex() {
        assert_eq!(
            ini_item("daemon-port = 11898"),
            Some(("daemon-port", "11898"))
        );
        assert_eq!(
            ini_item("  key=value with one space "),
            Some(("key", "value with one space"))
        );
        assert_eq!(ini_item("key = two  spaces"), None);
        assert_eq!(ini_item("# a comment"), None);
        assert_eq!(ini_item("#a=b"), Some(("#a", "b")));
        assert_eq!(ini_item("{"), None);
        assert_eq!(ini_item("  \"daemon-port\": 11898,"), None);
        assert_eq!(ini_item("key ="), None);
        assert_eq!(stoi("12abc"), Ok(12));
        assert_eq!(stoi(" -3"), Ok(-3));
        assert_eq!(stoi("abc"), Err("stoi"));
    }

    #[test]
    fn old_config_files_are_converted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("walletd.conf");
        std::fs::write(
            &path,
            "# walletd settings\ncontainer-file = my.wallet\ncontainer-password = secret\n\
             rpc-legacy-security = 1\nbind-port = 18070\n",
        )
        .unwrap();
        let path_text = path.to_str().unwrap().to_owned();
        let c = cli(&["-c", &path_text, "--bind-address", "0.0.0.0"]);
        // the settings are read and the file rewritten as JSON; the container does not exist
        match load(&c, "") {
            Err(Fatal(m)) => assert!(m.contains("does not exist"), "{m}"),
            other => panic!("{other:?}"),
        }
        let mut config = Config::defaults(Network::Mainnet);
        read_json(&path_text, &mut config).unwrap();
        assert_eq!(config.container_file, "my.wallet");
        assert_eq!(config.container_password, "secret");
        assert!(config.legacy_security);
        assert_eq!(config.bind_port, 18070);
        // as in C++, the settings given on the command line go into the converted file too
        assert_eq!(config.bind_address, "0.0.0.0");
        let backup = std::fs::read_to_string(format!("{path_text}.ini.bak")).unwrap();
        assert!(backup.starts_with("# walletd settings"));
        // an unknown key is refused
        std::fs::write(&path, "no-such-key = 1\n").unwrap();
        let mut config = Config::defaults(Network::Mainnet);
        assert_eq!(
            read_ini(&path_text, &mut config),
            Err("Unknown option: no-such-key".to_owned())
        );
    }

    #[test]
    fn json_config_and_command_line_override() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = dir.path().join("w.wallet");
        std::fs::write(&wallet, b"x").unwrap();
        let path = dir.path().join("walletd.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "container-file": wallet.to_str().unwrap(),
                "rpc-password": "from-file",
                "bind-port": 1234,
                "daemon-port": 22,
            }))
            .unwrap(),
        )
        .unwrap();
        let c = cli(&["-c", path.to_str().unwrap(), "--daemon-port", "33"]);
        let Ok(Startup::Run(config)) = load(&c, "") else {
            panic!("starts");
        };
        assert_eq!(config.rpc_password, "from-file");
        assert_eq!(config.bind_port, 1234);
        assert_eq!(config.daemon_port, 33);

        std::fs::write(&path, br#"{"bind-port": "x"}"#).unwrap();
        let Ok(Startup::Exit { code: 1, stdout }) = load(&c, "") else {
            panic!("refused");
        };
        assert!(stdout.contains("type must be number"), "{stdout}");

        let dump = cli(&["--dump-config", "--bind-port", "5"]);
        let Ok(Startup::Exit { code: 0, stdout }) = load(&dump, "HEADER\n") else {
            panic!("dumps");
        };
        assert!(stdout.starts_with("HEADER\n{\n"));
        assert!(stdout.contains("\"bind-port\": 5,"));

        let out = dir.path().join("saved.json");
        let save = cli(&[
            "--save-config",
            out.to_str().unwrap(),
            "--rpc-password",
            "p",
        ]);
        let Ok(Startup::Exit { code: 0, .. }) = load(&save, "") else {
            panic!("saves");
        };
        let saved: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(saved["rpc-password"], json!("p"));
    }

    #[test]
    fn startup_checks() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = dir.path().join("w.wallet");
        let w = wallet.to_str().unwrap();
        let fatal = |args: &[&str]| match load(&cli(args), "") {
            Err(Fatal(m)) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(fatal(&[]), "You must specify a wallet file to open!");
        assert!(fatal(&["-w", w]).contains("does not exist"));
        assert_eq!(
            fatal(&["-w", w, "--spend-key", "ab"]),
            "The wallet file you specified does not exist; please check your spelling and try \
             again."
        );
        std::fs::write(&wallet, b"x").unwrap();
        assert_eq!(
            fatal(&["-w", w, "--spend-key", "ab"]),
            "--generate-container is required"
        );
        assert_eq!(
            fatal(&["-w", w, "-g", "--spend-key", "ab", "--mnemonic-seed", "x"]),
            "You cannot specify import from both Mnemonic seed and private keys"
        );
        assert!(fatal(&["-w", w]).contains("RPC password"));
        assert_eq!(
            fatal(&["-w", w, "--rpc-password", "x", "--log-level", "6"]),
            "log-level must be between 0..5"
        );
        assert!(matches!(
            load(&cli(&["-w", w, "--rpc-legacy-security"]), ""),
            Ok(Startup::Run(_))
        ));
        assert!(matches!(
            load(&cli(&["-w", w, "-g"]), ""),
            Ok(Startup::Run(_))
        ));
    }
}
