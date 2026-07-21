use alloc::format;
use alloc::vec::Vec;

use miden_protocol::account::{AccountId, StorageMapKey, StorageMapWitness, StorageSlotName};
use miden_protocol::asset::{Asset, AssetId, AssetWitness};
use miden_protocol::crypto::merkle::MerkleError;
use miden_protocol::crypto::merkle::smt::{
    Backend,
    LargeSmtForest,
    LargeSmtForestError,
    LineageId,
    SmtForestUpdateBatch,
    TreeId,
    TreeWithRoot,
    VersionId,
};
use miden_protocol::utils::serde::Serializable;
use miden_protocol::{EMPTY_WORD, Hasher, Word};

use super::StoreError;

// LINEAGE DERIVATION
// ================================================================================================

/// Returns the lineage identifier for an account's asset vault SMT.
pub fn vault_lineage_id(account_id: AccountId) -> LineageId {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"miden-client:vault");
    bytes.extend_from_slice(&account_id.to_bytes());
    LineageId::new(Hasher::hash(&bytes).as_bytes())
}

/// Returns the lineage identifier for an account's storage map SMT in the given slot.
pub fn storage_map_lineage_id(account_id: AccountId, slot_name: &StorageSlotName) -> LineageId {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"miden-client:storage-map");
    bytes.extend_from_slice(&account_id.to_bytes());
    // Length-prefix the variable-sized slot name so distinct (id, name) pairs cannot produce
    // the same preimage. The fixed-width u64 keeps the identifier platform-independent.
    bytes.extend_from_slice(&(slot_name.as_str().len() as u64).to_le_bytes());
    bytes.extend_from_slice(slot_name.as_str().as_bytes());
    LineageId::new(Hasher::hash(&bytes).as_bytes())
}

// ACCOUNT SMT FOREST
// ================================================================================================

/// Account-oriented wrapper around [`LargeSmtForest`].
///
/// Account SMTs are tracked as lineages, one per account vault and one per storage map slot,
/// with identifiers derived deterministically from the account ID (and slot name). Each lineage
/// evolves through strictly increasing versions supplied by the caller.
///
/// The wrapper is generic over the forest storage [`Backend`], so persistence is decided by the
/// store that owns it. Construction only loads tree metadata from the backend, which makes
/// short-lived (per store operation) instances cheap.
pub struct AccountSmtForest<B: Backend> {
    forest: LargeSmtForest<B>,
}

impl<B: Backend> AccountSmtForest<B> {
    /// Creates a forest over the provided backend, loading tree metadata from it.
    pub fn new(backend: B) -> Result<Self, StoreError> {
        Ok(Self {
            forest: LargeSmtForest::new(backend).map_err(forest_error)?,
        })
    }

    // READERS
    // --------------------------------------------------------------------------------------------

    /// Returns the latest root of the given lineage, or `None` if the lineage is unknown.
    pub fn latest_root(&self, lineage: LineageId) -> Option<Word> {
        self.forest.latest_root(lineage)
    }

    /// Retrieves the vault asset and its witness for a specific vault key.
    ///
    /// The proof is opened against the latest tree of the account's vault lineage, after
    /// verifying that its root matches `expected_vault_root` (the root recorded in the account
    /// tables). A mismatch means forest and account state are out of sync and is reported as a
    /// conflicting-roots error.
    pub fn get_asset_and_witness(
        &self,
        account_id: AccountId,
        expected_vault_root: Word,
        asset_id: AssetId,
    ) -> Result<(Asset, AssetWitness), StoreError> {
        let lineage = vault_lineage_id(account_id);
        let tree = self.verified_latest_tree(lineage, expected_vault_root)?;

        let hashed_key: Word = asset_id.hash().into();
        let proof = self.forest.open(tree, hashed_key).map_err(forest_error)?;
        let asset_word = proof
            .get(&hashed_key)
            .ok_or(StoreError::VaultKeyNotTracked(asset_id, hashed_key))?;
        if asset_word == EMPTY_WORD {
            return Err(StoreError::VaultKeyNotTracked(asset_id, hashed_key));
        }

        let asset = Asset::from_id_and_value(asset_id, asset_word)?;
        let witness = AssetWitness::new(proof, [asset_id])?;
        Ok((asset, witness))
    }

    /// Retrieves the storage map witness for a specific map item.
    ///
    /// The proof is opened against the latest tree of the map's lineage, after verifying that
    /// its root matches `expected_map_root` (the root recorded in the account tables).
    pub fn get_storage_map_item_witness(
        &self,
        account_id: AccountId,
        slot_name: &StorageSlotName,
        expected_map_root: Word,
        key: StorageMapKey,
    ) -> Result<StorageMapWitness, StoreError> {
        let lineage = storage_map_lineage_id(account_id, slot_name);
        let tree = self.verified_latest_tree(lineage, expected_map_root)?;

        let hashed_key = key.hash();
        let proof = self.forest.open(tree, Word::from(hashed_key)).map_err(forest_error)?;
        Ok(StorageMapWitness::new(proof, [key])?)
    }

    // MUTATIONS
    // --------------------------------------------------------------------------------------------

    /// Applies a batch of updates at the given version, returning the new tree roots.
    ///
    /// Lineages unknown to the forest are created from the empty tree; known lineages are
    /// updated from their latest tree. `new_version` must be strictly greater than the latest
    /// version of every updated lineage.
    pub fn apply_updates(
        &mut self,
        new_version: VersionId,
        updates: SmtForestUpdateBatch,
    ) -> Result<Vec<TreeWithRoot>, StoreError> {
        let mutations = self
            .forest
            .compute_forest_mutations(new_version, updates)
            .map_err(forest_error)?;
        self.forest.apply_mutations(mutations).map_err(forest_error)
    }

    // HELPERS
    // --------------------------------------------------------------------------------------------

    /// Resolves the latest tree of a lineage and verifies its root against the expected value.
    fn verified_latest_tree(
        &self,
        lineage: LineageId,
        expected_root: Word,
    ) -> Result<TreeId, StoreError> {
        let version = self
            .forest
            .latest_version(lineage)
            .ok_or_else(|| StoreError::DatabaseError(format!("unknown lineage {lineage}")))?;
        let root = self.forest.latest_root(lineage).expect("lineage has a latest version");
        if root != expected_root {
            return Err(StoreError::MerkleStoreError(MerkleError::ConflictingRoots {
                expected_root,
                actual_root: root,
            }));
        }
        Ok(TreeId::new(lineage, version))
    }
}

// BATCH BUILDING
// ================================================================================================

/// Adds vault asset changes for an account to an update batch.
pub fn add_vault_ops(
    batch: &mut SmtForestUpdateBatch,
    account_id: AccountId,
    updated_assets: impl Iterator<Item = Asset>,
    removed_asset_ids: impl Iterator<Item = AssetId>,
) {
    let lineage = vault_lineage_id(account_id);
    let ops = batch.operations(lineage);
    for asset in updated_assets {
        ops.add_insert(asset.id().hash().into(), asset.to_value_word());
    }
    for asset_id in removed_asset_ids {
        ops.add_remove(asset_id.hash().into());
    }
}

/// Adds storage map entry changes for one of an account's map slots to an update batch.
///
/// Entries with an empty-word value are removals.
pub fn add_storage_map_ops(
    batch: &mut SmtForestUpdateBatch,
    account_id: AccountId,
    slot_name: &StorageSlotName,
    entries: impl Iterator<Item = (StorageMapKey, Word)>,
) {
    let lineage = storage_map_lineage_id(account_id, slot_name);
    let ops = batch.operations(lineage);
    for (key, value) in entries {
        let key_word = Word::from(key.hash());
        if value == EMPTY_WORD {
            ops.add_remove(key_word);
        } else {
            ops.add_insert(key_word, value);
        }
    }
}

// ERROR MAPPING
// ================================================================================================

/// Maps forest-level errors onto [`StoreError`].
///
/// Takes the error by value so it can be used directly with `map_err`.
#[allow(clippy::needless_pass_by_value)]
pub fn forest_error(err: LargeSmtForestError) -> StoreError {
    StoreError::DatabaseError(format!("smt forest error: {err}"))
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::account::StorageMapKey;
    use miden_protocol::asset::FungibleAsset;
    use miden_protocol::crypto::merkle::smt::ForestInMemoryBackend;
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
        ACCOUNT_ID_PUBLIC_NON_FUNGIBLE_FAUCET,
    };
    use miden_protocol::{ONE, ZERO};

    use super::*;

    fn account_a() -> AccountId {
        AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET).unwrap()
    }

    fn account_b() -> AccountId {
        AccountId::try_from(ACCOUNT_ID_PUBLIC_NON_FUNGIBLE_FAUCET).unwrap()
    }

    fn slot(name: &str) -> StorageSlotName {
        StorageSlotName::new(name).unwrap()
    }

    #[test]
    fn lineage_ids_are_deterministic_and_distinct() {
        // Deterministic
        assert_eq!(vault_lineage_id(account_a()), vault_lineage_id(account_a()));
        assert_eq!(
            storage_map_lineage_id(account_a(), &slot("miden::test::map")),
            storage_map_lineage_id(account_a(), &slot("miden::test::map")),
        );

        // Distinct across accounts, slots, and domains
        assert_ne!(vault_lineage_id(account_a()), vault_lineage_id(account_b()));
        assert_ne!(
            storage_map_lineage_id(account_a(), &slot("miden::test::map_one")),
            storage_map_lineage_id(account_a(), &slot("miden::test::map_two")),
        );
        assert_ne!(
            storage_map_lineage_id(account_a(), &slot("miden::test::map")),
            storage_map_lineage_id(account_b(), &slot("miden::test::map")),
        );
        assert_ne!(
            vault_lineage_id(account_a()),
            storage_map_lineage_id(account_a(), &slot("miden::test::map")),
        );
    }

    #[test]
    fn apply_updates_and_read_witnesses() {
        let mut forest = AccountSmtForest::new(ForestInMemoryBackend::new()).unwrap();
        let id = account_a();

        let asset: Asset = FungibleAsset::new(account_a(), 100).unwrap().into();
        let map_slot = slot("miden::test::map");
        let map_key = StorageMapKey::new([ONE, ZERO, ZERO, ZERO].into());
        let map_value: Word = [ONE, ONE, ONE, ONE].into();

        let mut batch = SmtForestUpdateBatch::empty();
        add_vault_ops(&mut batch, id, [asset].into_iter(), core::iter::empty());
        add_storage_map_ops(&mut batch, id, &map_slot, [(map_key, map_value)].into_iter());
        forest.apply_updates(1, batch).unwrap();

        let vault_root = forest.latest_root(vault_lineage_id(id)).unwrap();
        let map_root = forest.latest_root(storage_map_lineage_id(id, &map_slot)).unwrap();

        // Witness reads against the recorded roots succeed.
        let (read_asset, _witness) =
            forest.get_asset_and_witness(id, vault_root, asset.id()).unwrap();
        assert_eq!(read_asset, asset);

        let witness =
            forest.get_storage_map_item_witness(id, &map_slot, map_root, map_key).unwrap();
        assert_eq!(witness.get(map_key), Some(map_value));
    }

    #[test]
    fn witness_reads_reject_mismatched_roots() {
        let mut forest = AccountSmtForest::new(ForestInMemoryBackend::new()).unwrap();
        let id = account_a();

        let asset: Asset = FungibleAsset::new(account_a(), 100).unwrap().into();
        let mut batch = SmtForestUpdateBatch::empty();
        add_vault_ops(&mut batch, id, [asset].into_iter(), core::iter::empty());
        forest.apply_updates(1, batch).unwrap();

        // A stale expected root (the empty word here) must be rejected.
        let result = forest.get_asset_and_witness(id, EMPTY_WORD, asset.id());
        assert!(matches!(
            result,
            Err(StoreError::MerkleStoreError(MerkleError::ConflictingRoots { .. }))
        ));
    }

    #[test]
    fn removals_are_applied() {
        let mut forest = AccountSmtForest::new(ForestInMemoryBackend::new()).unwrap();
        let id = account_a();

        let map_slot = slot("miden::test::map");
        let map_key = StorageMapKey::new([ONE, ZERO, ZERO, ZERO].into());
        let map_value: Word = [ONE, ONE, ONE, ONE].into();

        let mut batch = SmtForestUpdateBatch::empty();
        add_storage_map_ops(&mut batch, id, &map_slot, [(map_key, map_value)].into_iter());
        forest.apply_updates(1, batch).unwrap();
        let root_with_entry = forest.latest_root(storage_map_lineage_id(id, &map_slot)).unwrap();

        // An empty-word value removes the entry, collapsing the tree back to the empty root.
        let mut batch = SmtForestUpdateBatch::empty();
        add_storage_map_ops(&mut batch, id, &map_slot, [(map_key, EMPTY_WORD)].into_iter());
        forest.apply_updates(2, batch).unwrap();
        let root_after_removal = forest.latest_root(storage_map_lineage_id(id, &map_slot)).unwrap();

        assert_ne!(root_with_entry, root_after_removal);
        assert_eq!(root_after_removal, miden_protocol::account::StorageMap::default().root());
    }
}
