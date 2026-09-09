use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::{AccountId, PartialAccount, StorageMapKey, StorageMapWitness};
use miden_protocol::asset::{AssetId, AssetWitness};
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::NoteScript;
use miden_protocol::transaction::{AccountInputs, PartialBlockchain};
use miden_tx::TransactionMastStore;

use crate::utils::RwLock;

// DATA STORE CACHE
// ================================================================================================

/// Account states served to the executor as part of its transaction inputs.
type AccountCache = BTreeMap<AccountId, PartialAccount>;

/// Reference block headers and partial blockchains served to the executor as part of its
/// transaction inputs. Both derive from the requested reference blocks alone, never from the
/// account, so they are keyed and cached independently of it.
type BlockchainCache = BTreeMap<BTreeSet<BlockNumber>, (BlockHeader, PartialBlockchain)>;

/// Vault asset witnesses keyed by (vault root, asset ID). The vault root commits to the whole
/// vault state, so the witness for an asset is the same no matter which account holds the vault.
type VaultWitnessCache = BTreeMap<(Word, AssetId), AssetWitness>;

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
    /// Account states served to the executor. Entries are only inserted when the execution-input
    /// cache is enabled via [`super::ClientDataStore::with_execution_input_cache`], which the note
    /// screener does for its trial executions, where the account states do not change across the
    /// pass. Otherwise this map stays empty, as during real transaction execution, whose account
    /// state evolves between executions.
    partial_accounts: RwLock<AccountCache>,
    /// Reference block headers and partial blockchains served to the executor. Populated under
    /// the same conditions as `partial_accounts`.
    blockchains: RwLock<BlockchainCache>,
    /// Vault asset witnesses served to the executor. The requested keys always include the fee
    /// asset key, so this memoizes the per-execution fee witness lookup across a screening batch.
    /// Populated under the same conditions as `partial_accounts`.
    vault_asset_witnesses: RwLock<VaultWitnessCache>,
    /// Whether `partial_accounts`, `blockchains` and `vault_asset_witnesses` are used at all.
    /// When unset, their getters miss and their setters do nothing, so the maps stay empty.
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
            partial_accounts: RwLock::new(BTreeMap::new()),
            blockchains: RwLock::new(BTreeMap::new()),
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

    /// Returns the cached state of the given account, if any.
    ///
    /// Returns `None` if the execution-input cache is disabled.
    pub(super) fn get_partial_account(&self, account_id: AccountId) -> Option<PartialAccount> {
        if !self.cache_execution_inputs {
            return None;
        }

        self.partial_accounts.read().get(&account_id).cloned()
    }

    /// Caches the state of the given account.
    ///
    /// Does nothing if the execution-input cache is disabled.
    pub(super) fn insert_partial_account(&self, account: &PartialAccount) {
        if !self.cache_execution_inputs {
            return;
        }

        self.partial_accounts.write().insert(account.id(), account.clone());
    }

    /// Returns the cached reference block header and partial blockchain for the given reference
    /// blocks, if any.
    ///
    /// Returns `None` if the execution-input cache is disabled.
    pub(super) fn get_blockchain(
        &self,
        ref_blocks: &BTreeSet<BlockNumber>,
    ) -> Option<(BlockHeader, PartialBlockchain)> {
        if !self.cache_execution_inputs {
            return None;
        }

        self.blockchains.read().get(ref_blocks).cloned()
    }

    /// Caches the reference block header and partial blockchain for the given reference blocks.
    ///
    /// Does nothing if the execution-input cache is disabled.
    pub(super) fn insert_blockchain(
        &self,
        ref_blocks: BTreeSet<BlockNumber>,
        header: &BlockHeader,
        blockchain: &PartialBlockchain,
    ) {
        if !self.cache_execution_inputs {
            return;
        }

        self.blockchains
            .write()
            .insert(ref_blocks, (header.clone(), blockchain.clone()));
    }

    /// Returns the cached witnesses for the given vault root and requested asset IDs, or `None`
    /// if any of them is missing.
    ///
    /// Returns `None` if the execution-input cache is disabled.
    pub(super) fn get_vault_asset_witnesses(
        &self,
        vault_root: Word,
        asset_ids: &BTreeSet<AssetId>,
    ) -> Option<Vec<AssetWitness>> {
        if !self.cache_execution_inputs {
            return None;
        }

        let cache = self.vault_asset_witnesses.read();
        asset_ids
            .iter()
            .map(|asset_id| cache.get(&(vault_root, *asset_id)).cloned())
            .collect()
    }

    /// Caches the witnesses resolved for the given vault root and requested asset IDs, matched
    /// positionally.
    ///
    /// Does nothing if the execution-input cache is disabled.
    pub(super) fn insert_vault_asset_witnesses(
        &self,
        vault_root: Word,
        asset_ids: &BTreeSet<AssetId>,
        witnesses: &[AssetWitness],
    ) {
        if !self.cache_execution_inputs {
            return;
        }

        let mut cache = self.vault_asset_witnesses.write();
        for (asset_id, witness) in asset_ids.iter().zip(witnesses) {
            cache.insert((vault_root, *asset_id), witness.clone());
        }
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
