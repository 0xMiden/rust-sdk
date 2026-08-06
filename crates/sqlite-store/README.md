# SQLite Store

SQLite-backed `Store` implementation for the Miden client. This crate provides a production‑ready
persistence layer for std environments using SQLite (via `rusqlite`) with a small in‑memory
MerkleStore cache for fast proof queries.

- Persists accounts, notes, transactions, block headers, and MMR nodes
- Atomic updates on transaction and state sync paths
- Connection pooling (Deadpool) and bundled SQLite for reproducible builds

## Quick Start

Add to `Cargo.toml`:

```toml
miden-client              = { version = "0.13" }
miden-client-sqlite-store = { version = "0.13" }
```

## Migrations

The schema is built by replaying the migrations listed in `MIGRATION_SCRIPTS`
(`src/db_management/utils.rs`), which include the files under `src/migrations/` in order. A file's
four-digit prefix is its schema version, which is the value SQLite records in `PRAGMA user_version`.

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
2. Append `include_str!("../migrations/000N_short_name.sql")` to `MIGRATION_SCRIPTS` in
   `src/db_management/utils.rs`. Nothing scans the directory, so a file that is not listed here is
   never applied.
3. Append one entry to `PINNED_SCHEMA_HASHES` in that file's test module. Run
   `cargo test -p miden-client-sqlite-store --lib migration_schema_hashes_are_stable` and take the
   new hash from the failure output. Leave the existing entries alone. If they changed, the
   migration edited the schema an older version built.
4. Add a `CHANGELOG.md` entry under `[store]`.

`scripts/check-migrations.sh` runs in CI and fails a pull request that modifies, renames or deletes
a file that already exists on the base branch.

### Migrations that transform data

Some upgrades cannot be expressed in SQL. The store holds serialized protocol objects as blobs, so
a change to how an account, note or transaction is encoded has to be applied by decoding each row
with the old type and re-encoding it with the new one. SQLite has no way to do that.

`rusqlite_migration` covers this with `up_with_hook`, where the hook is a Rust
closure taking the migration's `&Transaction`. Per migration the library runs the SQL, then the
foreign key check, then the hook, and all of it is inside the transaction the whole upgrade commits
at the end, so a hook that returns an error rolls back the migration exactly like failing SQL does.

## License
This project is licensed under the MIT License. See the [LICENSE](../../LICENSE) file for details.
