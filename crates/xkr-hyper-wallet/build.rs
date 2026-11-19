//! Run-time search path for libbare-kit in this crate's test programs (they start the Bare
//! worker that finds a node over Hyperswarm): the same as xkrd's `build.rs`, which explains it;
//! the shared argument code is `crates/xkrd/build_rpath.rs`.

use std::env;
use std::path::PathBuf;

#[path = "../xkrd/build_rpath.rs"]
mod build_rpath;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../xkrd/build_rpath.rs");
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
