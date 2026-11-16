//! The run-time search path the xkrd binaries are linked with (`build_rpath.rs`, used by
//! `build.rs`): release builds search only next to the executable; the absolute library
//! directory follows those only for debug builds and with `XKR_DEV_RPATH=1`.

#[path = "../build_rpath.rs"]
mod build_rpath;

use build_rpath::{dev_rpath, rpath_args};

#[test]
fn release_rpath() {
    assert!(!dev_rpath(Some("release"), None));
    assert!(!dev_rpath(Some("release"), Some("0")));
    assert!(!dev_rpath(None, None));
    assert!(dev_rpath(Some("release"), Some("1")));
    assert!(dev_rpath(Some("debug"), None));

    assert_eq!(
        rpath_args("linux", "unix", None),
        ["-Wl,-rpath,$ORIGIN", "-Wl,-rpath,$ORIGIN/lib"]
    );
    assert_eq!(
        rpath_args("linux", "unix", Some("/src/xkr/target/bare-kit/lib")),
        [
            "-Wl,-rpath,$ORIGIN",
            "-Wl,-rpath,$ORIGIN/lib",
            "-Wl,-rpath,/src/xkr/target/bare-kit/lib",
        ]
    );
    assert_eq!(
        rpath_args("freebsd", "unix", None),
        ["-Wl,-rpath,$ORIGIN", "-Wl,-rpath,$ORIGIN/lib"]
    );
    assert_eq!(
        rpath_args("macos", "unix", None),
        [
            "-Wl,-rpath,@executable_path",
            "-Wl,-rpath,@executable_path/lib"
        ]
    );
    assert!(rpath_args("windows", "windows", Some("C:/bare")).is_empty());
}
