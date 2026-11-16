//! Links bare-kit 3.1.0, which runs the Bare workers of the bridge (`src/bare`).
//!
//! The library is never downloaded or built by cargo. `scripts/build-bare-kit.sh` builds it once
//! into `target/bare-kit/lib`, and `.cargo/config.toml` sets `LIBBARE_DIR` to that directory (a
//! `LIBBARE_DIR` in the environment wins; give it as an absolute path). `LIBBARE_NAME` overrides
//! the library name (default `bare-kit`, the `OUTPUT_NAME` of bare-kit's shared library target:
//! `libbare-kit.so` on Linux, `libbare-kit.dylib` on macOS, the import library `bare-kit.lib` on
//! Windows, whose `bare-kit.dll` goes to `prebuilds/bin`). The shared library is self-contained
//! (Bare, V8 and libuv are linked into it). `docs/hyper-transport.md`, "Building".
//!
//! The run-time search path set here (the executable's own directory and its `lib/`, then the
//! library directory for debug builds and with `XKR_DEV_RPATH=1`) only reaches this crate's
//! tests and examples: link arguments of a library's build script do not reach the binaries
//! that depend on it, so xkrd's `build.rs` sets the same for xkrd.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LIBBARE_DIR");
    println!("cargo:rerun-if-env-changed=LIBBARE_NAME");
    println!("cargo:rerun-if-env-changed=XKR_DEV_RPATH");

    let name = env::var("LIBBARE_NAME").unwrap_or_else(|_| "bare-kit".to_owned());
    let Some(dir) = env::var_os("LIBBARE_DIR").map(PathBuf::from) else {
        panic!(
            "xkr-p2p-hyper needs lib{name} from bare-kit 3.1.0, and LIBBARE_DIR is not set. Run \
             scripts/build-bare-kit.sh once from the repository root (.cargo/config.toml then \
             points LIBBARE_DIR at target/bare-kit/lib), or set LIBBARE_DIR to a directory that \
             holds the library (docs/hyper-transport.md, \"Building\")."
        );
    };
    let files = [
        format!("lib{name}.so"),
        format!("lib{name}.dylib"),
        format!("{name}.lib"),
    ];
    assert!(
        files.iter().any(|f| dir.join(f).is_file()),
        "LIBBARE_DIR={} contains none of {}. Run scripts/build-bare-kit.sh once from the \
         repository root (it builds bare-kit 3.1.0 into target/bare-kit/lib), or set \
         LIBBARE_DIR/LIBBARE_NAME (docs/hyper-transport.md, \"Building\").",
        dir.display(),
        files.join(", ")
    );
    let dir = dir.canonicalize().unwrap_or(dir);
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=dylib={name}");
    let dev = env::var("PROFILE").is_ok_and(|p| p == "debug")
        || env::var("XKR_DEV_RPATH").is_ok_and(|v| v == "1");
    let dir = dir.display().to_string();
    for arg in rpath_args(dev.then_some(dir.as_str())) {
        println!("cargo:rustc-link-arg={arg}");
    }
}

/// Linker arguments that let the loader find the library next to the executable and in its
/// `lib/` (`-rpath` `$ORIGIN` and `$ORIGIN/lib` on ELF systems, `@executable_path` and
/// `@executable_path/lib` on macOS), then in `dev_dir` when given (debug builds and
/// `XKR_DEV_RPATH=1`); none on Windows, which looks for the DLL next to the executable and on
/// `PATH`. The same as `crates/xkrd/build_rpath.rs`.
fn rpath_args(dev_dir: Option<&str>) -> Vec<String> {
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let family = env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let here = match os.as_str() {
        "macos" | "ios" => "@executable_path",
        _ if family.split(',').any(|f| f == "unix") => "$ORIGIN",
        _ => return Vec::new(),
    };
    let mut args = vec![
        format!("-Wl,-rpath,{here}"),
        format!("-Wl,-rpath,{here}/lib"),
    ];
    if let Some(dir) = dev_dir {
        args.push(format!("-Wl,-rpath,{dir}"));
    }
    args
}
