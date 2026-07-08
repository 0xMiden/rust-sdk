use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::account::{
    Account,
    AccountCode,
    AccountId,
    PartialAccount,
    StorageMapKey,
    StorageMapWitness,
    StorageSlot,
    StorageSlotContent,
    StorageSlotName,
};
use miden_protocol::asset::{AssetVaultKey, AssetWitness};
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::{InOrderIndex, MmrPeaks, PartialMmr};
use miden_protocol::crypto::merkle::{MerkleError, MerklePath};
use miden_protocol::note::{NoteScript, NoteScriptRoot};
use miden_protocol::transaction::{AccountInputs, PartialBlockchain};
use miden_protocol::vm::FutureMaybeSend;
use miden_protocol::{MastForest, Word, ZERO};
use miden_tx::{DataStore, DataStoreError, MastForestStore, TransactionMastStore};

use super::{AccountStorageFilter, PartialBlockchainFilter, Store};
use crate::rpc::domain::account::{
    AccountStorageRequirements,
    GetAccountRequest,
    StorageMapEntries,
    StorageMapFetch,
};
use crate::rpc::{AccountStateAt, NodeRpcClient};
use crate::store::StoreError;
use crate::transaction::fetch_public_account_inputs;

mod cache;
use cache::DataStoreCache;

// DATA STORE
// ================================================================================================

/// Wrapper structure that implements [`DataStore`] over any [`Store`].
pub struct ClientDataStore {
    /// Local database containing information about the accounts managed by this client.
    store: alloc::sync::Arc<dyn Store>,
    /// In-memory state served to the executor for the duration of the execution session.
    cache: DataStoreCache,
    /// RPC client used to lazy-load foreign account data on cache miss.
    rpc_api: Arc<dyn NodeRpcClient>,
}

impl ClientDataStore {
    pub fn new(store: alloc::sync::Arc<dyn Store>, rpc_api: Arc<dyn NodeRpcClient>) -> Self {
        Self {
            store,
            cache: DataStoreCache::new(),
            rpc_api,
        }
    }

    pub fn mast_store(&self) -> Arc<TransactionMastStore> {
        self.cache.mast_store.clone()
    }

    /// Returns the underlying [`Store`], so callers (e.g. the batch builder) can serve witnesses
    /// against in-batch state without reconstructing accounts.
    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    /// Stores the provided foreign account inputs so they can be served to the executor upon
    /// request.
    pub fn register_foreign_account_inputs(
        &self,
        foreign_accounts: impl IntoIterator<Item = AccountInputs>,
    ) {
        self.cache.replace_foreign_account_inputs(foreign_accounts);
    }

    /// Registers note scripts so they can be served to the executor upon request.
    ///
    /// Scripts accumulate across calls (they are not cleared) so that a data store reused for
    /// several executions — e.g. by [`crate::transaction::BatchBuilder`] — keeps serving the
    /// scripts registered for earlier transactions.
    pub fn register_note_scripts(&self, note_scripts: impl IntoIterator<Item = NoteScript>) {
        self.cache.insert_note_scripts(note_scripts);
    }

    /// Attempts to resolve a storage map witness from the local store.
    ///
    /// This covers any account present in the store (local or foreign) as well as any
    /// foreign account previously cached in `foreign_account_inputs`.
    ///
    /// Returns `Ok(None)` when the map is not found locally.
    async fn get_local_storage_map_witness(
        &self,
        account_id: AccountId,
        map_root: Word,
        map_key: StorageMapKey,
    ) -> Result<Option<StorageMapWitness>, DataStoreError> {
        match self
            .store
            .get_account_storage(account_id, AccountStorageFilter::Root(map_root))
            .await
        {
            Ok(account_storage) => {
                match account_storage.slots().first().map(StorageSlot::content) {
                    Some(StorageSlotContent::Map(map)) => Ok(Some(map.open(&map_key))),
                    Some(StorageSlotContent::Value(value)) => Err(DataStoreError::other(format!(
                        "found StorageSlotContent::Value with {value} as its value."
                    ))),
                    _ => Ok(None),
                }
            },
            Err(err) => {
                tracing::debug!(
                    %account_id,
                    %err,
                    "storage map not found locally, will try remote fetch"
                );
                Ok(None)
            },
        }
    }

    /// Lazily fetches a foreign account's inputs from the network, loads its code into the MAST
    /// store, and caches the result in [`Self::foreign_account_inputs`].
    async fn fetch_and_cache_foreign_account(
        &self,
        account_id: AccountId,
        account_state_at: AccountStateAt,
    ) -> Result<AccountInputs, DataStoreError> {
        let account_inputs = fetch_public_account_inputs(
            &self.store,
            &self.rpc_api,
            account_id,
            AccountStorageRequirements::default(),
            account_state_at,
        )
        .await
        .map_err(|err| {
            DataStoreError::other_with_source("failed to fetch foreign account inputs", err)
        })?;

        self.cache.mast_store.load_account_code(account_inputs.code());
        self.cache.insert_foreign_account_inputs(account_inputs.clone());

        Ok(account_inputs)
    }

    /// Fetches a storage map witness for a specific key from the network via RPC and caches it.
    async fn fetch_and_cache_storage_map_witness(
        &self,
        account_id: AccountId,
        map_root: Word,
        slot_name: StorageSlotName,
        map_key: StorageMapKey,
        known_code: AccountCode,
    ) -> Result<StorageMapWitness, DataStoreError> {
        let storage_requirements = AccountStorageRequirements::new([(slot_name, &[map_key])]);
        let (_, account_proof): (BlockNumber, _) = self
            .rpc_api
            .get_account(
                account_id,
                GetAccountRequest::new()
                    .with_storage(StorageMapFetch::Slots(storage_requirements))
                    .with_known_code(Some(known_code)),
            )
            .await
            .map_err(|err| {
                DataStoreError::other_with_source("failed to fetch storage map via RPC", err)
            })?;

        let (_, account_details) = account_proof.into_parts();
        let details = account_details.ok_or_else(|| {
            DataStoreError::other(format!(
                "RPC returned no account details for account {account_id}"
            ))
        })?;

        let map_detail =
            details.storage_details.map_details.into_iter().next().ok_or_else(|| {
                DataStoreError::other(format!(
                    "RPC returned no storage map details for account {account_id}"
                ))
            })?;

        let proof = match map_detail.entries {
            StorageMapEntries::EntriesWithProofs(proofs) => {
                // We requested a single key, so we expect a single proof.
                proofs.into_iter().next().ok_or_else(|| {
                    DataStoreError::other("RPC returned no proofs for the requested key")
                })?
            },
            StorageMapEntries::AllEntries(_) => {
                return Err(DataStoreError::other(
                    "unexpected AllEntries response; specific keys were requested",
                ));
            },
        };

        let witness = StorageMapWitness::new(proof, [map_key]).map_err(|err| {
            DataStoreError::other_with_source("failed to create storage map witness", err)
        })?;
        self.cache.insert_storage_map_witness(map_root, map_key, witness.clone());
        Ok(witness)
    }
}

impl DataStore for ClientDataStore {
    async fn get_transaction_inputs(
        &self,
        account_id: AccountId,
        mut block_refs: BTreeSet<BlockNumber>,
    ) -> Result<(PartialAccount, BlockHeader, PartialBlockchain), DataStoreError> {
        let current_peaks = self.store.get_current_blockchain_peaks().await?;

        // Pop last block, used as reference (it does not need to be authenticated manually)
        let ref_block = block_refs.pop_last().ok_or(DataStoreError::other("block set is empty"))?;

        // Cache the reference block so lazy-loading methods can use it
        self.cache.set_ref_block(ref_block);

        let partial_account_record = self
            .store
            .get_minimal_partial_account(account_id)
            .await?
            .ok_or(DataStoreError::AccountNotFound(account_id))?;

        // New accounts (nonce == 0) need full storage maps as advice inputs for the
        // kernel to validate during account creation. For these, fetch the full account
        // and convert to PartialAccount (which includes full storage for new accounts).
        // Existing accounts use the minimal partial record directly.
        let partial_account: PartialAccount = if partial_account_record.nonce() == ZERO {
            let full_record = self
                .store
                .get_account(account_id)
                .await?
                .ok_or(DataStoreError::AccountNotFound(account_id))?;
            let account: Account = full_record
                .try_into()
                .map_err(|_| DataStoreError::AccountNotFound(account_id))?;
            PartialAccount::from(&account)
        } else {
            partial_account_record
                .try_into()
                .map_err(|_| DataStoreError::AccountNotFound(account_id))?
        };

        // Get header data
        let (block_header, _had_notes) = self
            .store
            .get_block_header_by_num(ref_block)
            .await?
            .ok_or(DataStoreError::BlockNotFound(ref_block))?;

        let block_headers: Vec<BlockHeader> = self
            .store
            .get_block_headers(&block_refs)
            .await?
            .into_iter()
            .map(|(header, _has_notes)| header)
            .collect();

        // TODO: the client stores only the peaks of the MMR at the current sync height, so we are
        // not actually following the block_ref here. If the block_ref != current_sync_height, this
        // would return an invalid partial blockchain.
        let partial_mmr =
            build_partial_mmr_with_paths(&self.store, current_peaks, &block_headers).await?;

        let partial_blockchain =
            PartialBlockchain::new(partial_mmr, block_headers).map_err(|err| {
                DataStoreError::other_with_source(
                    "error creating PartialBlockchain from internal data",
                    err,
                )
            })?;
        Ok((partial_account, block_header, partial_blockchain))
    }

    async fn get_vault_asset_witnesses(
        &self,
        account_id: AccountId,
        vault_root: Word,
        vault_keys: BTreeSet<AssetVaultKey>,
    ) -> Result<Vec<AssetWitness>, DataStoreError> {
        let mut asset_witnesses = vec![];
        for vault_key in vault_keys {
            match self.store.get_account_asset(account_id, vault_key).await {
                Ok(Some((_, asset_witness))) => asset_witnesses.push(asset_witness),
                Ok(None) | Err(StoreError::MerkleStoreError(MerkleError::RootNotInStore(_))) => {
                    let vault = self.store.get_account_vault(account_id).await?;

                    if vault.root() != vault_root {
                        return Err(DataStoreError::other("Vault root mismatch"));
                    }

                    let asset_witness =
                        AssetWitness::new(vault.open(vault_key).into()).map_err(|err| {
                            DataStoreError::other_with_source(
                                "Failed to open vault asset tree",
                                err,
                            )
                        })?;
                    asset_witnesses.push(asset_witness);
                },
                Err(err) => {
                    return Err(DataStoreError::other_with_source(
                        "Failed to get account asset",
                        err,
                    ));
                },
            }
        }
        Ok(asset_witnesses)
    }

    /// Retrieves the [`StorageMapWitness`] requested from the store. Alternatively fetching it
    /// from the RPC if not available locally. Witnesses fetched via RPC are cached in memory so
    /// that repeated accesses to the same map entry within a transaction avoid additional RPC
    /// calls.
    async fn get_storage_map_witness(
        &self,
        account_id: AccountId,
        map_root: Word,
        map_key: StorageMapKey,
    ) -> Result<StorageMapWitness, DataStoreError> {
        // Check the in-memory witness cache first.
        if let Some(witness) = self.cache.get_storage_map_witness(map_root, map_key) {
            return Ok(witness);
        }

        // Try the local store.
        if let Some(witness) =
            self.get_local_storage_map_witness(account_id, map_root, map_key).await?
        {
            return Ok(witness);
        }

        // Resolve against the cached account inputs (without cloning them), fetching and caching
        // the account first if it isn't cached yet.
        let resolution = if let Some(resolution) =
            self.cache.with_foreign_account_inputs(account_id, |inputs| {
                resolve_witness_from_inputs(inputs, map_root, map_key)
            }) {
            resolution?
        } else {
            let account_state_at = self
                .cache
                .ref_block()
                .map(AccountStateAt::Block)
                .expect("reference block should be set");
            let inputs = self.fetch_and_cache_foreign_account(account_id, account_state_at).await?;
            resolve_witness_from_inputs(&inputs, map_root, map_key)?
        };

        match resolution {
            WitnessResolution::Witness(witness) => Ok(witness),
            WitnessResolution::FetchParams(slot_name, known_code) => {
                self.fetch_and_cache_storage_map_witness(
                    account_id, map_root, slot_name, map_key, known_code,
                )
                .await
            },
        }
    }

    /// Returns the [`AccountInputs`] for the given foreign account from the cache or alternatively
    /// fetching them from the RPC if not available locally.
    async fn get_foreign_account_inputs(
        &self,
        foreign_account_id: AccountId,
        ref_block: BlockNumber,
    ) -> Result<AccountInputs, DataStoreError> {
        // Fast path: check the cache first.
        if let Some(inputs) = self.cache.get_foreign_account_inputs(foreign_account_id) {
            return Ok(inputs);
        }

        self.fetch_and_cache_foreign_account(foreign_account_id, AccountStateAt::Block(ref_block))
            .await
    }

    /// Returns the [`NoteScript`] for the given script root from the registered session scripts,
    /// the store, or alternatively fetching it from the RPC if not available locally.
    fn get_note_script(
        &self,
        script_root: NoteScriptRoot,
    ) -> impl FutureMaybeSend<Result<Option<NoteScript>, DataStoreError>> {
        let registered_script = self.cache.get_note_script(script_root.into());
        let store = self.store.clone();
        let rpc_api = self.rpc_api.clone();

        async move {
            // Fastest path: scripts registered for the in-flight transaction request.
            if let Some(note_script) = registered_script {
                return Ok(Some(note_script));
            }

            // Fast path: check the local store first.
            match store.get_note_script(script_root.into()).await {
                Ok(note_script) => return Ok(Some(note_script)),
                Err(StoreError::NoteScriptNotFound(_)) => {},
                Err(err) => {
                    return Err(DataStoreError::other_with_source(
                        format!("failed to get note script {script_root} from store"),
                        err,
                    ));
                },
            }

            // Store miss, fetch from the network via RPC.
            let Some(note_script) =
                rpc_api.get_note_script_by_root(script_root.into()).await.map_err(|err| {
                    DataStoreError::other_with_source("failed to fetch note script via RPC", err)
                })?
            else {
                return Ok(None);
            };

            // Persist for future lookups.
            if let Err(err) = store.upsert_note_scripts(core::slice::from_ref(&note_script)).await {
                tracing::warn!(
                    %err,
                    "Failed to persist fetched note script to store"
                );
            }

            Ok(Some(note_script))
        }
    }
}

// MAST FOREST STORE
// ================================================================================================

impl MastForestStore for ClientDataStore {
    fn get(&self, procedure_hash: &Word) -> Option<Arc<MastForest>> {
        self.cache.mast_store.get(procedure_hash)
    }
}

// HELPER FUNCTIONS
// ================================================================================================

/// Outcome of resolving a storage map witness against an account's inputs: either the witness
/// itself, or the parameters needed to fetch it via RPC.
enum WitnessResolution {
    Witness(StorageMapWitness),
    /// The [`AccountCode`] is not needed to build the witness: it is only sent along with the
    /// RPC request so the node can omit the account code from its response.
    FetchParams(StorageSlotName, AccountCode),
}

/// Tries to open the witness from the inputs' partial storage maps (this can miss if the
/// account's storage is too big); on a miss, resolves the slot name and account code needed to
/// fetch the witness via RPC.
fn resolve_witness_from_inputs(
    inputs: &AccountInputs,
    map_root: Word,
    map_key: StorageMapKey,
) -> Result<WitnessResolution, DataStoreError> {
    if let Some(partial_map) = inputs.storage().maps().find(|m| m.root() == map_root)
        && let Ok(witness) = partial_map.open(&map_key)
    {
        return Ok(WitnessResolution::Witness(witness));
    }

    let account_id = inputs.id();
    let slot_name = inputs
        .storage()
        .header()
        .slots()
        .find(|slot| slot.slot_type().is_map() && slot.value() == map_root)
        .map(|slot| slot.name().clone())
        .ok_or_else(|| {
            DataStoreError::other(format!(
                "did not find map slot with root {map_root} for foreign account {account_id}"
            ))
        })?;

    Ok(WitnessResolution::FetchParams(slot_name, inputs.code().clone()))
}

/// Builds a [`PartialMmr`] from the given peaks and a list of blocks that should be
/// authenticated against them.
///
/// `authenticated_blocks` must not contain the block whose forest matches `peaks`. For that
/// block the kernel extends the MMR itself, so an authentication path is not needed.
pub(crate) async fn build_partial_mmr_with_paths(
    store: &alloc::sync::Arc<dyn Store>,
    peaks: MmrPeaks,
    authenticated_blocks: &[BlockHeader],
) -> Result<PartialMmr, DataStoreError> {
    let mut partial_mmr: PartialMmr = PartialMmr::from_peaks(peaks);

    let block_nums: Vec<BlockNumber> =
        authenticated_blocks.iter().map(BlockHeader::block_num).collect();

    let authentication_paths =
        get_authentication_path_for_blocks(store, &block_nums, partial_mmr.forest().num_leaves())
            .await?;

    for (header, path) in authenticated_blocks.iter().zip(authentication_paths.iter()) {
        partial_mmr
            .track(header.block_num().as_usize(), header.commitment(), path)
            .map_err(|err| DataStoreError::other(format!("error constructing MMR: {err}")))?;
    }

    Ok(partial_mmr)
}

/// Retrieves all Partial Blockchain nodes required for authenticating the set of blocks, and then
/// constructs the path for each of them.
///
/// This function assumes `block_nums` doesn't contain values above or equal to `forest`.
/// If there are any such values, the function will panic when calling `mmr_merkle_path_len()`.
async fn get_authentication_path_for_blocks(
    store: &alloc::sync::Arc<dyn Store>,
    block_nums: &[BlockNumber],
    forest: usize,
) -> Result<Vec<MerklePath>, StoreError> {
    let mut node_indices = BTreeSet::new();

    // Calculate all needed nodes indices for generating the paths
    for block_num in block_nums {
        let path_depth = mmr_merkle_path_len(block_num.as_usize(), forest);

        let mut idx = InOrderIndex::from_leaf_pos(block_num.as_usize());

        for _ in 0..path_depth {
            node_indices.insert(idx.sibling());
            idx = idx.parent();
        }
    }

    // Get all MMR nodes based on collected indices
    let node_indices: Vec<InOrderIndex> = node_indices.into_iter().collect();

    let filter = PartialBlockchainFilter::List(node_indices);
    let mmr_nodes = store.get_partial_blockchain_nodes(filter).await?;

    // Construct authentication paths
    let mut authentication_paths = vec![];
    for block_num in block_nums {
        let mut merkle_nodes = vec![];
        let mut idx = InOrderIndex::from_leaf_pos(block_num.as_usize());

        while let Some(node) = mmr_nodes.get(&idx.sibling()) {
            merkle_nodes.push(*node);
            idx = idx.parent();
        }
        let path = MerklePath::new(merkle_nodes);
        authentication_paths.push(path);
    }

    Ok(authentication_paths)
}

/// Calculates the merkle path length for an MMR of a specific forest and a leaf index
/// `leaf_index` is a 0-indexed leaf number and `forest` is the total amount of leaves
/// in the MMR at this point.
fn mmr_merkle_path_len(leaf_index: usize, forest: usize) -> usize {
    let before: usize = forest & leaf_index;
    let after = forest ^ before;

    after.ilog2() as usize
}
