//! Run-time search path for libbare-kit, which runs the Bare workers of the Hyperswarm bridge.
//!
//! xkr-p2p-hyper's `build.rs` checks and links the library from `LIBBARE_DIR` (set by
//! `.cargo/config.toml` to `target/bare-kit/lib`, which `scripts/build-bare-kit.sh` fills), but
//! link arguments of a library's build script do not reach the binaries that depend on it. So the
//! search path is set here, for every xkrd binary and test ([`build_rpath::rpath_args`]): the
//! executable's own directory and its `lib/` (`$ORIGIN` and `$ORIGIN/lib` on Linux and other ELF
//! systems, `@executable_path` and `@executable_path/lib` on macOS), so a `libbare-kit` shipped
//! next to `xkrd` is found; for debug builds and with `XKR_DEV_RPATH=1` also the absolute
//! `LIBBARE_DIR` after those ([`build_rpath::dev_rpath`]), so `target/<profile>/xkrd` runs as
//! built without `LD_LIBRARY_PATH`. Windows needs no search path: it loads `bare-kit.dll` from
//! the executable's directory or `PATH`.

use std::env;
use std::path::PathBuf;

#[path = "build_rpath.rs"]
mod build_rpath;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_rpath.rs");
    println!("cargo:rerun-if-env-changed=LIBBARE_DIR");
    println!("cargo:rerun-if-env-changed=XKR_DEV_RPATH");
    // A missing variable or library stops the build in xkr-p2p-hyper's build.rs with the steps
    // to follow; without a directory only the executable's own ones are added.
    let dev = build_rpath::dev_rpath(
        env::var("PROFILE").ok().as_deref(),
        env::var("XKR_DEV_RPATH").ok().as_deref(),
    );
    let dir = env::var_os("LIBBARE_DIR")
        .filter(|_| dev)
        .map(PathBuf::from)
        .map(|d| d.canonicalize().unwrap_or(d));

    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let family = env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let dir = dir.map(|d| d.display().to_string());
    for arg in build_rpath::rpath_args(&os, &family, dir.as_deref()) {
        println!("cargo:rustc-link-arg={arg}");
    }
}
