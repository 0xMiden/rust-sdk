# SQLite Store

SQLite-backed `Store` implementation for the Miden client. This crate provides a production‑ready
persistence layer for std environments using SQLite (via `rusqlite`).

- Persists accounts, notes, transactions, block headers, MMR nodes, and the account SMT forest
- Atomic updates on transaction and state sync paths
- WAL journaling and bundled SQLite for reproducible builds

## Quick Start

Add to `Cargo.toml`:

```toml
miden-client              = { version = "0.16.0-alpha.1" }
miden-client-sqlite-store = { version = "0.16.0-alpha.1" }
```

## Migrations

The schema is built by replaying the migrations listed in `CLIENT_MIGRATIONS`
(`src/db_management/migration.rs`), which include the files under `src/migrations/` in order. A
file's four-digit prefix is its schema version, which is the value SQLite records in
`PRAGMA user_version`.

Migrations are **append-only**. Every store on a user's disk was built by replaying these exact
files. On open the client replays the migrations against an in-memory database to derive the
fingerprint each version should have, and verifies that the schema it finds on disk matches the one
for the version the database claims. A store that was altered outside the migrations is rejected
rather than migrated further. Unlike chain state, a store holds private notes and account seeds
that cannot be recovered from the network.

Upgrades are forward-only. There are no down migrations.

### Adding a migration

1. Add `src/migrations/000N_short_name.sql` with the next unused prefix. Never edit an existing
   file, including its comments.
2. Append `SqliteMigration::new(include_str!("../migrations/000N_short_name.sql"))` to
   `CLIENT_MIGRATIONS` in `src/db_management/migration.rs`. Nothing scans the directory, so a file
   that is not listed here is never applied. Use `SqliteMigration::with_hook` instead if the upgrade
   also needs Rust, as described below.
3. Append one entry to `PINNED_SCHEMA_HASHES` in that file's test module. Run
   `cargo test -p miden-client-sqlite-store --lib migration_schema_hashes_are_stable` and take the
   new hash from the failure output. Leave the existing entries alone. If they changed, the
   migration edited the schema an older version built.
4. Add a `CHANGELOG.md` entry under `[store]`.

`scripts/check-migrations.sh` runs in CI and fails a pull request that modifies, renames or deletes
a file that already exists on the base branch. The `no migration check` label skips it, for schema
changes that no released client can encounter yet: in that case edit the existing migration and
update its pinned schema hash instead of adding a new file.

### Migrations that transform data

Some upgrades cannot be expressed in SQL. The store holds serialized protocol objects as blobs, so
a change to how an account, note or transaction is encoded has to be applied by decoding each row
with the old type and re-encoding it with the new one. SQLite has no way to do that.

Such a migration pairs its `.sql` file with a hook, a `fn(&Transaction<'_>) -> HookResult`:

```rust
SqliteMigration::with_hook(include_str!("../migrations/000N_short_name.sql"), reencode_rows)
```

Per migration the library runs the SQL, then the foreign key check, then the hook. Three
consequences are worth knowing before writing one:

- Every pending migration and every hook run inside the single transaction the upgrade commits at
  the end, so a hook returning an error rolls back the whole upgrade, not just its own version.
- A hook runs *after* its migration's foreign key check, so rows it writes itself are not covered by
  that check. It has to leave the database referentially whole on its own.
- A hook also runs while the fingerprint of each version is being derived, against an empty
  database, so it has to tolerate finding no rows.

The fingerprint check each version ends with is itself such a hook, wrapped around the migration's
own one, which is what lets a rejected upgrade roll back. A migration's hook therefore always runs
before its version is fingerprinted.

Schema a hook creates is part of the version's fingerprint, exactly like schema its SQL creates.
Data a hook writes is not: the fingerprint reads `sqlite_schema` only. A released hook is therefore
as append-only as the SQL beside it, but nothing enforces that yet, since
`scripts/check-migrations.sh` guards the `src/migrations/` directory rather than the Rust code.

## License
This project is licensed under the MIT License. See the [LICENSE](../../LICENSE) file for details.
