//! The shared libraries the Bare addons need beyond the C library, loaded before the first
//! worker starts.
//!
//! The prebuilt `.bare` addons in `bridge/node_modules` (udx-native, sodium-native) are C++
//! libraries that list `libstdc++.so.6` as needed. xkrd loads it process-wide (`RTLD_GLOBAL`)
//! before the first worker: by absolute path from the places a NixOS system or a release
//! archive keeps it, else by its soname from the loader's default directories (most Linux
//! systems); the loader then uses the loaded copy, found by its soname, when bare-kit loads the
//! addons.
//! On macOS and Windows nothing is checked.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The C++ runtime the Bare addons need on Linux.
pub const CXX_RUNTIME: &str = "libstdc++.so.6";
/// The system-wide library directory of a NixOS system.
pub const NIXOS_SYSTEM_LIB: &str = "/run/current-system/sw/lib";
/// Why Hyperswarm stays off when [`CXX_RUNTIME`] cannot be loaded.
pub const CXX_RUNTIME_MISSING: &str = "libstdc++.so.6 not found (needed by the Hyperswarm addons). \
     Install it, or on NixOS add gcc's lib directory to LD_LIBRARY_PATH.";

/// Where [`CXX_RUNTIME`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeSource {
    /// Nothing to load on this system (not Linux).
    NotNeeded,
    /// The loader's own search by soname (default directories, `LD_LIBRARY_PATH`, the
    /// executable's run-time path), after no candidate file loaded.
    Default,
    /// This file.
    File(PathBuf),
}

/// The files to try, in order, before the loader's own search: `libstdc++.so.6` in every
/// directory of `NIX_LD_LIBRARY_PATH` (`nix_ld_library_path`), in [`NIXOS_SYSTEM_LIB`], then in
/// `lib/` next to the executable and next to the executable itself (`exe_dir`), so a release
/// archive can ship it. Empty entries of the path list are skipped.
#[must_use]
pub fn runtime_candidates(
    nix_ld_library_path: Option<&OsStr>,
    exe_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = nix_ld_library_path.map_or_default(|p| {
        std::env::split_paths(p)
            .filter(|d| !d.as_os_str().is_empty())
            .collect()
    });
    dirs.push(PathBuf::from(NIXOS_SYSTEM_LIB));
    if let Some(dir) = exe_dir {
        dirs.push(dir.join("lib"));
        dirs.push(dir.to_path_buf());
    }
    let mut files: Vec<PathBuf> = Vec::with_capacity(dirs.len());
    for d in dirs {
        let f = d.join(CXX_RUNTIME);
        if !files.contains(&f) {
            files.push(f);
        }
    }
    files
}

/// Loads [`CXX_RUNTIME`]: each of `candidates` that `exists` by its absolute path
/// (`load(Some(file))`), then by its soname (`load(None)`), and stops at the first that loads.
/// `Err` is [`CXX_RUNTIME_MISSING`]. The file system and the loader are parameters, so the order
/// can be tested without either.
pub fn preload_with(
    candidates: &[PathBuf],
    exists: impl Fn(&Path) -> bool,
    mut load: impl FnMut(Option<&Path>) -> Result<(), String>,
) -> Result<RuntimeSource, String> {
    for file in candidates.iter().filter(|f| f.is_absolute() && exists(f)) {
        match load(Some(file)) {
            Ok(()) => return Ok(RuntimeSource::File(file.clone())),
            Err(e) => {
                tracing::debug!(file = %file.display(), error = %e, "cannot load {CXX_RUNTIME}");
            }
        }
    }
    match load(None) {
        Ok(()) => Ok(RuntimeSource::Default),
        Err(e) => {
            tracing::debug!(error = %e, "{CXX_RUNTIME} is not on the loader's search path");
            Err(CXX_RUNTIME_MISSING.to_owned())
        }
    }
}

/// Makes sure the libraries the Bare addons need are loaded: on Linux [`CXX_RUNTIME`], found as
/// [`preload_with`] describes (candidates from [`runtime_candidates`] with the
/// `NIX_LD_LIBRARY_PATH` environment variable and the executable's directory); elsewhere
/// nothing. The work is done on the first call; later calls return the same result. `Err` is
/// the line to show the user ([`CXX_RUNTIME_MISSING`]).
pub fn check_runtime() -> Result<RuntimeSource, String> {
    static RESULT: OnceLock<Result<RuntimeSource, String>> = OnceLock::new();
    RESULT.get_or_init(load_runtime).clone()
}

#[cfg(target_os = "linux")]
fn load_runtime() -> Result<RuntimeSource, String> {
    let nix = std::env::var_os("NIX_LD_LIBRARY_PATH");
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf));
    let candidates = runtime_candidates(nix.as_deref(), exe_dir.as_deref());
    let found = preload_with(&candidates, Path::is_file, |file| {
        super::ffi::load_library_global(file.map_or(OsStr::new(CXX_RUNTIME), Path::as_os_str))
    });
    if let Ok(RuntimeSource::File(f)) = &found {
        tracing::info!(file = %f.display(), "loaded {CXX_RUNTIME} for the Hyperswarm addons");
    }
    found
}

#[cfg(not(target_os = "linux"))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "same signature as the Linux version"
)]
fn load_runtime() -> Result<RuntimeSource, String> {
    Ok(RuntimeSource::NotNeeded)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn candidates_follow_the_search_order() {
        let nix = OsStr::new("/nix/store/a-gcc-lib/lib::/nix/store/b/lib");
        let c = runtime_candidates(Some(nix), Some(Path::new("/opt/xkr")));
        assert_eq!(
            c,
            paths(&[
                "/nix/store/a-gcc-lib/lib/libstdc++.so.6",
                "/nix/store/b/lib/libstdc++.so.6",
                "/run/current-system/sw/lib/libstdc++.so.6",
                "/opt/xkr/lib/libstdc++.so.6",
                "/opt/xkr/libstdc++.so.6",
            ])
        );
        // Without the variable and the executable's directory only the NixOS system directory.
        assert_eq!(
            runtime_candidates(None, None),
            paths(&["/run/current-system/sw/lib/libstdc++.so.6"])
        );
        // A directory listed twice is tried once.
        let twice = OsStr::new("/run/current-system/sw/lib");
        assert_eq!(runtime_candidates(Some(twice), None).len(), 1);
    }

    #[test]
    fn candidate_files_come_before_the_soname() {
        let tried = RefCell::new(Vec::new());
        let r = preload_with(
            &paths(&["/a/libstdc++.so.6", "relative/libstdc++.so.6"]),
            |_| true,
            |f| {
                tried.borrow_mut().push(f.map(Path::to_path_buf));
                Ok(())
            },
        );
        assert_eq!(r, Ok(RuntimeSource::File("/a/libstdc++.so.6".into())));
        assert_eq!(*tried.borrow(), vec![Some("/a/libstdc++.so.6".into())]);

        // no candidate on disk: the soname, last; relative paths are never tried
        tried.borrow_mut().clear();
        let r = preload_with(
            &paths(&["/a/libstdc++.so.6", "relative/libstdc++.so.6"]),
            Path::is_relative,
            |f| {
                tried.borrow_mut().push(f.map(Path::to_path_buf));
                Ok(())
            },
        );
        assert_eq!(r, Ok(RuntimeSource::Default));
        assert_eq!(*tried.borrow(), vec![None]);
    }

    #[test]
    fn missing_files_are_skipped_and_the_first_that_loads_wins() {
        let candidates = paths(&[
            "/nix/store/x/lib/libstdc++.so.6",
            "/run/current-system/sw/lib/libstdc++.so.6",
            "/opt/xkr/lib/libstdc++.so.6",
            "/opt/xkr/libstdc++.so.6",
        ]);
        // A fake file system: the first file is missing, the second exists but does not load
        // (another architecture), the third and fourth load.
        let present = paths(&[
            "/run/current-system/sw/lib/libstdc++.so.6",
            "/opt/xkr/lib/libstdc++.so.6",
            "/opt/xkr/libstdc++.so.6",
        ]);
        let tried = RefCell::new(Vec::new());
        let r = preload_with(
            &candidates,
            |f| present.iter().any(|p| p == f),
            |f| {
                tried.borrow_mut().push(f.map(Path::to_path_buf));
                match f {
                    None => Err("not found".into()),
                    Some(p) if p.starts_with("/run") => Err("wrong ELF class".into()),
                    Some(_) => Ok(()),
                }
            },
        );
        assert_eq!(
            r,
            Ok(RuntimeSource::File("/opt/xkr/lib/libstdc++.so.6".into()))
        );
        assert_eq!(
            *tried.borrow(),
            vec![
                Some("/run/current-system/sw/lib/libstdc++.so.6".into()),
                Some("/opt/xkr/lib/libstdc++.so.6".into()),
            ]
        );
    }

    #[test]
    fn nothing_found_gives_the_message_for_the_user() {
        let r = preload_with(
            &paths(&["/run/current-system/sw/lib/libstdc++.so.6"]),
            |_| false,
            |_| Err("not found".into()),
        );
        assert_eq!(r, Err(CXX_RUNTIME_MISSING.to_owned()));
        assert_eq!(
            CXX_RUNTIME_MISSING,
            "libstdc++.so.6 not found (needed by the Hyperswarm addons). Install it, or on NixOS \
             add gcc's lib directory to LD_LIBRARY_PATH."
        );
    }
}
