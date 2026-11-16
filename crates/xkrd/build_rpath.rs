//! The run-time search path of the xkrd binaries, shared by `build.rs` and
//! `tests/release_rpath.rs`.

/// Whether the absolute library directory (`LIBBARE_DIR`) goes into the run-time search path:
/// for debug builds (cargo's `PROFILE` is `debug`) and when `XKR_DEV_RPATH` is `1`, so
/// `target/<profile>/xkrd` runs as built. Release builds search only next to the executable.
#[must_use]
pub fn dev_rpath(profile: Option<&str>, xkr_dev_rpath: Option<&str>) -> bool {
    profile == Some("debug") || xkr_dev_rpath == Some("1")
}

/// The linker arguments for the run-time search path on `target_os` / `target_family` (cargo's
/// `CARGO_CFG_TARGET_OS` / `CARGO_CFG_TARGET_FAMILY`): the executable's own directory and its
/// `lib/` (`$ORIGIN` and `$ORIGIN/lib` on Linux and other ELF systems, `@executable_path` and
/// `@executable_path/lib` on macOS), then `dev_dir` when given. None on Windows, which loads
/// `bare-kit.dll` from the executable's directory or `PATH`.
#[must_use]
pub fn rpath_args(target_os: &str, target_family: &str, dev_dir: Option<&str>) -> Vec<String> {
    let here = match target_os {
        "macos" | "ios" => "@executable_path",
        _ if target_family.split(',').any(|f| f == "unix") => "$ORIGIN",
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
