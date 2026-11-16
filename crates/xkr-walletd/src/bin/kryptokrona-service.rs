//! `kryptokrona-service`: the same program as `xkr-walletd` under the C++ binary's name, so an
//! operator only has to swap the binary.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    xkr_walletd::cli::run()
}
