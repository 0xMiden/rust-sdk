use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::account::{
    AccountId,
    AccountStorageHeader,
    AccountStoragePatch,
    AccountVaultPatch,
    PartialAccount,
    PartialStorage,
    StorageMapKey,
    StorageMapPatch,
    StorageMapWitness,
    StorageSlotHeader,
    StorageSlotName,
    StorageSlotPatch,
    StorageSlotType,
};
use miden_protocol::asset::{AssetId, AssetWitness, PartialVault};
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::smt::{PartialSmt, SmtProof};
use miden_protocol::note::{NoteScript, NoteScriptRoot};
use miden_protocol::transaction::{AccountInputs, ExecutedTransaction, PartialBlockchain};
use miden_protocol::vm::FutureMaybeSend;
use miden_protocol::{EMPTY_WORD, Word, ZERO};
use miden_tx::{
    DataStore,
    DataStoreError,
    LoadedMastForest,
    MastForestStore,
    TransactionMastStore,
};

use super::staged_smt::StagedSmt;
use crate::ClientError;
use crate::store::data_store::ClientDataStore;

// IN-MEMORY BATCH DATA STORE
// ================================================================================================

/// A [`DataStore`] that lets a [`crate::transaction::BatchBuilder`] stack in-memory account
/// state for any number of local accounts. For each account pushed into the batch, the executor
/// sees the in-batch [`PartialAccount`] state instead of the stale store state.
///
/// Witnesses for the in-batch state are served entirely client-side: each account SMT (the vault
/// and every map slot) is viewed through a [`StagedSmt`] that replays the batch's accumulated
/// writes onto committed-root proofs fetched from the inner [`ClientDataStore`]. The store's
/// committed state is never mutated and no full [`miden_protocol::account::Account`] is ever
/// reconstructed.
pub(crate) struct InMemoryBatchDataStore {
    inner: ClientDataStore,
    current_accounts: BTreeMap<AccountId, CachedAccountState>,
}

/// The in-batch state of one account: the partial account served to the executor, plus the
/// staged view of each of its SMTs from which witnesses at the in-batch roots are opened.
///
/// Cloned to apply a transaction's writes atomically; the staged trees only hold the paths the
/// batch has touched, so a copy stays proportional to the batch rather than to the account.
#[derive(Clone)]
struct CachedAccountState {
    account: PartialAccount,
    vault: StagedSmt,
    maps: BTreeMap<StorageSlotName, StagedSmt>,
}

impl CachedAccountState {
    /// Anchors the staged trees at the committed account state — the initial account of the
    /// first in-batch transaction, which is exactly what the store served for it.
    fn new(committed: &PartialAccount) -> Self {
        let vault = StagedSmt::new(committed.vault().root());
        let maps = committed
            .storage()
            .header()
            .slots()
            .filter(|slot| slot.slot_type() == StorageSlotType::Map)
            .map(|slot| (slot.name().clone(), StagedSmt::new(slot.value())))
            .collect();

        Self { account: committed.clone(), vault, maps }
    }

    /// Folds a transaction's vault writes into the staged vault and returns the new in-batch
    /// vault root.
    async fn fold_vault_writes(
        &mut self,
        inner: &ClientDataStore,
        account_id: AccountId,
        patch: &AccountVaultPatch,
    ) -> Result<Word, ClientError> {
        let written: BTreeSet<AssetId> = patch
            .updated_assets()
            .map(|asset| asset.id())
            .chain(patch.removed_asset_ids().copied())
            .collect();
        let proofs = inner
            .committed_vault_proofs(account_id, self.vault.committed_root(), written)
            .await?;

        let entries = patch
            .updated_assets()
            .map(|asset| (Word::from(asset.id().hash()), asset.to_value_word()))
            .chain(patch.removed_asset_ids().map(|id| (Word::from(id.hash()), EMPTY_WORD)));

        self.vault.apply_entries(proofs, entries).map_err(|err| {
            DataStoreError::other_with_source("failed to stage vault writes", err).into()
        })
    }

    /// Folds a transaction's storage writes into the staged map trees and returns the resulting
    /// storage header: value slots take their patched values, map slots the new staged roots.
    async fn fold_storage_writes(
        &mut self,
        inner: &ClientDataStore,
        account_id: AccountId,
        patch: &AccountStoragePatch,
    ) -> Result<AccountStorageHeader, ClientError> {
        let current: Vec<StorageSlotHeader> =
            self.account.storage().header().slots().cloned().collect();

        let mut slots = Vec::with_capacity(current.len());
        for slot in current {
            let new_value = match (slot.slot_type(), patch.get(slot.name())) {
                (_, None) => slot.value(),
                (StorageSlotType::Value, Some(StorageSlotPatch::Value(value_patch))) => {
                    // A removed value slot commits to the empty word.
                    value_patch.value().unwrap_or(EMPTY_WORD)
                },
                (StorageSlotType::Map, Some(StorageSlotPatch::Map(map_patch))) => {
                    self.fold_map_writes(inner, account_id, slot.name(), map_patch).await?
                },
                (slot_type, Some(_)) => {
                    return Err(DataStoreError::other(format!(
                        "storage slot {} of account {account_id} was patched as a different kind than its type {slot_type:?}",
                        slot.name()
                    ))
                    .into());
                },
            };
            slots.push(StorageSlotHeader::new(slot.name().clone(), slot.slot_type(), new_value));
        }

        AccountStorageHeader::new(slots).map_err(|err| {
            DataStoreError::other_with_source("failed to rebuild in-batch storage header", err)
                .into()
        })
    }

    /// Folds a transaction's writes to one storage map into its staged view and returns the new
    /// in-batch map root. A removed map re-anchors the view at the empty tree.
    async fn fold_map_writes(
        &mut self,
        inner: &ClientDataStore,
        account_id: AccountId,
        slot_name: &StorageSlotName,
        map_patch: &StorageMapPatch,
    ) -> Result<Word, ClientError> {
        let map = self.maps.get_mut(slot_name).ok_or_else(|| {
            DataStoreError::other(format!(
                "no staged tree for map slot {slot_name} of account {account_id}"
            ))
        })?;

        let Some(entries) = map_patch.entries() else {
            *map = StagedSmt::empty();
            return Ok(map.current_root());
        };
        let entries = entries.as_map();

        let mut committed_proofs = Vec::new();
        for map_key in entries.keys() {
            if let Some(proof) =
                inner.committed_map_proof(account_id, map.committed_root(), *map_key).await?
            {
                committed_proofs.push(proof);
            }
        }

        map.apply_entries(
            committed_proofs,
            entries.iter().map(|(map_key, value)| (Word::from(map_key.hash()), *value)),
        )
        .map_err(|err| {
            DataStoreError::other_with_source("failed to stage storage map writes", err).into()
        })
    }
}

impl InMemoryBatchDataStore {
    /// Wraps the provided [`ClientDataStore`] with an empty in-batch account cache.
    pub(crate) fn new(inner: ClientDataStore) -> Self {
        Self { inner, current_accounts: BTreeMap::new() }
    }

    /// Folds an executed transaction into the in-batch state of its account: stages the patch's
    /// writes onto the account's SMT views and rebuilds the cached [`PartialAccount`], so later
    /// transactions in the batch observe the post-transaction state and can obtain witnesses for
    /// any of its keys.
    ///
    /// The fold is applied to a copy that replaces the cached state only once every step has
    /// succeeded, so a failure here leaves the batch's view of the account exactly as it was and
    /// the caller may keep building on it.
    pub(crate) async fn apply_executed_transaction(
        &mut self,
        executed_tx: &ExecutedTransaction,
    ) -> Result<(), ClientError> {
        let account_id = executed_tx.account_id();
        let final_account = executed_tx.final_account();
        let patch = executed_tx.account_patch();

        let mut state = match self.current_accounts.get(&account_id) {
            Some(state) => state.clone(),
            None => CachedAccountState::new(executed_tx.initial_account()),
        };

        let vault_root = state.fold_vault_writes(&self.inner, account_id, patch.vault()).await?;
        ensure_matches("vault root", vault_root, final_account.vault_root(), account_id)?;

        let storage_header =
            state.fold_storage_writes(&self.inner, account_id, patch.storage()).await?;
        ensure_matches(
            "storage commitment",
            storage_header.to_commitment(),
            final_account.storage_commitment(),
            account_id,
        )?;

        let code = patch.code().unwrap_or_else(|| state.account.code()).clone();
        ensure_matches(
            "code commitment",
            code.commitment(),
            final_account.code_commitment(),
            account_id,
        )?;

        let storage = PartialStorage::new(storage_header, vec![])
            .expect("partial storage creation from empty maps is infallible");

        let seed = if final_account.nonce() == ZERO {
            executed_tx.initial_account().seed()
        } else {
            None
        };

        state.account = PartialAccount::new(
            account_id,
            final_account.nonce(),
            code,
            storage,
            PartialVault::new(vault_root),
            seed,
        )
        .map_err(ClientError::AccountError)?;

        self.current_accounts.insert(account_id, state);

        Ok(())
    }

    /// Returns the inner [`ClientDataStore`]'s MAST store so callers can load account
    /// or note code prior to execution.
    pub(crate) fn mast_store(&self) -> Arc<TransactionMastStore> {
        self.inner.mast_store()
    }

    /// Registers foreign account inputs on the inner [`ClientDataStore`] so the executor
    /// can resolve foreign-procedure invocations during transaction execution.
    pub(crate) fn register_foreign_account_inputs(
        &self,
        foreign_accounts: impl IntoIterator<Item = AccountInputs>,
    ) {
        self.inner.register_foreign_account_inputs(foreign_accounts);
    }

    /// Registers note scripts on the inner [`ClientDataStore`] so the executor can resolve
    /// the request's output note scripts during transaction execution.
    pub(crate) fn register_note_scripts(&self, note_scripts: impl IntoIterator<Item = NoteScript>) {
        self.inner.register_note_scripts(note_scripts);
    }

    /// Returns the in-batch account state if a transaction earlier in the batch cached one.
    /// `None` means the caller should fall back to the account's persisted state.
    pub(crate) fn cached_account(&self, account_id: AccountId) -> Option<PartialAccount> {
        self.current_accounts.get(&account_id).map(|state| state.account.clone())
    }
}

// HELPERS
// ================================================================================================

/// Guards against divergence between the locally-staged state and the executor's result: on a
/// mismatch every later witness would be built on a corrupt view, so the push fails instead.
fn ensure_matches(
    what: &str,
    staged: Word,
    executed: Word,
    account_id: AccountId,
) -> Result<(), ClientError> {
    if staged == executed {
        return Ok(());
    }
    Err(DataStoreError::other(format!(
        "staged {what} does not match executed state for account {account_id}: staged = {staged:?}, executed = {executed:?}"
    ))
    .into())
}

/// Batch-private helpers for fetching the committed-root proofs that anchor staged SMT views.
/// The methods stay private to this module so the anchoring semantics don't leak into the
/// store's public surface.
impl ClientDataStore {
    /// Fetches the committed-root proofs anchoring `asset_ids` in a staged vault view. A view
    /// anchored at the empty tree needs none: an account created in-batch has no committed vault
    /// in the store, and every key of the empty tree is implicitly provable anyway.
    async fn committed_vault_proofs(
        &self,
        account_id: AccountId,
        committed_root: Word,
        asset_ids: BTreeSet<AssetId>,
    ) -> Result<Vec<SmtProof>, DataStoreError> {
        if committed_root == PartialSmt::EMPTY_ROOT || asset_ids.is_empty() {
            return Ok(Vec::new());
        }
        let witnesses =
            self.get_vault_asset_witnesses(account_id, committed_root, asset_ids).await?;
        Ok(witnesses.into_iter().map(SmtProof::from).collect())
    }

    /// Fetches the committed-root proof anchoring `map_key` in a staged map view, or `None` for
    /// a view anchored at the empty tree. See [`Self::committed_vault_proofs`].
    async fn committed_map_proof(
        &self,
        account_id: AccountId,
        committed_root: Word,
        map_key: StorageMapKey,
    ) -> Result<Option<SmtProof>, DataStoreError> {
        if committed_root == PartialSmt::EMPTY_ROOT {
            return Ok(None);
        }
        let witness = self.get_storage_map_witness(account_id, committed_root, map_key).await?;
        Ok(Some(SmtProof::from(witness)))
    }
}

// DATA STORE IMPL
// ================================================================================================

impl DataStore for InMemoryBatchDataStore {
    async fn get_transaction_inputs(
        &self,
        account_id: AccountId,
        ref_blocks: BTreeSet<BlockNumber>,
    ) -> Result<(PartialAccount, BlockHeader, PartialBlockchain), DataStoreError> {
        let (mut partial_account, block_header, partial_blockchain) =
            self.inner.get_transaction_inputs(account_id, ref_blocks).await?;

        if let Some(state) = self.current_accounts.get(&account_id) {
            partial_account = state.account.clone();
        }

        Ok((partial_account, block_header, partial_blockchain))
    }

    async fn get_vault_asset_witnesses(
        &self,
        account_id: AccountId,
        vault_root: Word,
        asset_ids: BTreeSet<AssetId>,
    ) -> Result<Vec<AssetWitness>, DataStoreError> {
        let Some(state) = self.current_accounts.get(&account_id) else {
            return self.inner.get_vault_asset_witnesses(account_id, vault_root, asset_ids).await;
        };

        let in_batch_root = state.vault.current_root();
        if in_batch_root != vault_root {
            return Err(DataStoreError::other(format!(
                "vault root mismatch for account {account_id}: in-batch root = {in_batch_root:?}, requested root = {vault_root:?}",
            )));
        }

        // Anchor the requested keys with their committed proofs, replay the batch's writes, and
        // open every key at the in-batch root.
        let committed_proofs = self
            .inner
            .committed_vault_proofs(account_id, state.vault.committed_root(), asset_ids.clone())
            .await?;
        let staged = state.vault.staged_view(committed_proofs).map_err(|err| {
            DataStoreError::other_with_source("failed to build staged vault view", err)
        })?;

        asset_ids
            .into_iter()
            .map(|asset_id| {
                let proof = staged.open(&asset_id.hash().into()).map_err(|err| {
                    DataStoreError::other_with_source("failed to open staged vault witness", err)
                })?;
                AssetWitness::new(proof, [asset_id]).map_err(|err| {
                    DataStoreError::other_with_source("failed to build staged vault witness", err)
                })
            })
            .collect()
    }

    async fn get_storage_map_witness(
        &self,
        account_id: AccountId,
        map_root: Word,
        map_key: StorageMapKey,
    ) -> Result<StorageMapWitness, DataStoreError> {
        let Some(state) = self.current_accounts.get(&account_id) else {
            return self.inner.get_storage_map_witness(account_id, map_root, map_key).await;
        };

        let Some(staged_map) = state.maps.values().find(|map| map.current_root() == map_root)
        else {
            return Err(DataStoreError::other(format!(
                "storage map root not found in in-batch account state for account {account_id}: requested root = {map_root:?}",
            )));
        };

        // Anchor the requested key with its committed proof, replay the batch's writes, and open
        // the key at the in-batch root.
        let committed_proof = self
            .inner
            .committed_map_proof(account_id, staged_map.committed_root(), map_key)
            .await?;
        let staged = staged_map.staged_view(committed_proof).map_err(|err| {
            DataStoreError::other_with_source("failed to build staged storage map view", err)
        })?;
        let proof = staged.open(&Word::from(map_key.hash())).map_err(|err| {
            DataStoreError::other_with_source("failed to open staged storage map witness", err)
        })?;

        StorageMapWitness::new(proof, [map_key]).map_err(|err| {
            DataStoreError::other_with_source("failed to build staged storage map witness", err)
        })
    }

    async fn get_foreign_account_inputs(
        &self,
        foreign_account_id: AccountId,
        ref_block: BlockNumber,
    ) -> Result<AccountInputs, DataStoreError> {
        self.inner.get_foreign_account_inputs(foreign_account_id, ref_block).await
    }

    fn get_note_script(
        &self,
        script_root: NoteScriptRoot,
    ) -> impl FutureMaybeSend<Result<Option<NoteScript>, DataStoreError>> {
        self.inner.get_note_script(script_root)
    }
}

// MAST FOREST STORE IMPL
// ================================================================================================

impl MastForestStore for InMemoryBatchDataStore {
    fn get(&self, procedure_hash: &Word) -> Option<LoadedMastForest> {
        self.inner.get(procedure_hash)
    }
}
