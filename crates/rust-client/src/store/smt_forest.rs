use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec::Vec;

use miden_protocol::account::{
    AccountId,
    AccountStoragePatch,
    AccountVaultPatch,
    StorageMapKey,
    StorageMapPatch,
    StorageMapWitness,
    StorageSlot,
    StorageSlotContent,
    StorageSlotName,
};
use miden_protocol::asset::{Asset, AssetId, AssetWitness};
use miden_protocol::crypto::merkle::MerkleError;
use miden_protocol::crypto::merkle::smt::{
    Backend,
    BackendReader,
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

/// Changes recorded for one lineage.
#[derive(Default)]
struct LineageOps {
    /// When set, the lineage's computed root must equal this before the update is applied.
    expect_root: Option<Word>,
    /// When set, keys absent from `pairs` are removed, so the tree ends up holding exactly the
    /// recorded pairs.
    exhaustive: bool,
    /// Key-value pairs in recording order. An empty-word value is a removal, and a later pair
    /// for the same key supersedes an earlier one.
    pairs: Vec<(Word, Word)>,
}

/// Account SMT changes, applied as a single batch by [`AccountSmtForest::apply`].
///
/// Recording is pure bookkeeping: the entries a change implies are worked out when the update is
/// applied, which is where the forest can be read.
#[derive(Default)]
pub struct AccountUpdate {
    ops: BTreeMap<LineageId, LineageOps>,
}

impl AccountUpdate {
    /// Creates an update with no recorded changes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an account's vault patch, along with the vault root the transaction produced.
    ///
    /// [`apply`] checks the resulting root against `expected_root`. That check is what ties the
    /// vault tree back to the transaction kernel's result, so a wrong root fails the update
    /// instead of being persisted.
    ///
    /// [`apply`]: AccountSmtForest::apply
    pub fn vault_patch(
        &mut self,
        account_id: AccountId,
        patch: &AccountVaultPatch,
        expected_root: Word,
    ) {
        let vault = self.entry(vault_lineage_id(account_id));
        vault.expect_root = Some(expected_root);
        vault
            .pairs
            .extend(patch.updated_assets().map(|a| (a.id().hash().into(), a.to_value_word())));
        vault
            .pairs
            .extend(patch.removed_asset_ids().map(|id| (id.hash().into(), EMPTY_WORD)));
    }

    /// Records an account's storage patch.
    ///
    /// Map slots are layered onto their current tree for `Update` patches and replaced wholesale
    /// for `Create` and `Remove`. No per-slot root is recorded: the store checks the resulting map
    /// roots collectively against the transaction's storage commitment, which also catches a tree
    /// that had drifted from the account tables.
    pub fn storage_patch(&mut self, account_id: AccountId, patch: &AccountStoragePatch) {
        for (slot_name, map_patch) in patch.maps() {
            let ops = self.entry(storage_map_lineage_id(account_id, slot_name));
            ops.pairs.extend(
                map_patch
                    .entries()
                    .into_iter()
                    .flat_map(|e| e.as_map().iter())
                    .map(|(key, value)| (Word::from(key.hash()), *value)),
            );
            if matches!(map_patch, StorageMapPatch::Create { .. } | StorageMapPatch::Remove) {
                ops.exhaustive = true;
            }
        }
    }

    /// Records that an account's vault and map slots hold exactly the provided state.
    ///
    /// Slots that the account no longer has are not implied by `slots` and must be named with
    /// [`Self::clear_map`].
    pub fn full_state<'a>(
        &mut self,
        account_id: AccountId,
        assets: impl Iterator<Item = Asset>,
        slots: impl Iterator<Item = &'a StorageSlot>,
    ) {
        let vault = self.entry(vault_lineage_id(account_id));
        vault.exhaustive = true;
        vault.pairs.extend(assets.map(|a| (a.id().hash().into(), a.to_value_word())));

        for slot in slots {
            if let StorageSlotContent::Map(map) = slot.content() {
                let ops = self.entry(storage_map_lineage_id(account_id, slot.name()));
                ops.exhaustive = true;
                ops.pairs
                    .extend(map.entries().map(|(key, value)| (Word::from(key.hash()), *value)));
            }
        }
    }

    /// Records that one of an account's map slots holds nothing.
    pub fn clear_map(&mut self, account_id: AccountId, slot_name: &StorageSlotName) {
        self.entry(storage_map_lineage_id(account_id, slot_name)).exhaustive = true;
    }

    fn entry(&mut self, lineage: LineageId) -> &mut LineageOps {
        self.ops.entry(lineage).or_default()
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
/// The wrapper is generic over the forest storage [`BackendReader`], so read-only backends can
/// serve roots and witnesses. Applying updates additionally requires [`Backend`]. Construction
/// loads the backend's tree metadata.
pub struct AccountSmtForest<B: BackendReader> {
    forest: LargeSmtForest<B>,
}

impl<B: BackendReader> AccountSmtForest<B> {
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
}

// MUTATIONS
// ================================================================================================

impl<B: Backend> AccountSmtForest<B> {
    /// Applies a recorded update at the given version.
    ///
    /// Lineages unknown to the forest are created from the empty tree; known lineages are
    /// updated from their latest tree. `new_version` must be strictly greater than the latest
    /// version of every updated lineage. Resulting roots are read back with [`Self::vault_root`]
    /// and [`Self::map_root`].
    ///
    /// Any root recorded on the update is verified against the computed mutations before they are
    /// applied, so a mismatch is rejected without modifying the forest.
    pub fn apply(
        &mut self,
        new_version: VersionId,
        update: AccountUpdate,
    ) -> Result<(), StoreError> {
        let mut batch = SmtForestUpdateBatch::empty();
        let mut expected_roots = Vec::new();

        for (lineage, ops) in update.ops {
            if let Some(expected_root) = ops.expect_root {
                expected_roots.push((lineage, expected_root));
            }

            // Removals are staged as they are seen so a key removed and then re-inserted ends up
            // inserted, and vice versa: the batch keeps the last operation per key.
            let stored_keys = if ops.exhaustive {
                self.lineage_entry_keys(lineage)?
            } else {
                Vec::new()
            };
            let batch_ops = batch.operations(lineage);
            let mut target = BTreeMap::new();
            for (key, value) in ops.pairs {
                if value == EMPTY_WORD {
                    target.remove(&key);
                    batch_ops.add_remove(key);
                } else {
                    target.insert(key, value);
                }
            }
            for key in stored_keys {
                if !target.contains_key(&key) {
                    batch_ops.add_remove(key);
                }
            }
            for (key, value) in target {
                batch_ops.add_insert(key, value);
            }
        }

        let mutations =
            self.forest.compute_forest_mutations(new_version, batch).map_err(forest_error)?;

        for (lineage, expected_root) in expected_roots {
            let actual_root = mutations
                .roots()
                .find(|root| root.lineage() == lineage)
                .map(|root| root.root())
                .expect("every expected lineage has a computed mutation");
            if actual_root != expected_root {
                return Err(StoreError::MerkleStoreError(MerkleError::ConflictingRoots {
                    expected_root,
                    actual_root,
                }));
            }
        }

        self.forest.apply_mutations(mutations).map_err(forest_error)?;

        Ok(())
    }
}

impl<B: BackendReader> AccountSmtForest<B> {
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
    use miden_protocol::account::StorageMap;
    use miden_protocol::asset::{AssetVault, FungibleAsset};
    use miden_protocol::crypto::merkle::smt::ForestInMemoryBackend;
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
        ACCOUNT_ID_PUBLIC_NON_FUNGIBLE_FAUCET,
    };

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

    fn asset(amount: u64) -> Asset {
        FungibleAsset::new(account_a(), amount).unwrap().into()
    }

    fn forest() -> AccountSmtForest<ForestInMemoryBackend> {
        AccountSmtForest::new(ForestInMemoryBackend::new()).unwrap()
    }

    fn set_vault(forest: &mut AccountSmtForest<ForestInMemoryBackend>, version: u64, of: &[Asset]) {
        let mut update = AccountUpdate::new();
        update.full_state(account_a(), of.iter().copied(), core::iter::empty::<&StorageSlot>());
        forest.apply(version, update).unwrap();
    }

    #[test]
    fn accepts_read_only_backend() {
        let backend = ForestInMemoryBackend::new();
        let forest = AccountSmtForest::new(backend.reader().unwrap()).unwrap();

        assert_eq!(forest.vault_root(account_a()), None);
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

    /// A full-state record is exhaustive: assets missing from it are dropped, not merged.
    #[test]
    fn full_state_replaces_previous_entries() {
        let mut forest = forest();
        let id = account_a();
        let (old, new) = (asset(100), asset(250));

        set_vault(&mut forest, 1, &[old]);
        let (read, _) = forest
            .get_asset_and_witness(id, forest.vault_root(id).unwrap(), old.id())
            .unwrap();
        assert_eq!(read, old);

        set_vault(&mut forest, 2, &[new]);
        let (read, _) = forest
            .get_asset_and_witness(id, forest.vault_root(id).unwrap(), new.id())
            .unwrap();
        assert_eq!(read, new);

        // Same faucet, so both assets share a vault key; the replacement is visible as the value.
        assert_ne!(old.to_value_word(), new.to_value_word());
    }

    /// An empty full state clears the vault rather than leaving the old entries in place.
    #[test]
    fn full_state_can_empty_a_vault() {
        let mut forest = forest();
        let id = account_a();
        let held = asset(100);

        set_vault(&mut forest, 1, &[held]);
        set_vault(&mut forest, 2, &[]);

        let vault_root = forest.vault_root(id).unwrap();
        assert_eq!(vault_root, StorageMap::default().root());
        assert!(matches!(
            forest.get_asset_and_witness(id, vault_root, held.id()),
            Err(StoreError::VaultKeyNotTracked(..))
        ));
    }

    /// Witness reads are the point at which forest/account divergence is caught.
    #[test]
    fn witness_reads_reject_mismatched_roots() {
        let mut forest = forest();
        let held = asset(100);
        set_vault(&mut forest, 1, &[held]);

        let result = forest.get_asset_and_witness(account_a(), EMPTY_WORD, held.id());
        assert!(matches!(
            result,
            Err(StoreError::MerkleStoreError(MerkleError::ConflictingRoots { .. }))
        ));
    }

    #[test]
    fn rejected_update_does_not_advance_forest() {
        let mut forest = forest();
        let id = account_a();
        let (old, new) = (asset(100), asset(250));
        set_vault(&mut forest, 1, &[old]);

        let old_root = forest.vault_root(id).unwrap();
        let new_root = AssetVault::new(&[new]).unwrap().root();
        assert_ne!(new_root, old_root);

        let mut rejected = AccountUpdate::new();
        rejected.vault_patch(id, &AccountVaultPatch::with_assets([new]), old_root);
        assert!(matches!(
            forest.apply(2, rejected),
            Err(StoreError::MerkleStoreError(MerkleError::ConflictingRoots {
                expected_root,
                actual_root,
            })) if expected_root == old_root && actual_root == new_root
        ));
        assert_eq!(forest.vault_root(id), Some(old_root));

        let mut accepted = AccountUpdate::new();
        accepted.vault_patch(id, &AccountVaultPatch::with_assets([new]), new_root);
        forest.apply(2, accepted).unwrap();
        assert_eq!(forest.vault_root(id), Some(new_root));
    }
}
