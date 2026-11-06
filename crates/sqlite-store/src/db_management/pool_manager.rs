use std::path::PathBuf;
use std::time::Duration;

use deadpool::Runtime;
use deadpool::managed::{Manager, Metrics, RecycleResult};
use rusqlite::Connection;
use rusqlite::vtab::array;

use super::errors::SqliteStoreError;

deadpool::managed_reexports!(
    "miden-client-sqlite-store",
    SqlitePoolManager,
    deadpool::managed::Object<SqlitePoolManager>,
    rusqlite::Error,
    SqliteStoreError
);

const RUNTIME: Runtime = Runtime::Tokio1;

/// How long a connection waits for a lock before surfacing `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

// POOL MANAGER
// ================================================================================================

/// `SQLite` connection pool manager
pub struct SqlitePoolManager {
    database_path: PathBuf,
}

/// `SQLite` connection pool manager
impl SqlitePoolManager {
    pub fn new(database_path: PathBuf) -> Self {
        Self { database_path }
    }

    fn new_connection(&self) -> rusqlite::Result<Connection> {
        let conn = Connection::open(&self.database_path)?;

        // Restrict database file permissions to owner-only on Unix.
        // Also covers WAL and SHM journal files that SQLite may create.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            for suffix in &["", "-wal", "-shm"] {
                let mut path = self.database_path.as_os_str().to_owned();
                path.push(suffix);
                let path = std::path::PathBuf::from(path);
                if path.exists()
                    && let Err(e) = std::fs::set_permissions(&path, perms.clone())
                {
                    eprintln!("Warning: failed to set permissions on {}: {e}", path.display());
                }
            }
        }

        // Feature used to support `IN` and `NOT IN` queries. We need to load
        // this module for every connection we create to the DB to support the
        // queries we want to run
        array::load_module(&conn)?;

        // The store is designed for a single accessor, but SQLite locks at the file
        // level: an accidental second process on the same database (e.g. a CLI
        // invocation while another client instance runs) would otherwise fail
        // instantly with `SQLITE_BUSY`. Waiting briefly turns that accident into a
        // short delay. Also set before the journal-mode switch below, which itself
        // takes a lock.
        conn.busy_timeout(BUSY_TIMEOUT)?;

        // WAL commits append pages once to the `-wal` file instead of the rollback
        // journal's double write (journal + database), and it is what makes
        // `synchronous = NORMAL` below safe. `journal_mode` returns the resulting
        // mode as a row (and legitimately reports a non-WAL mode for in-memory
        // databases), so it cannot go through `pragma_update`.
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;

        // In WAL mode, NORMAL fsyncs at checkpoints rather than on every commit —
        // the store applies many small transactions per sync cycle. Database
        // integrity on crash is preserved; at most the final commits before a power
        // loss roll back, which the next state sync re-derives.
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        // Enable foreign key checks.
        conn.pragma_update(None, "foreign_keys", "ON")?;

        Ok(conn)
    }
}

impl Manager for SqlitePoolManager {
    type Type = deadpool_sync::SyncWrapper<Connection>;
    type Error = rusqlite::Error;

    async fn create(&self) -> Result<Self::Type, Self::Error> {
        let conn = self.new_connection();
        deadpool_sync::SyncWrapper::new(RUNTIME, move || conn).await
    }

    async fn recycle(&self, _: &mut Self::Type, _: &Metrics) -> RecycleResult<Self::Error> {
        Ok(())
    }
}
