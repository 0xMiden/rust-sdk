use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec::Vec;

use miden_protocol::account::{
    AccountId,
    AccountStoragePatch,
    StorageMap,
    StorageMapKey,
    StorageMapPatch,
    StorageMapWitness,
    StorageSlotName,
};
use miden_protocol::asset::{Asset, AssetId, AssetWitness};
use miden_protocol::crypto::merkle::MerkleError;
use miden_protocol::crypto::merkle::smt::{
    Backend,
    LargeSmtForest,
    LargeSmtForestError,
    LineageId,
    SmtForestUpdateBatch,
    TreeId,
    VersionId,
};
use miden_protocol::utils::serde::Serializable;
use miden_protocol::{EMPTY_WORD, Hasher, Word};

use super::StoreError;

// LINEAGE DERIVATION
// ================================================================================================

/// Returns the lineage identifier for an account's asset vault SMT.
fn vault_lineage_id(account_id: AccountId) -> LineageId {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"miden-client:vault");
    bytes.extend_from_slice(&account_id.to_bytes());
    LineageId::new(Hasher::hash(&bytes).as_bytes())
}

/// Returns the lineage identifier for an account's storage map SMT in the given slot.
fn storage_map_lineage_id(account_id: AccountId, slot_name: &StorageSlotName) -> LineageId {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"miden-client:storage-map");
    bytes.extend_from_slice(&account_id.to_bytes());
    // Length-prefix the variable-sized slot name so distinct (id, name) pairs cannot produce
    // the same preimage. The fixed-width u64 keeps the identifier platform-independent.
    bytes.extend_from_slice(&(slot_name.as_str().len() as u64).to_le_bytes());
    bytes.extend_from_slice(slot_name.as_str().as_bytes());
    LineageId::new(Hasher::hash(&bytes).as_bytes())
}

// ACCOUNT UPDATE
// ================================================================================================

/// A set of pending account SMT changes, applied as a single batch by
/// [`AccountSmtForest::apply`].
///
/// Operations accumulate per account lineage. Where a key receives more than one operation, the
/// last one recorded wins, which lets callers stage a wholesale reset followed by the entries
/// that survive it.
pub struct AccountUpdate {
    batch: SmtForestUpdateBatch,
}

impl Default for AccountUpdate {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountUpdate {
    /// Creates an update with no pending changes.
    pub fn new() -> Self {
        Self { batch: SmtForestUpdateBatch::empty() }
    }

    /// Records vault asset changes for an account.
    pub fn vault_ops(
        &mut self,
        account_id: AccountId,
        updated_assets: impl Iterator<Item = Asset>,
        removed_asset_ids: impl Iterator<Item = AssetId>,
    ) {
        let lineage = vault_lineage_id(account_id);
        let ops = self.batch.operations(lineage);
        for asset in updated_assets {
            ops.add_insert(asset.id().hash().into(), asset.to_value_word());
        }
        for asset_id in removed_asset_ids {
            ops.add_remove(asset_id.hash().into());
        }
    }

    /// Records storage map entry changes for one of an account's map slots.
    ///
    /// Entries with an empty-word value are removals.
    pub fn map_ops(
        &mut self,
        account_id: AccountId,
        slot_name: &StorageSlotName,
        entries: impl Iterator<Item = (StorageMapKey, Word)>,
    ) {
        let lineage = storage_map_lineage_id(account_id, slot_name);
        let ops = self.batch.operations(lineage);
        for (key, value) in entries {
            let key_word = Word::from(key.hash());
            if value == EMPTY_WORD {
                ops.add_remove(key_word);
            } else {
                ops.add_insert(key_word, value);
            }
        }
    }
}

// ACCOUNT SMT FOREST
// ================================================================================================

/// Account-oriented wrapper around [`LargeSmtForest`].
///
/// Account SMTs are tracked as lineages, one per account vault and one per storage map slot,
/// with identifiers derived deterministically from the account ID (and slot name). Each lineage
/// evolves through strictly increasing versions supplied by the caller.
///
/// Lineage identifiers are an implementation detail: callers address trees by account ID and
/// slot name, so no store can construct a lineage that diverges from the one this wrapper
/// derives.
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

    /// Returns the latest root of the account's asset vault SMT, or `None` if the forest does
    /// not track the account.
    pub fn vault_root(&self, account_id: AccountId) -> Option<Word> {
        self.forest.latest_root(vault_lineage_id(account_id))
    }

    /// Returns the latest root of the account's storage map SMT in the given slot, or `None` if
    /// the forest does not track that slot.
    pub fn map_root(&self, account_id: AccountId, slot_name: &StorageSlotName) -> Option<Word> {
        self.forest.latest_root(storage_map_lineage_id(account_id, slot_name))
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

    // UPDATE BUILDING
    // --------------------------------------------------------------------------------------------

    /// Records the removal of every entry currently stored in one of an account's map slots.
    ///
    /// Entries added to `update` after this call take precedence, so a reset followed by the
    /// slot's new entries leaves the lineage holding exactly those entries.
    pub fn reset_map(
        &self,
        update: &mut AccountUpdate,
        account_id: AccountId,
        slot_name: &StorageSlotName,
    ) -> Result<(), StoreError> {
        let lineage = storage_map_lineage_id(account_id, slot_name);
        let stored_keys = self.lineage_entry_keys(lineage)?;

        let ops = update.batch.operations(lineage);
        for key in stored_keys {
            ops.add_remove(key);
        }
        Ok(())
    }

    /// Records the operations for every map slot of a storage patch, returning the touched slot
    /// names.
    ///
    /// `Update` layers its entries onto the lineage's latest tree, after verifying that the
    /// tree's root matches the old root recorded in the account tables (a mismatch means the
    /// forest and the account state diverged). `Create` and `Remove` reset the lineage first, so
    /// the resulting tree reflects only the patch's own entries, or collapses to the empty root
    /// for `Remove`.
    pub fn add_storage_patch_ops(
        &self,
        update: &mut AccountUpdate,
        account_id: AccountId,
        old_map_roots: &BTreeMap<StorageSlotName, Word>,
        storage_patch: &AccountStoragePatch,
    ) -> Result<Vec<StorageSlotName>, StoreError> {
        let default_map_root = StorageMap::default().root();
        let mut touched = Vec::new();

        for (slot_name, map_patch) in storage_patch.maps() {
            touched.push(slot_name.clone());

            match map_patch {
                StorageMapPatch::Update { .. } => {
                    // A lineage the forest does not know yet starts from the empty tree, which
                    // is consistent with an absent old root.
                    let forest_root =
                        self.map_root(account_id, slot_name).unwrap_or(default_map_root);
                    let expected_root =
                        old_map_roots.get(slot_name).copied().unwrap_or(default_map_root);
                    if forest_root != expected_root {
                        return Err(StoreError::MerkleStoreError(MerkleError::ConflictingRoots {
                            expected_root,
                            actual_root: forest_root,
                        }));
                    }
                },
                StorageMapPatch::Create { .. } | StorageMapPatch::Remove => {
                    self.reset_map(update, account_id, slot_name)?;
                },
            }

            let entries = map_patch
                .entries()
                .into_iter()
                .flat_map(|e| e.as_map().iter())
                .map(|(key, value)| (*key, *value));
            update.map_ops(account_id, slot_name, entries);
        }

        Ok(touched)
    }

    /// Records the operations that make an account's lineages match the provided targets
    /// exactly: keys absent from a target are removed and every target pair is upserted.
    ///
    /// Both targets are keyed by hashed SMT key. A slot mapped to an empty target has its
    /// lineage reset to the empty tree, so callers undoing account state should include every
    /// slot that may still hold entries.
    pub fn reconcile_account(
        &self,
        update: &mut AccountUpdate,
        account_id: AccountId,
        vault_target: &BTreeMap<Word, Word>,
        map_targets: &BTreeMap<StorageSlotName, BTreeMap<Word, Word>>,
    ) -> Result<(), StoreError> {
        self.reconcile_lineage(update, vault_lineage_id(account_id), vault_target)?;
        for (slot_name, target) in map_targets {
            self.reconcile_lineage(update, storage_map_lineage_id(account_id, slot_name), target)?;
        }
        Ok(())
    }

    // MUTATIONS
    // --------------------------------------------------------------------------------------------

    /// Applies a pending update at the given version.
    ///
    /// Lineages unknown to the forest are created from the empty tree; known lineages are
    /// updated from their latest tree. `new_version` must be strictly greater than the latest
    /// version of every updated lineage. Resulting roots are read back with [`Self::vault_root`]
    /// and [`Self::map_root`].
    pub fn apply(
        &mut self,
        new_version: VersionId,
        update: AccountUpdate,
    ) -> Result<(), StoreError> {
        let mutations = self
            .forest
            .compute_forest_mutations(new_version, update.batch)
            .map_err(forest_error)?;
        self.forest.apply_mutations(mutations).map_err(forest_error)?;
        Ok(())
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

    /// Returns the SMT keys currently stored in a lineage, or an empty list if the forest does
    /// not track it yet.
    fn lineage_entry_keys(&self, lineage: LineageId) -> Result<Vec<Word>, StoreError> {
        let Some(version) = self.forest.latest_version(lineage) else {
            return Ok(Vec::new());
        };

        let entries = self.forest.entries(TreeId::new(lineage, version)).map_err(forest_error)?;
        let mut keys = Vec::new();
        for entry in entries {
            keys.push(entry.map_err(forest_error)?.key);
        }
        Ok(keys)
    }

    /// Records the operations that make one lineage match `target` exactly.
    fn reconcile_lineage(
        &self,
        update: &mut AccountUpdate,
        lineage: LineageId,
        target: &BTreeMap<Word, Word>,
    ) -> Result<(), StoreError> {
        let stored_keys = self.lineage_entry_keys(lineage)?;

        let ops = update.batch.operations(lineage);
        for key in stored_keys {
            if !target.contains_key(&key) {
                ops.add_remove(key);
            }
        }
        for (key, value) in target {
            ops.add_insert(*key, *value);
        }
        Ok(())
    }
}

// ERROR MAPPING
// ================================================================================================

/// Maps forest-level errors onto [`StoreError`].
///
/// Takes the error by value so it can be used directly with `map_err`.
#[allow(clippy::needless_pass_by_value)]
fn forest_error(err: LargeSmtForestError) -> StoreError {
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

    fn forest() -> AccountSmtForest<ForestInMemoryBackend> {
        AccountSmtForest::new(ForestInMemoryBackend::new()).unwrap()
    }

    /// Colliding lineages would silently serve one account's witnesses from another's tree, so
    /// the derivation must separate accounts, slots, and the vault/map domains.
    #[test]
    fn lineage_ids_are_distinct() {
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
        let mut forest = forest();
        let id = account_a();

        let asset: Asset = FungibleAsset::new(account_a(), 100).unwrap().into();
        let map_slot = slot("miden::test::map");
        let map_key = StorageMapKey::new([ONE, ZERO, ZERO, ZERO].into());
        let map_value: Word = [ONE, ONE, ONE, ONE].into();

        let mut update = AccountUpdate::new();
        update.vault_ops(id, [asset].into_iter(), core::iter::empty());
        update.map_ops(id, &map_slot, [(map_key, map_value)].into_iter());
        forest.apply(1, update).unwrap();

        let vault_root = forest.vault_root(id).unwrap();
        let map_root = forest.map_root(id, &map_slot).unwrap();

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
        let mut forest = forest();
        let id = account_a();

        let asset: Asset = FungibleAsset::new(account_a(), 100).unwrap().into();
        let mut update = AccountUpdate::new();
        update.vault_ops(id, [asset].into_iter(), core::iter::empty());
        forest.apply(1, update).unwrap();

        // A stale expected root (the empty word here) must be rejected.
        let result = forest.get_asset_and_witness(id, EMPTY_WORD, asset.id());
        assert!(matches!(
            result,
            Err(StoreError::MerkleStoreError(MerkleError::ConflictingRoots { .. }))
        ));
    }

    #[test]
    fn removals_are_applied() {
        let mut forest = forest();
        let id = account_a();

        let map_slot = slot("miden::test::map");
        let map_key = StorageMapKey::new([ONE, ZERO, ZERO, ZERO].into());
        let map_value: Word = [ONE, ONE, ONE, ONE].into();

        let mut update = AccountUpdate::new();
        update.map_ops(id, &map_slot, [(map_key, map_value)].into_iter());
        forest.apply(1, update).unwrap();
        let root_with_entry = forest.map_root(id, &map_slot).unwrap();

        // An empty-word value removes the entry, collapsing the tree back to the empty root.
        let mut update = AccountUpdate::new();
        update.map_ops(id, &map_slot, [(map_key, EMPTY_WORD)].into_iter());
        forest.apply(2, update).unwrap();
        let root_after_removal = forest.map_root(id, &map_slot).unwrap();

        assert_ne!(root_with_entry, root_after_removal);
        assert_eq!(root_after_removal, StorageMap::default().root());
    }

    #[test]
    fn reconcile_account_matches_targets_exactly() {
        let mut forest = forest();
        let id = account_a();

        let map_slot = slot("miden::test::map");
        let stale_key = StorageMapKey::new([ONE, ZERO, ZERO, ZERO].into());
        let target_key = StorageMapKey::new([ZERO, ONE, ZERO, ZERO].into());
        let value: Word = [ONE, ONE, ONE, ONE].into();

        let asset: Asset = FungibleAsset::new(account_a(), 100).unwrap().into();

        let mut update = AccountUpdate::new();
        update.vault_ops(id, [asset].into_iter(), core::iter::empty());
        update.map_ops(id, &map_slot, [(stale_key, value)].into_iter());
        forest.apply(1, update).unwrap();

        // Reconcile to an empty vault and a map holding only `target_key`.
        let mut map_targets = BTreeMap::new();
        map_targets
            .insert(map_slot.clone(), BTreeMap::from([(Word::from(target_key.hash()), value)]));

        let mut update = AccountUpdate::new();
        forest
            .reconcile_account(&mut update, id, &BTreeMap::new(), &map_targets)
            .unwrap();
        forest.apply(2, update).unwrap();

        // The vault collapsed to empty, so the asset is no longer tracked.
        let vault_root = forest.vault_root(id).unwrap();
        assert!(matches!(
            forest.get_asset_and_witness(id, vault_root, asset.id()),
            Err(StoreError::VaultKeyNotTracked(..))
        ));

        let map_root = forest.map_root(id, &map_slot).unwrap();
        let witness =
            forest.get_storage_map_item_witness(id, &map_slot, map_root, stale_key).unwrap();
        assert_eq!(witness.get(stale_key), Some(EMPTY_WORD));

        let witness = forest
            .get_storage_map_item_witness(id, &map_slot, map_root, target_key)
            .unwrap();
        assert_eq!(witness.get(target_key), Some(value));
    }
}
