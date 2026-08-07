use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use super::errors::SqliteStoreError;

// PRE-MIGRATION BACKUP
// ================================================================================================

/// Suffix appended to the store's filename to name its pre-migration backup.
const BACKUP_SUFFIX: &str = ".pre-migration-backup";

/// Files `SQLite` keeps next to the database, which describe the database they were written for and
/// must not outlive it.
const SIDECAR_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];

/// Returns the path of the backup taken before migrating the store at `database_filepath`.
pub fn backup_path(database_filepath: &Path) -> PathBuf {
    let mut path = OsString::from(database_filepath);
    path.push(BACKUP_SUFFIX);
    PathBuf::from(path)
}

/// Copies the database into `backup_filepath`, replacing a backup left behind by an earlier run.
///
/// `VACUUM INTO` writes a consistent snapshot even while the connection is open, so this does not
/// depend on the caller quiescing the store.
pub fn create_backup(conn: &Connection, backup_filepath: &Path) -> Result<(), SqliteStoreError> {
    // A backup that is still here was left by a run that died mid-migration. Its database was
    // already restored or abandoned, and `VACUUM INTO` refuses to write to a file that exists.
    discard_backup(backup_filepath)?;

    conn.execute("VACUUM INTO ?1", params![path_argument(backup_filepath)?])?;

    Ok(())
}

/// Puts the backup back in place of the database, consuming the backup.
///
/// The caller must have closed every connection to the database first. Restoring under an open
/// connection would leave that connection reading a file that no longer exists, and on Windows the
/// replacement cannot happen at all.
pub fn restore_backup(
    database_filepath: &Path,
    backup_filepath: &Path,
) -> Result<(), SqliteStoreError> {
    // These describe the database being replaced, not the backup, so leaving one behind would let
    // `SQLite` apply it to the restored file.
    for suffix in SIDECAR_SUFFIXES {
        discard_backup(&sidecar_path(database_filepath, suffix))?;
    }

    std::fs::rename(backup_filepath, database_filepath).map_err(|err| {
        SqliteStoreError::BackupRestoreFailed {
            backup: backup_filepath.display().to_string(),
            reason: err.to_string(),
        }
    })
}

/// Removes `backup_filepath` if it exists.
pub fn discard_backup(backup_filepath: &Path) -> Result<(), SqliteStoreError> {
    match std::fs::remove_file(backup_filepath) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(SqliteStoreError::BackupFailed {
            backup: backup_filepath.display().to_string(),
            reason: err.to_string(),
        }),
    }
}

/// Renders a path for `SQLite`, which takes filenames as text.
fn path_argument(path: &Path) -> Result<&str, SqliteStoreError> {
    path.to_str().ok_or_else(|| SqliteStoreError::BackupFailed {
        backup: path.display().to_string(),
        reason: String::from("backup path is not valid UTF-8"),
    })
}

/// Returns the path of the `SQLite` sidecar file for `database_filepath` with the given suffix.
fn sidecar_path(database_filepath: &Path, suffix: &str) -> PathBuf {
    let mut path = OsString::from(database_filepath);
    path.push(suffix);
    PathBuf::from(path)
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    // `tempfile` rather than `create_test_store_path`: `TempDir` removes the database and any
    // backup left behind on drop, so repeated runs do not accumulate files in the system temp
    // directory.

    use rusqlite::Connection;

    use super::{backup_path, create_backup, restore_backup, sidecar_path};

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
        let backup = backup_path(&database);

        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT);
             INSERT INTO items (id, value) VALUES (1, 'before');",
        )
        .unwrap();

        create_backup(&conn, &backup).unwrap();
        assert!(backup.exists());

        // The change the restore is meant to undo.
        conn.execute_batch(
            "DROP TABLE items;
             CREATE TABLE migrated (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        drop(conn);

        restore_backup(&database, &backup).unwrap();
        assert!(!backup.exists(), "a consumed backup should not be left behind");

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
        let backup = backup_path(&database);
        std::fs::write(&backup, b"not a database").unwrap();

        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY);").unwrap();

        create_backup(&conn, &backup).unwrap();

        let restored = Connection::open(&backup).unwrap();
        assert_eq!(table_names(&restored), vec![String::from("items")]);
    }

    #[test]
    fn restore_removes_sidecars_of_the_replaced_database() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("store.sqlite3");
        let backup = backup_path(&database);

        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY);").unwrap();
        create_backup(&conn, &backup).unwrap();
        drop(conn);

        let journal = sidecar_path(&database, "-journal");
        std::fs::write(&journal, b"stale").unwrap();

        restore_backup(&database, &backup).unwrap();

        assert!(!journal.exists(), "a sidecar of the replaced database must not survive it");
    }
}
