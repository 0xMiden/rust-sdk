use std::string::{String, ToString};
use std::sync::LazyLock;
use std::vec::Vec;

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations, SchemaVersion};

use super::errors::SqliteStoreError;
use super::schema::SchemaHash;

// CLIENT MIGRATIONS
// ================================================================================================

/// The migrations that build the store schema, in the order they are applied.
pub(crate) const MIGRATION_SCRIPTS: [&str; 1] = [include_str!("../migrations/0001_init.sql")];

/// The migration set this client ships.
///
/// Building it replays every migration to derive the fingerprint each version produces, so it is
/// built once per process rather than once per store.
static CLIENT_MIGRATION: LazyLock<SqliteMigration> =
    LazyLock::new(|| SqliteMigration::from_scripts(&MIGRATION_SCRIPTS));

// SQLITE MIGRATION
// ================================================================================================

/// An ordered set of migrations that build a store schema, paired with the fingerprint the schema
/// has once each of them has been applied.
#[derive(Debug)]
pub(crate) struct SqliteMigration {
    /// Applies the migration scripts, recording the version reached in `PRAGMA user_version`.
    migrations: Migrations<'static>,
    /// The fingerprint the schema has once a migration has been applied.
    expected_schema_hashes: Vec<SchemaHash>,
}

impl SqliteMigration {
    // CONSTRUCTORS
    // --------------------------------------------------------------------------------------------

    /// Returns the migration set this client ships.
    pub(crate) fn client() -> &'static Self {
        &CLIENT_MIGRATION
    }

    /// Builds the migration set `scripts` describes, deriving the fingerprint each version produces
    /// by replaying the scripts rather than by trusting a recorded value.
    pub(crate) fn from_scripts(scripts: &[&'static str]) -> Self {
        let expected_schema_hashes = Self::replay_schema_hashes(scripts);

        Self::new(scripts, expected_schema_hashes)
    }

    /// Pairs the migrations `scripts` describes with the fingerprint each of their versions builds.
    ///
    /// # Panics
    /// If there is not one fingerprint per migration, since every fingerprint is looked up by the
    /// version whose index it sits at.
    fn new(scripts: &[&'static str], expected_schema_hashes: Vec<SchemaHash>) -> Self {
        assert_eq!(
            scripts.len(),
            expected_schema_hashes.len(),
            "every migration needs the fingerprint of the schema it builds"
        );

        Self {
            migrations: Self::migrations_from(scripts),
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
    pub(crate) fn has_pending(&self, conn: &Connection) -> Result<bool, SqliteStoreError> {
        match self.migrations.current_version(conn)? {
            SchemaVersion::Inside(ver) => Ok(ver.get() < self.latest_version()),
            SchemaVersion::NoneSet | SchemaVersion::Outside(_) => Ok(false),
        }
    }

    /// Brings `conn` up to the latest schema version, creating the schema if it is empty.
    pub(crate) fn apply(&self, conn: &mut Connection) -> Result<(), SqliteStoreError> {
        match self.migrations.current_version(conn)? {
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

        self.migrations.to_latest(conn)?;

        let version = self.latest_version();
        if let Some((expected, actual)) = self.schema_mismatch_at(conn, version)? {
            return Err(SqliteStoreError::MigratedSchemaMismatch { version, expected, actual });
        }

        Ok(())
    }

    /// Applies the migrations up to `version`, to build a database that is behind the latest
    /// version.
    #[cfg(test)]
    pub(crate) fn migrate_to_version(
        &self,
        conn: &mut Connection,
        version: usize,
    ) -> Result<(), SqliteStoreError> {
        self.migrations.to_version(conn, version).map_err(Into::into)
    }

    // HELPERS
    // --------------------------------------------------------------------------------------------

    /// Builds the migrations `scripts` describes.
    ///
    /// Each one runs `SQLite`'s foreign key check inside the transaction it is applied in, so a
    /// migration that orphans a row fails instead of committing.
    fn migrations_from(scripts: &[&'static str]) -> Migrations<'static> {
        Migrations::new(scripts.iter().map(|&script| M::up(script).foreign_key_check()).collect())
    }

    /// Computes the fingerprint each version produces by replaying `scripts` on an in-memory
    /// database.
    fn replay_schema_hashes(scripts: &[&'static str]) -> Vec<SchemaHash> {
        let migrations = Self::migrations_from(scripts);
        let mut conn =
            Connection::open_in_memory().expect("in-memory database creation should not fail");
        conn.pragma_update(None, "foreign_keys", "ON")
            .expect("enabling foreign keys on the reference database should not fail");

        (1..=scripts.len())
            .map(|version| {
                migrations
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

// TESTS
// ================================================================================================

#[cfg(test)]
pub(crate) mod tests {
    use rusqlite::Connection;

    use super::{MIGRATION_SCRIPTS, SqliteMigration};
    use crate::db_management::errors::SqliteStoreError;

    const PINNED_SCHEMA_HASHES: [&str; MIGRATION_SCRIPTS.len()] =
        ["0x749fba4988cae911b43dd2a3efef634ce5f514515ae26687f791fb17612c5b7a"];

    // FIXTURES
    // --------------------------------------------------------------------------------------------

    /// The migrations this client ships with one more appended that drops `input_notes`, recorded
    /// as building the schema of the version before it.
    ///
    /// Migrating commits the dropped table and only then fails verification, which is the shape
    /// every failure the pre-migration backup exists for takes. A later migration that made
    /// `input_notes` undroppable would roll the appended script back instead, which the tests
    /// catch by asserting the rejection they expect.
    pub(crate) fn damaging_migration() -> SqliteMigration {
        let mut scripts = MIGRATION_SCRIPTS.to_vec();
        scripts.push("DROP TABLE input_notes;");

        let mut expected_schema_hashes = SqliteMigration::client().expected_schema_hashes.clone();
        expected_schema_hashes
            .push(*expected_schema_hashes.last().expect("the client ships at least one migration"));

        SqliteMigration::new(&scripts, expected_schema_hashes)
    }

    // TESTS
    // --------------------------------------------------------------------------------------------

    #[test]
    fn a_rejected_migration_has_already_changed_the_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        SqliteMigration::client().apply(&mut conn).unwrap();

        let damaging = damaging_migration();
        let err = damaging.apply(&mut conn).unwrap_err();
        let SqliteStoreError::MigratedSchemaMismatch { version, expected, actual } = err else {
            panic!(
                "a migration that builds the wrong schema should be reported as a mismatch, got {err:?}"
            );
        };
        assert_eq!(version, damaging.latest_version());
        assert_ne!(expected, actual);

        // The rejection comes after the migration was committed, which is why the store has to be
        // copied before it is migrated: this schema cannot be undone from here.
        let tables: u32 = conn
            .query_row("SELECT COUNT(*) FROM sqlite_schema WHERE name = 'input_notes'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(tables, 0, "the dropped table should be gone");
        let version: usize = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, damaging.latest_version(), "the version should have advanced");
    }

    #[test]
    fn migration_schema_hashes_are_stable() {
        let replayed = SqliteMigration::client()
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
