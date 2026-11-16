//! `xkr-walletd`: the Rust kryptokrona-service (walletd); see the library documentation.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    xkr_walletd::cli::run()
}
