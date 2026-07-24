use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::{
    Account,
    AccountId,
    PartialAccount,
    StorageMapKey,
    StorageMapWitness,
    StorageSlotContent,
};
use miden_protocol::asset::{AssetId, AssetWitness};
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{NoteScript, NoteScriptRoot};
use miden_protocol::transaction::{AccountInputs, PartialBlockchain};
use miden_protocol::vm::FutureMaybeSend;
use miden_tx::{
    DataStore,
    DataStoreError,
    LoadedMastForest,
    MastForestStore,
    TransactionMastStore,
};

use crate::store::data_store::ClientDataStore;

// IN-MEMORY BATCH DATA STORE
// ================================================================================================

/// A [`DataStore`] that lets a [`crate::transaction::BatchBuilder`] stack in-memory account
/// states for any number of local accounts. For each account registered in
/// `current_accounts`, the executor sees the in-batch state instead of the stale store state.
/// All other reads pass through to the inner [`ClientDataStore`].
pub(crate) struct InMemoryBatchDataStore {
    inner: ClientDataStore,
    current_accounts: BTreeMap<AccountId, Account>,
}

impl InMemoryBatchDataStore {
    /// Wraps the provided [`ClientDataStore`] with an empty in-batch account cache.
    pub(crate) fn new(inner: ClientDataStore) -> Self {
        Self { inner, current_accounts: BTreeMap::new() }
    }

    /// Returns the in-batch account state for `id`, if a transaction earlier in the batch
    /// has cached one. A return of `None` means subsequent transactions targeting this
    /// account will see the store's state instead.
    pub(crate) fn get_account(&self, id: AccountId) -> Option<&Account> {
        self.current_accounts.get(&id)
    }

    /// Records the post-execution state of an account so that later transactions in the
    /// same batch targeting `id` observe the in-batch state. Overwrites any previously
    /// cached entry for `id`.
    pub(crate) fn cache_account(&mut self, id: AccountId, new_state: Account) {
        self.current_accounts.insert(id, new_state);
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

        if let Some(account) = self.current_accounts.get(&account_id) {
            partial_account = PartialAccount::from(account);
        }

        Ok((partial_account, block_header, partial_blockchain))
    }

    async fn get_vault_asset_witnesses(
        &self,
        account_id: AccountId,
        vault_root: Word,
        asset_ids: BTreeSet<AssetId>,
    ) -> Result<Vec<AssetWitness>, DataStoreError> {
        if let Some(account) = self.current_accounts.get(&account_id) {
            let vault = account.vault();
            let in_batch_root = vault.root();
            if in_batch_root != vault_root {
                return Err(DataStoreError::other(format!(
                    "vault root mismatch for account {account_id}: in-batch root = {in_batch_root:?}, requested root = {vault_root:?}",
                )));
            }
            let witnesses = asset_ids.into_iter().map(|key| vault.open(key)).collect();
            Ok(witnesses)
        } else {
            self.inner.get_vault_asset_witnesses(account_id, vault_root, asset_ids).await
        }
    }

    async fn get_storage_map_witness(
        &self,
        account_id: AccountId,
        map_root: Word,
        map_key: StorageMapKey,
    ) -> Result<StorageMapWitness, DataStoreError> {
        if let Some(account) = self.current_accounts.get(&account_id) {
            for slot in account.storage().slots() {
                if let StorageSlotContent::Map(map) = slot.content()
                    && map.root() == map_root
                {
                    return Ok(map.open(&map_key));
                }
            }
            return Err(DataStoreError::other(format!(
                "storage map root not found in in-batch account state for account {account_id}: requested root = {map_root:?}",
            )));
        }
        self.inner.get_storage_map_witness(account_id, map_root, map_key).await
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
