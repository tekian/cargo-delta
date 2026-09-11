use core::sync::atomic::{AtomicU64, Ordering};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    atomic_write_with(path, contents, replace_file)
}

fn atomic_write_with(path: &Path, contents: &[u8], replace: impl FnOnce(&Path, &Path) -> io::Result<()>) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "output path must name a file"))?;

    let (temporary_path, mut temporary_file) = create_temporary_sibling(parent, file_name)?;
    let mut cleanup = RemoveOnDrop(Some(temporary_path.clone()));

    temporary_file.write_all(contents)?;
    temporary_file.flush()?;
    temporary_file.sync_all()?;
    drop(temporary_file);

    replace(&temporary_path, path)?;
    cleanup.0 = None;
    Ok(())
}

fn create_temporary_sibling(parent: &Path, file_name: &std::ffi::OsStr) -> io::Result<(PathBuf, File)> {
    for _ in 0..100 {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let temporary_name = format!(".{}.cargo-delta-{}-{sequence}.tmp", file_name.to_string_lossy(), std::process::id());
        let temporary_path = parent.join(temporary_name);

        match OpenOptions::new().write(true).create_new(true).open(&temporary_path) {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to create a unique temporary output file",
    ))
}

struct RemoveOnDrop(Option<PathBuf>);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
    }

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(core::iter::once(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(core::iter::once(0)).collect();

    // SAFETY: both pointers reference live, nul-terminated UTF-16 buffers for the duration
    // of the call, and the flags are valid for MoveFileExW.
    let result = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
    if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(any(unix, windows)))]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::test_directory;

    #[test]
    #[cfg_attr(miri, ignore)]
    fn atomic_write_replaces_existing_file() {
        let directory = test_directory("atomic-replace");
        let destination = directory.join("artifact.txt");
        fs::write(&destination, "old").unwrap();

        atomic_write(&destination, b"new").unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn failed_replacement_preserves_existing_file_and_removes_temporary() {
        let directory = test_directory("atomic-failure");
        let destination = directory.join("artifact.txt");
        fs::write(&destination, "old").unwrap();

        let result = atomic_write_with(&destination, b"new", |_from, _to| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected replacement failure"))
        });

        assert!(result.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"old");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn atomic_write_creates_zero_byte_file() {
        let directory = test_directory("atomic-empty");
        let destination = directory.join("empty.txt");

        atomic_write(&destination, b"").unwrap();

        assert_eq!(fs::metadata(&destination).unwrap().len(), 0);
        let _ = fs::remove_dir_all(directory);
    }
}
