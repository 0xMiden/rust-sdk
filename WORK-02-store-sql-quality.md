# Store/SQL quality improvements ahead of the stable release

The SQLite store works, but several internals need hardening before we commit to a stable release — most urgently the migration story and anything that touches the schema, since those get much more expensive to change once real databases are in the wild.

## Motivation

- The store holds private notes and account state that are **not re-derivable from the chain** — a bricked or corrupted store can mean lost funds. Today any edit to `store.sql` hard-fails existing databases with no recovery path, and the file has been touched 83 times over its history.
- Schema mistakes become near-permanent after release: every fix must ship as a migration executed against every user database in the wild. Getting STRICT typing, keys, and indices right now is an order of magnitude cheaper than after the freeze.
- Two of the index gaps sit on the sync path and full-scan tables that grow with wallet age — performance quietly decays for exactly the oldest, most invested users.
- The store's design contract is single-accessor, but nothing enforces it: the pool defaults to ~CPU×4 connections and `Store` is `Arc<dyn Store>` with `&self` methods, so accidental concurrency (an embedding server, two CLI processes on one file) collides on SQLite locks and surfaces as opaque strings; callers can't distinguish "retry" from "corrupt".

## Migrations (blocking)

- The entire schema ships as a single migration (`MIGRATION_SCRIPTS` is a one-element array containing `store.sql`), and the hash check makes *any* edit to it — including a comment edit, since preprocessing only strips whitespace — hard-fail existing databases with `MigrationHashMismatch` and no recovery path.
- Freeze the current schema as `migrations/0001_init.sql`, adopt one numbered file per migration (never mutate old ones), and strip comments in the hash preprocessing.
- Document the policy: forward-only vs. down migrations, schema-version ↔ client-version compatibility.
- Give `MigrationHashMismatch` / `MissingMigrationsTable` proper `ErrorHint`s telling users what to do.
- The review artifact's `stab-2` design adds pieces to adopt here: Rust `Rewrite` steps for blob re-serialization migrations, automatic backup before migrating, and fixture-DB upgrade tests (zcash/schemerz + BDK pattern).

## Schema (blocking — must ride the same pre-freeze migration)

- Make all tables `STRICT` (only `migrations` and `settings` are today; everywhere else `UNSIGNED BIG INT`/`BOOLEAN` are just affinity hints) and normalize the type vocabulary (`BIGINT`, `UNSIGNED BIG INT`, `INT`, `INTEGER`, `BOOL`, `BOOLEAN` all appear).
- Resolve the `output_notes` script-normalization TODO while schema changes are cheap.
- Give `tags` a real primary key; drop the redundant `UNIQUE` on `historical_account_headers.account_commitment`.

## Indices (new indices are schema changes — same migration)

- `TransactionFilter::ExpiredBefore` full-scans `transactions`: `status_variant != x AND != y` can't use the index and `block_num` is unindexed. Rewrite as `status_variant = 0` and add a partial index on `transactions(block_num) WHERE status_variant = 0`. Runs every sync.
- `get_unspent_input_note_nullifiers` full-scans `input_notes`: `NOT IN` defeats `idx_input_notes_state`. Invert to a positive `IN` list of unspent states. Runs every sync.
- `get_input_note_by_offset` filters on unindexed `consumer_account_id`; add composite `(consumer_account_id, consumed_block_height, consumed_tx_order)` matching filter + `ORDER BY`.
- FK child columns are unindexed (SQLite never auto-indexes them), so `account_code` GC deletes scan three tables. Index `code_commitment` on `latest_account_headers`, `historical_account_headers`, `foreign_account_code`.
- Optional: complementary partial index for the `has_client_notes = 0` pruning delete (table is kept small by that pruning, so low priority).
- Add an `EXPLAIN QUERY PLAN` regression test that runs every canonical query (each `NoteFilter`/`TransactionFilter` variant, sync/prune statements) and asserts no `SCAN` on growing tables — so the next filter variant added can't silently regress to a table scan.

## Connection handling

- ~~The pool only sets `foreign_keys = ON`. Add `journal_mode = WAL`, `busy_timeout`, `synchronous = NORMAL` — without them, concurrent pooled access produces `SQLITE_BUSY` failures.~~ **Done (2026-07-23, `pool_manager.rs`):** `busy_timeout(5s)` → WAL → `synchronous = NORMAL`, in that order (timeout must precede the first WAL switch, which itself takes a lock). Rationale under the single-accessor contract: WAL + NORMAL are commit-cost wins for a *serial* workload (single append instead of the rollback journal's double write; fsync at checkpoints instead of every commit, with no corruption risk); busy_timeout is insurance for accidental cross-process overlap only.
- Replace the `eprintln!` permissions warning with tracing; store `database_filepath` as `PathBuf` (currently a lossy `String`).
- Make the single-accessor contract real: the pool currently defaults to ~CPU×4 connections while the design intends no concurrent access — set `max_size(1)` (accidental in-process concurrency then queues on the pool instead of colliding on SQLite locks) and document the contract on `SqliteStore`.

## Errors

- `sql_error.rs` collapses rusqlite into three stringly variants; callers can't tell "busy, retry" from "corrupt, stop". Add structured variants (busy, constraint violation, corruption). Pairs with the WAL work and with WORK-09's typed conflict errors.
- `set_setting`/`remove_setting` use `debug_assert_eq!(count, 1)` — panics in dev, silent in release; pick one behavior.

## Internal typing & organization (can trail the release)

- All ~70 row reads are positional (`row.get(0)`) beside hand-written SELECTs — column reorder is a silent corruption bug. Move to named access or per-table row mappers owning a canonical column-list const.
- Route the remaining stray `as i64` casts through the central `u64`↔`i64` helpers.
- Move `store.sql` into `migrations/`; split `db_management/utils.rs` (macros + migration engine + settings CRUD) into focused modules; `account/helpers.rs` → `account/rows.rs` as home for the row mappers.

## Priority

**Migrations**, **Schema**, and **Indices** must land before the release freeze since they're schema-affecting; connection/error work is small and high-value; typing/organization can trail. WORK-04's `CHECK` constraints and WORK-08's NTL tables ride the same pre-freeze migration.

Cross-links: WORK-04 (CHECK constraints), WORK-08 (NTL storage), WORK-09 (typed conflict errors). Overlaps with the review artifact: `perf-1` (WAL/busy_timeout — same fix), `stab-2` (migrations), `perf-6`/`perf-8` (store scale: keyset pagination, SMT-forest deep-clone-per-write) — the artifact owns the scale items; this file owns schema/index/typing correctness.
