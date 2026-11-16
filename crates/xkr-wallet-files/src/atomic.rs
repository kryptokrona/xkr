//! Replacing a file in one step: write a temporary file next to it, flush it to disk, rename it
//! over the target and flush the directory.
//!
//! The old file stays complete until the rename, so a write stopped at any point (process
//! killed, power lost) leaves either the old or the new file. A temporary file left behind by a
//! stopped write is never read; [`remove_leftover_temps`] deletes it when the wallet is opened
//! again.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Writes `bytes` to `path` through a temporary file in the same directory, `fsync`s it, renames
/// it over `path` and `fsync`s the directory (Unix). A new file gets mode `0600` on Unix; an
/// existing file keeps its permissions. On error the temporary file is removed and `path` is
/// untouched.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let existing = fs::metadata(path).ok().map(|m| m.permissions());

    let (tmp_path, file) = create_temp(dir, &name.to_string_lossy())?;
    let result = finish(file, &tmp_path, path, dir, bytes, existing);
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn finish(
    mut file: File,
    tmp_path: &Path,
    path: &Path,
    dir: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> io::Result<()> {
    #[cfg(any(test, feature = "fault-injection"))]
    if let Some(fault) = fault::take(path) {
        return fault.apply(&mut file, bytes);
    }
    file.write_all(bytes)?;
    if let Some(p) = permissions {
        file.set_permissions(p)?;
    }
    file.sync_all()?;
    drop(file);
    fs::rename(tmp_path, path)?;
    sync_dir(dir)
}

/// The name of a temporary file for `name`: `.{name}.{pid}.{n}.tmp`.
fn temp_name(name: &str, pid: u32, n: u32) -> String {
    format!(".{name}.{pid}.{n}.tmp")
}

/// Deletes the temporary files that writes of `path` by other processes left behind (a write
/// stopped before its rename); returns how many. The files of this process are kept, as one
/// of its writes may be running.
pub fn remove_leftover_temps(path: &Path) -> io::Result<usize> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let Some(name) = path.file_name() else {
        return Ok(0);
    };
    let prefix = format!(".{}.", name.to_string_lossy());
    let own = std::process::id();
    let mut removed = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some(rest) = file_name
            .strip_prefix(&prefix)
            .and_then(|r| r.strip_suffix(".tmp"))
        else {
            continue;
        };
        let Some((pid, n)) = rest.split_once('.') else {
            continue;
        };
        let (Ok(pid), Ok(_)) = (pid.parse::<u32>(), n.parse::<u32>()) else {
            continue;
        };
        if pid != own && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

fn create_temp(dir: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    let pid = std::process::id();
    let mut last_err = None;
    for n in 0..1000u32 {
        let tmp = dir.join(temp_name(name, pid, n));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| io::Error::other("no free temporary file name")))
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Writes that fail part way, for tests: the next [`atomic_write`] of a file stops where its
/// [`Fault`] says, as a process killed at that point would (the temporary file is then removed,
/// as after a failed write; a killed process leaves it for [`remove_leftover_temps`]).
#[cfg(any(test, feature = "fault-injection"))]
pub mod fault {
    use std::fs::File;
    use std::io::{self, Write as _};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Where a write stops.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Fault {
        /// After this many bytes of the temporary file are written.
        AfterBytes(usize),
        /// After the whole temporary file is written and flushed, before the rename.
        BeforeRename,
    }

    static FAULTS: Mutex<Vec<(PathBuf, Fault)>> = Mutex::new(Vec::new());

    /// Makes the next write of `path` stop at `fault`.
    pub fn inject(path: &Path, fault: Fault) {
        if let Ok(mut faults) = FAULTS.lock() {
            faults.push((path.to_path_buf(), fault));
        }
    }

    /// The fault for `path`, if one is waiting (taken).
    pub(super) fn take(path: &Path) -> Option<Fault> {
        let mut faults = FAULTS.lock().ok()?;
        let pos = faults.iter().position(|(p, _)| p == path)?;
        Some(faults.remove(pos).1)
    }

    impl Fault {
        pub(super) fn apply(self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
            match self {
                Self::AfterBytes(n) => file.write_all(&bytes[..n.min(bytes.len())])?,
                Self::BeforeRename => {
                    file.write_all(bytes)?;
                    file.sync_all()?;
                }
            }
            Err(io::Error::other("write stopped (fault injection)"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_write_leaves_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wallet");
        atomic_write(&path, b"the old content").unwrap();
        let new = b"the new content, longer than the old one".to_vec();
        let mut faults: Vec<fault::Fault> = (0..=new.len()).map(fault::Fault::AfterBytes).collect();
        faults.push(fault::Fault::BeforeRename);
        for f in faults {
            fault::inject(&path, f);
            assert!(atomic_write(&path, &new).is_err(), "{f:?}");
            assert_eq!(fs::read(&path).unwrap(), b"the old content", "{f:?}");
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "{f:?}");
        }
        atomic_write(&path, &new).unwrap();
        assert_eq!(fs::read(&path).unwrap(), new);
    }

    #[test]
    fn leftover_temps_of_other_processes_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wallet");
        atomic_write(&path, b"x").unwrap();
        let other = std::process::id().wrapping_add(1);
        let stale = dir.path().join(temp_name("w.wallet", other, 0));
        let mine = dir
            .path()
            .join(temp_name("w.wallet", std::process::id(), 3));
        let unrelated = dir.path().join(".w.wallet.notes");
        for p in [&stale, &mine, &unrelated] {
            fs::write(p, b"partial").unwrap();
        }
        assert_eq!(remove_leftover_temps(&path).unwrap(), 1);
        assert!(!stale.exists());
        assert!(mine.exists() && unrelated.exists());
        assert_eq!(fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn writes_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wallet");
        atomic_write(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");
        atomic_write(&path, b"second, longer").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second, longer");
        // no temporary files left behind
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("w.wallet")]);
    }

    #[cfg(unix)]
    #[test]
    fn new_file_is_private_and_existing_mode_is_kept() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        atomic_write(&path, b"x").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        atomic_write(&path, b"y").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn missing_directory_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such-dir").join("w.wallet");
        assert!(atomic_write(&path, b"x").is_err());
    }
}
