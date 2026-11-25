//! Opening `SQLite` connections, and the single connection the store runs on where it has no
//! connection pool.

use std::path::Path;
#[cfg(any(target_arch = "wasm32", test))]
use std::path::PathBuf;
#[cfg(any(target_arch = "wasm32", test))]
use std::sync::{Mutex, TryLockError};
use std::time::Duration;

#[cfg(any(target_arch = "wasm32", test))]
use miden_client::store::StoreError;
use rusqlite::Connection;
use rusqlite::vtab::array;

// CONNECTION SETUP
// ================================================================================================

/// Opens a connection to the database at `database_path` and configures it for the store.
pub(crate) fn open_connection(database_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(database_path)?;

    // Restrict database file permissions to owner-only on Unix. Also covers WAL and SHM journal
    // files that SQLite may create.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        for suffix in &["", "-wal", "-shm"] {
            let mut path = database_path.as_os_str().to_owned();
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

    // Feature used to support `IN` and `NOT IN` queries. We need to load this module for every
    // connection we create to the DB to support the queries we want to run
    array::load_module(&conn)?;

    conn.busy_timeout(Duration::from_secs(5))?;

    configure_journal(&conn, database_path)?;

    // Enable foreign key checks.
    conn.pragma_update(None, "foreign_keys", "ON")?;

    Ok(conn)
}

/// Switches `conn` to WAL journaling, and relaxes `synchronous` to the level that is safe under it.
#[cfg(not(target_arch = "wasm32"))]
fn configure_journal(conn: &Connection, database_path: &Path) -> rusqlite::Result<()> {
    let journal_mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(
            journal_mode = %journal_mode,
            path = %database_path.display(),
            "database does not support WAL journal mode; commits will be slower and readers \
             will block on writes"
        );
    }

    conn.pragma_update(None, "synchronous", "NORMAL")
}

/// Leaves `conn` on the rollback journal and on `synchronous = FULL`, the defaults `SQLite` opens a
/// connection with.
///
/// The `wasm32` file systems have no shared memory for a WAL index, so WAL is not available. A
/// rollback journal is only safe against a crash at `FULL`, which is why `synchronous` is not
/// relaxed to `NORMAL` as it is under WAL.
#[cfg(target_arch = "wasm32")]
#[allow(
    clippy::unnecessary_wraps,
    reason = "keeps the signature of the WAL variant, which can fail"
)]
fn configure_journal(_conn: &Connection, _database_path: &Path) -> rusqlite::Result<()> {
    Ok(())
}

// SINGLE CONNECTION
// ================================================================================================

/// One connection to the database, which runs every query inline on the calling thread.
///
/// This is how the store reaches its database on `wasm32`, where there is no blocking thread for a
/// pool to run a query on. It gives the same guarantees between queries that the pool gives when it
/// recycles a connection: a transaction a query left open is rolled back, and a connection a query
/// panicked on is replaced.
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) struct SingleConnection {
    conn: Mutex<Connection>,
    database_path: PathBuf,
}

#[cfg(any(target_arch = "wasm32", test))]
impl SingleConnection {
    /// Opens the connection to the database at `database_path`.
    pub(crate) fn open(database_path: PathBuf) -> rusqlite::Result<Self> {
        let conn = open_connection(&database_path)?;
        Ok(Self { conn: Mutex::new(conn), database_path })
    }

    /// Runs `f` on the connection and returns its result.
    ///
    /// # Errors
    ///
    /// Returns an error without running `f` if the connection is already running a query. A query
    /// is a synchronous closure that cannot reach the store, so this only happens after a panic
    /// that did not unwind left the connection locked.
    pub(crate) fn interact<R>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let mut conn = match self.conn.try_lock() {
            Ok(conn) => conn,
            Err(TryLockError::WouldBlock) => {
                return Err(StoreError::DatabaseError(
                    "the sqlite connection is already running a query".to_string(),
                ));
            },
            // A query panicked while it held the connection, so the state of the connection is
            // unknown. Replace it, as the pool does with a connection whose mutex is poisoned.
            Err(TryLockError::Poisoned(poisoned)) => {
                let mut conn = poisoned.into_inner();
                *conn = open_connection(&self.database_path).map_err(|err| {
                    StoreError::DatabaseError(format!(
                        "failed to replace the sqlite connection a query panicked on: {err}"
                    ))
                })?;
                self.conn.clear_poison();
                conn
            },
        };

        // A closure that issued a bare `BEGIN` and returned early leaves the transaction open on
        // the connection, holding its locks and hiding its writes from the next caller. `rusqlite`
        // only rolls back for a dropped `Transaction` guard, so undo it here.
        if !conn.is_autocommit() {
            conn.execute_batch("ROLLBACK").map_err(|err| {
                StoreError::DatabaseError(format!("failed to reset the sqlite connection: {err}"))
            })?;
        }

        f(&mut conn)
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use miden_client::store::StoreError;
    use miden_client::testing::common::create_test_store_path;

    use super::SingleConnection;
    use crate::sql_error::SqlResultExt;

    fn open_test_connection() -> SingleConnection {
        let connection = SingleConnection::open(create_test_store_path()).unwrap();
        connection
            .interact(|conn| {
                conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);")
                    .into_store_error()
            })
            .unwrap();
        connection
    }

    fn row_count(connection: &SingleConnection) -> i64 {
        connection
            .interact(|conn| {
                conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
                    .into_store_error()
            })
            .unwrap()
    }

    #[test]
    fn connection_pragmas_are_applied() {
        let connection = SingleConnection::open(create_test_store_path()).unwrap();

        let (busy_timeout, foreign_keys) = connection
            .interact(|conn| {
                let busy_timeout: i32 = conn
                    .pragma_query_value(None, "busy_timeout", |row| row.get(0))
                    .into_store_error()?;
                let foreign_keys: i32 = conn
                    .pragma_query_value(None, "foreign_keys", |row| row.get(0))
                    .into_store_error()?;
                Ok((busy_timeout, foreign_keys))
            })
            .unwrap();

        assert_eq!(busy_timeout, 5_000);
        assert_eq!(foreign_keys, 1);
    }

    /// A bare `BEGIN` that is never committed hides its writes from the next query and holds the
    /// write lock, so the next query has to start from a rolled back connection.
    #[test]
    fn leaked_transaction_is_rolled_back() {
        let connection = open_test_connection();

        connection
            .interact(|conn| {
                conn.execute_batch("BEGIN; INSERT INTO t (id) VALUES (1);").into_store_error()
            })
            .unwrap();

        let autocommit = connection.interact(|conn| Ok(conn.is_autocommit())).unwrap();
        assert!(autocommit, "the leaked transaction was not rolled back");
        assert_eq!(row_count(&connection), 0);
    }

    /// A panic inside a query poisons the connection's mutex. The next query has to run on a
    /// replacement connection rather than fail forever.
    #[test]
    fn poisoned_connection_is_replaced() {
        let connection = open_test_connection();

        let panicked = catch_unwind(AssertUnwindSafe(|| {
            connection.interact(|conn| -> Result<(), StoreError> {
                conn.execute_batch("BEGIN; INSERT INTO t (id) VALUES (1);").into_store_error()?;
                panic!("poisoning the connection on purpose")
            })
        }));
        assert!(panicked.is_err());

        // The replacement connection never saw the transaction the panicked query left open.
        assert_eq!(row_count(&connection), 0);
        connection
            .interact(|conn| conn.execute("INSERT INTO t (id) VALUES (2)", []).into_store_error())
            .unwrap();
        assert_eq!(row_count(&connection), 1);
    }

    /// An error a query returns reaches the caller unchanged, and leaves the connection usable.
    #[test]
    fn query_error_is_returned() {
        let connection = open_test_connection();

        let err = connection
            .interact(|_| Err::<(), _>(StoreError::DatabaseError("forced failure".into())))
            .unwrap_err();
        assert!(err.to_string().contains("forced failure"), "unexpected error: {err}");

        assert_eq!(row_count(&connection), 0);
    }
}
