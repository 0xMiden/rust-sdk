use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt::{self, Debug, Display, Formatter};

use miden_protocol::account::{
    Account, AccountCode, AccountHeader, AccountId, AccountStorage, AccountStorageHeader,
    StorageMap, StorageMapKey, StorageSlot, StorageSlotHeader, StorageSlotName, StorageSlotType,
};
use miden_protocol::asset::{Asset, AssetVault};
use miden_protocol::block::BlockNumber;
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::crypto::merkle::SparseMerklePath;
use miden_protocol::crypto::merkle::smt::PartialSmt;
use miden_protocol::{EMPTY_WORD, Word};
use miden_tx::utils::ToHex;
use miden_tx::utils::serde::{Deserializable, Serializable};
use thiserror::Error;

use crate::alloc::string::ToString;
use crate::rpc::{AccountStateAt, RpcError};
use crate::rpc::domain::MissingFieldHelper;
use crate::rpc::errors::RpcConversionError;
use crate::rpc::generated::rpc::account_request::account_detail_request::storage_map_detail_request::{MapKeys, SlotData};
use crate::rpc::generated::rpc::account_request::account_detail_request::{
    StorageMapDetailRequest, StorageMapDetailRequests, StorageRequest,
};
use crate::rpc::generated::{self as proto};

// ACCOUNT ID
// ================================================================================================

impl Display for proto::account::AccountId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("0x{}", self.id.to_hex()))
    }
}

impl Debug for proto::account::AccountId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

// INTO PROTO ACCOUNT ID
// ================================================================================================

impl From<AccountId> for proto::account::AccountId {
    fn from(account_id: AccountId) -> Self {
        Self { id: account_id.to_bytes() }
    }
}

// FROM PROTO ACCOUNT ID
// ================================================================================================

impl TryFrom<proto::account::AccountId> for AccountId {
    type Error = RpcConversionError;

    fn try_from(account_id: proto::account::AccountId) -> Result<Self, Self::Error> {
        AccountId::read_from_bytes(&account_id.id).map_err(|_| RpcConversionError::NotAValidFelt)
    }
}

// ACCOUNT HEADER
// ================================================================================================

impl TryInto<AccountHeader> for proto::account::AccountHeader {
    type Error = crate::rpc::RpcError;

    fn try_into(self) -> Result<AccountHeader, Self::Error> {
        use miden_protocol::Felt;

        use crate::rpc::domain::MissingFieldHelper;

        let proto::account::AccountHeader {
            account_id,
            nonce,
            vault_root,
            storage_commitment,
            code_commitment,
        } = self;

        let account_id: AccountId = account_id
            .ok_or(proto::account::AccountHeader::missing_field(stringify!(account_id)))?
            .try_into()?;
        let vault_root = vault_root
            .ok_or(proto::account::AccountHeader::missing_field(stringify!(vault_root)))?
            .try_into()?;
        let storage_commitment = storage_commitment
            .ok_or(proto::account::AccountHeader::missing_field(stringify!(storage_commitment)))?
            .try_into()?;
        let code_commitment = code_commitment
            .ok_or(proto::account::AccountHeader::missing_field(stringify!(code_commitment)))?
            .try_into()?;

        let nonce = Felt::new(nonce).map_err(|_| RpcConversionError::NotAValidFelt)?;
        Ok(AccountHeader::new(
            account_id,
            nonce,
            vault_root,
            storage_commitment,
            code_commitment,
        ))
    }
}

// ACCOUNT STORAGE HEADER
// ================================================================================================

impl TryInto<AccountStorageHeader> for proto::account::AccountStorageHeader {
    type Error = crate::rpc::RpcError;

    fn try_into(self) -> Result<AccountStorageHeader, Self::Error> {
        use crate::rpc::RpcError;
        use crate::rpc::domain::MissingFieldHelper;

        let mut header_slots: Vec<StorageSlotHeader> = Vec::with_capacity(self.slots.len());

        for slot in self.slots {
            let slot_value: Word = slot
                .commitment
                .ok_or(proto::account::account_storage_header::StorageSlot::missing_field(
                    stringify!(commitment),
                ))?
                .try_into()?;

            let slot_type = u8::try_from(slot.slot_type)
                .map_err(|e| RpcError::InvalidResponse(e.to_string()))
                .and_then(|v| {
                    StorageSlotType::try_from(v)
                        .map_err(|e| RpcError::InvalidResponse(e.to_string()))
                })?;
            let slot_name = StorageSlotName::new(slot.slot_name)
                .map_err(|err| RpcError::InvalidResponse(err.to_string()))?;

            header_slots.push(StorageSlotHeader::new(slot_name, slot_type, slot_value));
        }

        header_slots.sort_by_key(StorageSlotHeader::id);
        AccountStorageHeader::new(header_slots)
            .map_err(|err| RpcError::InvalidResponse(err.to_string()))
    }
}

// FROM PROTO ACCOUNT HEADERS
// ================================================================================================

#[cfg(feature = "tonic")]
impl proto::rpc::account_response::AccountDetails {
    /// Converts the RPC response into `AccountDetails`.
    ///
    /// The RPC response may omit unchanged account codes. If so, this function uses
    /// `known_account_codes` to fill in the missing code. If a required code cannot be found in
    /// the response or `known_account_codes`, an error is returned.
    ///
    /// `storage_requirements` is the request this response answers, used to check that each
    /// partial map covers exactly the keys that were asked for.
    ///
    /// # Errors
    /// - If account code is missing both on `self` and `known_account_codes`
    /// - If data cannot be correctly deserialized
    /// - If a partial map does not cover exactly the keys requested for its slot
    pub fn into_domain(
        self,
        known_account_codes: &BTreeMap<Word, AccountCode>,
        storage_requirements: &AccountStorageRequirements,
    ) -> Result<AccountDetails, crate::rpc::RpcError> {
        use crate::rpc::RpcError;
        use crate::rpc::domain::MissingFieldHelper;

        let proto::rpc::account_response::AccountDetails {
            header,
            storage_details,
            code,
            vault_details,
        } = self;
        let header: AccountHeader = header
            .ok_or(proto::rpc::account_response::AccountDetails::missing_field(stringify!(header)))?
            .try_into()?;

        let storage_details: AccountStorageDetails = storage_details
            .ok_or(proto::rpc::account_response::AccountDetails::missing_field(stringify!(
                storage_details
            )))?
            .try_into()?;

        storage_details.validate_against_request(storage_requirements)?;

        // If an account code was received, it means the previously known account code is no longer
        // valid. If it was not, it means we sent a code commitment that matched and so our code
        // is still valid
        let code = {
            let received_code = code.map(|c| AccountCode::read_from_bytes(&c)).transpose()?;
            match received_code {
                Some(code) => code,
                None => known_account_codes
                    .get(&header.code_commitment())
                    .ok_or(RpcError::InvalidResponse(
                        "Account code was not provided, but the response did not contain it either"
                            .into(),
                    ))?
                    .clone(),
            }
        };

        let vault_details = vault_details
            .ok_or(proto::rpc::AccountVaultDetails::missing_field(stringify!(vault_details)))?
            .try_into()?;

        Ok(AccountDetails {
            header,
            storage_details,
            code,
            vault_details,
        })
    }
}

// ACCOUNT PROOF
// ================================================================================================

/// Contains a block number, and a list of account proofs at that block.
pub type AccountProofs = (BlockNumber, Vec<AccountProof>);

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

impl TryFrom<&AccountDetails> for Account {
    type Error = RpcError;

    /// Builds an [`Account`] from [`AccountDetails`].
    ///
    /// This conversion fails if the account details are incomplete, i.e., when the account's
    /// storage maps or vault exceed the node's size threshold, or when only specific map keys
    /// were requested.
    fn try_from(details: &AccountDetails) -> Result<Self, Self::Error> {
        if details.vault_details.too_many_assets {
            return Err(RpcError::ExpectedDataMissing(
                "cannot build account: vault has too many assets".into(),
            ));
        }

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

        let asset_vault = AssetVault::new(&details.vault_details.assets).map_err(|err| {
            RpcError::InvalidResponse(format!("rpc api returned non-valid assets: {err}"))
        })?;

        let account_storage = AccountStorage::new(slots).map_err(|err| {
            RpcError::InvalidResponse(format!("rpc api returned non-valid storage slots: {err}"))
        })?;

        Account::new(
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
        })
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
    /// - If a partial map covers a different number of keys than were requested for its slot.
    /// - If a partial map covers a key that was not requested.
    pub fn validate_against_request(
        &self,
        storage_requirements: &AccountStorageRequirements,
    ) -> Result<(), RpcError> {
        for map_detail in &self.map_details {
            let StorageMapEntries::PartialMap { map_keys, .. } = &map_detail.entries else {
                continue;
            };

            let requested_keys = storage_requirements.keys_for_slot(&map_detail.slot_name);
            if map_keys.len() != requested_keys.len() {
                return Err(RpcError::InvalidResponse(format!(
                    "expected {} keys for storage map slot '{}', got {}",
                    requested_keys.len(),
                    map_detail.slot_name,
                    map_keys.len(),
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

impl TryFrom<proto::rpc::AccountStorageDetails> for AccountStorageDetails {
    type Error = RpcError;

    fn try_from(value: proto::rpc::AccountStorageDetails) -> Result<Self, Self::Error> {
        let header: AccountStorageHeader = value
            .header
            .ok_or(proto::account::AccountStorageHeader::missing_field(stringify!(header)))?
            .try_into()?;
        let map_details = value
            .map_details
            .into_iter()
            .map(core::convert::TryInto::try_into)
            .collect::<Result<Vec<AccountStorageMapDetails>, RpcError>>()?;

        // A partial map is only worth anything if it is anchored to the slot root the account
        // commitment covers. Without this check the node could serve a self-consistent tree of
        // its own making.
        for map_detail in &map_details {
            let StorageMapEntries::PartialMap { partial_smt, .. } = &map_detail.entries else {
                continue;
            };

            let slot = header
                .slots()
                .find(|slot| *slot.name() == map_detail.slot_name)
                .ok_or_else(|| {
                    RpcError::InvalidResponse(format!(
                        "partial storage map references slot '{}', which is absent from the \
                         storage header",
                        map_detail.slot_name,
                    ))
                })?;
            if slot.slot_type() != StorageSlotType::Map {
                return Err(RpcError::InvalidResponse(format!(
                    "partial storage map references slot '{}', which is not a map",
                    map_detail.slot_name,
                )));
            }
            if partial_smt.root() != slot.value() {
                return Err(RpcError::InvalidResponse(format!(
                    "partial storage map for slot '{}' has root {} but the storage header reports \
                     {}",
                    map_detail.slot_name,
                    partial_smt.root(),
                    slot.value(),
                )));
            }
        }

        Ok(Self { header, map_details })
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

    /// Returns `true` when the node reported that this slot has more entries than it will return
    /// in a single response, meaning the entries have to be fetched through
    /// [`crate::rpc::NodeRpcClient::sync_storage_maps`] instead.
    pub fn is_limit_exceeded(&self) -> bool {
        matches!(self.entries, StorageMapEntries::LimitExceeded)
    }
}

impl TryFrom<proto::rpc::account_storage_details::AccountStorageMapDetails>
    for AccountStorageMapDetails
{
    type Error = RpcError;

    fn try_from(
        value: proto::rpc::account_storage_details::AccountStorageMapDetails,
    ) -> Result<Self, Self::Error> {
        use proto::rpc::account_storage_details::account_storage_map_details::Result as ProtoResult;

        let slot_name = StorageSlotName::new(value.slot_name)
            .map_err(|err| RpcError::ExpectedDataMissing(err.to_string()))?;

        let entries = match value.result {
            Some(ProtoResult::TooManyEntries(true)) => StorageMapEntries::LimitExceeded,
            Some(ProtoResult::TooManyEntries(false)) => {
                return Err(RpcError::InvalidResponse(
                    "too_many_entries must be true when set".into(),
                ));
            },
            Some(ProtoResult::AllEntries(all_entries)) => {
                let entries = all_entries
                    .entries
                    .into_iter()
                    .map(core::convert::TryInto::try_into)
                    .collect::<Result<Vec<StorageMapEntry>, RpcError>>()?;
                StorageMapEntries::AllEntries(entries)
            },
            Some(ProtoResult::PartialMap(partial_map)) => {
                if partial_map.map_keys.len() > Self::MAX_PARTIAL_MAP_KEYS {
                    return Err(RpcError::InvalidResponse(format!(
                        "partial storage map for slot '{slot_name}' contains {} keys, exceeding \
                         the limit of {}",
                        partial_map.map_keys.len(),
                        Self::MAX_PARTIAL_MAP_KEYS,
                    )));
                }

                let map_keys = partial_map
                    .map_keys
                    .into_iter()
                    .map(|key| Word::try_from(key).map(StorageMapKey::new))
                    .collect::<Result<Vec<_>, _>>()?;
                if let Some(key) = first_duplicate_key(&map_keys) {
                    return Err(RpcError::InvalidResponse(format!(
                        "partial storage map for slot '{slot_name}' repeats key {}",
                        key.to_hex(),
                    )));
                }

                let partial_smt: PartialSmt = partial_map
                    .partial_smt
                    .ok_or(proto::rpc::account_storage_details::account_storage_map_details::PartialStorageMap::missing_field(
                        stringify!(partial_smt),
                    ))?
                    .try_into()?;

                // The response sends the values only inside the tree, so a key the tree does not
                // track carries no value at all and would fail later, at read time.
                for key in &map_keys {
                    partial_smt.get_value(&key.hash().as_word()).map_err(|_| {
                        RpcError::InvalidResponse(format!(
                            "partial storage map for slot '{slot_name}' does not track key {}",
                            key.to_hex(),
                        ))
                    })?;
                }

                StorageMapEntries::PartialMap { map_keys, partial_smt }
            },
            None => {
                return Err(RpcError::InvalidResponse(format!(
                    "storage map details for slot '{slot_name}' carry no result",
                )));
            },
        };

        Ok(Self { slot_name, entries })
    }
}

/// Returns the first key that appears more than once, if any.
///
/// The key lists this guards are bounded by [`AccountStorageMapDetails::MAX_PARTIAL_MAP_KEYS`],
/// so the quadratic scan avoids allocating a set.
fn first_duplicate_key(keys: &[StorageMapKey]) -> Option<&StorageMapKey> {
    keys.iter()
        .enumerate()
        .find_map(|(index, key)| keys[..index].contains(key).then_some(key))
}

// STORAGE MAP ENTRY
// ================================================================================================

/// A storage map entry containing a key-value pair.
#[derive(Clone, Debug)]
pub struct StorageMapEntry {
    pub key: StorageMapKey,
    pub value: Word,
}

impl TryFrom<proto::rpc::account_storage_details::account_storage_map_details::all_map_entries::StorageMapEntry>
    for StorageMapEntry
{
    type Error = RpcError;

    fn try_from(value: proto::rpc::account_storage_details::account_storage_map_details::all_map_entries::StorageMapEntry) -> Result<Self, Self::Error> {
        let key: StorageMapKey =
            value.key.ok_or(RpcError::ExpectedDataMissing("key".into()))?.try_into()?;
        let value = value.value.ok_or(RpcError::ExpectedDataMissing("value".into()))?.try_into()?;
        Ok(Self { key, value })
    }
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

#[derive(Clone, Debug)]
pub struct AccountVaultDetails {
    /// A flag that is set to true if the account contains too many assets. This indicates
    /// to the user that `SyncAccountVault` endpoint should be used to retrieve the
    /// account's assets
    pub too_many_assets: bool,
    /// When `too_many_assets` == false, this will contain the list of assets in the
    /// account's vault
    pub assets: Vec<Asset>,
}

impl TryFrom<proto::rpc::AccountVaultDetails> for AccountVaultDetails {
    type Error = RpcError;

    fn try_from(value: proto::rpc::AccountVaultDetails) -> Result<Self, Self::Error> {
        let too_many_assets = value.too_many_assets;
        let assets = value
            .assets
            .into_iter()
            .map(Asset::try_from)
            .collect::<Result<Vec<Asset>, _>>()?;

        Ok(Self { too_many_assets, assets })
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
    /// Creates a new [`AccountProof`].
    pub fn new(
        account_witness: AccountWitness,
        account_details: Option<AccountDetails>,
    ) -> Result<Self, AccountProofError> {
        if let Some(AccountDetails {
            header: account_header,
            storage_details: _,
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

#[cfg(feature = "tonic")]
impl TryFrom<proto::rpc::AccountResponse> for AccountProof {
    type Error = RpcError;
    fn try_from(account_proof: proto::rpc::AccountResponse) -> Result<Self, Self::Error> {
        let Some(witness) = account_proof.witness else {
            return Err(RpcError::ExpectedDataMissing(
                "GetAccount returned an account without witness".to_string(),
            ));
        };

        let details: Option<AccountDetails> = {
            match account_proof.details {
                None => None,
                Some(details) => Some(
                    details
                        .into_domain(&BTreeMap::new(), &AccountStorageRequirements::default())?,
                ),
            }
        };
        AccountProof::new(witness.try_into()?, details)
            .map_err(|err| RpcError::InvalidResponse(format!("{err}")))
    }
}

// ACCOUNT WITNESS
// ================================================================================================

impl TryFrom<proto::account::AccountWitness> for AccountWitness {
    type Error = RpcError;

    fn try_from(account_witness: proto::account::AccountWitness) -> Result<Self, Self::Error> {
        let state_commitment = account_witness
            .commitment
            .ok_or(proto::account::AccountWitness::missing_field(stringify!(state_commitment)))?
            .try_into()?;
        let merkle_path = account_witness
            .path
            .ok_or(proto::account::AccountWitness::missing_field(stringify!(merkle_path)))?
            .try_into()?;
        let account_id = account_witness
            .witness_id
            .ok_or(proto::account::AccountWitness::missing_field(stringify!(witness_id)))?
            .try_into()?;

        let witness = AccountWitness::new(account_id, state_commitment, merkle_path)
            .map_err(|err| RpcError::InvalidResponse(format!("{err}")))?;
        Ok(witness)
    }
}

// ACCOUNT STORAGE REQUEST
// ================================================================================================

/// Per-slot map data to include in a `/GetAccount` response. Slots absent here are omitted
/// from `map_details` (the storage header still lists every slot).
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

impl From<AccountStorageRequirements> for Vec<StorageMapDetailRequest> {
    fn from(value: AccountStorageRequirements) -> Vec<StorageMapDetailRequest> {
        let request_map = value.0;
        let mut requests = Vec::with_capacity(request_map.len());
        for (slot_name, map_keys) in request_map {
            let slot_data = if map_keys.is_empty() {
                Some(SlotData::AllEntries(true))
            } else {
                let keys = map_keys.into_iter().map(|key| Word::from(key).into()).collect();
                Some(SlotData::MapKeys(MapKeys { map_keys: keys }))
            };
            requests.push(StorageMapDetailRequest {
                slot_name: slot_name.to_string(),
                slot_data,
            });
        }
        requests
    }
}

impl Serializable for AccountStorageRequirements {
    fn write_into<W: miden_tx::utils::serde::ByteWriter>(&self, target: &mut W) {
        target.write(&self.0);
    }
}

impl Deserializable for AccountStorageRequirements {
    fn read_from<R: miden_tx::utils::serde::ByteReader>(
        source: &mut R,
    ) -> Result<Self, miden_tx::utils::serde::DeserializationError> {
        Ok(AccountStorageRequirements(source.read()?))
    }
}

// GET ACCOUNT REQUEST
// ================================================================================================

/// Controls whether vault data is included in a `/GetAccount` response.
#[derive(Clone, Debug, Default)]
pub enum VaultFetch {
    /// Do not include vault data in the response.
    #[default]
    Skip,
    /// Always include vault data in the response.
    Always,
    /// Include vault data only if the account's current vault root differs from this commitment.
    ///
    /// An omitted asset list is byte-identical to a genuinely empty vault, so callers must keep
    /// the vault whose root they send and verify any reconstruction against the header's vault
    /// root.
    IfChangedFrom(Word),
}

impl From<VaultFetch> for Option<proto::primitives::Digest> {
    /// Encodes the policy as the request's `asset_vault_commitment`: `None` skips the vault, the
    /// empty word (which no real vault root equals) always fetches it, and a concrete commitment
    /// fetches only when it differs.
    fn from(vault: VaultFetch) -> Self {
        match vault {
            VaultFetch::Skip => None,
            VaultFetch::Always => Some(EMPTY_WORD.into()),
            VaultFetch::IfChangedFrom(commitment) => Some(commitment.into()),
        }
    }
}

/// Which storage map entries to include in a `/GetAccount` response.
///
/// Mirrors the node's `AccountDetailRequest` storage request: the storage header (slot roots) is
/// always returned; this only controls which map *entries* come with it. The variants are
/// mutually exclusive.
#[derive(Clone, Debug, Default)]
pub enum StorageMapFetch {
    /// Don't request any map entries; only the storage header is returned.
    #[default]
    Skip,
    /// Request entries for every storage map slot, without naming the slots in advance. Oversize
    /// maps come back as [`StorageMapEntries::LimitExceeded`], to be resolved via
    /// [`crate::rpc::NodeRpcClient::sync_storage_maps`].
    All,
    /// Request entries only for the explicitly named slots. See [`AccountStorageRequirements`]
    /// for the per-slot semantics.
    Slots(AccountStorageRequirements),
}

impl From<StorageMapFetch> for Option<StorageRequest> {
    fn from(storage: StorageMapFetch) -> Self {
        match storage {
            StorageMapFetch::Skip => None,
            StorageMapFetch::All => Some(StorageRequest::AllStorageMaps(true)),
            StorageMapFetch::Slots(reqs) => {
                Some(StorageRequest::StorageMaps(StorageMapDetailRequests {
                    storage_maps: reqs.into(),
                }))
            },
        }
    }
}

/// Parameters for [`crate::rpc::NodeRpcClient::get_account`].
#[derive(Clone, Debug, Default)]
pub struct GetAccountRequest {
    /// Which storage map entries to include in the response.
    pub storage: StorageMapFetch,
    /// Block at which to retrieve the proof.
    pub at: AccountStateAt,
    /// Code commitment the client already has. When the on-chain commitment matches, the node
    /// skips re-sending the code.
    pub known_code: Option<AccountCode>,
    /// Vault data retrieval policy.
    pub vault: VaultFetch,
}

impl GetAccountRequest {
    /// Creates a request for the minimal account data: the account commitment and storage header
    /// at the chain tip, with no map entries, no known code, and no vault data. Opt into
    /// additional data with the builder methods.
    #[must_use]
    pub fn new() -> Self {
        Self {
            storage: StorageMapFetch::Skip,
            at: AccountStateAt::ChainTip,
            known_code: None,
            vault: VaultFetch::Skip,
        }
    }

    /// Sets which storage map entries to include in the response.
    #[must_use]
    pub fn with_storage(mut self, storage: StorageMapFetch) -> Self {
        self.storage = storage;
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

    /// Sets the vault data retrieval policy.
    #[must_use]
    pub fn with_vault(mut self, vault: VaultFetch) -> Self {
        self.vault = vault;
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
}
