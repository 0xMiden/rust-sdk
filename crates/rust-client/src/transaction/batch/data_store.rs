use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::account::{
    AccountDelta,
    AccountId,
    PartialAccount,
    StorageMapKey,
    StorageMapWitness,
};
use miden_protocol::asset::{AssetVaultKey, AssetWitness};
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{NoteScript, NoteScriptRoot};
use miden_protocol::transaction::{AccountInputs, PartialBlockchain, TransactionInputs};
use miden_protocol::vm::FutureMaybeSend;
use miden_protocol::{MastForest, Word};
use miden_tx::{DataStore, DataStoreError, MastForestStore, TransactionMastStore};

use crate::ClientError;
use crate::account::AccountReader;
use crate::store::data_store::ClientDataStore;

// IN-MEMORY BATCH DATA STORE
// ================================================================================================

/// A [`DataStore`] that lets a [`crate::transaction::BatchBuilder`] stack in-memory account
/// inputs for any number of local accounts. For each account pushed into the batch, a
/// [`PartialAccount`] is cached; the executor sees the in-batch partial account state instead of
/// the stale store state.
///
/// Witness reads for the cached account are first resolved from the prior transaction's
/// execution advice (which covers the keys that transaction touched). Keys that no prior in-batch
/// transaction touched are absent from the advice; those are served by the [`crate::store::Store`]
/// by staging the accumulated in-batch delta onto its committed Merkle forest, so no full account
/// is ever reconstructed.
pub(crate) struct InMemoryBatchDataStore {
    inner: ClientDataStore,
    current_accounts: BTreeMap<AccountId, CachedAccountState>,
}

struct CachedAccountState {
    account: PartialAccount,
    tx_inputs: TransactionInputs,
    /// Accumulated delta from the account's committed state to the current in-batch state. Used
    /// to serve witnesses for keys not present in `tx_inputs`' execution advice.
    accumulated_delta: AccountDelta,
}

impl InMemoryBatchDataStore {
    /// Wraps the provided [`ClientDataStore`] with an empty in-batch account cache.
    pub(crate) fn new(inner: ClientDataStore) -> Self {
        Self { inner, current_accounts: BTreeMap::new() }
    }

    /// Caches the post-transaction partial account and the transaction inputs carrying the
    /// execution advice for the just-executed transaction, and folds `delta` into the account's
    /// accumulated in-batch delta so later transactions can resolve witnesses for any key.
    pub(crate) fn cache_account(
        &mut self,
        account: PartialAccount,
        tx_inputs: TransactionInputs,
        delta: AccountDelta,
    ) -> Result<(), ClientError> {
        match self.current_accounts.get_mut(&account.id()) {
            Some(state) => {
                state.accumulated_delta.merge(delta)?;
                state.account = account;
                state.tx_inputs = tx_inputs;
            },
            None => {
                self.current_accounts.insert(
                    account.id(),
                    CachedAccountState {
                        account,
                        tx_inputs,
                        accumulated_delta: delta,
                    },
                );
            },
        }
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

    pub(crate) async fn current_account(
        &self,
        account_reader: &AccountReader,
    ) -> Result<PartialAccount, ClientError> {
        let account_id = account_reader.account_id();
        if let Some(state) = self.current_accounts.get(&account_id) {
            return Ok(state.account.clone());
        }

        account_reader.partial_account().await
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
        vault_keys: BTreeSet<AssetVaultKey>,
    ) -> Result<Vec<AssetWitness>, DataStoreError> {
        let Some(state) = self.current_accounts.get(&account_id) else {
            return self.inner.get_vault_asset_witnesses(account_id, vault_root, vault_keys).await;
        };

        let in_batch_root = state.account.vault().root();
        if in_batch_root != vault_root {
            return Err(DataStoreError::other(format!(
                "vault root mismatch for account {account_id}: in-batch root = {in_batch_root:?}, requested root = {vault_root:?}",
            )));
        }

        // Fast path: keys the prior in-batch transaction touched are in its execution advice.
        if let Ok(witnesses) =
            state.tx_inputs.read_vault_asset_witnesses(vault_root, vault_keys.clone())
        {
            return Ok(witnesses);
        }

        // Miss: a key no prior in-batch transaction touched. Serve it from the store by staging
        // the accumulated in-batch delta onto the committed vault, without reconstructing the
        // account.
        self.inner
            .store()
            .vault_asset_witnesses_after_delta(
                account_id,
                state.accumulated_delta.clone(),
                vault_root,
                vault_keys,
            )
            .await
            .map_err(DataStoreError::from)
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

        if !state.account.storage().header().map_slot_roots().any(|root| root == map_root) {
            return Err(DataStoreError::other(format!(
                "storage map root not found in in-batch account state for account {account_id}: requested root = {map_root:?}",
            )));
        }

        // Fast path: a key the prior in-batch transaction touched is in its execution advice.
        if let Ok(witness) = state.tx_inputs.read_storage_map_witness(map_root, map_key) {
            return Ok(witness);
        }

        // Miss: a key no prior in-batch transaction touched. Serve it from the store by staging
        // the accumulated in-batch delta onto the committed map, without reconstructing the
        // account.
        self.inner
            .store()
            .storage_map_witness_after_delta(
                account_id,
                state.accumulated_delta.clone(),
                map_root,
                map_key,
            )
            .await
            .map_err(DataStoreError::from)
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
    fn get(&self, procedure_hash: &Word) -> Option<Arc<MastForest>> {
        self.inner.get(procedure_hash)
    }
}
