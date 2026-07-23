use std::path::PathBuf;

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

        // Enable foreign key checks.
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Concurrent writers race to upgrade their transactions to write locks; wait for the
        // other writer instead of failing immediately with SQLITE_BUSY.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

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
