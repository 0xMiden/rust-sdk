use std::string::{String, ToString};
use std::sync::{Arc, LazyLock, Mutex};
use std::vec::Vec;

use rusqlite::{Connection, Transaction};
use rusqlite_migration::{HookError, HookResult, M, Migrations, SchemaVersion};

use super::errors::SqliteStoreError;
use super::schema::SchemaHash;

// CLIENT MIGRATIONS
// ================================================================================================

/// The migrations that build the store schema, in the order they are applied.
pub(crate) const CLIENT_MIGRATIONS: [SqliteMigration; 1] =
    [SqliteMigration::new(include_str!("../migrations/0001_init.sql"))];

/// The migrations this client ships.
///
/// Building this replays every migration to derive the fingerprint each version produces, so it is
/// built once per process rather than once per store.
static CLIENT_MIGRATOR: LazyLock<SqliteMigrator> =
    LazyLock::new(|| SqliteMigrator::new(&CLIENT_MIGRATIONS));

// SQLITE MIGRATION
// ================================================================================================

/// Rust code a migration runs on top of its SQL, taking the transaction the migration is applied
/// in.
///
/// This is the `fn` form of [`rusqlite_migration::MigrationHook`], which keeps a migration
/// `Copy` and constructible in a `const`.
pub(crate) type MigrationHook = fn(&Transaction<'_>) -> HookResult;

/// Carries the rejection a verifying hook cannot return by value out to
/// [`SqliteMigrator::apply`].
type RejectionReport = Arc<Mutex<Option<SqliteStoreError>>>;

/// One schema version: the SQL that builds it and, optionally, the Rust code that moves data the
/// SQL cannot.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SqliteMigration {
    /// The SQL that takes the schema to this version.
    sql: &'static str,
    /// Rust code applied on top of the SQL.
    hook: Option<MigrationHook>,
}

impl SqliteMigration {
    // CONSTRUCTORS
    // --------------------------------------------------------------------------------------------

    /// Builds a migration that is applied by running `sql`.
    pub(crate) const fn new(sql: &'static str) -> Self {
        Self { sql, hook: None }
    }

    /// Builds a migration that runs `sql` and then `hook`.
    ///
    /// `hook` receives the transaction the whole upgrade commits at the end, which has two
    /// consequences. Returning an error rolls back every migration the upgrade is applying, not
    /// just this one. And the hook runs after this migration's foreign key check, so rows it writes
    /// itself are not covered by that check and it has to leave the database referentially whole on
    /// its own.
    #[cfg_attr(not(test), expect(dead_code, reason = "no shipped migration needs a hook yet"))]
    pub(crate) const fn with_hook(sql: &'static str, hook: MigrationHook) -> Self {
        Self { sql, hook: Some(hook) }
    }

    // CONVERSIONS
    // --------------------------------------------------------------------------------------------

    /// Returns this migration in the form the migration library applies.
    ///
    /// It runs `SQLite`'s foreign key check inside the transaction it is applied in, so a migration
    /// whose SQL orphans a row fails instead of committing.
    fn to_library_migration(self) -> M<'static> {
        match self.hook {
            Some(hook) => M::up_with_hook(self.sql, hook),
            None => M::up(self.sql),
        }
        .foreign_key_check()
    }
}

// SQLITE MIGRATOR
// ================================================================================================

/// An ordered set of migrations that build a store schema, paired with the fingerprint the schema
/// has once each of them has been applied.
#[derive(Debug)]
pub(crate) struct SqliteMigrator {
    /// The migrations in the order they are applied, the one for version `v` at index `v - 1`.
    migrations: Vec<SqliteMigration>,
    /// The fingerprint the schema has once a migration has been applied.
    expected_schema_hashes: Vec<SchemaHash>,
}

impl SqliteMigrator {
    // CONSTRUCTORS
    // --------------------------------------------------------------------------------------------

    /// Returns the migrations this client ships.
    pub(crate) fn client() -> &'static Self {
        &CLIENT_MIGRATOR
    }

    /// Builds the migrator for `migrations`, deriving the fingerprint each version produces by
    /// replaying them rather than by trusting a recorded value.
    pub(crate) fn new(migrations: &[SqliteMigration]) -> Self {
        let expected_schema_hashes = Self::replay_schema_hashes(migrations);

        Self::with_expected_hashes(migrations, expected_schema_hashes)
    }

    /// Pairs `migrations` with the fingerprint each of their versions builds.
    ///
    /// # Panics
    /// If there is not one fingerprint per migration, since every fingerprint is looked up by the
    /// version whose index it sits at.
    fn with_expected_hashes(
        migrations: &[SqliteMigration],
        expected_schema_hashes: Vec<SchemaHash>,
    ) -> Self {
        assert_eq!(
            migrations.len(),
            expected_schema_hashes.len(),
            "every migration needs the fingerprint of the schema it builds"
        );

        Self {
            migrations: migrations.to_vec(),
            expected_schema_hashes,
        }
    }

    // ACCESSORS
    // --------------------------------------------------------------------------------------------

    /// Returns the highest schema version these migrations build.
    pub(crate) fn latest_version(&self) -> usize {
        self.expected_schema_hashes.len()
    }

    /// Returns the fingerprint each version is defined to build, version `v` at index `v - 1`.
    #[cfg(test)]
    pub(crate) fn expected_schema_hashes(&self) -> &[SchemaHash] {
        &self.expected_schema_hashes
    }

    // MIGRATION
    // --------------------------------------------------------------------------------------------

    /// Returns whether `conn` holds a schema that is behind the latest version.
    #[cfg(test)]
    pub(crate) fn has_pending(&self, conn: &Connection) -> Result<bool, SqliteStoreError> {
        match Self::library_migrations(&self.migrations).current_version(conn)? {
            SchemaVersion::Inside(ver) => Ok(ver.get() < self.latest_version()),
            SchemaVersion::NoneSet | SchemaVersion::Outside(_) => Ok(false),
        }
    }

    /// Brings `conn` up to the latest schema version, creating the schema if it is empty.
    pub(crate) fn apply(&self, conn: &mut Connection) -> Result<(), SqliteStoreError> {
        let rejection = RejectionReport::default();
        let migrations = self.verified_library_migrations(&rejection);

        match migrations.current_version(conn)? {
            SchemaVersion::NoneSet => {
                if !Self::is_empty_database(conn)? {
                    return Err(SqliteStoreError::NotAClientStore);
                }
            },
            SchemaVersion::Inside(ver) => {
                if let Some((expected, actual)) = self.schema_mismatch_at(conn, ver.get())? {
                    return Err(SqliteStoreError::SchemaDrift {
                        version: ver.get(),
                        expected,
                        actual,
                    });
                }
            },
            SchemaVersion::Outside(ver) => {
                return Err(SqliteStoreError::SchemaTooNew {
                    found: ver.get(),
                    supported: self.latest_version(),
                });
            },
        }

        migrations.to_latest(conn).map_err(|err| {
            take_rejection(&rejection).unwrap_or_else(|| SqliteStoreError::from(err))
        })
    }

    /// Applies the migrations up to `version`, to build a database that is behind the latest
    /// version.
    #[cfg(test)]
    pub(crate) fn migrate_to_version(
        &self,
        conn: &mut Connection,
        version: usize,
    ) -> Result<(), SqliteStoreError> {
        Self::library_migrations(&self.migrations)
            .to_version(conn, version)
            .map_err(Into::into)
    }

    // HELPERS
    // --------------------------------------------------------------------------------------------

    /// Builds `migrations` in the form the migration library applies.
    fn library_migrations(migrations: &[SqliteMigration]) -> Migrations<'static> {
        Migrations::new(
            migrations.iter().copied().map(SqliteMigration::to_library_migration).collect(),
        )
    }

    /// Builds these migrations in the form the migration library applies, each one followed by the
    /// check that the version it just built has the fingerprint it is defined to build.
    fn verified_library_migrations(&self, rejection: &RejectionReport) -> Migrations<'static> {
        let migrations = self
            .migrations
            .iter()
            .zip(&self.expected_schema_hashes)
            .enumerate()
            .map(|(index, (migration, &expected))| {
                let version = index + 1;
                let hook = migration.hook;
                let rejection = Arc::clone(rejection);

                M::up_with_hook(migration.sql, move |tx: &Transaction<'_>| {
                    if let Some(hook) = hook {
                        hook(tx)?;
                    }

                    let actual = SchemaHash::of(tx).map_err(|err| hook_error(&err))?;
                    if actual == expected {
                        return Ok(());
                    }

                    let mismatch = SqliteStoreError::MigratedSchemaMismatch {
                        version,
                        expected: expected.to_string(),
                        actual: actual.to_string(),
                    };
                    let message = mismatch.to_string();
                    *rejection.lock().expect("rejection lock not poisoned") = Some(mismatch);

                    Err(HookError::Hook(message))
                })
                .foreign_key_check()
            })
            .collect();

        Migrations::new(migrations)
    }

    /// Computes the fingerprint each version produces by replaying `migrations` on an in-memory
    /// database.
    fn replay_schema_hashes(migrations: &[SqliteMigration]) -> Vec<SchemaHash> {
        let library_migrations = Self::library_migrations(migrations);
        let mut conn =
            Connection::open_in_memory().expect("in-memory database creation should not fail");
        conn.pragma_update(None, "foreign_keys", "ON")
            .expect("enabling foreign keys on the reference database should not fail");

        (1..=migrations.len())
            .map(|version| {
                library_migrations
                    .to_version(&mut conn, version)
                    .expect("replaying a migration on the reference database should not fail");
                SchemaHash::of(&conn).expect("hashing the reference schema should not fail")
            })
            .collect()
    }

    /// Returns the fingerprint version `version` is defined to build and the one `conn` holds,
    /// rendered for reporting, when the two differ.
    fn schema_mismatch_at(
        &self,
        conn: &Connection,
        version: usize,
    ) -> Result<Option<(String, String)>, SqliteStoreError> {
        let expected = self.expected_schema_hashes[version - 1];
        let actual = SchemaHash::of(conn)?;

        Ok((actual != expected).then(|| (expected.to_string(), actual.to_string())))
    }

    /// Returns whether the database holds no objects of its own.
    fn is_empty_database(conn: &Connection) -> Result<bool, SqliteStoreError> {
        let objects: u32 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
            [],
            |row| row.get(0),
        )?;

        Ok(objects == 0)
    }
}

// HELPERS
// ================================================================================================

/// Renders a store error for the migration library, whose hooks can only fail with text.
fn hook_error(err: &SqliteStoreError) -> HookError {
    HookError::Hook(err.to_string())
}

/// Takes the rejection a verifying hook left behind, if the failure came from one.
fn take_rejection(rejection: &RejectionReport) -> Option<SqliteStoreError> {
    rejection.lock().expect("rejection lock not poisoned").take()
}

// TESTS
// ================================================================================================

#[cfg(test)]
pub(crate) mod tests {
    use rusqlite::Connection;

    use super::{CLIENT_MIGRATIONS, SqliteMigration, SqliteMigrator};
    use crate::db_management::errors::SqliteStoreError;

    const PINNED_SCHEMA_HASHES: [&str; CLIENT_MIGRATIONS.len()] =
        ["0xd02b6d09378d300dd92bfc44a2ce15f5852d76eec336b204f87e0d3a916cfa08"];

    // FIXTURES
    // --------------------------------------------------------------------------------------------

    /// The migrations this client ships with one more appended that drops `input_notes`, recorded
    /// as building the schema of the version before it.
    ///
    /// Applying it drops the table and is then rejected by the fingerprint check, which is the
    /// shape every failure the rollback has to undo takes.
    pub(crate) fn damaging_migration() -> SqliteMigrator {
        let mut migrations = CLIENT_MIGRATIONS.to_vec();
        migrations.push(SqliteMigration::new("DROP TABLE input_notes;"));

        let mut expected_schema_hashes = SqliteMigrator::client().expected_schema_hashes.clone();
        expected_schema_hashes
            .push(*expected_schema_hashes.last().expect("the client ships at least one migration"));

        SqliteMigrator::with_expected_hashes(&migrations, expected_schema_hashes)
    }

    // TESTS
    // --------------------------------------------------------------------------------------------

    #[test]
    fn a_rejected_migration_is_rolled_back() {
        let mut conn = Connection::open_in_memory().unwrap();
        SqliteMigrator::client().apply(&mut conn).unwrap();

        let damaging = damaging_migration();
        let err = damaging.apply(&mut conn).unwrap_err();
        let SqliteStoreError::MigratedSchemaMismatch { version, expected, actual } = err else {
            panic!(
                "a migration that builds the wrong schema should be reported as a mismatch, got {err:?}"
            );
        };
        assert_eq!(version, damaging.latest_version());
        assert_ne!(expected, actual);

        // The rejection happens while the upgrade is still uncommitted, so the drop went back with
        // it.
        let tables: u32 = conn
            .query_row("SELECT COUNT(*) FROM sqlite_schema WHERE name = 'input_notes'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(tables, 1, "the dropped table should have come back");
        let version: usize = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(
            version,
            SqliteMigrator::client().latest_version(),
            "the version should not have advanced"
        );
    }

    #[test]
    fn migration_schema_hashes_are_stable() {
        let replayed = SqliteMigrator::client()
            .expected_schema_hashes()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let pinned = PINNED_SCHEMA_HASHES.map(str::to_string).to_vec();

        assert_eq!(
            replayed, pinned,
            "a released migration builds a different schema than it did when it was pinned. \
             Append a new migration instead of editing an existing one. If this is a new \
             migration, append its hash rather than rewriting the entries before it."
        );
    }
}
