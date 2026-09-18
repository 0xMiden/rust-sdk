use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::vec::Vec;

use miden_objects::DecodeMessageExt;
use miden_protocol::account::{AccountCode, AccountHeader};
use miden_protocol::account::{AccountStorageHeader, StorageMapKey, StorageSlotName, StorageSlotType};
use miden_protocol::asset::Asset;
use miden_protocol::crypto::merkle::smt::PartialSmt;
use miden_protocol::{EMPTY_WORD, Word};

use super::MissingFieldHelper;
use crate::rpc::RpcError;
use crate::rpc::domain::account::{AccountDetails, AccountProof};
use crate::rpc::domain::account::{
    AccountStorageDetails,
    AccountStorageMapDetails,
    AccountStorageRequirements,
    AccountVaultDetails,
    StorageMapEntries,
    StorageMapEntry,
    StorageMapFetch,
    VaultFetch,
};
use crate::rpc::generated::rpc::account_request::account_detail_request::storage_map_detail_request::{MapKeys, SlotData};
use crate::rpc::generated::rpc::account_request::account_detail_request::{
    StorageMapDetailRequest,
    StorageMapDetailRequests,
    StorageRequest,
};
use crate::rpc::generated::{self as proto};

// FROM PROTO ACCOUNT HEADERS
// ================================================================================================

impl proto::rpc::account_response::AccountDetails {
    /// Converts the RPC response into `AccountDetails`.
    ///
    /// The RPC response may omit unchanged account codes. If so, this function uses
    /// `known_account_codes` to fill in the missing code. If a required code cannot be found in the
    /// response or `known_account_codes`, an error is returned.
    ///
    /// `storage_requirements` is the request this response answers, used to check that each partial
    /// map covers exactly the keys that were asked for.
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
        let proto::rpc::account_response::AccountDetails {
            header,
            storage_details,
            code,
            vault_details,
        } = self;
        let header: AccountHeader = header
            .ok_or(proto::rpc::account_response::AccountDetails::missing_field(stringify!(header)))?
            .decode_and_verify()?;

        let storage_details: AccountStorageDetails = storage_details
            .ok_or(proto::rpc::account_response::AccountDetails::missing_field(stringify!(
                storage_details
            )))?
            .try_into()?;

        storage_details.validate_against_request(storage_requirements)?;

        // If an account code was received, it means the previously known account code is no longer
        // valid. If it was not, it means we sent a code commitment that matched and so our code is
        // still valid
        let code = {
            let received_code: Option<AccountCode> =
                code.map(DecodeMessageExt::decode_and_verify).transpose()?;
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

// ACCOUNT STORAGE DETAILS
// ================================================================================================

impl TryFrom<proto::rpc::AccountStorageDetails> for AccountStorageDetails {
    type Error = RpcError;

    fn try_from(value: proto::rpc::AccountStorageDetails) -> Result<Self, Self::Error> {
        let header: AccountStorageHeader = value
            .header
            .ok_or(proto::account::AccountStorageHeader::missing_field(stringify!(header)))?
            .decode_and_verify()?;
        let map_details = value
            .map_details
            .into_iter()
            .map(core::convert::TryInto::try_into)
            .collect::<Result<Vec<AccountStorageMapDetails>, RpcError>>()?;

        // A partial map is only worth anything if it is anchored to the slot root the account
        // commitment covers. Without this check the node could serve a self-consistent tree of its
        // own making.
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

// ACCOUNT STORAGE MAP DETAILS
// ================================================================================================

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
                        .decode_and_verify()?;

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
/// The key lists this guards are bounded by [`AccountStorageMapDetails::MAX_PARTIAL_MAP_KEYS`], so
/// the quadratic scan avoids allocating a set.
fn first_duplicate_key(keys: &[StorageMapKey]) -> Option<&StorageMapKey> {
    keys.iter()
        .enumerate()
        .find_map(|(index, key)| keys[..index].contains(key).then_some(key))
}

// STORAGE MAP ENTRY
// ================================================================================================

impl TryFrom<proto::rpc::account_storage_details::account_storage_map_details::all_map_entries::StorageMapEntry>
    for StorageMapEntry
{
    type Error = RpcError;

    fn try_from(value: proto::rpc::account_storage_details::account_storage_map_details::all_map_entries::StorageMapEntry) -> Result<Self, Self::Error> {
        let key = Word::try_from(value.key.ok_or(RpcError::ExpectedDataMissing("key".into()))?)
            .map(StorageMapKey::new)?;
        let value = value.value.ok_or(RpcError::ExpectedDataMissing("value".into()))?.try_into()?;
        Ok(Self { key, value })
    }
}

// ACCOUNT VAULT DETAILS
// ================================================================================================

impl TryFrom<proto::rpc::AccountVaultDetails> for AccountVaultDetails {
    type Error = RpcError;

    fn try_from(value: proto::rpc::AccountVaultDetails) -> Result<Self, Self::Error> {
        let too_many_assets = value.too_many_assets;
        let assets = value
            .assets
            .into_iter()
            .map(DecodeMessageExt::decode_and_verify)
            .collect::<Result<Vec<Asset>, _>>()?;

        Ok(Self { too_many_assets, assets })
    }
}

// ACCOUNT PROOF
// ================================================================================================

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
        AccountProof::new(witness.decode_and_verify()?, details)
            .map_err(|err| RpcError::InvalidResponse(format!("{err}")))
    }
}

// ACCOUNT STORAGE REQUIREMENTS
// ================================================================================================

impl From<AccountStorageRequirements> for Vec<StorageMapDetailRequest> {
    fn from(value: AccountStorageRequirements) -> Vec<StorageMapDetailRequest> {
        let request_map = value.inner();
        let mut requests = Vec::with_capacity(request_map.len());
        for (slot_name, map_keys) in request_map {
            let slot_data = if map_keys.is_empty() {
                Some(SlotData::AllEntries(true))
            } else {
                let keys = map_keys.iter().map(|key| Word::from(*key).into()).collect();
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

// VAULT FETCH
// ================================================================================================

impl From<VaultFetch> for Option<proto::primitives::Word> {
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

// STORAGE MAP FETCH
// ================================================================================================

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
