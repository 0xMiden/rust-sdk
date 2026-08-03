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
files, and the client verifies on open that the schema it finds matches `PINNED_SCHEMA_HASHES` for
the version the database claims. That constant, not a replay of the current migration files, is the
definition of what each version's schema is, so editing a released migration is caught rather than
silently redefining the schema those databases were supposed to have. Unlike chain state, a store
holds private notes and account seeds that cannot be recovered from the network.

Upgrades are forward-only. There are no down migrations.

### Adding a migration

1. Add `src/migrations/000N_short_name.sql` with the next unused prefix. Never edit an existing
   file, including its comments.
2. Append `include_str!("../migrations/000N_short_name.sql")` to `MIGRATION_SCRIPTS` in
   `src/db_management/utils.rs`. Nothing scans the directory, so a file that is not listed here is
   never applied.
3. Append one entry to `PINNED_SCHEMA_HASHES` in the same file. Run
   `cargo test -p miden-client-sqlite-store --lib migration_schema_hashes_are_stable` and take the
   new hash from the failure output. Leave the existing entries alone. If they changed, the
   migration edited the schema an older version built.
4. Add a `CHANGELOG.md` entry under `[store]`.

`scripts/check-migrations.sh` runs in CI and fails a pull request that modifies, renames or deletes
a file that already exists on the base branch.

## License
This project is licensed under the MIT License. See the [LICENSE](../../LICENSE) file for details.
