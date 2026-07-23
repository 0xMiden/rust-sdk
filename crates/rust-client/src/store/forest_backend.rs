//! Generic row-oriented storage backend for [`LargeSmtForest`].
//!
//! [`RowForestBackend`] implements the forest [`Backend`] contract on top of a
//! [`ForestRowStore`], a small synchronous interface over three logical row collections (tree
//! metadata, key-value entries, and 8-level subtree blobs). Store implementations only translate
//! point reads and row writes to their storage engine; all Merkle computation, validation, and
//! write planning is shared here.
//!
//! Trees are stored per lineage as their full set of key-value entries, their inner nodes packed
//! as 8-level subtree blobs (the same layout as miden-crypto's persistent forest backend), and a
//! metadata row (latest version, root, and entry count). Witness reads load one leaf plus the
//! eight subtree blobs on its path, so their cost is independent of the tree size. Mutations load
//! the affected lineage's data on demand, so memory usage is bounded by the trees touched by an
//! operation rather than by the total account state.
//!
//! Every read the backend performs is keyed by the lineage, a key, a leaf position, or a subtree
//! root index, all of which derive deterministically from the queried key or update batch. Row
//! stores that cannot read synchronously (for example asynchronous browser storage) can therefore
//! load the required rows ahead of an operation and serve them from memory.
//!
//! [`LargeSmtForest`]: miden_protocol::crypto::merkle::smt::LargeSmtForest

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::vec::Vec;
use core::fmt;

use miden_protocol::crypto::merkle::smt::{
    AppliedLineageMutation,
    Backend,
    BackendReader,
    InnerNode,
    LeafIndex,
    LineageMutation,
    LineageMutationKind,
    MAX_LEAF_ENTRIES,
    MutationSet,
    NodeMutation,
    SMT_DEPTH,
    Smt,
    SmtLeaf,
    SmtProof,
    Subtree,
    TreeEntry,
};
// The crypto types appearing in this module's API surface, re-exported so row-store
// implementations in other crates need no direct miden-crypto dependency.
pub use miden_protocol::crypto::merkle::smt::{
    BackendError,
    LineageId,
    SmtForestUpdateBatch,
    TreeWithRoot,
    VersionId,
};
use miden_protocol::crypto::merkle::{EmptySubtreeRoots, MerkleError, NodeIndex, SparseMerklePath};
use miden_protocol::{EMPTY_WORD, Word};

type Result<T> = core::result::Result<T, BackendError>;
type SmtMutationSet = MutationSet<SMT_DEPTH, Word, Word>;

// ROW TYPES
// ================================================================================================

/// Metadata row of a lineage's latest tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForestTreeMeta {
    /// Latest version of the lineage.
    pub version: VersionId,
    /// Root of the latest tree.
    pub root: Word,
    /// Number of key-value entries in the latest tree.
    pub entry_count: usize,
}

/// One stored key-value entry of a lineage, together with its stored leaf position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForestEntryRow {
    /// SMT key of the entry.
    pub key: Word,
    /// Value stored under the key. Never the empty word; removals delete the row.
    pub value: Word,
    /// Leaf position the row is stored under. Must match the position derived from the key.
    pub leaf_position: u64,
}

// ROW STORE
// ================================================================================================

/// Synchronous row storage for [`RowForestBackend`].
///
/// Implementations persist three logical collections per lineage, all keyed as noted on each
/// method. Reads must reflect every write previously performed through the same store value, so
/// that later forest operations within one logical store operation observe earlier uncommitted
/// writes.
///
/// Reads are strict. A store that serves rows from a preloaded cache must return an error (via
/// [`BackendError::Internal`]) for rows it cannot answer authoritatively, rather than reporting
/// them as absent, because the backend treats an absent row as valid empty state and would
/// otherwise compute an incorrect tree.
///
/// Stores used with [`RowForestBackend`]'s [`Backend`] implementation must additionally be
/// [`Clone`], where a clone is a cheap handle onto the same underlying rows (a live view, not a
/// snapshot): the backend's reader observes writes performed through the store it was cloned
/// from, matching the same-transaction visibility documented on [`RowForestBackendReader`].
pub trait ForestRowStore {
    /// Returns the metadata row of a lineage, or `None` if the lineage is unknown.
    fn tree_meta(&self, lineage: LineageId) -> Result<Option<ForestTreeMeta>>;

    /// Returns the metadata of all stored lineages.
    fn trees(&self) -> Result<Vec<TreeWithRoot>>;

    /// Returns the value stored under `key` in a lineage, or `None` if no row exists.
    fn entry_value(&self, lineage: LineageId, key: Word) -> Result<Option<Word>>;

    /// Returns the key-value entries stored at the given leaf position of a lineage, in any
    /// order.
    fn leaf_entries(&self, lineage: LineageId, position: u64) -> Result<Vec<(Word, Word)>>;

    /// Streams all entry rows of a lineage, in any order, calling `f` for each row and stopping
    /// at the first error.
    ///
    /// A callback instead of a returned collection lets stores stream rows from their storage
    /// engine (or iterate an in-memory snapshot) without materializing the full lineage twice on
    /// the bulk-update path.
    fn for_each_entry(
        &self,
        lineage: LineageId,
        f: &mut dyn FnMut(ForestEntryRow) -> Result<()>,
    ) -> Result<()>;

    /// Returns the serialized subtree blob rooted at (`depth`, `position`) of a lineage, or
    /// `None` if no blob is stored there.
    fn subtree_blob(&self, lineage: LineageId, depth: u8, position: u64)
    -> Result<Option<Vec<u8>>>;

    /// Inserts or replaces the entry row for `key` of a lineage.
    fn upsert_entry(
        &mut self,
        lineage: LineageId,
        key: Word,
        value: Word,
        leaf_position: u64,
    ) -> Result<()>;

    /// Deletes the entry row for `key` of a lineage, if present.
    fn delete_entry(&mut self, lineage: LineageId, key: Word) -> Result<()>;

    /// Inserts or replaces the subtree blob rooted at (`depth`, `position`) of a lineage.
    fn upsert_subtree(
        &mut self,
        lineage: LineageId,
        depth: u8,
        position: u64,
        blob: Vec<u8>,
    ) -> Result<()>;

    /// Deletes the subtree blob rooted at (`depth`, `position`) of a lineage, if present.
    fn delete_subtree(&mut self, lineage: LineageId, depth: u8, position: u64) -> Result<()>;

    /// Inserts or replaces the metadata row of a lineage.
    fn upsert_tree_meta(&mut self, lineage: LineageId, meta: ForestTreeMeta) -> Result<()>;

    /// Runs a validate-then-write group atomically.
    ///
    /// The backend validates its prepared mutations against current state inside `body` before
    /// writing, so the store must guarantee that the observed state cannot change between that
    /// validation and the writes, and that no write performed by `body` remains visible if it
    /// returns an error. Stores holding an isolated transaction map this to a savepoint (or
    /// no-op); an in-memory staging cache may run `body` directly when its callers discard the
    /// whole cache on error.
    fn write_atomically<T>(&mut self, body: impl FnOnce(&mut Self) -> Result<T>) -> Result<T>
    where
        Self: Sized;
}

// BACKEND
// ================================================================================================

/// A [`LargeSmtForest`] backend over a [`ForestRowStore`].
///
/// [`LargeSmtForest`]: miden_protocol::crypto::merkle::smt::LargeSmtForest
#[derive(Clone, Copy)]
pub struct RowForestBackend<S> {
    store: S,
}

impl<S: ForestRowStore> RowForestBackend<S> {
    /// Creates a backend over the provided row store.
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

impl<S> fmt::Debug for RowForestBackend<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RowForestBackend").finish_non_exhaustive()
    }
}

/// Read-only view over the same row store.
///
/// A separate type because the [`Backend::Reader`] contract requires a view that implements
/// [`BackendReader`] but not [`Backend`]; every method delegates to the wrapped backend. The
/// view observes the store's current (uncommitted) state, intentionally, so that later forest
/// queries within a store operation see earlier writes of the same operation. This deviates
/// from the upstream contract's point-in-time snapshot wording (like the no-IO wording on
/// `entry_count`); both deviations are safe for backends whose forests live only inside a
/// single store operation, and are raised in the upstream API discussion.
pub struct RowForestBackendReader<S>(RowForestBackend<S>);

impl<S> fmt::Debug for RowForestBackendReader<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RowForestBackendReader").finish_non_exhaustive()
    }
}

/// Backend-prepared data for two-phase mutations: one forward SMT mutation set per touched
/// lineage.
pub struct RowPreparedMutations {
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

// READ HELPERS
// ================================================================================================

fn require_tree_meta<S: ForestRowStore>(store: &S, lineage: LineageId) -> Result<ForestTreeMeta> {
    store.tree_meta(lineage)?.ok_or(BackendError::UnknownLineage(lineage))
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
/// Entries are sorted by key because a multi-entry leaf's hash is order-sensitive and row order
/// is unspecified; sorting by [`Word`] matches the canonical order the SMT maintains inside its
/// leaves.
fn load_leaf_entries<S: ForestRowStore>(
    store: &S,
    lineage: LineageId,
    position: u64,
) -> Result<Vec<(Word, Word)>> {
    let mut entries = store.leaf_entries(lineage, position)?;
    for (key, value) in &entries {
        require_non_empty_value(lineage, *key, *value)?;
        require_consistent_position(lineage, *key, position)?;
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
fn load_leaf<S: ForestRowStore>(
    store: &S,
    lineage: LineageId,
    leaf_index: LeafIndex<SMT_DEPTH>,
) -> Result<SmtLeaf> {
    let entries = load_leaf_entries(store, lineage, leaf_index.position())?;
    leaf_from_entries(lineage, leaf_index, entries)
}

/// Loads the subtree blob rooted at `root_index`, or an empty subtree if none is stored.
fn load_subtree<S: ForestRowStore>(
    store: &S,
    lineage: LineageId,
    root_index: NodeIndex,
) -> Result<Subtree> {
    let blob = store.subtree_blob(lineage, root_index.depth(), root_index.position())?;
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
fn compute_merkle_path<S: ForestRowStore>(
    store: &S,
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
            subtree = Some(load_subtree(store, lineage, root_index)?);
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
    /// Net change in the lineage's key count when `forward` is applied.
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
#[allow(clippy::too_many_lines)]
fn compute_update_mutations<S: ForestRowStore>(
    store: &S,
    lineage: LineageId,
    old_root: Word,
    entry_count: usize,
    kv_ops: impl Iterator<Item = (Word, Word)>,
) -> Result<ComputedLineageMutations> {
    let kv_ops: Vec<(Word, Word)> = kv_ops.collect();

    // Stored subtrees on touched paths; never mutated during compute, so lookups through this
    // cache always observe the pre-update state.
    let mut subtrees: BTreeMap<NodeIndex, Subtree> = BTreeMap::new();
    // Effective (batch-mutated) sorted entries of each touched leaf. Small batches load each
    // touched leaf with a point query; batches comparable to the tree size (full-state
    // presentations) load every leaf in one scan instead, which is far cheaper than one point
    // query per op. When bulk-loaded, a position absent from the map is a stored-empty leaf.
    let mut leaves: BTreeMap<u64, Vec<(Word, Word)>> = BTreeMap::new();
    let bulk_loaded = takes_bulk_path(kv_ops.len(), entry_count);
    if bulk_loaded {
        store.for_each_entry(lineage, &mut |row| {
            require_non_empty_value(lineage, row.key, row.value)?;
            require_consistent_position(lineage, row.key, row.leaf_position)?;
            leaves.entry(row.leaf_position).or_default().push((row.key, row.value));
            Ok(())
        })?;
        for entries in leaves.values_mut() {
            entries.sort_by_key(|(key, _)| *key);
        }
    }
    let mut forward_nodes: BTreeMap<NodeIndex, NodeMutation> = BTreeMap::new();
    // The stored node at each mutated index, captured once before any overlay, for the reverse
    // set.
    let mut original_nodes: BTreeMap<NodeIndex, Option<InnerNode>> = BTreeMap::new();
    let mut forward_pairs: Vec<(Word, Word)> = Vec::new();
    let mut reverse_pairs: Vec<(Word, Word)> = Vec::new();
    let mut seen_keys: BTreeSet<Word> = BTreeSet::new();
    // Leaves whose stored path has been authenticated against `old_root`.
    let mut verified_leaves: BTreeSet<u64> = BTreeSet::new();
    let mut new_root = old_root;
    let mut entry_count_delta: i64 = 0;

    let stored_inner_node = |subtrees: &mut BTreeMap<NodeIndex, Subtree>,
                             index: NodeIndex|
     -> Result<Option<InnerNode>> {
        let root_index = Subtree::find_subtree_root(index);
        let subtree = match subtrees.entry(root_index) {
            alloc::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(load_subtree(store, lineage, root_index)?)
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

        if let alloc::collections::btree_map::Entry::Vacant(entry) = leaves.entry(position) {
            let entries = if bulk_loaded {
                Vec::new()
            } else {
                load_leaf_entries(store, lineage, position)?
            };
            entry.insert(entries);
        }
        let entries = leaves.get_mut(&position).expect("leaf loaded above");

        let old_value = entries.iter().find(|(k, _)| *k == key).map_or(EMPTY_WORD, |(_, v)| *v);
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
    let mut reverse_nodes: BTreeMap<NodeIndex, NodeMutation> = BTreeMap::new();
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

// WRITE HELPERS
// ================================================================================================

/// Writes the changed key-value pairs of a forward mutation set to the entry rows. Values equal
/// to the empty word are deletions.
fn write_pairs<S: ForestRowStore>(
    store: &mut S,
    lineage: LineageId,
    forward: &SmtMutationSet,
) -> Result<()> {
    for (key, value) in forward.new_pairs() {
        if *value == EMPTY_WORD {
            store.delete_entry(lineage, *key)?;
        } else {
            let leaf_position = LeafIndex::<SMT_DEPTH>::from(*key).position();
            store.upsert_entry(lineage, *key, *value, leaf_position)?;
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
fn write_subtrees<S: ForestRowStore>(
    store: &mut S,
    lineage: LineageId,
    forward: &SmtMutationSet,
) -> Result<()> {
    let mut groups: BTreeMap<(u8, u64), Vec<(&NodeIndex, &NodeMutation)>> = BTreeMap::new();
    for (index, mutation) in forward.node_mutations() {
        let root_index = Subtree::find_subtree_root(*index);
        groups
            .entry((root_index.depth(), root_index.position()))
            .or_default()
            .push((index, mutation));
    }

    for ((depth, position), mutations) in groups {
        let root_index =
            NodeIndex::new(depth, position).expect("subtree root computed from a valid node index");
        let mut subtree = load_subtree(store, lineage, root_index)?;

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
            store.delete_subtree(lineage, depth, position)?;
        } else {
            store.upsert_subtree(lineage, depth, position, subtree.to_vec())?;
        }
    }
    Ok(())
}

// BACKEND READER
// ================================================================================================

impl<S: ForestRowStore> BackendReader for RowForestBackend<S> {
    fn open(&self, lineage: LineageId, key: Word) -> Result<SmtProof> {
        let stored_root = require_tree_meta(&self.store, lineage)?.root;

        let leaf_index = LeafIndex::<SMT_DEPTH>::from(key);
        let leaf = load_leaf(&self.store, lineage, leaf_index)?;
        let path = compute_merkle_path(&self.store, lineage, leaf_index.into())?;

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
        require_tree_meta(&self.store, lineage)?;
        load_leaf(&self.store, lineage, leaf_index)
    }

    fn get(&self, lineage: LineageId, key: Word) -> Result<Option<Word>> {
        require_tree_meta(&self.store, lineage)?;
        self.store
            .entry_value(lineage, key)?
            .map(|value| {
                require_non_empty_value(lineage, key, value)?;
                Ok(value)
            })
            .transpose()
    }

    fn version(&self, lineage: LineageId) -> Result<VersionId> {
        Ok(require_tree_meta(&self.store, lineage)?.version)
    }

    fn lineages(&self) -> Result<impl Iterator<Item = LineageId>> {
        Ok(self.store.trees()?.into_iter().map(|t| t.lineage()))
    }

    fn trees(&self) -> Result<impl Iterator<Item = TreeWithRoot>> {
        Ok(self.store.trees()?.into_iter())
    }

    fn entry_count(&self, lineage: LineageId) -> Result<usize> {
        Ok(require_tree_meta(&self.store, lineage)?.entry_count)
    }

    fn entries(&self, lineage: LineageId) -> Result<impl Iterator<Item = Result<TreeEntry>>> {
        require_tree_meta(&self.store, lineage)?;
        let mut entries = Vec::new();
        self.store.for_each_entry(lineage, &mut |row| {
            require_non_empty_value(lineage, row.key, row.value)?;
            entries.push((row.key, row.value));
            Ok(())
        })?;
        Ok(entries.into_iter().map(|(key, value)| Ok(TreeEntry { key, value })))
    }
}

impl<S: ForestRowStore> BackendReader for RowForestBackendReader<S> {
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

impl<S: ForestRowStore + Clone> Backend for RowForestBackend<S> {
    type Reader = RowForestBackendReader<S>;
    type PreparedMutations = RowPreparedMutations;

    fn reader(&self) -> Result<Self::Reader> {
        Ok(RowForestBackendReader(RowForestBackend { store: self.store.clone() }))
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
            let (old_version, kind, computed) = if let Some(meta) = self.store.tree_meta(lineage)? {
                // Path-local computation: reads only the affected leaves and the subtree
                // blobs on their paths, so cost scales with the change set.
                let computed = compute_update_mutations(
                    &self.store,
                    lineage,
                    meta.root,
                    meta.entry_count,
                    kv_ops,
                )?;
                (Some(meta.version), LineageMutationKind::UpdateTree, computed)
            } else {
                // A new lineage starts from the empty tree, so there is no stored state to
                // read and the in-memory computation is already proportional to the batch.
                let forward = Smt::new().compute_mutations(kv_ops)?;
                let computed = ComputedLineageMutations {
                    forward,
                    reverse: SmtMutationSet::default(),
                    entry_count_delta: 0,
                };
                (None, LineageMutationKind::AddLineage, computed)
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

        Ok((mutations, RowPreparedMutations { entries: prepared }))
    }

    fn apply_mutations(
        &mut self,
        mutations: Self::PreparedMutations,
    ) -> Result<Vec<AppliedLineageMutation>> {
        // Both validation and the writes run inside the store's atomic scope: validation must
        // observe the same state the writes apply to, and write_subtrees can fail on corrupted
        // blobs partway through, so the scope keeps the whole application atomic even for
        // callers that catch the error and keep writing through the same store.
        self.store.write_atomically(|store| {
            validate_prepared_mutations(store, &mutations)?;
            apply_validated_mutations(store, mutations)
        })
    }
}

/// Validates prepared mutations against the store's current state, so user-derived errors are
/// reported before anything is written and leave the backend consistent.
fn validate_prepared_mutations<S: ForestRowStore>(
    store: &S,
    mutations: &RowPreparedMutations,
) -> Result<()> {
    for p in &mutations.entries {
        match p.kind {
            LineageMutationKind::AddLineage => {
                if store.tree_meta(p.lineage)?.is_some() {
                    return Err(BackendError::DuplicateLineage(p.lineage));
                }
            },
            LineageMutationKind::UpdateTree => {
                let meta = require_tree_meta(store, p.lineage)?;
                if Some(meta.version) != p.old_version {
                    return Err(BackendError::BadVersion {
                        provided: p.old_version.unwrap_or_default(),
                        latest: meta.version,
                    });
                }
                if meta.root != p.forward.old_root() {
                    // Stale prepared mutations are a user-derived error, matching the
                    // in-memory backend's classification.
                    return Err(BackendError::Merkle(MerkleError::ConflictingRoots {
                        expected_root: p.forward.old_root(),
                        actual_root: meta.root,
                    }));
                }
            },
        }
    }
    Ok(())
}

/// Applies already-validated prepared mutations. Must run inside an atomic write scope so a
/// mid-application error does not leave partial writes visible.
fn apply_validated_mutations<S: ForestRowStore>(
    store: &mut S,
    mutations: RowPreparedMutations,
) -> Result<Vec<AppliedLineageMutation>> {
    let mut applied = Vec::with_capacity(mutations.entries.len());
    for p in mutations.entries {
        let old_root = p.forward.old_root();
        let new_root = p.forward.root();

        match p.kind {
            LineageMutationKind::AddLineage => {
                write_pairs(store, p.lineage, &p.forward)?;
                write_subtrees(store, p.lineage, &p.forward)?;
                let entry_count =
                    p.forward.new_pairs().values().filter(|v| **v != EMPTY_WORD).count();
                store.upsert_tree_meta(
                    p.lineage,
                    ForestTreeMeta {
                        version: p.new_version,
                        root: new_root,
                        entry_count,
                    },
                )?;

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
                    let old_count = require_tree_meta(store, p.lineage)?.entry_count;
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

                let old_count = require_tree_meta(store, p.lineage)?.entry_count;
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

                write_pairs(store, p.lineage, &p.forward)?;
                write_subtrees(store, p.lineage, &p.forward)?;
                store.upsert_tree_meta(
                    p.lineage,
                    ForestTreeMeta {
                        version: p.new_version,
                        root: new_root,
                        entry_count: new_count,
                    },
                )?;

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

// PREFETCH PLANNING
// ================================================================================================
//
// Stores that cannot read synchronously (such as asynchronous browser storage) load rows ahead
// of an operation and serve them from a strict in-memory cache. The functions below enumerate
// the rows an operation reads, and live next to the algorithm so the plan and the reads cannot
// drift apart. The complete read contract is:
//
// - Forest construction reads all tree metadata rows (`ForestRowStore::trees`), so the cache must
//   hold the complete metadata snapshot; a lineage absent from that snapshot is authoritatively
//   absent, and every row of such a lineage may be answered as absent without prefetching
//   (additions compute against the empty tree, but applying them still reads the subtree blobs and
//   metadata of the new lineage).
// - A witness or leaf read for a key loads the key's complete leaf bucket and the eight subtree
//   blobs on its path ([`plan_witness_read`]); an exact `get` reads only the entry row.
// - An update batch reads, per touched key of an existing lineage, the same bucket and path blobs;
//   lineages taking the bulk-load path stream their complete entry set instead of buckets, but
//   still read the path blobs of every touched key ([`plan_update`]). Batches are normalized to the
//   last operation per key before any read.

/// Depth of one packed subtree blob band.
const SUBTREE_DEPTH: u8 = 8;

/// Batch sizes above this take the bulk-load path.
const BULK_MIN_OPS: usize = 64;

/// Returns `true` when a batch of `op_count` normalized operations on a lineage with
/// `entry_count` stored entries loads the whole lineage instead of per-leaf buckets.
///
/// One definition shared by the computation and the prefetch planner, so a heuristic change
/// cannot turn a correct prefetch into a runtime cache miss.
fn takes_bulk_path(op_count: usize, entry_count: usize) -> bool {
    op_count > BULK_MIN_OPS && op_count * 4 >= entry_count
}

/// The rows one forest operation reads, keyed the same way [`ForestRowStore`] methods are.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ForestPrefetchPlan {
    /// Complete leaf buckets, per (lineage, leaf position).
    pub buckets: BTreeSet<(LineageId, u64)>,
    /// Subtree blobs, per (lineage, depth, position).
    pub subtrees: BTreeSet<(LineageId, u8, u64)>,
    /// Lineages whose complete entry set is streamed (bulk-load path).
    pub full_lineages: BTreeSet<LineageId>,
}

impl ForestPrefetchPlan {
    /// Returns `true` if the plan names no rows.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty() && self.subtrees.is_empty() && self.full_lineages.is_empty()
    }
}

/// Returns the leaf position of an SMT key.
pub fn key_leaf_position(key: Word) -> u64 {
    LeafIndex::<SMT_DEPTH>::from(key).position()
}

/// Adds the leaf bucket and the eight path subtree blobs of `key` to the plan.
fn add_key_path(plan: &mut ForestPrefetchPlan, lineage: LineageId, key: Word) {
    let position = key_leaf_position(key);
    plan.buckets.insert((lineage, position));
    add_path_subtrees(plan, lineage, position);
}

/// Adds the eight subtree blobs on the path of the leaf at `position` to the plan.
///
/// The subtree roots on a leaf's path sit at depths 0, 8, ..., 56, at the position of the
/// leaf's ancestor at each of those depths.
fn add_path_subtrees(plan: &mut ForestPrefetchPlan, lineage: LineageId, position: u64) {
    let mut depth = 0u8;
    while depth < SMT_DEPTH {
        let ancestor = if depth == 0 { 0 } else { position >> (SMT_DEPTH - depth) };
        plan.subtrees.insert((lineage, depth, ancestor));
        depth += SUBTREE_DEPTH;
    }
}

/// Plans the rows needed to open a witness (or read a leaf) for `key` in `lineage`.
pub fn plan_witness_read(lineage: LineageId, key: Word) -> ForestPrefetchPlan {
    let mut plan = ForestPrefetchPlan::default();
    add_key_path(&mut plan, lineage, key);
    plan
}

/// Plans the rows needed to compute and apply `batch`.
///
/// `tree_meta` is the complete lineage metadata snapshot, used to distinguish additions (which
/// read no stored rows beyond the metadata snapshot) from updates and to evaluate the bulk-load
/// heuristic on the same normalized operation count the computation later sees.
pub fn plan_update(
    batch: SmtForestUpdateBatch,
    tree_meta: &BTreeMap<LineageId, ForestTreeMeta>,
) -> ForestPrefetchPlan {
    let mut plan = ForestPrefetchPlan::default();

    for (lineage, ops) in batch {
        let Some(meta) = tree_meta.get(&lineage) else {
            continue;
        };

        if takes_bulk_path(ops.len(), meta.entry_count) {
            plan.full_lineages.insert(lineage);
            // Path subtrees are still read per touched leaf during compute and apply.
            for op in &ops {
                add_path_subtrees(&mut plan, lineage, key_leaf_position(op.key()));
            }
        } else {
            for op in &ops {
                add_key_path(&mut plan, lineage, op.key());
            }
        }
    }

    plan
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::rc::Rc;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use miden_protocol::crypto::merkle::smt::{LargeSmtForest, TreeId};
    use miden_protocol::{Felt, ONE, ZERO};

    use super::*;

    /// Minimal in-memory row store, serving as the reference implementation of the trait. Rows
    /// live behind a shared handle so the reader clones required by [`Backend::reader`] observe
    /// the same state.
    #[derive(Default, Clone)]
    struct MemoryRows(Rc<RefCell<MemoryRowsInner>>);

    #[derive(Default)]
    struct MemoryRowsInner {
        trees: BTreeMap<LineageId, ForestTreeMeta>,
        entries: BTreeMap<(LineageId, Word), (Word, u64)>,
        subtrees: BTreeMap<(LineageId, u8, u64), Vec<u8>>,
    }

    impl fmt::Debug for MemoryRows {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("MemoryRows").finish_non_exhaustive()
        }
    }

    impl ForestRowStore for MemoryRows {
        fn tree_meta(&self, lineage: LineageId) -> Result<Option<ForestTreeMeta>> {
            Ok(self.0.borrow().trees.get(&lineage).copied())
        }

        fn trees(&self) -> Result<Vec<TreeWithRoot>> {
            Ok(self
                .0
                .borrow()
                .trees
                .iter()
                .map(|(lineage, meta)| TreeWithRoot::new(*lineage, meta.version, meta.root))
                .collect())
        }

        fn entry_value(&self, lineage: LineageId, key: Word) -> Result<Option<Word>> {
            Ok(self.0.borrow().entries.get(&(lineage, key)).map(|(value, _)| *value))
        }

        fn leaf_entries(&self, lineage: LineageId, position: u64) -> Result<Vec<(Word, Word)>> {
            Ok(self
                .0
                .borrow()
                .entries
                .iter()
                .filter(|((l, _), (_, p))| *l == lineage && *p == position)
                .map(|((_, key), (value, _))| (*key, *value))
                .collect())
        }

        fn for_each_entry(
            &self,
            lineage: LineageId,
            f: &mut dyn FnMut(ForestEntryRow) -> Result<()>,
        ) -> Result<()> {
            let rows: Vec<ForestEntryRow> = self
                .0
                .borrow()
                .entries
                .iter()
                .filter(|((l, _), _)| *l == lineage)
                .map(|((_, key), (value, leaf_position))| ForestEntryRow {
                    key: *key,
                    value: *value,
                    leaf_position: *leaf_position,
                })
                .collect();
            for row in rows {
                f(row)?;
            }
            Ok(())
        }

        fn subtree_blob(
            &self,
            lineage: LineageId,
            depth: u8,
            position: u64,
        ) -> Result<Option<Vec<u8>>> {
            Ok(self.0.borrow().subtrees.get(&(lineage, depth, position)).cloned())
        }

        fn upsert_entry(
            &mut self,
            lineage: LineageId,
            key: Word,
            value: Word,
            leaf_position: u64,
        ) -> Result<()> {
            self.0.borrow_mut().entries.insert((lineage, key), (value, leaf_position));
            Ok(())
        }

        fn delete_entry(&mut self, lineage: LineageId, key: Word) -> Result<()> {
            self.0.borrow_mut().entries.remove(&(lineage, key));
            Ok(())
        }

        fn upsert_subtree(
            &mut self,
            lineage: LineageId,
            depth: u8,
            position: u64,
            blob: Vec<u8>,
        ) -> Result<()> {
            self.0.borrow_mut().subtrees.insert((lineage, depth, position), blob);
            Ok(())
        }

        fn delete_subtree(&mut self, lineage: LineageId, depth: u8, position: u64) -> Result<()> {
            self.0.borrow_mut().subtrees.remove(&(lineage, depth, position));
            Ok(())
        }

        fn upsert_tree_meta(&mut self, lineage: LineageId, meta: ForestTreeMeta) -> Result<()> {
            self.0.borrow_mut().trees.insert(lineage, meta);
            Ok(())
        }

        // Correct for tests only because they never keep using a store after a failed apply;
        // a real store must roll partial writes back (see the trait docs).
        fn write_atomically<T>(&mut self, body: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
            body(self)
        }
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
    fn add_update_and_open_match_reference_smt() {
        let rows = MemoryRows::default();
        let mut forest = LargeSmtForest::new(RowForestBackend::new(rows.clone())).unwrap();

        forest
            .add_lineages(1, batch(lid(1), &[(w(10), w(100)), (w(20), w(200))]))
            .unwrap();
        forest
            .update_forest(2, batch(lid(1), &[(w(10), w(111)), (w(30), w(300))]))
            .unwrap();

        let reference =
            Smt::with_entries([(w(10), w(111)), (w(20), w(200)), (w(30), w(300))]).unwrap();
        let proof = forest.open(TreeId::new(lid(1), 2), w(30)).unwrap();
        assert!(proof.verify_presence(&w(30), &w(300), &reference.root()).is_ok());

        // A forest constructed over the same rows serves the same state, so persistence does not
        // depend on the forest instance that produced the writes.
        let reopened = LargeSmtForest::new(RowForestBackend::new(rows)).unwrap();
        let proof = reopened.open(TreeId::new(lid(1), 2), w(10)).unwrap();
        assert!(proof.verify_presence(&w(10), &w(111), &reference.root()).is_ok());
    }

    #[test]
    fn stale_mutations_rejected() {
        let rows = MemoryRows::default();
        let mut forest = LargeSmtForest::new(RowForestBackend::new(rows)).unwrap();
        forest.add_lineages(1, batch(lid(1), &[(w(10), w(100))])).unwrap();

        let stale = forest.compute_forest_mutations(2, batch(lid(1), &[(w(20), w(200))])).unwrap();
        forest.update_forest(2, batch(lid(1), &[(w(30), w(300))])).unwrap();

        assert!(forest.apply_mutations(stale).is_err());
    }

    /// Builds a key whose leaf position is `pos` (the most significant felt determines the
    /// leaf).
    fn wp(pos: u64, n: u64) -> Word {
        Word::from([Felt::new(n).unwrap(), ZERO, ZERO, Felt::new(pos).unwrap()])
    }

    fn tree_root(rows: &MemoryRows, lineage: LineageId) -> Word {
        rows.tree_meta(lineage).unwrap().expect("lineage exists").root
    }

    fn tree_count(rows: &MemoryRows, lineage: LineageId) -> usize {
        rows.tree_meta(lineage).unwrap().expect("lineage exists").entry_count
    }

    #[test]
    fn computed_mutations_match_reference_smt_exactly() {
        let rows = MemoryRows::default();
        let mut forest = LargeSmtForest::new(RowForestBackend::new(rows.clone())).unwrap();

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
            let old_count = tree_count(&rows, lid(1));

            let computed = compute_update_mutations(
                &rows,
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
            assert_eq!(tree_root(&rows, lid(1)), reference.root());
            let count = tree_count(&rows, lid(1));
            assert_eq!(count, reference.num_entries());
            assert_eq!(
                i64::try_from(count).unwrap() - i64::try_from(old_count).unwrap(),
                computed.entry_count_delta
            );
        }
    }

    #[test]
    fn reverse_pairs_restore_previous_root() {
        let rows = MemoryRows::default();
        let mut forest = LargeSmtForest::new(RowForestBackend::new(rows.clone())).unwrap();
        let initial = [(wp(7, 1), w(100)), (wp(7, 2), w(200)), (wp(9, 3), w(300))];
        forest.add_lineages(1, batch(lid(1), &initial)).unwrap();
        let root_before = tree_root(&rows, lid(1));

        let ops = [(wp(7, 1), w(111)), (wp(9, 3), EMPTY_WORD), (wp(5, 4), w(400))];
        let computed =
            compute_update_mutations(&rows, lid(1), root_before, 3, ops.iter().copied()).unwrap();

        let mut b = SmtForestUpdateBatch::empty();
        b.operations(lid(1)).add_insert(wp(7, 1), w(111));
        b.operations(lid(1)).add_remove(wp(9, 3));
        b.operations(lid(1)).add_insert(wp(5, 4), w(400));
        forest.update_forest(2, b).unwrap();
        assert_eq!(tree_root(&rows, lid(1)), computed.forward.root());

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
        assert_eq!(tree_root(&rows, lid(1)), root_before);
        for (key, value) in initial {
            assert_eq!(RowForestBackend::new(rows.clone()).get(lid(1), key).unwrap(), Some(value));
        }
    }

    #[test]
    fn bulk_loaded_snapshot_matches_reference_smt() {
        let rows = MemoryRows::default();
        let mut forest = LargeSmtForest::new(RowForestBackend::new(rows.clone())).unwrap();

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
            compute_update_mutations(&rows, lid(1), reference.root(), 100, ops.iter().copied())
                .unwrap();
        let forward_ref = reference.compute_mutations(ops.iter().copied()).unwrap();
        let reverse_ref = reference.apply_mutations_with_reversion(forward_ref.clone()).unwrap();
        assert_eq!(computed.forward, forward_ref);
        assert_eq!(computed.reverse, reverse_ref);
        assert_eq!(computed.forward.new_pairs().len(), 3);
        assert_eq!(computed.entry_count_delta, 0);
    }

    #[test]
    fn full_snapshot_noop_batch_is_noop() {
        let rows = MemoryRows::default();
        let mut forest = LargeSmtForest::new(RowForestBackend::new(rows.clone())).unwrap();
        let initial = [(wp(7, 1), w(100)), (wp(9, 2), w(200)), (wp(11, 3), w(300))];
        forest.add_lineages(1, batch(lid(1), &initial)).unwrap();
        let root_before = tree_root(&rows, lid(1));

        // A snapshot-shaped batch: every stored pair resubmitted unchanged, plus one change.
        let computed = compute_update_mutations(
            &rows,
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
            compute_update_mutations(&rows, lid(1), root_before, 3, initial.iter().copied())
                .unwrap();
        assert!(computed.forward.is_empty());
        assert_eq!(computed.forward.root(), root_before);
        forest.update_forest(2, batch(lid(1), &initial)).unwrap();
        assert_eq!(tree_root(&rows, lid(1)), root_before);
    }

    #[test]
    fn update_plan_covers_all_compute_and_apply_reads() {
        let rows = MemoryRows::default();
        let mut forest = LargeSmtForest::new(RowForestBackend::new(rows.clone())).unwrap();
        let initial = [(wp(7, 1), w(100)), (wp(9, 3), w(300))];
        forest.add_lineages(1, batch(lid(1), &initial)).unwrap();

        let meta = rows.tree_meta(lid(1)).unwrap().expect("lineage exists");
        let mut tree_meta = BTreeMap::new();
        tree_meta.insert(lid(1), meta);

        // Non-bulk: one touched existing leaf and one fresh leaf; both buckets and both paths
        // (plus a second lineage that is an addition, contributing nothing).
        let mut b = SmtForestUpdateBatch::empty();
        b.operations(lid(1)).add_insert(wp(7, 1), w(111));
        b.operations(lid(1)).add_insert(wp(5, 4), w(400));
        b.operations(lid(2)).add_insert(w(1), w(1));
        let plan = plan_update(b, &tree_meta);
        assert_eq!(plan.buckets.len(), 2);
        assert!(plan.buckets.contains(&(lid(1), 7)) && plan.buckets.contains(&(lid(1), 5)));
        // Depth-0 subtree is shared by both paths; deeper bands may or may not be.
        assert!(plan.subtrees.contains(&(lid(1), 0, 0)));
        assert!(plan.full_lineages.is_empty());
        assert!(!plan.subtrees.iter().any(|(l, ..)| *l == lid(2)));

        // Bulk: a snapshot-sized batch flips the same lineage to full coverage.
        let mut b = SmtForestUpdateBatch::empty();
        for i in 0..=70u64 {
            b.operations(lid(1)).add_insert(wp(i, i + 1), w(i + 1));
        }
        let plan = plan_update(b, &tree_meta);
        assert!(plan.full_lineages.contains(&lid(1)));
        assert!(plan.buckets.is_empty());
        assert!(!plan.subtrees.is_empty());
    }
}
