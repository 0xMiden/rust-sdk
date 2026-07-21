//! `SQLite` storage backend for [`LargeSmtForest`], scoped to a rusqlite [`Transaction`].
//!
//! The backend borrows the store's transaction, so every forest write commits or rolls back
//! together with the account-table writes performed in the same transaction. A
//! `LargeSmtForest<SqliteForestBackend>` is constructed per store operation (forest construction
//! only reads tree metadata) and dropped before the transaction is committed. Rolling back the
//! transaction discards all forest changes; there is no separate in-memory state to reconcile.
//!
//! Trees are stored per lineage as their full set of key-value entries plus a metadata row
//! (latest version, root, and entry count). Mutations load the affected lineage's SMT on demand,
//! so memory usage is bounded by the trees touched by an operation rather than by the total
//! account state.

use std::fmt;

use miden_client::store::StoreError;
use miden_client::utils::{Deserializable, Serializable};
use miden_protocol::crypto::merkle::MerkleError;
use miden_protocol::crypto::merkle::smt::{
    AppliedLineageMutation,
    Backend,
    BackendError,
    BackendReader,
    LeafIndex,
    LineageId,
    LineageMutation,
    LineageMutationKind,
    MutationSet,
    SMT_DEPTH,
    Smt,
    SmtForestUpdateBatch,
    SmtLeaf,
    SmtProof,
    TreeEntry,
    TreeWithRoot,
    VersionId,
};
use miden_protocol::{EMPTY_WORD, Word};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::sql_error::SqlResultExt;
use crate::{column_value_as_u64, u64_to_value};

type Result<T> = core::result::Result<T, BackendError>;
type SmtMutationSet = MutationSet<SMT_DEPTH, Word, Word>;

/// An account SMT forest scoped to a rusqlite transaction.
pub(crate) type ScopedAccountForest<'a, 'conn> =
    miden_client::store::AccountSmtForest<SqliteForestBackend<'a, 'conn>>;

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

// BACKEND
// ================================================================================================

/// A [`LargeSmtForest`] backend that reads and writes through a borrowed rusqlite transaction.
///
/// [`LargeSmtForest`]: miden_protocol::crypto::merkle::smt::LargeSmtForest
#[derive(Clone, Copy)]
pub(crate) struct SqliteForestBackend<'a, 'conn> {
    tx: &'a Transaction<'conn>,
}

impl<'a, 'conn> SqliteForestBackend<'a, 'conn> {
    pub(crate) fn new(tx: &'a Transaction<'conn>) -> Self {
        Self { tx }
    }
}

impl fmt::Debug for SqliteForestBackend<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteForestBackend").finish_non_exhaustive()
    }
}

/// Read-only view over the same transaction.
///
/// A separate type because the [`Backend::Reader`] contract requires a view that implements
/// [`BackendReader`] but not [`Backend`]; every method delegates to the wrapped backend. The
/// view observes the transaction's current (uncommitted) state, intentionally, so that later
/// forest queries within a store operation see earlier writes of the same transaction. This
/// deviates from the upstream contract's point-in-time snapshot wording (like the no-IO
/// wording on `entry_count`); both deviations are safe for this crate-private backend, whose
/// forests live only inside a single store operation, and are raised in the upstream API
/// discussion.
#[derive(Clone, Copy)]
pub(crate) struct SqliteForestBackendReader<'a, 'conn>(SqliteForestBackend<'a, 'conn>);

impl fmt::Debug for SqliteForestBackendReader<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteForestBackendReader").finish_non_exhaustive()
    }
}

/// Backend-prepared data for two-phase mutations: one forward SMT mutation set per touched
/// lineage.
pub(crate) struct SqlitePreparedMutations {
    entries: Vec<PreparedLineage>,
}

struct PreparedLineage {
    lineage: LineageId,
    old_version: Option<VersionId>,
    new_version: VersionId,
    kind: LineageMutationKind,
    forward: SmtMutationSet,
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

fn require_tree_meta(conn: &Connection, lineage: LineageId) -> Result<(VersionId, Word, usize)> {
    tree_meta(conn, lineage)?.ok_or(BackendError::UnknownLineage(lineage))
}

fn load_entries(conn: &Connection, lineage: LineageId) -> Result<Vec<(Word, Word)>> {
    let mut stmt = conn
        .prepare_cached("SELECT key, value FROM forest_entries WHERE lineage = ?1")
        .map_err(internal)?;
    let rows = stmt
        .query_map(params![lineage.as_bytes().as_slice()], |row| {
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

/// Loads the full SMT for a lineage from its stored entries.
fn load_smt(conn: &Connection, lineage: LineageId) -> Result<Smt> {
    let entries = load_entries(conn, lineage)?;
    // A reconstruction failure means the persisted entries are invalid (for example duplicate
    // keys), which is corruption of backend data rather than a caller-derived Merkle failure.
    Smt::with_entries(entries).map_err(|e| {
        BackendError::CorruptedData(format!("stored entries of lineage {lineage} are invalid: {e}"))
    })
}

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

/// Writes the changed key-value pairs of a forward mutation set to the entries table. Values
/// equal to the empty word are deletions.
fn write_pairs(conn: &Connection, lineage: LineageId, forward: &SmtMutationSet) -> Result<()> {
    let mut upsert = conn
        .prepare_cached(
            "INSERT INTO forest_entries (lineage, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(lineage, key) DO UPDATE SET value = excluded.value",
        )
        .map_err(internal)?;
    let mut delete = conn
        .prepare_cached("DELETE FROM forest_entries WHERE lineage = ?1 AND key = ?2")
        .map_err(internal)?;

    for (key, value) in forward.new_pairs() {
        if *value == EMPTY_WORD {
            delete
                .execute(params![lineage.as_bytes().as_slice(), key.to_bytes()])
                .map_err(internal)?;
        } else {
            upsert
                .execute(params![lineage.as_bytes().as_slice(), key.to_bytes(), value.to_bytes()])
                .map_err(internal)?;
        }
    }
    Ok(())
}

fn upsert_tree_meta(
    conn: &Connection,
    lineage: LineageId,
    version: VersionId,
    root: &Word,
    entry_count: usize,
) -> Result<()> {
    conn.execute(
        "INSERT INTO forest_trees (lineage, version, root, entry_count) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(lineage) DO UPDATE SET
             version = excluded.version,
             root = excluded.root,
             entry_count = excluded.entry_count",
        params![
            lineage.as_bytes().as_slice(),
            u64_to_value(version),
            root.to_bytes(),
            u64_to_value(entry_count as u64)
        ],
    )
    .map_err(internal)?;
    Ok(())
}

// BACKEND READER
// ================================================================================================

impl BackendReader for SqliteForestBackend<'_, '_> {
    fn open(&self, lineage: LineageId, key: Word) -> Result<SmtProof> {
        require_tree_meta(self.tx, lineage)?;
        let smt = load_smt(self.tx, lineage)?;
        Ok(smt.open(&key))
    }

    fn get_leaf(&self, lineage: LineageId, leaf_index: LeafIndex<SMT_DEPTH>) -> Result<SmtLeaf> {
        require_tree_meta(self.tx, lineage)?;
        let smt = load_smt(self.tx, lineage)?;
        Ok(smt
            .get_leaf_by_index(leaf_index)
            .unwrap_or_else(|| SmtLeaf::new_empty(leaf_index)))
    }

    fn get(&self, lineage: LineageId, key: Word) -> Result<Option<Word>> {
        require_tree_meta(self.tx, lineage)?;
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

    fn version(&self, lineage: LineageId) -> Result<VersionId> {
        Ok(require_tree_meta(self.tx, lineage)?.0)
    }

    fn lineages(&self) -> Result<impl Iterator<Item = LineageId>> {
        Ok(self.trees()?.map(|t| t.lineage()))
    }

    fn trees(&self) -> Result<impl Iterator<Item = TreeWithRoot>> {
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
        Ok(trees.into_iter())
    }

    fn entry_count(&self, lineage: LineageId) -> Result<usize> {
        Ok(require_tree_meta(self.tx, lineage)?.2)
    }

    fn entries(&self, lineage: LineageId) -> Result<impl Iterator<Item = Result<TreeEntry>>> {
        require_tree_meta(self.tx, lineage)?;
        let entries = load_entries(self.tx, lineage)?;
        Ok(entries.into_iter().map(|(key, value)| Ok(TreeEntry { key, value })))
    }
}

impl BackendReader for SqliteForestBackendReader<'_, '_> {
    fn open(&self, lineage: LineageId, key: Word) -> Result<SmtProof> {
        self.0.open(lineage, key)
    }

    fn get_leaf(&self, lineage: LineageId, leaf_index: LeafIndex<SMT_DEPTH>) -> Result<SmtLeaf> {
        self.0.get_leaf(lineage, leaf_index)
    }

    fn get(&self, lineage: LineageId, key: Word) -> Result<Option<Word>> {
        self.0.get(lineage, key)
    }

    fn version(&self, lineage: LineageId) -> Result<VersionId> {
        self.0.version(lineage)
    }

    fn lineages(&self) -> Result<impl Iterator<Item = LineageId>> {
        self.0.lineages()
    }

    fn trees(&self) -> Result<impl Iterator<Item = TreeWithRoot>> {
        self.0.trees()
    }

    fn entry_count(&self, lineage: LineageId) -> Result<usize> {
        self.0.entry_count(lineage)
    }

    fn entries(&self, lineage: LineageId) -> Result<impl Iterator<Item = Result<TreeEntry>>> {
        self.0.entries(lineage)
    }
}

// BACKEND
// ================================================================================================

impl<'a, 'conn> Backend for SqliteForestBackend<'a, 'conn> {
    type Reader = SqliteForestBackendReader<'a, 'conn>;
    type PreparedMutations = SqlitePreparedMutations;

    fn reader(&self) -> Result<Self::Reader> {
        Ok(SqliteForestBackendReader(*self))
    }

    fn compute_mutations(
        &self,
        new_version: VersionId,
        updates: SmtForestUpdateBatch,
    ) -> Result<(Vec<LineageMutation>, Self::PreparedMutations)> {
        let mut mutations = Vec::new();
        let mut prepared = Vec::new();

        for (lineage, ops) in updates {
            let kv_ops = ops.into_iter().map(Into::into);
            let (old_version, kind, forward) = match tree_meta(self.tx, lineage)? {
                Some((version, _root, _count)) => {
                    let smt = load_smt(self.tx, lineage)?;
                    (Some(version), LineageMutationKind::UpdateTree, smt.compute_mutations(kv_ops)?)
                },
                None => {
                    (None, LineageMutationKind::AddLineage, Smt::new().compute_mutations(kv_ops)?)
                },
            };

            mutations.push(LineageMutation::new(
                lineage,
                old_version,
                new_version,
                forward.old_root(),
                forward.root(),
                kind,
            ));
            prepared.push(PreparedLineage {
                lineage,
                old_version,
                new_version,
                kind,
                forward,
            });
        }

        Ok((mutations, SqlitePreparedMutations { entries: prepared }))
    }

    fn apply_mutations(
        &mut self,
        mutations: Self::PreparedMutations,
    ) -> Result<Vec<AppliedLineageMutation>> {
        // Validate everything against the current state before writing anything, so user-derived
        // errors leave the backend consistent.
        for p in &mutations.entries {
            match p.kind {
                LineageMutationKind::AddLineage => {
                    if tree_meta(self.tx, p.lineage)?.is_some() {
                        return Err(BackendError::DuplicateLineage(p.lineage));
                    }
                },
                LineageMutationKind::UpdateTree => {
                    let (version, root, _count) = require_tree_meta(self.tx, p.lineage)?;
                    if Some(version) != p.old_version {
                        return Err(BackendError::BadVersion {
                            provided: p.old_version.unwrap_or_default(),
                            latest: version,
                        });
                    }
                    if root != p.forward.old_root() {
                        // Stale prepared mutations are a user-derived error, matching the
                        // in-memory backend's classification.
                        return Err(BackendError::Merkle(MerkleError::ConflictingRoots {
                            expected_root: p.forward.old_root(),
                            actual_root: root,
                        }));
                    }
                },
            }
        }

        let mut applied = Vec::with_capacity(mutations.entries.len());
        for p in mutations.entries {
            let old_root = p.forward.old_root();
            let new_root = p.forward.root();

            match p.kind {
                LineageMutationKind::AddLineage => {
                    write_pairs(self.tx, p.lineage, &p.forward)?;
                    let entry_count =
                        p.forward.new_pairs().values().filter(|v| **v != EMPTY_WORD).count();
                    upsert_tree_meta(self.tx, p.lineage, p.new_version, &new_root, entry_count)?;

                    applied.push(AppliedLineageMutation::new(
                        p.lineage,
                        p.old_version,
                        p.new_version,
                        old_root,
                        new_root,
                        0,
                        SmtMutationSet::default(),
                        p.kind,
                    ));
                },
                LineageMutationKind::UpdateTree => {
                    if p.forward.is_empty() {
                        // No-op update: no new tree version is allocated.
                        let (_, _, old_count) = require_tree_meta(self.tx, p.lineage)?;
                        applied.push(AppliedLineageMutation::new(
                            p.lineage,
                            p.old_version,
                            p.new_version,
                            old_root,
                            new_root,
                            old_count,
                            p.forward,
                            p.kind,
                        ));
                        continue;
                    }

                    let mut smt = load_smt(self.tx, p.lineage)?;
                    let old_count = smt.num_entries();

                    write_pairs(self.tx, p.lineage, &p.forward)?;
                    let reverse =
                        smt.apply_mutations_with_reversion(p.forward).map_err(internal)?;
                    debug_assert_eq!(smt.root(), new_root);

                    upsert_tree_meta(
                        self.tx,
                        p.lineage,
                        p.new_version,
                        &new_root,
                        smt.num_entries(),
                    )?;

                    applied.push(AppliedLineageMutation::new(
                        p.lineage,
                        p.old_version,
                        p.new_version,
                        old_root,
                        new_root,
                        old_count,
                        reverse,
                        p.kind,
                    ));
                },
            }
        }

        Ok(applied)
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::crypto::merkle::smt::{LargeSmtForest, TreeId};
    use miden_protocol::{Felt, ONE, ZERO};

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
            let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
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
        let forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
        let proof = forest.open(TreeId::new(lid(1), 1), w(10)).unwrap();
        assert_eq!(proof.get(&w(10)), Some(w(100)));
        assert!(proof.verify_presence(&w(10), &w(100), &expected_root).is_ok());
    }

    #[test]
    fn two_phase_and_dependent_updates_in_one_txn() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();

        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        let mutations =
            forest.compute_forest_mutations(2, batch(lid(1), &[(w(20), w(200))])).unwrap();
        assert_eq!(mutations.lineage_mutations().len(), 1);
        assert_eq!(mutations.lineage_mutations()[0].new_version(), 2);

        // Nothing changes until apply.
        assert_eq!(SqliteForestBackend::new(&tx).version(lid(1)).unwrap(), 1);
        forest.apply_mutations(mutations).unwrap();
        assert_eq!(SqliteForestBackend::new(&tx).version(lid(1)).unwrap(), 2);

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
            let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
            forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();
            drop(forest);
            tx.commit().unwrap();
        }

        {
            let tx = conn.transaction().unwrap();
            let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
            forest
                .update_forest(2, batch(lid(1), &[(w(10), w(999)), (w(20), w(200))]))
                .unwrap();
            forest.add_lineages(2, batch(lid(2), &[(w(1), w(1))])).unwrap();
            drop(forest);
            // Dropping the transaction without committing rolls everything back.
        }

        let tx = conn.transaction().unwrap();
        let forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
        assert_eq!(SqliteForestBackend::new(&tx).version(lid(1)).unwrap(), 1);
        let proof = forest.open(TreeId::new(lid(1), 1), w(10)).unwrap();
        assert_eq!(proof.get(&w(10)), Some(w(100)));
        assert!(forest.open(TreeId::new(lid(2), 2), w(1)).is_err());
    }

    #[test]
    fn compute_without_apply_changes_nothing() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        let mutations =
            forest.compute_forest_mutations(2, batch(lid(1), &[(w(10), w(999))])).unwrap();
        drop(mutations);

        assert_eq!(SqliteForestBackend::new(&tx).version(lid(1)).unwrap(), 1);
        let proof = forest.open(TreeId::new(lid(1), 1), w(10)).unwrap();
        assert_eq!(proof.get(&w(10)), Some(w(100)));
    }

    #[test]
    fn stale_mutations_rejected() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        let stale = forest.compute_forest_mutations(2, batch(lid(1), &[(w(20), w(200))])).unwrap();
        forest.update_forest(2, batch(lid(1), &[(w(30), w(300))])).unwrap();

        assert!(forest.apply_mutations(stale).is_err());
    }

    #[test]
    fn removal_updates_entries_and_count() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
        forest
            .add_lineages(1, batch(lid(1), &[(w(10), w(100)), (w(20), w(200))]))
            .unwrap();

        let mut b = SmtForestUpdateBatch::empty();
        b.operations(lid(1)).add_remove(w(10));
        forest.update_forest(2, b).unwrap();

        let backend = SqliteForestBackend::new(&tx);
        assert_eq!(backend.get(lid(1), w(10)).unwrap(), None);
        assert_eq!(backend.get(lid(1), w(20)).unwrap(), Some(w(200)));
        assert_eq!(backend.entry_count(lid(1)).unwrap(), 1);

        let reference = Smt::with_entries([(w(20), w(200))]).unwrap();
        let proof = forest.open(TreeId::new(lid(1), 2), w(20)).unwrap();
        assert!(proof.verify_presence(&w(20), &w(200), &reference.root()).is_ok());
    }
}
