//! `SQLite` storage backend for [`LargeSmtForest`], scoped to a rusqlite [`Transaction`].
//!
//! The backend borrows the store's transaction, so every forest write commits or rolls back
//! together with the account-table writes performed in the same transaction. A
//! `LargeSmtForest<SqliteForestBackend>` is constructed per store operation (forest construction
//! only reads tree metadata) and dropped before the transaction is committed. Rolling back the
//! transaction discards all forest changes; there is no separate in-memory state to reconcile.
//!
//! Trees are stored per lineage as their full set of key-value entries, their inner nodes packed
//! as 8-level subtree blobs (the same layout as miden-crypto's persistent forest backend), and a
//! metadata row (latest version, root, and entry count). Witness reads load one leaf plus the
//! eight subtree blobs on its path, so their cost is independent of the tree size. Mutations load
//! the affected lineage's SMT on demand, so memory usage is bounded by the trees touched by an
//! operation rather than by the total account state.

use std::fmt;

use miden_client::store::StoreError;
use miden_client::utils::{Deserializable, Serializable};
use miden_protocol::crypto::merkle::smt::{
    AppliedLineageMutation,
    Backend,
    BackendError,
    BackendReader,
    InnerNode,
    LeafIndex,
    LineageId,
    LineageMutation,
    LineageMutationKind,
    MAX_LEAF_ENTRIES,
    MutationSet,
    NodeMutation,
    SMT_DEPTH,
    Smt,
    SmtForestUpdateBatch,
    SmtLeaf,
    SmtProof,
    Subtree,
    TreeEntry,
    TreeWithRoot,
    VersionId,
};
use miden_protocol::crypto::merkle::{EmptySubtreeRoots, MerkleError, NodeIndex, SparseMerklePath};
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
    /// Precomputed reverse of `forward` (empty for [`LineageMutationKind::AddLineage`], whose
    /// applied mutation carries an empty reverse set).
    reverse: SmtMutationSet,
    /// Net change in the lineage's key count when `forward` is applied.
    entry_count_delta: i64,
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
        let (key, value) = (word_from_blob(&key_blob)?, word_from_blob(&value_blob)?);
        require_non_empty_value(lineage, key, value)?;
        entries.push((key, value));
    }
    Ok(entries)
}

/// Rejects stored empty values as corruption; the write path deletes them instead of storing
/// them.
fn require_non_empty_value(lineage: LineageId, key: Word, value: Word) -> Result<()> {
    if value == EMPTY_WORD {
        return Err(BackendError::CorruptedData(format!(
            "empty value stored for key {key} of lineage {lineage}"
        )));
    }
    Ok(())
}

/// Rejects rows whose stored `leaf_position` does not match the position derived from the key;
/// position-based lookups would otherwise silently miss the entry.
fn require_consistent_position(lineage: LineageId, key: Word, position: u64) -> Result<()> {
    let derived = LeafIndex::<SMT_DEPTH>::from(key).position();
    if derived != position {
        return Err(BackendError::CorruptedData(format!(
            "entry {key} of lineage {lineage} is stored at leaf position {position}, but its \
             key derives position {derived}"
        )));
    }
    Ok(())
}

/// Loads the sorted key-value entries of the SMT leaf at `position`.
///
/// Entries are sorted by key because a multi-entry leaf's hash is order-sensitive and `SQLite`
/// row order is unspecified; sorting by [`Word`] matches the canonical order the SMT maintains
/// inside its leaves.
fn load_leaf_entries(
    conn: &Connection,
    lineage: LineageId,
    position: u64,
) -> Result<Vec<(Word, Word)>> {
    let mut stmt = conn
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
        let (key, value) = (word_from_blob(&key_blob)?, word_from_blob(&value_blob)?);
        require_non_empty_value(lineage, key, value)?;
        require_consistent_position(lineage, key, position)?;
        entries.push((key, value));
    }
    entries.sort_by_key(|(key, _)| *key);
    Ok(entries)
}

/// Builds the [`SmtLeaf`] at `leaf_index` from a leaf's entries.
fn leaf_from_entries(
    lineage: LineageId,
    leaf_index: LeafIndex<SMT_DEPTH>,
    entries: Vec<(Word, Word)>,
) -> Result<SmtLeaf> {
    SmtLeaf::new(entries, leaf_index).map_err(|e| {
        BackendError::CorruptedData(format!(
            "stored entries of leaf {} of lineage {lineage} are invalid: {e}",
            leaf_index.position()
        ))
    })
}

/// Loads the SMT leaf at `leaf_index` from the stored entries of a lineage.
fn load_leaf(
    conn: &Connection,
    lineage: LineageId,
    leaf_index: LeafIndex<SMT_DEPTH>,
) -> Result<SmtLeaf> {
    let entries = load_leaf_entries(conn, lineage, leaf_index.position())?;
    leaf_from_entries(lineage, leaf_index, entries)
}

/// Loads the subtree blob rooted at `root_index`, or an empty subtree if none is stored.
fn load_subtree(conn: &Connection, lineage: LineageId, root_index: NodeIndex) -> Result<Subtree> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT data FROM forest_subtrees \
             WHERE lineage = ?1 AND depth = ?2 AND position = ?3",
        )
        .map_err(internal)?;
    let blob = stmt
        .query_row(
            params![
                lineage.as_bytes().as_slice(),
                root_index.depth(),
                u64_to_value(root_index.position())
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(internal)?;

    match blob {
        Some(blob) => Subtree::from_vec(root_index, &blob).map_err(|e| {
            BackendError::CorruptedData(format!(
                "stored subtree at depth {} position {} of lineage {lineage} is malformed: {e}",
                root_index.depth(),
                root_index.position()
            ))
        }),
        None => Ok(Subtree::new(root_index)),
    }
}

/// Computes the Merkle path for `leaf_index` from the stored subtree blobs on its path.
///
/// One subtree per 8-level band is loaded (roots at depths 56, 48, ..., 0); siblings of nodes
/// that are not present in a blob are empty subtree roots.
fn compute_merkle_path(
    conn: &Connection,
    lineage: LineageId,
    leaf_index: NodeIndex,
) -> Result<SparseMerklePath> {
    let mut path = Vec::with_capacity(SMT_DEPTH as usize);
    let mut node_index = leaf_index;
    let mut subtree: Option<Subtree> = None;

    while node_index.depth() > 0 {
        let is_right = node_index.is_position_odd();
        node_index = node_index.parent();

        let root_index = Subtree::find_subtree_root(node_index);
        if subtree.as_ref().map(Subtree::root_index) != Some(root_index) {
            subtree = Some(load_subtree(conn, lineage, root_index)?);
        }
        let subtree = subtree.as_ref().expect("subtree loaded above");

        let InnerNode { left, right } = subtree
            .get_inner_node(node_index)
            .unwrap_or_else(|| empty_inner_node(node_index.depth()));

        path.push(if is_right { left } else { right });
    }

    SparseMerklePath::from_sized_iter(path)
        .map_err(|e| BackendError::CorruptedData(format!("invalid Merkle path: {e}")))
}

/// Returns the inner node of an empty subtree at `node_depth` (both children are the empty
/// subtree root one level below).
fn empty_inner_node(node_depth: u8) -> InnerNode {
    let child = *EmptySubtreeRoots::entry(SMT_DEPTH, node_depth + 1);
    InnerNode { left: child, right: child }
}

// PATH-LOCAL MUTATION COMPUTATION
// ================================================================================================

/// Forward and reverse mutation sets for one lineage update, plus the entry-count change.
struct ComputedLineageMutations {
    forward: SmtMutationSet,
    reverse: SmtMutationSet,
    entry_count_delta: i64,
}

/// Computes the forward and reverse mutation sets for `kv_ops` on an existing lineage by reading
/// only the affected leaves and the subtree blobs on their paths.
///
/// This mirrors `SparseMerkleTree::compute_mutations_sequential` (and the reverse-set
/// construction of `apply_mutations_with_reversion`) over persisted state, so its cost scales
/// with the change set instead of the tree size. Because the new root is derived from stored
/// subtree data, every touched leaf's stored path is first authenticated against `old_root`
/// (both node halves per level); a missing or diverged blob is reported as corruption instead
/// of silently producing a wrong root. Corruption on untouched paths is not detectable without
/// a full scan and remains covered by the read-time root check. The stored `entry_count` is
/// trusted within representable range (deltas are applied to it, not recounted), and the
/// bulk-load heuristic keys off raw op count, not distinct leaf positions.
fn compute_update_mutations(
    conn: &Connection,
    lineage: LineageId,
    old_root: Word,
    entry_count: usize,
    kv_ops: impl Iterator<Item = (Word, Word)>,
) -> Result<ComputedLineageMutations> {
    use std::collections::{HashMap, HashSet};

    let kv_ops: Vec<(Word, Word)> = kv_ops.collect();

    // Stored subtrees on touched paths; never mutated during compute, so lookups through this
    // cache always observe the pre-update state.
    let mut subtrees: HashMap<NodeIndex, Subtree> = HashMap::new();
    // Effective (batch-mutated) sorted entries of each touched leaf. Small batches load each
    // touched leaf with a point query; batches comparable to the tree size (full-state
    // presentations) load every leaf in one scan instead, which is far cheaper than one point
    // query per op. When bulk-loaded, a position absent from the map is a stored-empty leaf.
    let mut leaves: HashMap<u64, Vec<(Word, Word)>> = HashMap::new();
    let bulk_loaded = kv_ops.len() > 64 && kv_ops.len() * 4 >= entry_count;
    if bulk_loaded {
        let mut stmt = conn
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
            let (key_blob, value_blob, position) = row.map_err(internal)?;
            let (key, value) = (word_from_blob(&key_blob)?, word_from_blob(&value_blob)?);
            require_non_empty_value(lineage, key, value)?;
            require_consistent_position(lineage, key, position)?;
            leaves.entry(position).or_default().push((key, value));
        }
        for entries in leaves.values_mut() {
            entries.sort_by_key(|(key, _)| *key);
        }
    }
    let mut forward_nodes: HashMap<NodeIndex, NodeMutation> = HashMap::new();
    // The stored node at each mutated index, captured once before any overlay, for the reverse
    // set.
    let mut original_nodes: HashMap<NodeIndex, Option<InnerNode>> = HashMap::new();
    let mut forward_pairs: Vec<(Word, Word)> = Vec::new();
    let mut reverse_pairs: Vec<(Word, Word)> = Vec::new();
    let mut seen_keys: HashSet<Word> = HashSet::new();
    // Leaves whose stored path has been authenticated against `old_root`.
    let mut verified_leaves: HashSet<u64> = HashSet::new();
    let mut new_root = old_root;
    let mut entry_count_delta: i64 = 0;

    let stored_inner_node = |subtrees: &mut HashMap<NodeIndex, Subtree>,
                             index: NodeIndex|
     -> Result<Option<InnerNode>> {
        let root_index = Subtree::find_subtree_root(index);
        let subtree = match subtrees.entry(root_index) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(load_subtree(conn, lineage, root_index)?)
            },
        };
        Ok(subtree.get_inner_node(index))
    };

    for (key, value) in kv_ops {
        if !seen_keys.insert(key) {
            return Err(BackendError::Merkle(MerkleError::DuplicateValuesForIndex(
                LeafIndex::<SMT_DEPTH>::from(key).position(),
            )));
        }

        let leaf_index = LeafIndex::<SMT_DEPTH>::from(key);
        let position = leaf_index.position();

        if !leaves.contains_key(&position) {
            let entries = if bulk_loaded {
                Vec::new()
            } else {
                load_leaf_entries(conn, lineage, position)?
            };
            leaves.insert(position, entries);
        }
        let entries = leaves.get_mut(&position).expect("leaf loaded above");

        let old_value =
            entries.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or(EMPTY_WORD);
        if value == old_value {
            continue;
        }

        // First mutation of a leaf: authenticate its stored entries and stored path against the
        // pre-update root, so a missing or diverged blob is reported as corruption instead of
        // silently producing a wrong new root. The cached entries are still the stored state
        // here (no earlier op mutated this leaf), and the subtree cache always is. Deferring
        // this to the first actual mutation keeps no-op ops at one point query.
        if !verified_leaves.contains(&position) {
            let leaf = leaf_from_entries(lineage, leaf_index, entries.clone())?;
            let mut hash = leaf.hash();
            let mut node_index = NodeIndex::from(leaf_index);
            while node_index.depth() > 0 {
                let is_right = node_index.is_position_odd();
                node_index = node_index.parent();
                let node = stored_inner_node(&mut subtrees, node_index)?
                    .unwrap_or_else(|| empty_inner_node(node_index.depth()));
                // Both halves are checked: the on-path child must match the hash derived so far
                // (a diverged on-path node would otherwise be silently healed forward while its
                // corrupt value leaks into the reverse set), and the sibling feeds the next hash.
                let on_path_child = if is_right { node.right } else { node.left };
                if on_path_child != hash {
                    return Err(BackendError::CorruptedData(format!(
                        "stored node at depth {} on the path of leaf {position} of lineage \
                         {lineage} diverges from its subtree",
                        node_index.depth()
                    )));
                }
                hash = node.hash();
            }
            if hash != old_root {
                return Err(BackendError::CorruptedData(format!(
                    "stored path of leaf {position} of lineage {lineage} yields root {hash}, \
                     but the tree root is {old_root}"
                )));
            }
            verified_leaves.insert(position);
        }
        reverse_pairs.push((key, old_value));

        if value == EMPTY_WORD {
            entries.retain(|(k, _)| *k != key);
            entry_count_delta -= 1;
        } else if let Some(entry) = entries.iter_mut().find(|(k, _)| *k == key) {
            entry.1 = value;
        } else {
            let insert_at = entries.partition_point(|(k, _)| *k < key);
            entries.insert(insert_at, (key, value));
            entry_count_delta += 1;
        }

        // Overfull leaves are a caller-derived Merkle failure, matching compute_mutations.
        if entries.len() > MAX_LEAF_ENTRIES {
            return Err(BackendError::Merkle(MerkleError::TooManyLeafEntries {
                actual: entries.len(),
            }));
        }
        let leaf = leaf_from_entries(lineage, leaf_index, entries.clone())?;
        let mut child_hash = leaf.hash();
        let mut node_index = NodeIndex::from(leaf_index);

        while node_index.depth() > 0 {
            let is_right = node_index.is_position_odd();
            node_index = node_index.parent();

            let old_node = match forward_nodes.get(&node_index) {
                Some(NodeMutation::Addition(node)) => node.clone(),
                Some(NodeMutation::Removal) => empty_inner_node(node_index.depth()),
                None => {
                    let stored = stored_inner_node(&mut subtrees, node_index)?;
                    original_nodes.entry(node_index).or_insert_with(|| stored.clone());
                    stored.unwrap_or_else(|| empty_inner_node(node_index.depth()))
                },
            };

            let new_node = if is_right {
                InnerNode { left: old_node.left, right: child_hash }
            } else {
                InnerNode { left: child_hash, right: old_node.right }
            };
            child_hash = new_node.hash();

            let is_removal = child_hash == *EmptySubtreeRoots::entry(SMT_DEPTH, node_index.depth());
            let mutation = if is_removal {
                NodeMutation::Removal
            } else {
                NodeMutation::Addition(new_node)
            };
            forward_nodes.insert(node_index, mutation);
        }

        new_root = child_hash;
        forward_pairs.push((key, value));
    }

    // Reverse node mutations, mirroring apply_mutations_with_reversion: restore the stored node
    // where one existed, remove nodes the forward set created, and skip removals of nodes that
    // never existed.
    let mut reverse_nodes: HashMap<NodeIndex, NodeMutation> = HashMap::new();
    for (index, original) in &original_nodes {
        match original {
            Some(node) => {
                reverse_nodes.insert(*index, NodeMutation::Addition(node.clone()));
            },
            None => {
                if matches!(forward_nodes.get(index), Some(NodeMutation::Addition(_))) {
                    reverse_nodes.insert(*index, NodeMutation::Removal);
                }
            },
        }
    }

    let forward = SmtMutationSet::from_parts(old_root, forward_nodes, forward_pairs, new_root);
    let reverse = SmtMutationSet::from_parts(new_root, reverse_nodes, reverse_pairs, old_root);
    Ok(ComputedLineageMutations { forward, reverse, entry_count_delta })
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
            "INSERT INTO forest_entries (lineage, key, value, leaf_position)
             VALUES (?1, ?2, ?3, ?4)
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
            let leaf_position = LeafIndex::<SMT_DEPTH>::from(*key).position();
            upsert
                .execute(params![
                    lineage.as_bytes().as_slice(),
                    key.to_bytes(),
                    value.to_bytes(),
                    u64_to_value(leaf_position)
                ])
                .map_err(internal)?;
        }
    }
    Ok(())
}

/// Applies the inner-node mutations of a forward mutation set to the stored subtree blobs.
///
/// Mutations are grouped by containing subtree so each affected blob is loaded, patched with one
/// batch call, and written back (or deleted once empty) exactly once. A removal that targets a
/// node absent from its blob means the stored subtrees have diverged from the stored entries,
/// which is corruption of backend data.
fn write_subtrees(conn: &Connection, lineage: LineageId, forward: &SmtMutationSet) -> Result<()> {
    let mut groups: std::collections::BTreeMap<(u8, u64), Vec<(&NodeIndex, &NodeMutation)>> =
        std::collections::BTreeMap::new();
    for (index, mutation) in forward.node_mutations() {
        let root_index = Subtree::find_subtree_root(*index);
        groups
            .entry((root_index.depth(), root_index.position()))
            .or_default()
            .push((index, mutation));
    }

    let mut upsert = conn
        .prepare_cached(
            "INSERT INTO forest_subtrees (lineage, depth, position, data)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(lineage, depth, position) DO UPDATE SET data = excluded.data",
        )
        .map_err(internal)?;
    let mut delete = conn
        .prepare_cached(
            "DELETE FROM forest_subtrees WHERE lineage = ?1 AND depth = ?2 AND position = ?3",
        )
        .map_err(internal)?;

    for ((depth, position), mutations) in groups {
        let root_index =
            NodeIndex::new(depth, position).expect("subtree root computed from a valid node index");
        let mut subtree = load_subtree(conn, lineage, root_index)?;

        for (index, mutation) in &mutations {
            if matches!(mutation, NodeMutation::Removal)
                && subtree.get_inner_node(**index).is_none()
            {
                return Err(BackendError::CorruptedData(format!(
                    "removal of absent inner node at depth {} position {} of lineage {lineage}",
                    index.depth(),
                    index.position()
                )));
            }
        }
        subtree.apply_mutations(mutations.iter().map(|(index, mutation)| (*index, *mutation)));

        if subtree.is_empty() {
            delete
                .execute(params![lineage.as_bytes().as_slice(), depth, u64_to_value(position)])
                .map_err(internal)?;
        } else {
            upsert
                .execute(params![
                    lineage.as_bytes().as_slice(),
                    depth,
                    u64_to_value(position),
                    subtree.to_vec()
                ])
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
        let (_version, stored_root, _count) = require_tree_meta(self.tx, lineage)?;

        let leaf_index = LeafIndex::<SMT_DEPTH>::from(key);
        let leaf = load_leaf(self.tx, lineage, leaf_index)?;
        let path = compute_merkle_path(self.tx, lineage, leaf_index.into())?;

        let proof = SmtProof::new(path, leaf).map_err(|e| {
            BackendError::CorruptedData(format!(
                "stored data of lineage {lineage} yields an invalid proof: {e}"
            ))
        })?;

        // The proof is assembled from two redundant representations (entry rows and subtree
        // blobs), so verify it against the stored root to catch any divergence between them.
        let computed_root = proof.compute_root();
        if computed_root != stored_root {
            return Err(BackendError::CorruptedData(format!(
                "proof for key {key} of lineage {lineage} yields root {computed_root}, but the \
                 stored root is {stored_root}"
            )));
        }
        Ok(proof)
    }

    fn get_leaf(&self, lineage: LineageId, leaf_index: LeafIndex<SMT_DEPTH>) -> Result<SmtLeaf> {
        require_tree_meta(self.tx, lineage)?;
        load_leaf(self.tx, lineage, leaf_index)
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
            .map(|blob| {
                let value = word_from_blob(&blob)?;
                require_non_empty_value(lineage, key, value)?;
                Ok(value)
            })
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
            let (old_version, kind, computed) = match tree_meta(self.tx, lineage)? {
                Some((version, root, count)) => {
                    // Path-local computation: reads only the affected leaves and the subtree
                    // blobs on their paths, so cost scales with the change set.
                    let computed = compute_update_mutations(self.tx, lineage, root, count, kv_ops)?;
                    (Some(version), LineageMutationKind::UpdateTree, computed)
                },
                None => {
                    // A new lineage starts from the empty tree, so there is no stored state to
                    // read and the in-memory computation is already proportional to the batch.
                    let forward = Smt::new().compute_mutations(kv_ops)?;
                    let computed = ComputedLineageMutations {
                        forward,
                        reverse: SmtMutationSet::default(),
                        entry_count_delta: 0,
                    };
                    (None, LineageMutationKind::AddLineage, computed)
                },
            };

            mutations.push(LineageMutation::new(
                lineage,
                old_version,
                new_version,
                computed.forward.old_root(),
                computed.forward.root(),
                kind,
            ));
            prepared.push(PreparedLineage {
                lineage,
                old_version,
                new_version,
                kind,
                forward: computed.forward,
                reverse: computed.reverse,
                entry_count_delta: computed.entry_count_delta,
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

        // The writes below span several tables and lineages, and write_subtrees can fail on
        // corrupted blobs partway through. The savepoint makes the whole application atomic even
        // for callers that catch the error and keep using the transaction.
        self.tx.execute_batch("SAVEPOINT forest_apply").map_err(internal)?;
        let result = self.apply_validated_mutations(mutations);
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

impl SqliteForestBackend<'_, '_> {
    /// Applies already-validated prepared mutations. Must run inside the `forest_apply`
    /// savepoint so a mid-application error does not leave partial writes visible.
    fn apply_validated_mutations(
        &mut self,
        mutations: SqlitePreparedMutations,
    ) -> Result<Vec<AppliedLineageMutation>> {
        let mut applied = Vec::with_capacity(mutations.entries.len());
        for p in mutations.entries {
            let old_root = p.forward.old_root();
            let new_root = p.forward.root();

            match p.kind {
                LineageMutationKind::AddLineage => {
                    write_pairs(self.tx, p.lineage, &p.forward)?;
                    write_subtrees(self.tx, p.lineage, &p.forward)?;
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

                    let (_, _, old_count) = require_tree_meta(self.tx, p.lineage)?;
                    let new_count = i64::try_from(old_count)
                        .ok()
                        .and_then(|count| count.checked_add(p.entry_count_delta))
                        .and_then(|count| usize::try_from(count).ok())
                        .ok_or_else(|| {
                            BackendError::CorruptedData(format!(
                                "entry count of lineage {} out of range",
                                p.lineage
                            ))
                        })?;

                    write_pairs(self.tx, p.lineage, &p.forward)?;
                    write_subtrees(self.tx, p.lineage, &p.forward)?;
                    upsert_tree_meta(self.tx, p.lineage, p.new_version, &new_root, new_count)?;

                    applied.push(AppliedLineageMutation::new(
                        p.lineage,
                        p.old_version,
                        p.new_version,
                        old_root,
                        new_root,
                        old_count,
                        p.reverse,
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
        let proof = SqliteForestBackend::new(tx).open(lineage, key).unwrap();
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
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();

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
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
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
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
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

        let err = SqliteForestBackend::new(&tx).open(lid(1), wp(1, 1)).unwrap_err();
        assert!(matches!(err, BackendError::CorruptedData(_)), "unexpected error: {err}");
    }

    #[test]
    fn malformed_subtree_blob_is_corruption() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        tx.execute(
            "UPDATE forest_subtrees SET data = X'DEADBEEF' WHERE lineage = ?1 AND depth = 0",
            params![lid(1).as_bytes().as_slice()],
        )
        .unwrap();

        let err = SqliteForestBackend::new(&tx).open(lid(1), w(10)).unwrap_err();
        assert!(matches!(err, BackendError::CorruptedData(_)), "unexpected error: {err}");
    }

    #[test]
    fn empty_stored_value_is_corruption() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        tx.execute(
            "UPDATE forest_entries SET value = ?2 WHERE lineage = ?1",
            params![lid(1).as_bytes().as_slice(), EMPTY_WORD.to_bytes()],
        )
        .unwrap();

        let backend = SqliteForestBackend::new(&tx);
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
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
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
        let backend = SqliteForestBackend::new(&tx);
        assert_eq!(backend.get(lid(1), w(10)).unwrap(), Some(w(100)));
        assert_eq!(backend.version(lid(1)).unwrap(), 1);

        // The outer transaction stays usable: unrelated lineages can still be written and read.
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
        forest.add_lineages(2, batch(lid(2), &[(w(20), w(200))])).unwrap();
        assert_open_matches_reference(&tx, lid(2), w(20), &[(w(20), w(200))]);
    }

    #[test]
    fn corrupted_path_rejected_at_compute_time() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
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
        assert_eq!(SqliteForestBackend::new(&tx).version(lid(1)).unwrap(), 1);
    }

    #[test]
    fn on_path_divergence_rejected_at_compute_time() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
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
        assert_eq!(SqliteForestBackend::new(&tx).version(lid(1)).unwrap(), 1);
    }

    #[test]
    fn historical_open_after_fast_update() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
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
    fn computed_mutations_match_reference_smt_exactly() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();

        let initial = [
            (wp(7, 1), w(100)),
            (wp(7, 2), w(200)),
            (wp(9, 3), w(300)),
            (wp(1 << 40, 4), w(400)),
        ];
        forest.add_lineages(1, batch(lid(1), &initial)).unwrap();
        let mut reference = Smt::with_entries(initial).unwrap();

        // Mixed rounds: updates, removals, inserts into fresh and colliding leaves, and no-ops.
        let rounds: [Vec<(Word, Word)>; 3] = [
            vec![(wp(7, 1), w(111)), (wp(2, 5), w(500)), (wp(9, 3), EMPTY_WORD)],
            vec![(wp(7, 2), EMPTY_WORD), (wp(7, 6), w(600)), (wp(1 << 40, 4), w(400))],
            vec![(wp(2, 5), EMPTY_WORD), (wp(7, 1), w(112))],
        ];
        for (round, ops) in rounds.into_iter().enumerate() {
            let (_, _, old_count) = require_tree_meta(&tx, lid(1)).unwrap();

            let computed = compute_update_mutations(
                &tx,
                lid(1),
                reference.root(),
                old_count,
                ops.iter().copied(),
            )
            .unwrap();
            let forward_ref = reference.compute_mutations(ops.iter().copied()).unwrap();
            let reverse_ref =
                reference.apply_mutations_with_reversion(forward_ref.clone()).unwrap();

            assert_eq!(computed.forward, forward_ref, "forward mismatch in round {round}");
            assert_eq!(computed.reverse, reverse_ref, "reverse mismatch in round {round}");

            // Persist through the regular path and check the stored count tracks the delta.
            let mut b = SmtForestUpdateBatch::empty();
            for (key, value) in &ops {
                if *value == EMPTY_WORD {
                    b.operations(lid(1)).add_remove(*key);
                } else {
                    b.operations(lid(1)).add_insert(*key, *value);
                }
            }
            forest.update_forest(round as u64 + 2, b).unwrap();
            let (_, root, count) = require_tree_meta(&tx, lid(1)).unwrap();
            assert_eq!(root, reference.root());
            assert_eq!(count, reference.num_entries());
            assert_eq!(
                i64::try_from(count).unwrap() - i64::try_from(old_count).unwrap(),
                computed.entry_count_delta
            );
        }
    }

    #[test]
    fn reverse_pairs_restore_previous_root() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
        let initial = [(wp(7, 1), w(100)), (wp(7, 2), w(200)), (wp(9, 3), w(300))];
        forest.add_lineages(1, batch(lid(1), &initial)).unwrap();
        let root_before = require_tree_meta(&tx, lid(1)).unwrap().1;

        let ops = [(wp(7, 1), w(111)), (wp(9, 3), EMPTY_WORD), (wp(5, 4), w(400))];
        let computed =
            compute_update_mutations(&tx, lid(1), root_before, 3, ops.iter().copied()).unwrap();

        let mut b = SmtForestUpdateBatch::empty();
        b.operations(lid(1)).add_insert(wp(7, 1), w(111));
        b.operations(lid(1)).add_remove(wp(9, 3));
        b.operations(lid(1)).add_insert(wp(5, 4), w(400));
        forest.update_forest(2, b).unwrap();
        assert_eq!(require_tree_meta(&tx, lid(1)).unwrap().1, computed.forward.root());

        // Applying the reverse set's pairs as regular operations must restore the previous root.
        let mut b = SmtForestUpdateBatch::empty();
        for (key, value) in computed.reverse.new_pairs() {
            if *value == EMPTY_WORD {
                b.operations(lid(1)).add_remove(*key);
            } else {
                b.operations(lid(1)).add_insert(*key, *value);
            }
        }
        forest.update_forest(3, b).unwrap();
        assert_eq!(require_tree_meta(&tx, lid(1)).unwrap().1, root_before);
        for (key, value) in initial {
            assert_open_matches_reference(&tx, lid(1), key, &initial);
            assert_eq!(SqliteForestBackend::new(&tx).get(lid(1), key).unwrap(), Some(value));
        }
    }

    #[test]
    fn bulk_loaded_snapshot_matches_reference_smt() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();

        // More than 64 entries so a snapshot-sized batch takes the bulk-load strategy.
        let initial: Vec<(Word, Word)> = (1..=100u64).map(|i| (wp(i % 10, i), w(i * 10))).collect();
        forest.add_lineages(1, batch(lid(1), &initial)).unwrap();
        let mut reference = Smt::with_entries(initial.iter().copied()).unwrap();

        // Snapshot-shaped batch: everything unchanged except one update, one removal, one insert.
        let mut ops: Vec<(Word, Word)> = initial.clone();
        ops[7].1 = w(7777);
        ops[42].1 = EMPTY_WORD;
        ops.push((wp(11, 200), w(2000)));

        let computed =
            compute_update_mutations(&tx, lid(1), reference.root(), 100, ops.iter().copied())
                .unwrap();
        let forward_ref = reference.compute_mutations(ops.iter().copied()).unwrap();
        let reverse_ref = reference.apply_mutations_with_reversion(forward_ref.clone()).unwrap();
        assert_eq!(computed.forward, forward_ref);
        assert_eq!(computed.reverse, reverse_ref);
        assert_eq!(computed.forward.new_pairs().len(), 3);
        assert_eq!(computed.entry_count_delta, 0);
    }

    /// Manual timing run for the path-local write path (release build recommended):
    /// `cargo test -p miden-client-sqlite-store --release timing_single_entry_update -- --ignored
    /// --nocapture`
    #[test]
    #[ignore = "manual timing run"]
    fn timing_single_entry_update() {
        use std::time::Instant;
        for n in [50_000u64, 1_000_000] {
            let path = std::env::temp_dir().join(format!("forest_timing_{n}.sqlite3"));
            let _ = std::fs::remove_file(&path);
            let mut conn = Connection::open(&path).unwrap();
            apply_migrations(&mut conn).unwrap();

            let tx = conn.transaction().unwrap();
            let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
            let mut b = SmtForestUpdateBatch::empty();
            {
                let ops = b.operations(lid(1));
                for i in 1..=n {
                    ops.add_insert(wp(i, i), w(i));
                }
            }
            let started = Instant::now();
            forest.add_lineages(1, b).unwrap();
            drop(forest);
            tx.commit().unwrap();
            println!("populate lineage n={n}: {:?}", started.elapsed());

            let started = Instant::now();
            let tx = conn.transaction().unwrap();
            let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
            forest.update_forest(2, batch(lid(1), &[(wp(1, 1), w(999_999))])).unwrap();
            drop(forest);
            tx.commit().unwrap();
            println!("single-entry committed forest update n={n}: {:?}", started.elapsed());
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn full_snapshot_noop_batch_is_noop() {
        let mut conn = setup_conn();
        let tx = conn.transaction().unwrap();
        let mut forest = LargeSmtForest::new(SqliteForestBackend::new(&tx)).unwrap();
        let initial = [(wp(7, 1), w(100)), (wp(9, 2), w(200)), (wp(11, 3), w(300))];
        forest.add_lineages(1, batch(lid(1), &initial)).unwrap();
        let root_before = require_tree_meta(&tx, lid(1)).unwrap().1;

        // A snapshot-shaped batch: every stored pair resubmitted unchanged, plus one change.
        let computed = compute_update_mutations(
            &tx,
            lid(1),
            root_before,
            3,
            initial.iter().copied().chain([(wp(13, 4), w(400))]),
        )
        .unwrap();
        assert_eq!(computed.forward.new_pairs().len(), 1);
        assert_eq!(computed.entry_count_delta, 1);

        // An entirely unchanged snapshot produces an empty (no-op) mutation set.
        let computed =
            compute_update_mutations(&tx, lid(1), root_before, 3, initial.iter().copied()).unwrap();
        assert!(computed.forward.is_empty());
        assert_eq!(computed.forward.root(), root_before);
        forest.update_forest(2, batch(lid(1), &initial)).unwrap();
        assert_eq!(require_tree_meta(&tx, lid(1)).unwrap().1, root_before);
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
