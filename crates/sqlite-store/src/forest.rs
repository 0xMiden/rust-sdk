//! `SQLite` row storage for the account SMT forest, scoped to a rusqlite [`Transaction`].
//!
//! [`SqliteForestRows`] implements [`ForestRowStore`], so the shared
//! [`RowForestBackend`] provides the actual [`LargeSmtForest`] backend logic; this module only
//! translates row reads and writes to SQL. The store borrows the store operation's transaction,
//! so every forest write commits or rolls back together with the account-table writes performed
//! in the same transaction. A `LargeSmtForest<SqliteForestBackend>` is constructed per store
//! operation (forest construction only reads tree metadata) and dropped before the transaction
//! is committed. Rolling back the transaction discards all forest changes; there is no separate
//! in-memory state to reconcile.
//!
//! [`LargeSmtForest`]: miden_protocol::crypto::merkle::smt::LargeSmtForest

use std::fmt;

use miden_client::store::StoreError;
use miden_client::store::forest_backend::{
    ForestEntryRow,
    ForestRowStore,
    ForestTreeMeta,
    RowForestBackend,
};
use miden_client::utils::{Deserializable, Serializable};
use miden_protocol::Word;
use miden_protocol::crypto::merkle::smt::{BackendError, LineageId, TreeWithRoot, VersionId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::sql_error::SqlResultExt;
use crate::{column_value_as_u64, u64_to_value};

type Result<T> = core::result::Result<T, BackendError>;

/// A [`LargeSmtForest`] backend over SQL rows in a rusqlite transaction.
///
/// [`LargeSmtForest`]: miden_protocol::crypto::merkle::smt::LargeSmtForest
pub(crate) type SqliteForestBackend<'a, 'conn> = RowForestBackend<SqliteForestRows<'a, 'conn>>;

/// An account SMT forest scoped to a rusqlite transaction.
pub(crate) type ScopedAccountForest<'a, 'conn> =
    miden_client::store::AccountSmtForest<SqliteForestBackend<'a, 'conn>>;

/// Creates a forest backend over the provided transaction.
pub(crate) fn forest_backend<'a, 'conn>(
    tx: &'a Transaction<'conn>,
) -> SqliteForestBackend<'a, 'conn> {
    RowForestBackend::new(SqliteForestRows::new(tx))
}

// FOREST REVISION
// ================================================================================================

/// Allocates the next database-wide forest revision.
///
/// Every mutating forest operation gets a fresh revision from this counter, allocated inside the
/// same transaction as the mutation itself. Committed revisions increase strictly and are never
/// reused (an allocation whose transaction rolls back may be handed out again, which is safe
/// because the mutation that used it rolled back with it). Rollbacks of account state are
/// represented as new forward mutations at a newer revision, not by rewinding versions.
pub(crate) fn allocate_forest_revision(tx: &Transaction<'_>) -> rusqlite::Result<VersionId> {
    tx.query_row(
        "UPDATE forest_revision SET next_version = next_version + 1 WHERE id = 0 \
         RETURNING next_version - 1",
        [],
        |row| column_value_as_u64(row, 0),
    )
}

// ROW STORE
// ================================================================================================

/// Row storage for the forest tables, borrowing a rusqlite transaction.
#[derive(Clone, Copy)]
pub(crate) struct SqliteForestRows<'a, 'conn> {
    tx: &'a Transaction<'conn>,
}

impl<'a, 'conn> SqliteForestRows<'a, 'conn> {
    pub(crate) fn new(tx: &'a Transaction<'conn>) -> Self {
        Self { tx }
    }
}

impl fmt::Debug for SqliteForestRows<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteForestRows").finish_non_exhaustive()
    }
}

// SQL HELPERS
// ================================================================================================

fn internal<E: std::error::Error + Send + Sync + 'static>(e: E) -> BackendError {
    BackendError::Internal(Box::new(e))
}

fn word_from_blob(blob: &[u8]) -> Result<Word> {
    Word::read_from_bytes(blob)
        .map_err(|e| BackendError::CorruptedData(format!("malformed word in forest table: {e}")))
}

fn tree_meta(conn: &Connection, lineage: LineageId) -> Result<Option<(VersionId, Word, usize)>> {
    conn.query_row(
        "SELECT version, root, entry_count FROM forest_trees WHERE lineage = ?1",
        params![lineage.as_bytes().as_slice()],
        |row| {
            Ok((
                column_value_as_u64(row, 0)?,
                row.get::<_, Vec<u8>>(1)?,
                column_value_as_u64(row, 2)?,
            ))
        },
    )
    .optional()
    .map_err(internal)?
    .map(|(version, root_blob, count)| {
        let count = usize::try_from(count)
            .map_err(|_| BackendError::CorruptedData("entry count out of range".into()))?;
        Ok((version, word_from_blob(&root_blob)?, count))
    })
    .transpose()
}

// STORE-SIDE HELPERS
// ================================================================================================

/// Returns the latest stored root of a lineage, or `None` if the lineage is unknown.
///
/// Store-side helper for consistency checks against roots recorded in the account tables.
pub(crate) fn forest_lineage_root(
    conn: &Connection,
    lineage: LineageId,
) -> core::result::Result<Option<Word>, StoreError> {
    match tree_meta(conn, lineage) {
        Ok(meta) => Ok(meta.map(|(_, root, _)| root)),
        Err(e) => Err(StoreError::DatabaseError(e.to_string())),
    }
}

/// Returns the SMT keys currently stored for a lineage.
///
/// Store-side helper for building reset and reconciliation update batches (removing keys that
/// are no longer part of a lineage's target state).
pub(crate) fn forest_entry_keys(
    conn: &Connection,
    lineage: LineageId,
) -> core::result::Result<Vec<Word>, StoreError> {
    let mut stmt = conn
        .prepare_cached("SELECT key FROM forest_entries WHERE lineage = ?1")
        .into_store_error()?;
    let rows = stmt
        .query_map(params![lineage.as_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))
        .into_store_error()?;

    let mut keys = Vec::new();
    for row in rows {
        let blob = row.into_store_error()?;
        keys.push(Word::read_from_bytes(&blob)?);
    }
    Ok(keys)
}

// ROW STORE IMPLEMENTATION
// ================================================================================================

impl ForestRowStore for SqliteForestRows<'_, '_> {
    fn tree_meta(&self, lineage: LineageId) -> Result<Option<ForestTreeMeta>> {
        Ok(tree_meta(self.tx, lineage)?.map(|(version, root, entry_count)| ForestTreeMeta {
            version,
            root,
            entry_count,
        }))
    }

    fn trees(&self) -> Result<Vec<TreeWithRoot>> {
        let mut stmt = self
            .tx
            .prepare_cached("SELECT lineage, version, root FROM forest_trees")
            .map_err(internal)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    column_value_as_u64(row, 1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(internal)?;

        let mut trees = Vec::new();
        for row in rows {
            let (lineage_blob, version, root_blob) = row.map_err(internal)?;
            let lineage_bytes: [u8; 32] = lineage_blob.try_into().map_err(|_| {
                BackendError::CorruptedData("malformed lineage id in forest table".into())
            })?;
            trees.push(TreeWithRoot::new(
                LineageId::new(lineage_bytes),
                version,
                word_from_blob(&root_blob)?,
            ));
        }
        Ok(trees)
    }

    fn entry_value(&self, lineage: LineageId, key: Word) -> Result<Option<Word>> {
        self.tx
            .query_row(
                "SELECT value FROM forest_entries WHERE lineage = ?1 AND key = ?2",
                params![lineage.as_bytes().as_slice(), key.to_bytes()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(internal)?
            .map(|blob| word_from_blob(&blob))
            .transpose()
    }

    fn leaf_entries(&self, lineage: LineageId, position: u64) -> Result<Vec<(Word, Word)>> {
        let mut stmt = self
            .tx
            .prepare_cached(
                "SELECT key, value FROM forest_entries WHERE lineage = ?1 AND leaf_position = ?2",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![lineage.as_bytes().as_slice(), u64_to_value(position)], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(internal)?;

        let mut entries = Vec::new();
        for row in rows {
            let (key_blob, value_blob) = row.map_err(internal)?;
            entries.push((word_from_blob(&key_blob)?, word_from_blob(&value_blob)?));
        }
        Ok(entries)
    }

    fn for_each_entry(
        &self,
        lineage: LineageId,
        f: &mut dyn FnMut(ForestEntryRow) -> Result<()>,
    ) -> Result<()> {
        let mut stmt = self
            .tx
            .prepare_cached(
                "SELECT key, value, leaf_position FROM forest_entries WHERE lineage = ?1",
            )
            .map_err(internal)?;
        let rows = stmt
            .query_map(params![lineage.as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    column_value_as_u64(row, 2)?,
                ))
            })
            .map_err(internal)?;

        for row in rows {
            let (key_blob, value_blob, leaf_position) = row.map_err(internal)?;
            f(ForestEntryRow {
                key: word_from_blob(&key_blob)?,
                value: word_from_blob(&value_blob)?,
                leaf_position,
            })?;
        }
        Ok(())
    }

    fn subtree_blob(
        &self,
        lineage: LineageId,
        depth: u8,
        position: u64,
    ) -> Result<Option<Vec<u8>>> {
        let mut stmt = self
            .tx
            .prepare_cached(
                "SELECT data FROM forest_subtrees \
                 WHERE lineage = ?1 AND depth = ?2 AND position = ?3",
            )
            .map_err(internal)?;
        stmt.query_row(
            params![lineage.as_bytes().as_slice(), depth, u64_to_value(position)],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(internal)
    }

    fn upsert_entry(
        &mut self,
        lineage: LineageId,
        key: Word,
        value: Word,
        leaf_position: u64,
    ) -> Result<()> {
        let mut stmt = self
            .tx
            .prepare_cached(
                "INSERT INTO forest_entries (lineage, key, value, leaf_position)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(lineage, key) DO UPDATE SET value = excluded.value",
            )
            .map_err(internal)?;
        stmt.execute(params![
            lineage.as_bytes().as_slice(),
            key.to_bytes(),
            value.to_bytes(),
            u64_to_value(leaf_position)
        ])
        .map_err(internal)?;
        Ok(())
    }

    fn delete_entry(&mut self, lineage: LineageId, key: Word) -> Result<()> {
        let mut stmt = self
            .tx
            .prepare_cached("DELETE FROM forest_entries WHERE lineage = ?1 AND key = ?2")
            .map_err(internal)?;
        stmt.execute(params![lineage.as_bytes().as_slice(), key.to_bytes()])
            .map_err(internal)?;
        Ok(())
    }

    fn upsert_subtree(
        &mut self,
        lineage: LineageId,
        depth: u8,
        position: u64,
        blob: Vec<u8>,
    ) -> Result<()> {
        let mut stmt = self
            .tx
            .prepare_cached(
                "INSERT INTO forest_subtrees (lineage, depth, position, data)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(lineage, depth, position) DO UPDATE SET data = excluded.data",
            )
            .map_err(internal)?;
        stmt.execute(params![lineage.as_bytes().as_slice(), depth, u64_to_value(position), blob])
            .map_err(internal)?;
        Ok(())
    }

    fn delete_subtree(&mut self, lineage: LineageId, depth: u8, position: u64) -> Result<()> {
        let mut stmt = self
            .tx
            .prepare_cached(
                "DELETE FROM forest_subtrees WHERE lineage = ?1 AND depth = ?2 AND position = ?3",
            )
            .map_err(internal)?;
        stmt.execute(params![lineage.as_bytes().as_slice(), depth, u64_to_value(position)])
            .map_err(internal)?;
        Ok(())
    }

    fn upsert_tree_meta(&mut self, lineage: LineageId, meta: ForestTreeMeta) -> Result<()> {
        self.tx
            .execute(
                "INSERT INTO forest_trees (lineage, version, root, entry_count) \
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(lineage) DO UPDATE SET
                     version = excluded.version,
                     root = excluded.root,
                     entry_count = excluded.entry_count",
                params![
                    lineage.as_bytes().as_slice(),
                    u64_to_value(meta.version),
                    meta.root.to_bytes(),
                    u64_to_value(meta.entry_count as u64)
                ],
            )
            .map_err(internal)?;
        Ok(())
    }

    /// Runs the writes inside a SQL savepoint, so a mid-application error does not leave partial
    /// writes visible even to callers that catch the error and keep using the transaction.
    fn write_atomically<T>(&mut self, writes: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.tx.execute_batch("SAVEPOINT forest_apply").map_err(internal)?;
        let result = writes(self);
        match &result {
            Ok(_) => {
                self.tx.execute_batch("RELEASE forest_apply").map_err(internal)?;
            },
            Err(_) => {
                // Best effort: an error here would mask the original failure, and the enclosing
                // transaction is rolled back by the store in that case anyway.
                let _ = self.tx.execute_batch("ROLLBACK TO forest_apply; RELEASE forest_apply");
            },
        }
        result
    }
}
// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::crypto::merkle::smt::{
        BackendReader,
        InnerNode,
        LargeSmtForest,
        SMT_DEPTH,
        Smt,
        SmtForestUpdateBatch,
        Subtree,
        TreeId,
    };
    use miden_protocol::crypto::merkle::{EmptySubtreeRoots, NodeIndex};
    use miden_protocol::{EMPTY_WORD, Felt, ONE, ZERO};

    use super::*;
    use crate::db_management::utils::apply_migrations;

    fn setup_conn() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations(&mut conn).unwrap();
        conn
    }

    fn lid(n: u8) -> LineageId {
        LineageId::new([n; 32])
    }

    fn w(n: u64) -> Word {
        Word::from([Felt::new(n).unwrap(), ZERO, ZERO, ONE])
    }

    fn batch(lineage: LineageId, pairs: &[(Word, Word)]) -> SmtForestUpdateBatch {
        let mut b = SmtForestUpdateBatch::empty();
        for (k, v) in pairs {
            b.operations(lineage).add_insert(*k, *v);
        }
        b
    }

    #[test]
    fn revision_allocator_is_monotonic() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let first = allocate_forest_revision(&tx).unwrap();
        let second = allocate_forest_revision(&tx).unwrap();
        assert!(second > first);
        drop(tx); // rollback

        // A rolled-back allocation may reuse values, which is fine: the allocation always
        // happens in the same transaction as the mutation that uses it.
        let tx = conn.transaction().unwrap();
        let third = allocate_forest_revision(&tx).unwrap();
        assert_eq!(third, first);
    }

    #[test]
    fn add_commit_reopen() {
        let mut conn = setup_conn();

        let expected_root = {
            let tx = conn.transaction().unwrap();
            let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
            let roots = forest
                .add_lineages(1, batch(lid(1), &[(w(10), w(100)), (w(20), w(200))]))
                .unwrap();
            let root = roots[0].root();
            drop(forest);
            tx.commit().unwrap();
            root
        };

        let reference = Smt::with_entries([(w(10), w(100)), (w(20), w(200))]).unwrap();
        assert_eq!(expected_root, reference.root());

        let tx = conn.transaction().unwrap();
        let forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        let proof = forest.open(TreeId::new(lid(1), 1), w(10)).unwrap();
        assert_eq!(proof.get(&w(10)), Some(w(100)));
        assert!(proof.verify_presence(&w(10), &w(100), &expected_root).is_ok());
    }

    #[test]
    fn two_phase_and_dependent_updates_in_one_txn() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();

        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        let mutations =
            forest.compute_forest_mutations(2, batch(lid(1), &[(w(20), w(200))])).unwrap();
        assert_eq!(mutations.lineage_mutations().len(), 1);
        assert_eq!(mutations.lineage_mutations()[0].new_version(), 2);

        // Nothing changes until apply.
        assert_eq!(forest_backend(&tx).version(lid(1)).unwrap(), 1);
        forest.apply_mutations(mutations).unwrap();
        assert_eq!(forest_backend(&tx).version(lid(1)).unwrap(), 2);

        // A dependent update in the same transaction sees the uncommitted state.
        forest.update_forest(3, batch(lid(1), &[(w(30), w(300))])).unwrap();

        let reference =
            Smt::with_entries([(w(10), w(100)), (w(20), w(200)), (w(30), w(300))]).unwrap();
        let proof = forest.open(TreeId::new(lid(1), 3), w(30)).unwrap();
        assert!(proof.verify_presence(&w(30), &w(300), &reference.root()).is_ok());

        drop(forest);
        tx.commit().unwrap();
    }

    #[test]
    fn rollback_discards_changes() {
        let mut conn = setup_conn();

        {
            let tx = conn.transaction().unwrap();
            let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
            forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();
            drop(forest);
            tx.commit().unwrap();
        }

        {
            let tx = conn.transaction().unwrap();
            let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
            forest
                .update_forest(2, batch(lid(1), &[(w(10), w(999)), (w(20), w(200))]))
                .unwrap();
            forest.add_lineages(2, batch(lid(2), &[(w(1), w(1))])).unwrap();
            drop(forest);
            // Dropping the transaction without committing rolls everything back.
        }

        let tx = conn.transaction().unwrap();
        let forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        assert_eq!(forest_backend(&tx).version(lid(1)).unwrap(), 1);
        let proof = forest.open(TreeId::new(lid(1), 1), w(10)).unwrap();
        assert_eq!(proof.get(&w(10)), Some(w(100)));
        assert!(forest.open(TreeId::new(lid(2), 2), w(1)).is_err());
    }

    #[test]
    fn compute_without_apply_changes_nothing() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        let mutations =
            forest.compute_forest_mutations(2, batch(lid(1), &[(w(10), w(999))])).unwrap();
        drop(mutations);

        assert_eq!(forest_backend(&tx).version(lid(1)).unwrap(), 1);
        let proof = forest.open(TreeId::new(lid(1), 1), w(10)).unwrap();
        assert_eq!(proof.get(&w(10)), Some(w(100)));
    }

    #[test]
    fn stale_mutations_rejected() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        let stale = forest.compute_forest_mutations(2, batch(lid(1), &[(w(20), w(200))])).unwrap();
        forest.update_forest(2, batch(lid(1), &[(w(30), w(300))])).unwrap();

        assert!(forest.apply_mutations(stale).is_err());
    }

    /// The Goldilocks field modulus; leaf positions are field elements, so they range over
    /// `0..FELT_MODULUS` (which includes values above `i64::MAX`).
    const FELT_MODULUS: u64 = 0xffff_ffff_0000_0001;

    /// Builds a key whose leaf position is `pos` (the most significant felt determines the leaf).
    fn wp(pos: u64, n: u64) -> Word {
        Word::from([Felt::new(n).unwrap(), ZERO, ZERO, Felt::new(pos).unwrap()])
    }

    fn subtree_rows(tx: &Transaction<'_>, lineage: LineageId) -> u64 {
        tx.query_row(
            "SELECT COUNT(*) FROM forest_subtrees WHERE lineage = ?1",
            params![lineage.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// Opens `key` through the backend and checks the proof against a reference SMT built from
    /// `entries`.
    fn assert_open_matches_reference(
        tx: &Transaction<'_>,
        lineage: LineageId,
        key: Word,
        entries: &[(Word, Word)],
    ) {
        let reference = Smt::with_entries(entries.iter().copied()).unwrap();
        let proof = forest_backend(tx).open(lineage, key).unwrap();
        assert_eq!(proof.compute_root(), reference.root(), "proof root mismatch for key {key}");
        let expected = entries.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        // `SmtProof::get` reports absent keys of a non-empty leaf as the empty word.
        let actual = proof.get(&key).filter(|value| *value != EMPTY_WORD);
        assert_eq!(actual, expected, "proof value mismatch for key {key}");
    }

    #[test]
    fn collision_leaf_transitions() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();

        // Two keys in the same leaf (same most significant felt) plus one in another leaf.
        let (ka, kb, kc) = (wp(7, 1), wp(7, 2), wp(9, 3));
        let all = [(ka, w(100)), (kb, w(200)), (kc, w(300))];
        forest.add_lineages(1, batch(lid(1), &all)).unwrap();
        for (key, _) in all {
            assert_open_matches_reference(&tx, lid(1), key, &all);
        }

        // Multiple -> Single.
        let mut b = SmtForestUpdateBatch::empty();
        b.operations(lid(1)).add_remove(kb);
        forest.update_forest(2, b).unwrap();
        let remaining = [(ka, w(100)), (kc, w(300))];
        for key in [ka, kb, kc] {
            assert_open_matches_reference(&tx, lid(1), key, &remaining);
        }

        // Single -> Empty, one key at a time down to the empty tree.
        let mut b = SmtForestUpdateBatch::empty();
        b.operations(lid(1)).add_remove(ka);
        b.operations(lid(1)).add_remove(kc);
        forest.update_forest(3, b).unwrap();
        for key in [ka, kb, kc] {
            assert_open_matches_reference(&tx, lid(1), key, &[]);
        }

        // Deleting the final entry must clear every subtree band.
        assert_eq!(subtree_rows(&tx, lid(1)), 0);
    }

    #[test]
    fn random_operations_match_reference_smt() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(wp(1, 1), w(1))])).unwrap();

        // Deterministic LCG so the test is reproducible without a rand dependency.
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state
        };

        // Every round does an explicit quota of 7 inserts and up to 3 removals, so both kinds of
        // operation are guaranteed to occur, unlike a pure coin flip on LCG bits (whose low bits
        // have short cycles). Positions mix a clustered range (frequent leaf collisions and
        // shared subtrees), the full u64 range (distinct subtrees in every band), and boundaries.
        let mut entries: Vec<(Word, Word)> = vec![(wp(1, 1), w(1))];
        let mut removals = 0u32;
        for round in 2..=6u64 {
            // Removals draw from entries of previous rounds only, so a batch never contains two
            // operations on the same key (which compute_mutations rejects).
            let mut b = SmtForestUpdateBatch::empty();
            let ops = b.operations(lid(1));
            for _ in 0..3 {
                if entries.is_empty() {
                    break;
                }
                let index = usize::try_from(next() % entries.len() as u64).unwrap();
                let (key, _) = entries.swap_remove(index);
                ops.add_remove(key);
                removals += 1;
            }
            for i in 0..7u64 {
                let position = match (round + i) % 4 {
                    // Full-range draw over the whole field, including positions above i64::MAX
                    // (stored bit-preserved as negative SQLite integers).
                    0 => next() % FELT_MODULUS,
                    // The largest representable position.
                    1 => FELT_MODULUS - 1,
                    _ => next() % 32,
                };
                let key = wp(position, next() % 1_000);
                let value = w(next() % 1_000 + 1);
                entries.retain(|(k, _)| *k != key);
                entries.push((key, value));
                ops.add_insert(key, value);
            }
            forest.update_forest(round, b).unwrap();

            // Check present keys, plus a key that was never inserted (position 40 is outside
            // the clustered range and not a boundary).
            for (key, _) in entries.iter().take(5) {
                assert_open_matches_reference(&tx, lid(1), *key, &entries);
            }
            assert_open_matches_reference(&tx, lid(1), wp(40, 0), &entries);
        }
        assert!(removals > 0, "the schedule must exercise removals");
    }

    #[test]
    fn missing_subtree_blob_is_corruption() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        // Two leaves whose paths diverge at depth 1, so the depth-0 subtree holds a non-empty
        // sibling of the queried path (a missing blob whose siblings were all empty would leave
        // the proof unchanged and is undetectable by design).
        forest
            .add_lineages(1, batch(lid(1), &[(wp(1, 1), w(100)), (wp(1 << 63, 2), w(200))]))
            .unwrap();

        tx.execute(
            "DELETE FROM forest_subtrees WHERE lineage = ?1 AND depth = 0",
            params![lid(1).as_bytes().as_slice()],
        )
        .unwrap();

        let err = forest_backend(&tx).open(lid(1), wp(1, 1)).unwrap_err();
        assert!(matches!(err, BackendError::CorruptedData(_)), "unexpected error: {err}");
    }

    #[test]
    fn malformed_subtree_blob_is_corruption() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        tx.execute(
            "UPDATE forest_subtrees SET data = X'DEADBEEF' WHERE lineage = ?1 AND depth = 0",
            params![lid(1).as_bytes().as_slice()],
        )
        .unwrap();

        let err = forest_backend(&tx).open(lid(1), w(10)).unwrap_err();
        assert!(matches!(err, BackendError::CorruptedData(_)), "unexpected error: {err}");
    }

    #[test]
    fn empty_stored_value_is_corruption() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        tx.execute(
            "UPDATE forest_entries SET value = ?2 WHERE lineage = ?1",
            params![lid(1).as_bytes().as_slice(), EMPTY_WORD.to_bytes()],
        )
        .unwrap();

        let backend = forest_backend(&tx);
        let open_err = backend.open(lid(1), w(10)).unwrap_err();
        assert!(matches!(open_err, BackendError::CorruptedData(_)), "open: {open_err}");
        let get_err = backend.get(lid(1), w(10)).unwrap_err();
        assert!(matches!(get_err, BackendError::CorruptedData(_)), "get: {get_err}");
        let entries_err = backend.entries(lid(1)).err().expect("entries must fail");
        assert!(matches!(entries_err, BackendError::CorruptedData(_)), "entries: {entries_err}");
    }

    #[test]
    fn failed_application_rolls_back_to_savepoint() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        // Compute valid prepared mutations first, then corrupt a blob, so the failure happens
        // inside write_subtrees after write_pairs already ran and the savepoint must undo it.
        let mutations =
            forest.compute_forest_mutations(2, batch(lid(1), &[(w(10), w(999))])).unwrap();
        tx.execute(
            "UPDATE forest_subtrees SET data = X'DEADBEEF' WHERE lineage = ?1 AND depth = 0",
            params![lid(1).as_bytes().as_slice()],
        )
        .unwrap();
        let err = forest.apply_mutations(mutations).unwrap_err();
        assert!(err.to_string().contains("malformed"), "unexpected error: {err}");

        // The savepoint must have undone the partial writes: entry value and version unchanged.
        let backend = forest_backend(&tx);
        assert_eq!(backend.get(lid(1), w(10)).unwrap(), Some(w(100)));
        assert_eq!(backend.version(lid(1)).unwrap(), 1);

        // The outer transaction stays usable: unrelated lineages can still be written and read.
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        forest.add_lineages(2, batch(lid(2), &[(w(20), w(200))])).unwrap();
        assert_open_matches_reference(&tx, lid(2), w(20), &[(w(20), w(200))]);
    }

    #[test]
    fn corrupted_path_rejected_at_compute_time() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        // A well-formed but diverged blob must be caught by the pre-mutation path
        // authentication, not persisted into a wrong new root. The divergence has to sit on a
        // sibling (the right child here; the stored key lives on the left half): on-path values
        // are recomputed from the leaf and overwritten anyway, so only sibling divergence can
        // corrupt a new root.
        let other = Smt::with_entries([(w(10), w(555))]).unwrap();
        let mut divergent = Subtree::new(NodeIndex::root());
        let root_inner = InnerNode {
            left: *EmptySubtreeRoots::entry(SMT_DEPTH, 1),
            right: other.root(),
        };
        divergent.insert_inner_node(NodeIndex::root(), root_inner);
        tx.execute(
            "UPDATE forest_subtrees SET data = ?2 WHERE lineage = ?1 AND depth = 0",
            params![lid(1).as_bytes().as_slice(), divergent.to_vec()],
        )
        .unwrap();

        let err = forest.update_forest(2, batch(lid(1), &[(w(10), w(999))])).unwrap_err();
        // Replacing the whole blob also drops the stored on-path nodes at depths 1..7, so the
        // on-path consistency check fires before the final root comparison; either rejection is
        // the corruption being caught at compute time.
        assert!(err.to_string().contains("corruption"), "unexpected error: {err}");
        assert_eq!(forest_backend(&tx).version(lid(1)).unwrap(), 1);
    }

    #[test]
    fn on_path_divergence_rejected_at_compute_time() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        // Corrupt the ON-PATH half of the root node (the stored key lives on the left half).
        // Forward computation would silently heal this, but the corrupt value would leak into
        // the reverse set, so authentication must reject it.
        let other = Smt::with_entries([(w(10), w(555))]).unwrap();
        let mut divergent = Subtree::new(NodeIndex::root());
        let root_inner = InnerNode {
            left: other.root(),
            right: *EmptySubtreeRoots::entry(SMT_DEPTH, 1),
        };
        divergent.insert_inner_node(NodeIndex::root(), root_inner);
        tx.execute(
            "UPDATE forest_subtrees SET data = ?2 WHERE lineage = ?1 AND depth = 0",
            params![lid(1).as_bytes().as_slice(), divergent.to_vec()],
        )
        .unwrap();

        let err = forest.update_forest(2, batch(lid(1), &[(w(10), w(999))])).unwrap_err();
        assert!(err.to_string().contains("diverges"), "unexpected error: {err}");
        assert_eq!(forest_backend(&tx).version(lid(1)).unwrap(), 1);
    }

    #[test]
    fn historical_open_after_fast_update() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        let initial = [(wp(7, 1), w(100)), (wp(7, 2), w(200)), (wp(9, 3), w(300))];
        forest.add_lineages(1, batch(lid(1), &initial)).unwrap();
        let reference_v1 = Smt::with_entries(initial).unwrap();

        // Mixed update: collision-leaf change, removal, and a fresh insert.
        let mut b = SmtForestUpdateBatch::empty();
        b.operations(lid(1)).add_insert(wp(7, 1), w(111));
        b.operations(lid(1)).add_remove(wp(9, 3));
        b.operations(lid(1)).add_insert(wp(5, 4), w(400));
        forest.update_forest(2, b).unwrap();

        // Historical opens at version 1 are served through the reverse sets this backend
        // produced; presence, collision-sibling presence, and absence must all verify.
        for (key, value) in [(wp(7, 1), Some(w(100))), (wp(9, 3), Some(w(300))), (wp(5, 4), None)] {
            let proof = forest.open(TreeId::new(lid(1), 1), key).unwrap();
            assert_eq!(proof.compute_root(), reference_v1.root(), "root mismatch for {key}");
            let actual = proof.get(&key).filter(|v| *v != EMPTY_WORD);
            assert_eq!(actual, value, "value mismatch for {key}");
        }
    }

    #[test]
    fn removal_updates_entries_and_count() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(forest_backend(&tx)).unwrap();
        forest
            .add_lineages(1, batch(lid(1), &[(w(10), w(100)), (w(20), w(200))]))
            .unwrap();

        let mut b = SmtForestUpdateBatch::empty();
        b.operations(lid(1)).add_remove(w(10));
        forest.update_forest(2, b).unwrap();

        let backend = forest_backend(&tx);
        assert_eq!(backend.get(lid(1), w(10)).unwrap(), None);
        assert_eq!(backend.get(lid(1), w(20)).unwrap(), Some(w(200)));
        assert_eq!(backend.entry_count(lid(1)).unwrap(), 1);

        let reference = Smt::with_entries([(w(20), w(200))]).unwrap();
        let proof = forest.open(TreeId::new(lid(1), 2), w(20)).unwrap();
        assert!(proof.verify_presence(&w(20), &w(200), &reference.root()).is_ok());
    }
}
