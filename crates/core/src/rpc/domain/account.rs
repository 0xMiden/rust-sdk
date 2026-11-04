use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::{
    Account,
    AccountCode,
    AccountHeader,
    AccountId,
    AccountStorage,
    AccountStorageHeader,
    PartialAccount,
    PartialStorage,
    PartialStorageMap,
    StorageMap,
    StorageMapKey,
    StorageSlot,
    StorageSlotHeader,
    StorageSlotName,
    StorageSlotType,
};
use miden_protocol::asset::{Asset, AssetId, AssetVault, PartialVault};
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::crypto::merkle::SparseMerklePath;
use miden_protocol::crypto::merkle::smt::PartialSmt;
use miden_protocol::transaction::AccountInputs;
use thiserror::Error;

use crate::errors::RpcError;
use crate::rpc::AccountStateAt;

// ACCOUNT DETAILS
// ================================================================================================

/// An account details.
#[derive(Clone, Debug)]
pub struct AccountDetails {
    pub header: AccountHeader,
    pub storage_details: AccountStorageDetails,
    pub code: AccountCode,
    pub vault_details: AccountVaultDetails,
}

impl TryFrom<AccountDetails> for PartialAccount {
    type Error = RpcError;

    /// Builds a partial account with the available vault and storage proofs.
    ///
    /// A map or vault that the node omitted or capped keeps its committed root. Reads fetch proofs
    /// for that root during execution.
    ///
    /// # Errors
    /// Returns an error if entries, assets, or proofs do not match the roots in the header, or if
    /// the account requires a seed.
    fn try_from(details: AccountDetails) -> Result<Self, Self::Error> {
        let AccountDetails {
            header,
            storage_details,
            code,
            vault_details,
        } = details;
        let mut storage_maps = Vec::with_capacity(storage_details.map_details.len());
        for map_details in storage_details.map_details {
            let map = match map_details.entries {
                StorageMapEntries::AllEntries(entries) => {
                    let map = StorageMap::with_entries(
                        entries.iter().map(|entry| (entry.key, entry.value)),
                    )
                    .map_err(|err| {
                        RpcError::InvalidResponse(format!("invalid storage map entries: {err}"))
                    })?;
                    let slot_root = storage_details
                        .header
                        .slots()
                        .find(|slot| *slot.name() == map_details.slot_name)
                        .map(StorageSlotHeader::value);
                    if Some(map.root()) != slot_root {
                        return Err(RpcError::InvalidResponse(format!(
                            "storage map entries for slot '{}' do not match the slot root",
                            map_details.slot_name
                        )));
                    }
                    PartialStorageMap::new_full(map)
                },
                StorageMapEntries::PartialMap { map_keys, partial_smt } => {
                    PartialStorageMap::try_from_parts(partial_smt, map_keys).map_err(|err| {
                        RpcError::InvalidResponse(format!("invalid storage map proof: {err}"))
                    })?
                },
                StorageMapEntries::LimitExceeded => continue,
            };
            storage_maps.push(map);
        }
        let vault = match vault_details {
            AccountVaultDetails::Assets(assets) => {
                let vault = AssetVault::new(&assets).map_err(|err| {
                    RpcError::InvalidResponse(format!("invalid asset list: {err}"))
                })?;
                if vault.root() == header.vault_root() {
                    PartialVault::new_full(vault)
                } else if assets.is_empty() {
                    // The node sends no assets when the request skips the vault or sends its
                    // current root.
                    PartialVault::new(header.vault_root())
                } else {
                    return Err(RpcError::InvalidResponse(
                        "asset list does not match the vault root".into(),
                    ));
                }
            },
            AccountVaultDetails::Partial(vault) => {
                if vault.root() != header.vault_root() {
                    return Err(RpcError::InvalidResponse(
                        "vault proofs do not match the vault root".into(),
                    ));
                }
                vault
            },
            AccountVaultDetails::TooManyAssets => PartialVault::new(header.vault_root()),
        };
        let storage = PartialStorage::new(storage_details.header, storage_maps)
            .map_err(|err| RpcError::InvalidResponse(format!("invalid partial storage: {err}")))?;
        let account = PartialAccount::new(header.id(), header.nonce(), code, storage, vault, None)
            .map_err(|err| RpcError::InvalidResponse(format!("invalid partial account: {err}")))?;
        if account.to_commitment() != header.to_commitment() {
            return Err(RpcError::InvalidResponse(
                "partial account does not match its header".into(),
            ));
        }
        Ok(account)
    }
}

impl TryFrom<&AccountDetails> for Account {
    type Error = RpcError;

    /// Builds an [`Account`] from [`AccountDetails`].
    ///
    /// This conversion fails if the account details are incomplete, i.e., when the account's
    /// storage maps or vault exceed the node's size threshold, or when only specific map keys or
    /// vault assets were requested. It also fails if the rebuilt account does not commit to the
    /// same value as the account header in the details.
    fn try_from(details: &AccountDetails) -> Result<Self, Self::Error> {
        let assets = match &details.vault_details {
            AccountVaultDetails::Assets(assets) => assets,
            AccountVaultDetails::TooManyAssets => {
                return Err(RpcError::ExpectedDataMissing(
                    "cannot build account: vault has too many assets".into(),
                ));
            },
            AccountVaultDetails::Partial(_) => {
                return Err(RpcError::ExpectedDataMissing(
                    "cannot build account: only specific vault assets were requested".into(),
                ));
            },
        };

        if let Some(slot_name) = details
            .storage_details
            .map_details
            .iter()
            .find(|m| m.is_limit_exceeded())
            .map(|m| &m.slot_name)
        {
            return Err(RpcError::ExpectedDataMissing(format!(
                "cannot build account: storage map slot '{slot_name}' has too many entries",
            )));
        }

        let mut slots: Vec<StorageSlot> = Vec::new();

        for slot_header in details.storage_details.header.slots() {
            match slot_header.slot_type() {
                StorageSlotType::Value => {
                    slots.push(StorageSlot::with_value(
                        slot_header.name().clone(),
                        slot_header.value(),
                    ));
                },
                StorageSlotType::Map => {
                    let map_details = details
                        .storage_details
                        .find_map_details(slot_header.name())
                        .ok_or_else(|| {
                            RpcError::ExpectedDataMissing(format!(
                                "slot '{}' is a map but has no map_details in response",
                                slot_header.name()
                            ))
                        })?;

                    let storage_map = map_details
                        .entries
                        .clone()
                        .into_storage_map()
                        .ok_or_else(|| {
                            RpcError::ExpectedDataMissing(format!(
                                "slot '{}' did not come back with all its entries, so the full \
                                 account cannot be built",
                                slot_header.name(),
                            ))
                        })?
                        .map_err(|err| {
                            RpcError::InvalidResponse(format!(
                                "the rpc api returned a non-valid map entry: {err}"
                            ))
                        })?;

                    slots.push(StorageSlot::with_map(slot_header.name().clone(), storage_map));
                },
            }
        }

        let asset_vault = AssetVault::new(assets).map_err(|err| {
            RpcError::InvalidResponse(format!("rpc api returned non-valid assets: {err}"))
        })?;

        let account_storage = AccountStorage::new(slots).map_err(|err| {
            RpcError::InvalidResponse(format!("rpc api returned non-valid storage slots: {err}"))
        })?;

        let account = Account::new(
            details.header.id(),
            asset_vault,
            account_storage,
            details.code.clone(),
            details.header.nonce(),
            None,
        )
        .map_err(|err| {
            RpcError::InvalidResponse(format!(
                "failed to construct account from rpc api response: {err}"
            ))
        })?;

        // Only the header is authenticated (through the account witness). The vault assets, the
        // storage values and the map entries must be the ones it commits to.
        let commitment = account.to_commitment();
        if commitment != details.header.to_commitment() {
            return Err(RpcError::InvalidResponse(format!(
                "account contents returned by the rpc api commit to {commitment} but the account \
                 header commits to {}",
                details.header.to_commitment(),
            )));
        }

        Ok(account)
    }
}

// ACCOUNT STORAGE DETAILS
// ================================================================================================

/// Account storage details for `AccountResponse`
#[derive(Clone, Debug)]
pub struct AccountStorageDetails {
    /// Account storage header (storage slot info for up to 256 slots)
    pub header: AccountStorageHeader,
    /// Additional data for the requested storage maps
    pub map_details: Vec<AccountStorageMapDetails>,
}

impl AccountStorageDetails {
    /// Find the matching details for a map, given its storage slot name.
    //  This linear search should be good enough since there can be
    //  only up to 256 slots, so locality probably wins here.
    pub fn find_map_details(&self, target: &StorageSlotName) -> Option<&AccountStorageMapDetails> {
        self.map_details.iter().find(|map_detail| map_detail.slot_name == *target)
    }

    /// Checks that every partial map covers exactly the keys that were requested for its slot.
    ///
    /// # Errors
    /// - If a partial map does not cover a key that was requested for its slot.
    /// - If a partial map covers a key that was not requested.
    pub fn validate_against_request(&self, storage: &StorageMapFetch) -> Result<(), RpcError> {
        for map_detail in &self.map_details {
            let StorageMapEntries::PartialMap { map_keys, .. } = &map_detail.entries else {
                continue;
            };

            let requested_keys = match storage {
                StorageMapFetch::Slots(requirements) => {
                    requirements.keys_for_slot(&map_detail.slot_name)
                },
                StorageMapFetch::Skip | StorageMapFetch::All => &[],
            };
            if let Some(key) = requested_keys.iter().find(|key| !map_keys.contains(key)) {
                return Err(RpcError::InvalidResponse(format!(
                    "partial storage map for slot '{}' does not cover requested key {}",
                    map_detail.slot_name,
                    key.to_hex(),
                )));
            }
            if let Some(key) = map_keys.iter().find(|key| !requested_keys.contains(key)) {
                return Err(RpcError::InvalidResponse(format!(
                    "partial storage map for slot '{}' covers key {}, which was not requested",
                    map_detail.slot_name,
                    key.to_hex(),
                )));
            }
        }

        Ok(())
    }
}

// ACCOUNT MAP DETAILS
// ================================================================================================

#[derive(Clone, Debug)]
pub struct AccountStorageMapDetails {
    /// Storage slot name of the storage map.
    pub slot_name: StorageSlotName,
    /// The map data the node returned for this slot. The variants are mutually exclusive.
    pub entries: StorageMapEntries,
}

impl AccountStorageMapDetails {
    /// The maximum number of keys the node will cover with a single partial map. The node counts
    /// this across all slots of a request, so honouring it per slot is a conservative bound.
    pub const MAX_PARTIAL_MAP_KEYS: usize = 64;

    /// Returns `true` when the node reported that this slot has more entries than it will return in
    /// a single response, meaning the entries have to be fetched through
    /// [`crate::rpc::NodeRpcClient::sync_storage_maps`] instead.
    pub fn is_limit_exceeded(&self) -> bool {
        matches!(self.entries, StorageMapEntries::LimitExceeded)
    }
}

// STORAGE MAP ENTRY
// ================================================================================================

/// A storage map entry containing a key-value pair.
#[derive(Clone, Debug)]
pub struct StorageMapEntry {
    pub key: StorageMapKey,
    pub value: Word,
}

// STORAGE MAP ENTRIES
// ================================================================================================

/// The map data a `/GetAccount` response carries for one storage map slot. The variants are
/// mutually exclusive, mirroring the node's response.
#[derive(Clone, Debug)]
pub enum StorageMapEntries {
    /// The slot has more entries than the node returns in a single response. No entries are
    /// carried; fetch them with [`crate::rpc::NodeRpcClient::sync_storage_maps`].
    LimitExceeded,
    /// All entries in the storage map (no proofs needed as the full map is available).
    AllEntries(Vec<StorageMapEntry>),
    /// The specific keys that were requested, covered by a single partial SMT.
    ///
    /// The values are carried only inside `partial_smt`: read one by hashing its raw key and
    /// calling [`PartialSmt::get_value`]. Every key in `map_keys` is guaranteed to be tracked by
    /// `partial_smt`, so such a read cannot fail.
    PartialMap {
        /// The original, unhashed keys covered by `partial_smt`.
        map_keys: Vec<StorageMapKey>,
        /// The partial SMT proving the value of every key in `map_keys`.
        partial_smt: PartialSmt,
    },
}

impl StorageMapEntries {
    /// Converts the entries into a [`StorageMap`].
    ///
    /// Returns `None` for every variant other than [`AllEntries`](Self::AllEntries), since only
    /// that one carries the whole map.
    pub fn into_storage_map(
        self,
    ) -> Option<Result<StorageMap, miden_protocol::errors::StorageMapError>> {
        match self {
            StorageMapEntries::AllEntries(entries) => {
                Some(StorageMap::with_entries(entries.into_iter().map(|e| (e.key, e.value))))
            },
            StorageMapEntries::LimitExceeded | StorageMapEntries::PartialMap { .. } => None,
        }
    }
}

// ACCOUNT VAULT DETAILS
// ================================================================================================

/// The vault data that a `/GetAccount` response carries for an account. The variants are mutually
/// exclusive.
#[derive(Clone, Debug)]
pub enum AccountVaultDetails {
    /// The vault has more assets than the node returns in a single response. No assets are carried.
    /// Fetch them with [`crate::rpc::NodeRpcClient::sync_account_vault`].
    TooManyAssets,
    /// The assets in the account's vault.
    ///
    /// The list is empty if the request skips the vault, or if the vault root is equal to the
    /// commitment in [`VaultFetch::IfChangedFrom`]. Thus, an empty list does not show that the
    /// vault is empty.
    Assets(Vec<Asset>),
    /// Proofs for the assets requested with [`VaultFetch::Assets`]. The proofs also cover requested
    /// assets that are not in the vault.
    Partial(PartialVault),
}

impl AccountVaultDetails {
    /// Returns `true` if the vault has more assets than the node returns in a single response.
    pub fn is_too_many_assets(&self) -> bool {
        matches!(self, Self::TooManyAssets)
    }

    /// Returns the asset list, or `None` for every variant other than [`Assets`](Self::Assets).
    pub fn assets(&self) -> Option<&[Asset]> {
        match self {
            Self::Assets(assets) => Some(assets),
            Self::TooManyAssets | Self::Partial(_) => None,
        }
    }

    /// Converts the details into the vault proofs, or returns `None` for every variant other than
    /// [`Partial`](Self::Partial).
    pub fn into_partial_vault(self) -> Option<PartialVault> {
        match self {
            Self::Partial(partial_vault) => Some(partial_vault),
            Self::TooManyAssets | Self::Assets(_) => None,
        }
    }

    /// Checks the vault details against the request they answer and against `vault_root`, the vault
    /// root in the account header.
    ///
    /// The node can answer a [`VaultFetch::Assets`] request with the full vault contents instead of
    /// proofs. This function converts that answer into a partial vault. Thus, for an asset request,
    /// the result is always [`Partial`](Self::Partial).
    ///
    /// # Errors
    /// - If the details hold proofs for a request other than [`VaultFetch::Assets`]
    /// - If the vault has too many assets to answer a [`VaultFetch::Assets`] request
    /// - If the proofs or the full vault contents do not verify against `vault_root`
    /// - If the proofs do not cover all requested assets
    /// - If the proofs track an asset that was not requested
    pub fn validate_against_request(
        self,
        vault_root: Word,
        vault_request: &VaultFetch,
    ) -> Result<Self, RpcError> {
        let VaultFetch::Assets(asset_ids) = vault_request else {
            if matches!(self, Self::Partial(_)) {
                return Err(RpcError::InvalidResponse(
                    "the response contains vault proofs that were not requested".into(),
                ));
            }
            return Ok(self);
        };

        let partial_vault = match self {
            Self::Partial(partial_vault) => {
                if let Some(asset_id) =
                    partial_vault.asset_ids().find(|asset_id| !asset_ids.contains(asset_id))
                {
                    return Err(RpcError::InvalidResponse(format!(
                        "the vault proofs track asset {asset_id}, which was not requested"
                    )));
                }
                partial_vault
            },
            Self::TooManyAssets => {
                return Err(RpcError::InvalidResponse(
                    "the node did not prove the requested assets because the vault is too large"
                        .into(),
                ));
            },
            Self::Assets(assets) => {
                PartialVault::new_full(AssetVault::new(&assets).map_err(|err| {
                    RpcError::InvalidResponse(format!("rpc api returned non-valid assets: {err}"))
                })?)
            },
        };

        if partial_vault.root() != vault_root {
            return Err(RpcError::InvalidResponse(format!(
                "the vault proofs verify against root {} but the account header has vault root \
                 {vault_root}",
                partial_vault.root(),
            )));
        }

        if let Some(asset_id) =
            asset_ids.iter().find(|asset_id| partial_vault.open(**asset_id).is_err())
        {
            return Err(RpcError::InvalidResponse(format!(
                "the vault proofs do not cover requested asset {asset_id}"
            )));
        }

        Ok(Self::Partial(partial_vault))
    }
}

// ACCOUNT PROOF
// ================================================================================================

/// Represents a proof of existence of an account's state at a specific block number.
#[derive(Clone, Debug)]
pub struct AccountProof {
    /// Account witness.
    account_witness: AccountWitness,
    /// State headers of public accounts.
    state_headers: Option<AccountDetails>,
}

impl AccountProof {
    /// Converts public account details and their witness into execution inputs.
    ///
    /// # Errors
    /// Returns an error if account details are absent or cannot form a partial account.
    pub fn into_account_inputs(self) -> Result<AccountInputs, RpcError> {
        let (witness, details) = self.into_parts();
        let details = details
            .ok_or_else(|| RpcError::ExpectedDataMissing("public account details".into()))?;
        Ok(AccountInputs::new(details.try_into()?, witness))
    }

    /// Creates a new [`AccountProof`].
    pub fn new(
        account_witness: AccountWitness,
        account_details: Option<AccountDetails>,
    ) -> Result<Self, AccountProofError> {
        if let Some(AccountDetails {
            header: account_header,
            storage_details,
            code,
            ..
        }) = &account_details
        {
            if account_header.to_commitment() != account_witness.state_commitment() {
                return Err(AccountProofError::InconsistentAccountCommitment);
            }
            if account_header.id() != account_witness.id() {
                return Err(AccountProofError::InconsistentAccountId);
            }
            if code.commitment() != account_header.code_commitment() {
                return Err(AccountProofError::InconsistentCodeCommitment);
            }
            if storage_details.header.to_commitment() != account_header.storage_commitment() {
                return Err(AccountProofError::InconsistentStorageCommitment);
            }
        }

        Ok(Self {
            account_witness,
            state_headers: account_details,
        })
    }

    /// Returns the account ID related to the account proof.
    pub fn account_id(&self) -> AccountId {
        self.account_witness.id()
    }

    /// Returns the account header, if present.
    pub fn account_header(&self) -> Option<&AccountHeader> {
        self.state_headers.as_ref().map(|account_details| &account_details.header)
    }

    /// Returns the storage header, if present.
    pub fn storage_header(&self) -> Option<&AccountStorageHeader> {
        self.state_headers
            .as_ref()
            .map(|account_details| &account_details.storage_details.header)
    }

    /// Returns the full storage details, if available (public accounts only).
    pub fn storage_details(&self) -> Option<&AccountStorageDetails> {
        self.state_headers.as_ref().map(|d| &d.storage_details)
    }

    /// Returns the vault details, if available (public accounts only).
    pub fn vault_details(&self) -> Option<&AccountVaultDetails> {
        self.state_headers.as_ref().map(|d| &d.vault_details)
    }

    /// Returns the storage map details for a specific slot, if available.
    pub fn find_map_details(
        &self,
        slot_name: &StorageSlotName,
    ) -> Option<&AccountStorageMapDetails> {
        self.state_headers
            .as_ref()
            .and_then(|details| details.storage_details.find_map_details(slot_name))
    }

    /// Returns the account code, if present.
    pub fn account_code(&self) -> Option<&AccountCode> {
        self.state_headers.as_ref().map(|headers| &headers.code)
    }

    /// Returns the code commitment, if account code is present in the state headers.
    pub fn code_commitment(&self) -> Option<Word> {
        self.account_code().map(AccountCode::commitment)
    }

    /// Returns the current state commitment of the account.
    pub fn account_commitment(&self) -> Word {
        self.account_witness.state_commitment()
    }

    pub fn account_witness(&self) -> &AccountWitness {
        &self.account_witness
    }

    /// Returns the proof of the account's inclusion.
    pub fn merkle_proof(&self) -> &SparseMerklePath {
        self.account_witness.path()
    }

    /// Deconstructs `AccountProof` into its individual parts.
    pub fn into_parts(self) -> (AccountWitness, Option<AccountDetails>) {
        (self.account_witness, self.state_headers)
    }

    /// Consumes the proof and returns the account details, if present (public accounts only).
    pub fn into_details(self) -> Option<AccountDetails> {
        self.state_headers
    }

    /// Mutable accessor for the account details, when present.
    ///
    /// Useful for resolving oversized vault or storage data in place via
    /// [`crate::rpc::NodeRpcClient::resolve_oversize_vault`] and
    /// [`crate::rpc::NodeRpcClient::resolve_oversize_storage_maps`].
    pub fn details_mut(&mut self) -> Option<&mut AccountDetails> {
        self.state_headers.as_mut()
    }
}

// ACCOUNT STORAGE REQUEST
// ================================================================================================

/// Per-slot map data to include in a `/GetAccount` response. Slots absent here are omitted from
/// `map_details` (the storage header still lists every slot).
///
/// - Empty key list: all entries, no proof. May come back as [`StorageMapEntries::LimitExceeded`].
/// - Non-empty key list: just those keys, covered by one partial SMT.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccountStorageRequirements(BTreeMap<StorageSlotName, Vec<StorageMapKey>>);

impl AccountStorageRequirements {
    /// Requests the specified ke ys per slot, covered by one partial SMT per slot. An empty key
    /// iterator for a slot behaves like [`Self::all_entries`].
    ///
    /// Repeated keys within a slot are collapsed, since the node rejects a request that names the
    /// same key twice.
    pub fn new<'a>(
        slots_and_keys: impl IntoIterator<
            Item = (StorageSlotName, impl IntoIterator<Item = &'a StorageMapKey>),
        >,
    ) -> Self {
        let map = slots_and_keys
            .into_iter()
            .map(|(slot_name, keys_iter)| {
                let mut keys_vec: Vec<StorageMapKey> = Vec::new();
                for key in keys_iter {
                    if !keys_vec.contains(key) {
                        keys_vec.push(*key);
                    }
                }
                (slot_name, keys_vec)
            })
            .collect();

        AccountStorageRequirements(map)
    }

    /// Requests every entry of each given slot, without a proof. Oversize maps come back as
    /// [`StorageMapEntries::LimitExceeded`].
    pub fn all_entries(slot_names: &[StorageSlotName]) -> Self {
        AccountStorageRequirements(
            slot_names.iter().map(|name| (name.clone(), Vec::new())).collect(),
        )
    }

    pub fn inner(&self) -> &BTreeMap<StorageSlotName, Vec<StorageMapKey>> {
        &self.0
    }

    /// Returns the keys requested for a given slot, or an empty slice if none were specified.
    pub fn keys_for_slot(&self, slot_name: &StorageSlotName) -> &[StorageMapKey] {
        self.0.get(slot_name).map_or(&[], Vec::as_slice)
    }
}

// GET ACCOUNT REQUEST
// ================================================================================================

/// Controls whether vault data is included in a `/GetAccount` response.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum VaultFetch {
    /// Do not include vault data in the response.
    #[default]
    Skip,
    /// Always include vault data in the response.
    Always,
    /// Include vault data only if the account's current vault root differs from this commitment.
    ///
    /// An omitted asset list is byte-identical to a genuinely empty vault, so callers must keep the
    /// vault whose root they send and verify any reconstruction against the header's vault root.
    IfChangedFrom(Word),
    /// Include proofs for these assets only, in [`AccountVaultDetails::Partial`]. The node proves
    /// an asset that is not in the vault as absent.
    ///
    /// The node accepts at most
    /// [`RpcLimits::vault_assets_limit`](crate::rpc::RpcLimits::vault_assets_limit) assets in a
    /// single request.
    Assets(BTreeSet<AssetId>),
}

/// Which storage map entries to include in a `/GetAccount` response.
///
/// Mirrors the node's `AccountDetailRequest` storage request: the storage header (slot roots) is
/// always returned; this only controls which map *entries* come with it. The variants are mutually
/// exclusive.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum StorageMapFetch {
    /// Don't request any map entries; only the storage header is returned.
    #[default]
    Skip,
    /// Request entries for every storage map slot, without naming the slots in advance. Oversize
    /// maps come back as [`StorageMapEntries::LimitExceeded`], to be resolved via
    /// [`crate::rpc::NodeRpcClient::sync_storage_maps`].
    All,
    /// Request entries only for the explicitly named slots. See [`AccountStorageRequirements`] for
    /// the per-slot semantics.
    Slots(AccountStorageRequirements),
}

/// Storage map entries and vault data to fetch for an account.
///
/// Both policies default to `Skip`. A transaction reads skipped entries and assets during
/// execution, one lookup at a time, from the store or from the node.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccountFetch {
    storage: StorageMapFetch,
    vault: VaultFetch,
}

impl AccountFetch {
    /// Creates a fetch policy that skips storage map entries and vault data.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets which storage map entries to fetch.
    #[must_use]
    pub fn with_storage(mut self, storage: StorageMapFetch) -> Self {
        self.storage = storage;
        self
    }

    /// Sets which vault data to fetch.
    #[must_use]
    pub fn with_vault(mut self, vault: VaultFetch) -> Self {
        self.vault = vault;
        self
    }

    /// Returns the storage map fetch policy.
    pub fn storage(&self) -> &StorageMapFetch {
        &self.storage
    }

    /// Returns the vault fetch policy.
    pub fn vault(&self) -> &VaultFetch {
        &self.vault
    }
}

/// Parameters for [`crate::rpc::NodeRpcClient::get_account`].
///
/// The node returns details for public accounts only. For a private account, the response holds the
/// witness alone, and `fetch` and `known_code` have no effect.
#[derive(Clone, Debug)]
pub struct GetAccountRequest {
    /// Account to fetch.
    pub account_id: AccountId,
    /// Storage map entries and vault data to include in the response.
    pub fetch: AccountFetch,
    /// Block at which to retrieve the proof.
    pub at: AccountStateAt,
    /// Code the client already has. The node omits code with the same commitment.
    pub known_code: Option<AccountCode>,
}

impl GetAccountRequest {
    /// Creates a request at the chain tip with no map entries, known code, or vault data.
    #[must_use]
    pub fn new(account_id: AccountId) -> Self {
        Self {
            account_id,
            fetch: AccountFetch::default(),
            at: AccountStateAt::ChainTip,
            known_code: None,
        }
    }

    /// Sets the storage map entries and vault data to include in the response.
    #[must_use]
    pub fn with_fetch(mut self, fetch: AccountFetch) -> Self {
        self.fetch = fetch;
        self
    }

    /// Sets the target block for this request.
    #[must_use]
    pub fn at(mut self, at: AccountStateAt) -> Self {
        self.at = at;
        self
    }

    /// Provides the code commitment the client already holds, so the node can skip re-sending
    /// matching code.
    #[must_use]
    pub fn with_known_code(mut self, known_code: Option<AccountCode>) -> Self {
        self.known_code = known_code;
        self
    }
}

// ERRORS
// ================================================================================================

#[derive(Debug, Error)]
pub enum AccountProofError {
    #[error(
        "the received account commitment doesn't match the received account header's commitment"
    )]
    InconsistentAccountCommitment,
    #[error("the received account id doesn't match the received account header's id")]
    InconsistentAccountId,
    #[error(
        "the received code commitment doesn't match the received account header's code commitment"
    )]
    InconsistentCodeCommitment,
    #[error(
        "the received storage header's commitment doesn't match the received account header's \
         storage commitment"
    )]
    InconsistentStorageCommitment,
}

#[cfg(test)]
mod tests {
    use miden_protocol::account::AccountId;
    use miden_protocol::asset::{Asset, AssetId, AssetVault, FungibleAsset, PartialVault};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1,
    };

    use super::{AccountVaultDetails, VaultFetch};

    /// Returns a vault that holds one asset, that asset, and the ID of an asset that is not in the
    /// vault.
    fn vault_with_one_asset() -> (AssetVault, Asset, AssetId) {
        let held_faucet = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET).unwrap();
        let absent_faucet = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1).unwrap();
        let asset: Asset = FungibleAsset::new(held_faucet, 100).unwrap().into();
        let vault = AssetVault::new(&[asset]).unwrap();
        (vault, asset, AssetId::new_fungible(absent_faucet))
    }

    /// Returns vault details that hold the proofs of `asset_ids` in `vault`.
    fn proof_details(vault: &AssetVault, asset_ids: &[AssetId]) -> AccountVaultDetails {
        AccountVaultDetails::Partial(
            PartialVault::with_witnesses(asset_ids.iter().map(|asset_id| vault.open(*asset_id)))
                .unwrap(),
        )
    }

    fn asset_request(asset_ids: &[AssetId]) -> VaultFetch {
        VaultFetch::Assets(asset_ids.iter().copied().collect())
    }

    #[test]
    fn proofs_that_cover_the_request_are_accepted() {
        let (vault, asset, absent) = vault_with_one_asset();
        let request = asset_request(&[asset.id(), absent]);

        let details = proof_details(&vault, &[asset.id(), absent])
            .validate_against_request(vault.root(), &request)
            .unwrap();

        let partial_vault = details.into_partial_vault().expect("expected vault proofs");
        assert_eq!(partial_vault.get(asset.id()).unwrap(), Some(asset));
        assert_eq!(partial_vault.get(absent).unwrap(), None);
    }

    #[test]
    fn proofs_against_another_root_are_rejected() {
        let (vault, asset, _) = vault_with_one_asset();
        let other_root = AssetVault::default().root();

        let result = proof_details(&vault, &[asset.id()])
            .validate_against_request(other_root, &asset_request(&[asset.id()]));

        assert!(result.is_err());
    }

    /// The vault holds a second asset, so the proof of the first asset does not cover it. With one
    /// asset only, its proof would also prove the absence of every other asset.
    #[test]
    fn proofs_that_miss_a_requested_asset_are_rejected() {
        let (_, asset, _) = vault_with_one_asset();
        let other_faucet = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1).unwrap();
        let other_asset: Asset = FungibleAsset::new(other_faucet, 5).unwrap().into();
        let vault = AssetVault::new(&[asset, other_asset]).unwrap();

        let result = proof_details(&vault, &[asset.id()]).validate_against_request(
            vault.root(),
            &asset_request(&[asset.id(), other_asset.id()]),
        );

        assert!(result.is_err());
    }

    #[test]
    fn proofs_that_were_not_requested_are_rejected() {
        let (vault, asset, _) = vault_with_one_asset();

        let result = proof_details(&vault, &[asset.id()])
            .validate_against_request(vault.root(), &VaultFetch::Always);

        assert!(result.is_err());
    }

    /// The node can send the full vault contents instead of proofs. The client converts that answer
    /// into proofs, so the callers handle only one shape.
    #[test]
    fn full_vault_answer_is_converted_to_proofs() {
        let (vault, asset, absent) = vault_with_one_asset();
        let details = AccountVaultDetails::Assets(vec![asset])
            .validate_against_request(vault.root(), &asset_request(&[asset.id(), absent]))
            .unwrap();

        let partial_vault = details.into_partial_vault().expect("expected vault proofs");
        assert_eq!(partial_vault.get(asset.id()).unwrap(), Some(asset));
        assert_eq!(partial_vault.get(absent).unwrap(), None);
    }

    #[test]
    fn too_many_assets_answer_to_an_asset_request_is_rejected() {
        let (vault, asset, _) = vault_with_one_asset();
        let result = AccountVaultDetails::TooManyAssets
            .validate_against_request(vault.root(), &asset_request(&[asset.id()]));

        assert!(result.is_err());
    }

    #[test]
    fn proofs_that_track_an_asset_that_was_not_requested_are_rejected() {
        let (_, asset, _) = vault_with_one_asset();
        let other_faucet = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1).unwrap();
        let other_asset: Asset = FungibleAsset::new(other_faucet, 5).unwrap().into();
        let vault = AssetVault::new(&[asset, other_asset]).unwrap();

        let result = proof_details(&vault, &[asset.id(), other_asset.id()])
            .validate_against_request(vault.root(), &asset_request(&[asset.id()]));

        assert!(result.is_err());
    }
}
