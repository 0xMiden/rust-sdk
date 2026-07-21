use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::{AccountId, PartialAccount, StorageMapKey, StorageMapWitness};
use miden_protocol::asset::{AssetVaultKey, AssetWitness};
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::NoteScript;
use miden_protocol::transaction::{AccountInputs, PartialBlockchain};
use miden_tx::TransactionMastStore;

use crate::utils::RwLock;

// DATA STORE CACHE
// ================================================================================================

/// Transaction inputs served to the executor: the account state, reference block header, and
/// partial blockchain.
type CachedTransactionInputs = (PartialAccount, BlockHeader, PartialBlockchain);

/// Transaction inputs keyed by target account, then by the requested reference blocks. The
/// reference blocks are the inner key so that a lookup can borrow the requested set instead of
/// cloning it to build a composite key.
type TransactionInputsCache =
    BTreeMap<AccountId, BTreeMap<BTreeSet<BlockNumber>, CachedTransactionInputs>>;

/// Vault asset witnesses keyed by (account, vault root), then by the requested vault keys. Nested
/// for the same reason as [`TransactionInputsCache`].
type VaultWitnessCache =
    BTreeMap<(AccountId, Word), BTreeMap<BTreeSet<AssetVaultKey>, Vec<AssetWitness>>>;

/// In-memory state that [`super::ClientDataStore`] serves to the executor without going through
/// the persistent store.
///
/// This bundles everything that exists only for the duration of an execution session: data
/// registered up front for the in-flight transaction request (account code, foreign account
/// inputs, output note scripts) plus data cached lazily while the transaction executes (RPC-fetched
/// foreign accounts and storage map witnesses, the reference block).
pub(super) struct DataStoreCache {
    /// Store used to provide MAST nodes to the transaction executor.
    pub(super) mast_store: Arc<TransactionMastStore>,
    /// Foreign account inputs that should be returned to the executor on demand.
    foreign_account_inputs: RwLock<BTreeMap<AccountId, AccountInputs>>,
    /// Note scripts known only to the in-flight transaction request (e.g. its expected output
    /// note scripts): they must be resolvable while the transaction executes, but they are
    /// persisted only as part of the store update applied after the transaction succeeds.
    note_scripts: RwLock<BTreeMap<Word, NoteScript>>,
    /// Storage map witnesses, keyed by (`map_root`, `map_key`). Avoids redundant RPC calls when
    /// the same map entry is accessed multiple times within a transaction.
    storage_map_witnesses: RwLock<BTreeMap<(Word, StorageMapKey), StorageMapWitness>>,
    /// Transaction inputs served to the executor. Only populated while the note screener runs,
    /// where trial executions share the same account and reference block; see
    /// [`super::ClientDataStore::with_execution_input_cache`]. Left empty during real transaction
    /// execution, whose account state evolves between executions.
    transaction_inputs: RwLock<TransactionInputsCache>,
    /// Vault asset witnesses served to the executor. The requested keys always include the fee
    /// asset key, so this memoizes the per-execution fee witness lookup across a screening batch.
    /// Populated under the same conditions as `transaction_inputs`.
    vault_asset_witnesses: RwLock<VaultWitnessCache>,
    /// Whether `transaction_inputs` and `vault_asset_witnesses` are used at all. When unset, their
    /// getters miss and their setters do nothing, so both maps stay empty.
    cache_execution_inputs: bool,
    /// The transaction reference block number.
    ref_block: RwLock<Option<BlockNumber>>,
}

impl DataStoreCache {
    pub(super) fn new() -> Self {
        Self {
            mast_store: Arc::new(TransactionMastStore::new()),
            foreign_account_inputs: RwLock::new(BTreeMap::new()),
            note_scripts: RwLock::new(BTreeMap::new()),
            storage_map_witnesses: RwLock::new(BTreeMap::new()),
            transaction_inputs: RwLock::new(BTreeMap::new()),
            vault_asset_witnesses: RwLock::new(BTreeMap::new()),
            cache_execution_inputs: false,
            ref_block: RwLock::new(None),
        }
    }

    /// Enables the transaction-input and vault-asset-witness caches.
    pub(super) fn enable_execution_input_cache(&mut self) {
        self.cache_execution_inputs = true;
    }

    /// Replaces the cached foreign account inputs with the provided ones.
    pub(super) fn replace_foreign_account_inputs(
        &self,
        foreign_accounts: impl IntoIterator<Item = AccountInputs>,
    ) {
        let mut cache = self.foreign_account_inputs.write();
        cache.clear();

        for account_inputs in foreign_accounts {
            cache.insert(account_inputs.id(), account_inputs);
        }
    }

    /// Caches the inputs of a single foreign account, overwriting any previous entry.
    pub(super) fn insert_foreign_account_inputs(&self, account_inputs: AccountInputs) {
        self.foreign_account_inputs.write().insert(account_inputs.id(), account_inputs);
    }

    /// Returns the cached inputs for the given foreign account, if any.
    pub(super) fn get_foreign_account_inputs(
        &self,
        account_id: AccountId,
    ) -> Option<AccountInputs> {
        self.foreign_account_inputs.read().get(&account_id).cloned()
    }

    /// Runs `f` against the cached inputs for the given foreign account, without cloning them.
    pub(super) fn with_foreign_account_inputs<R>(
        &self,
        account_id: AccountId,
        f: impl FnOnce(&AccountInputs) -> R,
    ) -> Option<R> {
        self.foreign_account_inputs.read().get(&account_id).map(f)
    }

    /// Registers note scripts, keyed by their root. Scripts accumulate across calls.
    pub(super) fn insert_note_scripts(&self, note_scripts: impl IntoIterator<Item = NoteScript>) {
        let mut cache = self.note_scripts.write();
        for script in note_scripts {
            cache.insert(script.root().into(), script);
        }
    }

    /// Returns the registered note script with the given root, if any.
    pub(super) fn get_note_script(&self, script_root: Word) -> Option<NoteScript> {
        self.note_scripts.read().get(&script_root).cloned()
    }

    /// Caches a storage map witness for the given (`map_root`, `map_key`) pair.
    pub(super) fn insert_storage_map_witness(
        &self,
        map_root: Word,
        map_key: StorageMapKey,
        witness: StorageMapWitness,
    ) {
        self.storage_map_witnesses.write().insert((map_root, map_key), witness);
    }

    /// Returns the cached storage map witness for the given (`map_root`, `map_key`) pair, if any.
    pub(super) fn get_storage_map_witness(
        &self,
        map_root: Word,
        map_key: StorageMapKey,
    ) -> Option<StorageMapWitness> {
        self.storage_map_witnesses.read().get(&(map_root, map_key)).cloned()
    }

    /// Returns the cached transaction inputs for the given account and reference blocks, if any.
    ///
    /// Always returns `None` while the execution-input cache is disabled.
    pub(super) fn get_transaction_inputs(
        &self,
        account_id: AccountId,
        ref_blocks: &BTreeSet<BlockNumber>,
    ) -> Option<CachedTransactionInputs> {
        if !self.cache_execution_inputs {
            return None;
        }

        let cache = self.transaction_inputs.read();
        cache.get(&account_id)?.get(ref_blocks).cloned()
    }

    /// Caches the transaction inputs for the given account and reference blocks.
    ///
    /// Does nothing while the execution-input cache is disabled.
    pub(super) fn insert_transaction_inputs(
        &self,
        account_id: AccountId,
        ref_blocks: BTreeSet<BlockNumber>,
        inputs: &CachedTransactionInputs,
    ) {
        if !self.cache_execution_inputs {
            return;
        }

        self.transaction_inputs
            .write()
            .entry(account_id)
            .or_default()
            .insert(ref_blocks, inputs.clone());
    }

    /// Returns the cached vault asset witnesses for the given account, vault root and requested
    /// keys, if any.
    ///
    /// Always returns `None` while the execution-input cache is disabled.
    pub(super) fn get_vault_asset_witnesses(
        &self,
        account_id: AccountId,
        vault_root: Word,
        vault_keys: &BTreeSet<AssetVaultKey>,
    ) -> Option<Vec<AssetWitness>> {
        if !self.cache_execution_inputs {
            return None;
        }

        let cache = self.vault_asset_witnesses.read();
        cache.get(&(account_id, vault_root))?.get(vault_keys).cloned()
    }

    /// Caches the vault asset witnesses for the given account, vault root and requested keys.
    ///
    /// Does nothing while the execution-input cache is disabled.
    pub(super) fn insert_vault_asset_witnesses(
        &self,
        account_id: AccountId,
        vault_root: Word,
        vault_keys: BTreeSet<AssetVaultKey>,
        witnesses: &[AssetWitness],
    ) {
        if !self.cache_execution_inputs {
            return;
        }

        self.vault_asset_witnesses
            .write()
            .entry((account_id, vault_root))
            .or_default()
            .insert(vault_keys, witnesses.to_vec());
    }

    /// Returns the cached transaction reference block, if set.
    pub(super) fn ref_block(&self) -> Option<BlockNumber> {
        *self.ref_block.read()
    }

    /// Caches the transaction reference block so lazy-loading methods can use it.
    pub(super) fn set_ref_block(&self, block_num: BlockNumber) {
        *self.ref_block.write() = Some(block_num);
    }
}
