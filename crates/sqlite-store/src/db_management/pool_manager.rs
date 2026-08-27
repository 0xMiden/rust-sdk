use std::path::PathBuf;
use std::time::Duration;

use deadpool::Runtime;
use deadpool::managed::{Manager, Metrics, RecycleError, RecycleResult};
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
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to restrict permissions on the database file"
                    );
                }
            }
        }

        // Feature used to support `IN` and `NOT IN` queries. We need to load
        // this module for every connection we create to the DB to support the
        // queries we want to run
        array::load_module(&conn)?;

        conn.busy_timeout(Duration::from_secs(5))?;

        let journal_mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(
                journal_mode = %journal_mode,
                path = %self.database_path.display(),
                "database does not support WAL journal mode; commits will be slower and readers \
                 will block on writes"
            );
        }

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

    async fn recycle(&self, conn: &mut Self::Type, _: &Metrics) -> RecycleResult<Self::Error> {
        if conn.is_mutex_poisoned() {
            return Err(RecycleError::message("sqlite connection mutex is poisoned"));
        }

        // A closure that issued a bare `BEGIN` and returned early leaves the transaction open on
        // the connection, holding its locks and hiding its writes from the next caller. `rusqlite`
        // only rolls back for a dropped `Transaction` guard, so undo it here.
        conn.interact(|conn| {
            if conn.is_autocommit() {
                Ok(())
            } else {
                conn.execute_batch("ROLLBACK")
            }
        })
        .await
        .map_err(|_| RecycleError::message("failed to reset the sqlite connection"))??;

        Ok(())
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client::store::Store;
    use miden_client::testing::common::create_test_store_path;

    use crate::SqliteStore;
    use crate::sql_error::SqlResultExt;
    use crate::tests::create_test_store;

    #[tokio::test]
    async fn connection_pragmas_are_applied() -> anyhow::Result<()> {
        let store = create_test_store().await;

        let (journal_mode, synchronous, busy_timeout, foreign_keys) = store
            .interact_with_connection(|conn| {
                let journal_mode: String = conn
                    .pragma_query_value(None, "journal_mode", |row| row.get(0))
                    .into_store_error()?;
                let synchronous: i32 = conn
                    .pragma_query_value(None, "synchronous", |row| row.get(0))
                    .into_store_error()?;
                let busy_timeout: i32 = conn
                    .pragma_query_value(None, "busy_timeout", |row| row.get(0))
                    .into_store_error()?;
                let foreign_keys: i32 = conn
                    .pragma_query_value(None, "foreign_keys", |row| row.get(0))
                    .into_store_error()?;

                Ok((journal_mode, synchronous, busy_timeout, foreign_keys))
            })
            .await?;

        // Asserted on the connection rather than on the `PRAGMA` return value, because
        // `pragma_update` reports success even when SQLite kept the previous journal mode.
        assert_eq!(journal_mode.to_lowercase(), "wal");
        // 1 is `NORMAL`.
        assert_eq!(synchronous, 1);
        assert_eq!(busy_timeout, 5_000);
        assert_eq!(foreign_keys, 1);

        Ok(())
    }

    /// A panic inside `interact` poisons the connection's mutex. `recycle` has to drop that
    /// connection, otherwise the pool keeps handing the poisoned one back to callers.
    #[tokio::test]
    async fn poisoned_connection_is_replaced() -> anyhow::Result<()> {
        let store = create_test_store().await;

        let panicked = store
            .interact_with_connection(|_| -> Result<(), miden_client::store::StoreError> {
                panic!("poisoning the connection on purpose")
            })
            .await;
        assert!(panicked.is_err());

        store.set_setting("after-panic".to_string(), b"value".to_vec()).await?;
        assert_eq!(store.get_setting("after-panic".to_string()).await?, Some(b"value".to_vec()));

        Ok(())
    }

    /// A bare `BEGIN` that is never committed holds the connection's locks and hides its writes
    /// from the next caller, so `recycle` has to roll it back.
    #[tokio::test]
    async fn leaked_transaction_is_rolled_back() -> anyhow::Result<()> {
        let store = create_test_store().await;

        store
            .interact_with_connection(|conn| {
                conn.execute_batch("BEGIN").into_store_error()?;
                conn.execute_batch("INSERT INTO settings (name, value) VALUES ('leaked', X'00')")
                    .into_store_error()?;
                Ok(())
            })
            .await?;

        let autocommit = store.interact_with_connection(|conn| Ok(conn.is_autocommit())).await?;
        assert!(autocommit, "the leaked transaction was not rolled back");
        assert_eq!(store.get_setting("leaked".to_string()).await?, None);

        Ok(())
    }

    /// Two stores open on the same file must wait on each other's write locks rather than fail
    /// with `SQLITE_BUSY`.
    #[tokio::test]
    async fn overlapping_accessors_wait_instead_of_failing() -> anyhow::Result<()> {
        let path = create_test_store_path();
        let first = SqliteStore::new(path.clone()).await?;
        let second = SqliteStore::new(path).await?;

        first.set_setting("from-first".to_string(), b"1".to_vec()).await?;
        second.set_setting("from-second".to_string(), b"2".to_vec()).await?;

        assert_eq!(second.get_setting("from-first".to_string()).await?, Some(b"1".to_vec()));
        assert_eq!(first.get_setting("from-second".to_string()).await?, Some(b"2".to_vec()));

        Ok(())
    }

    /// `Store::identifier` returns `&str`, so a path it cannot represent has to be refused at
    /// construction rather than reported as a placeholder every such path would share.
    #[cfg(unix)]
    #[tokio::test]
    async fn non_utf8_database_path_is_rejected() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let mut path = create_test_store_path();
        let mut file_name =
            path.file_name().expect("test path has a file name").as_bytes().to_vec();
        file_name.push(0xff);
        path.set_file_name(OsStr::from_bytes(&file_name));

        // `SqliteStore` is not `Debug`, so this cannot go through `expect_err`.
        let Err(error) = SqliteStore::new(path).await else {
            panic!("a non-UTF-8 path must be rejected");
        };
        assert!(error.to_string().contains("not valid UTF-8"), "unexpected error: {error}");
    }
}
