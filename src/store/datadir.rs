//! Data-directory preparation: permissions and the one-process lock.
//!
//! Both refusals are startup errors, not warnings: a world-readable data
//! directory or a second writer on the same database are local-boundary
//! violations (A10).

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use fs4::{FileExt, TryLockError};

use crate::config::Paths;
use crate::core::error::{Result, WidowError};

/// A prepared data directory. Holding this value keeps the advisory lock;
/// dropping it releases the lock (the file handle closes).
#[derive(Debug)]
pub struct DataDir {
    paths: Paths,
    _lock: File,
}

impl DataDir {
    pub fn open(paths: Paths) -> Result<Self> {
        ensure_private_dir(paths.root())?;
        check_private_dir(paths.root())?;
        ensure_private_dir(&paths.blobs())?;

        let lock_path = paths.lock();
        let mut lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|error| {
                WidowError::Internal(format!("cannot open {}: {error}", lock_path.display()))
            })?;

        match FileExt::try_lock(&lock) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut holder = String::new();
                let _ = lock.read_to_string(&mut holder);
                let holder = holder.trim();
                let holder = if holder.is_empty() { "unknown" } else { holder };
                return Err(WidowError::Busy(format!(
                    "data directory {} is already in use by pid {holder}; one process per data directory",
                    paths.root().display()
                )));
            }
            Err(TryLockError::Error(error)) => {
                return Err(WidowError::Internal(format!(
                    "cannot lock {}: {error}",
                    lock_path.display()
                )));
            }
        }

        // Record the holder so the next process can name it.
        lock.set_len(0).ok();
        lock.seek(SeekFrom::Start(0)).ok();
        let _ = write!(lock, "{}", std::process::id());
        let _ = lock.flush();

        Ok(Self { paths, _lock: lock })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }
}

pub(crate) fn ensure_private_dir(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .map_err(|error| WidowError::Internal(format!("cannot create {}: {error}", path.display())))
}

fn check_private_dir(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        WidowError::Internal(format!("cannot stat {}: {error}", path.display()))
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(WidowError::Internal(format!(
            "data directory {} is accessible by group/other (mode {mode:o}); must be 0700",
            path.display()
        )));
    }
    Ok(())
}

/// Write a file with 0600 permissions, replacing any previous content.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| {
            WidowError::Internal(format!("cannot write {}: {error}", path.display()))
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| WidowError::Internal(format!("cannot write {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> Paths {
        Paths::new(root.to_path_buf())
    }

    #[test]
    fn creates_private_directory_and_locks_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let dir = DataDir::open(paths(&root)).unwrap();
        let mode = std::fs::metadata(dir.paths().root())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        assert!(dir.paths().blobs().is_dir());
        assert!(dir.paths().lock().is_file());
        assert_eq!(
            std::fs::read_to_string(dir.paths().lock()).unwrap(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn refuses_a_world_readable_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = DataDir::open(paths(&root)).unwrap_err();
        assert!(error.to_string().contains("accessible by group/other"));
    }

    #[test]
    fn refuses_a_second_process() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let first = DataDir::open(paths(&root)).unwrap();
        let error = DataDir::open(paths(&root)).unwrap_err();
        assert_eq!(error.code(), crate::core::error::ErrorCode::Busy);
        assert!(error.to_string().contains(&std::process::id().to_string()));
        drop(first);
        // After the holder drops the lock, a new process may take over.
        DataDir::open(paths(&root)).expect("lock released on drop");
    }
}
