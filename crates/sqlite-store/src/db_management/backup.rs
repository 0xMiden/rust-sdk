use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::string::{String, ToString};

use rusqlite::{Connection, params};

use super::errors::SqliteStoreError;

// PRE-MIGRATION BACKUP
// ================================================================================================

/// Suffix appended to the store's filename to name its pre-migration backup.
const BACKUP_SUFFIX: &str = ".pre-migration-backup";

/// Files `SQLite` keeps next to the database, which describe the database they were written for and
/// must not outlive it.
const SIDECAR_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];

/// A copy of a store, taken before migrating it.
#[derive(Debug)]
pub(crate) struct SqliteBackup {
    database_filepath: PathBuf,
    backup_filepath: PathBuf,
}

impl SqliteBackup {
    /// Copies the database into its backup path, replacing a copy left behind by an earlier run.
    ///
    /// `VACUUM INTO` writes a consistent snapshot even while the connection is open, so this does
    /// not depend on the caller quiescing the store.
    pub(crate) fn create(
        conn: &Connection,
        database_filepath: PathBuf,
    ) -> Result<Self, SqliteStoreError> {
        let backup_filepath = Self::path_for(&database_filepath);

        // Remove any previous hanged migration
        remove_file(&backup_filepath)?;

        conn.execute("VACUUM INTO ?1", params![path_argument(&backup_filepath)?])?;

        Ok(Self { database_filepath, backup_filepath })
    }

    /// Returns the path of the backup of the store at `database_filepath`.
    pub(crate) fn path_for(database_filepath: &Path) -> PathBuf {
        with_suffix(database_filepath, BACKUP_SUFFIX)
    }

    /// Puts the copy back in place of the database, consuming it.
    ///
    /// The caller must have closed every connection to the database first.
    pub(crate) fn restore(self) -> Result<(), SqliteStoreError> {
        for suffix in SIDECAR_SUFFIXES {
            remove_file(&with_suffix(&self.database_filepath, suffix))?;
        }

        std::fs::rename(&self.backup_filepath, &self.database_filepath).map_err(|err| {
            SqliteStoreError::BackupRestoreFailed {
                backup: self.backup_filepath.display().to_string(),
                reason: err.to_string(),
            }
        })
    }

    /// Removes the copy, consuming it.
    pub(crate) fn discard(self) -> Result<(), SqliteStoreError> {
        remove_file(&self.backup_filepath)
    }

    /// Removes the backup of the store at `database_filepath`, if there is one.
    pub(crate) fn discard_for(database_filepath: &Path) -> Result<(), SqliteStoreError> {
        remove_file(&Self::path_for(database_filepath))
    }
}

/// Removes `filepath` if it exists.
fn remove_file(filepath: &Path) -> Result<(), SqliteStoreError> {
    match std::fs::remove_file(filepath) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(SqliteStoreError::BackupFailed {
            backup: filepath.display().to_string(),
            reason: err.to_string(),
        }),
    }
}

/// Returns `path` with `suffix` appended to its filename.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut suffixed = OsString::from(path);
    suffixed.push(suffix);

    PathBuf::from(suffixed)
}

/// Renders a path for `SQLite`, which takes filenames as text.
fn path_argument(path: &Path) -> Result<&str, SqliteStoreError> {
    path.to_str().ok_or_else(|| SqliteStoreError::BackupFailed {
        backup: path.display().to_string(),
        reason: String::from("backup path is not valid UTF-8"),
    })
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    // `tempfile` rather than `create_test_store_path`: `TempDir` removes the database and any
    // backup left behind on drop, so repeated runs do not accumulate files in the system temp
    // directory.

    use rusqlite::Connection;

    use super::{SqliteBackup, with_suffix};

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn backup_restores_the_database_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("store.sqlite3");

        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT);
             INSERT INTO items (id, value) VALUES (1, 'before');",
        )
        .unwrap();

        let backup = SqliteBackup::create(&conn, database.clone()).unwrap();
        assert!(SqliteBackup::path_for(&database).exists());

        // The change the restore is meant to undo.
        conn.execute_batch(
            "DROP TABLE items;
             CREATE TABLE migrated (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        drop(conn);

        backup.restore().unwrap();
        assert!(
            !SqliteBackup::path_for(&database).exists(),
            "a consumed backup should not be left behind"
        );

        let conn = Connection::open(&database).unwrap();
        assert_eq!(table_names(&conn), vec![String::from("items")]);
        let value: String = conn
            .query_row("SELECT value FROM items WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "before");
    }

    #[test]
    fn backup_replaces_one_left_by_an_earlier_run() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("store.sqlite3");
        let backup_filepath = SqliteBackup::path_for(&database);
        std::fs::write(&backup_filepath, b"not a database").unwrap();

        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY);").unwrap();

        SqliteBackup::create(&conn, database).unwrap();

        let restored = Connection::open(&backup_filepath).unwrap();
        assert_eq!(table_names(&restored), vec![String::from("items")]);
    }

    #[test]
    fn restore_removes_sidecars_of_the_replaced_database() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("store.sqlite3");

        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY);").unwrap();
        let backup = SqliteBackup::create(&conn, database.clone()).unwrap();
        drop(conn);

        let journal = with_suffix(&database, "-journal");
        std::fs::write(&journal, b"stale").unwrap();

        backup.restore().unwrap();

        assert!(!journal.exists(), "a sidecar of the replaced database must not survive it");
    }
}
