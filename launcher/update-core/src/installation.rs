use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

pub(super) const LOCK_FILE: &str = ".rebellion2-install.lock";

/// Owns exclusive access to an installation across launcher and helper processes.
/// The operating system releases the lock even if its owner is terminated. The
/// lock file stays in place so every process always locks the same file.
pub struct InstallationLock {
    _file: File,
}

impl InstallationLock {
    /// Acquires an installation without waiting, returning `WouldBlock` while another process owns it.
    pub fn try_acquire(directory: &Path) -> io::Result<Self> {
        let file = open_lock(directory)?;
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                "another launcher or update helper is using this installation",
            ),
            TryLockError::Error(error) => error,
        })?;
        Ok(Self { _file: file })
    }

    /// Waits for the launcher to release the installation before a helper completes its handoff.
    pub fn acquire(directory: &Path) -> io::Result<Self> {
        let file = open_lock(directory)?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}

/// Opens the persistent lock without truncating it or relying on stale process identifiers.
fn open_lock(directory: &Path) -> io::Result<File> {
    fs::create_dir_all(directory)?;
    let path = directory.join(LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "installation lock is not a regular file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_acquire_with_existing_owner_returns_would_block() {
        let directory = tempfile::tempdir().unwrap();
        let _owner = InstallationLock::try_acquire(directory.path()).unwrap();
        let result = InstallationLock::try_acquire(directory.path());
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn try_acquire_after_owner_exits_reuses_lock_file() {
        let directory = tempfile::tempdir().unwrap();
        drop(InstallationLock::try_acquire(directory.path()).unwrap());
        let _owner = InstallationLock::try_acquire(directory.path()).unwrap();
        assert!(directory.path().join(LOCK_FILE).is_file());
    }

    #[test]
    fn try_acquire_with_directory_at_lock_path_returns_error() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join(LOCK_FILE)).unwrap();
        assert_eq!(
            InstallationLock::try_acquire(directory.path())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
