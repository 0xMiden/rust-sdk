//! Cross-process writer lock for [`SqliteStore`](crate::SqliteStore).
//!
//! The lock is an exclusive advisory lock on a `.lock` file next to the database file, so that it
//! does not interfere with the `SQLite` locks. The operating system releases the lock when the
//! process stops, also after a crash.
//!
//! On Unix, the lock file has the `0o600` mode, because any user who can open the file can hold the
//! lock. On Windows, the file gets the permissions of its directory.

use std::fs::{File, TryLockError};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::string::ToString;

use miden_client_core::errors::StoreError;

use crate::database_error;

/// An exclusive lock on the `.lock` file of a database. Dropping the value releases the lock.
pub(crate) struct WriterLock {
    _file: File,
}

impl WriterLock {
    /// Takes the lock for the database at `database_filepath`, and creates the lock file if it does
    /// not exist.
    ///
    /// Returns [`StoreError::StoreLocked`] without waiting if another process or store holds the
    /// lock.
    pub(crate) fn acquire(database_filepath: &Path) -> Result<Self, StoreError> {
        let mut lock_path = database_filepath.as_os_str().to_owned();
        lock_path.push(".lock");

        let mut options = File::options();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        options.mode(0o600);

        let file = options.open(&lock_path).map_err(database_error)?;
        // The mode applies only to new files. A lock file that exists can have a wider mode.
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(database_error)?;

        file.try_lock().map_err(|err| match err {
            TryLockError::WouldBlock => {
                StoreError::StoreLocked(database_filepath.display().to_string())
            },
            TryLockError::Error(err) => database_error(err),
        })?;

        Ok(Self { _file: file })
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client::testing::common::create_test_store_path;

    use super::WriterLock;

    #[cfg(unix)]
    #[test]
    fn lock_file_permissions_are_owner_only() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let path = create_test_store_path();
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let mode = || -> anyhow::Result<u32> {
            Ok(std::fs::metadata(&lock_path)?.permissions().mode() & 0o777)
        };

        let lock = WriterLock::acquire(&path)?;
        assert_eq!(mode()?, 0o600);

        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o644))?;
        drop(lock);

        let _lock = WriterLock::acquire(&path)?;
        assert_eq!(mode()?, 0o600);
        Ok(())
    }
}
