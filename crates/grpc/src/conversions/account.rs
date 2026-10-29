use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};

use miden_client_core::errors::RpcError;
use miden_client_core::rpc::domain::account::{
    AccountDetails, AccountFetch, AccountStorageDetails, AccountStorageMapDetails,
    AccountStorageRequirements, AccountVaultDetails, StorageMapEntries, StorageMapEntry,
    StorageMapFetch, VaultFetch,
};
use miden_objects::DecodeMessageExt;
use miden_protocol::account::{
    AccountCode, AccountHeader, AccountStorageHeader, StorageMapKey, StorageSlotName,
    StorageSlotType,
};
use miden_protocol::asset::{Asset, AssetId, PartialVault};
use miden_protocol::crypto::merkle::smt::PartialSmt;
use miden_protocol::{EMPTY_WORD, Word};

use super::MissingFieldHelper;
use crate::generated::rpc::get_account_request::account_detail_request::storage_map_detail_request::{MapKeys, SlotData};
use crate::generated::rpc::get_account_request::account_detail_request::{
    StorageMapDetailRequest, StorageMapDetailRequests, StorageRequest, VaultAssetIds, VaultRequest,
};
use crate::generated::{self as proto};

// REGISTER ACCOUNT REQUEST
// ================================================================================================

/// Hides the invitation code, which is a secret that must not reach logs or error messages.
impl Debug for proto::rpc::RegisterAccountRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisterAccountRequest")
            .field("account_id", &self.account_id)
            .finish_non_exhaustive()
    }
}

/// Hides the invitation code, which is a secret that must not reach logs or error messages.
impl Debug for proto::rpc::IsInvitationCodeValidRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("IsInvitationCodeValidRequest").finish_non_exhaustive()
    }
}

// FROM PROTO ACCOUNT HEADERS
// ================================================================================================

impl proto::rpc::get_account_response::AccountDetails {
    /// Converts the RPC response into `AccountDetails`.
    ///
    /// The RPC response may omit unchanged account codes. If so, this function uses
    /// `known_account_codes` to fill in the missing code. If a required code cannot be found in the
    /// response or `known_account_codes`, an error is returned.
    ///
    /// `fetch` specifies the requested storage keys and vault assets. The proofs must cover these
    /// keys and assets and verify against the account's roots.
    ///
    /// # Errors
    /// - If account code is missing both on `self` and `known_account_codes`
    /// - If data cannot be correctly deserialized
    /// - If a partial map does not cover exactly the keys requested for its slot
    /// - If the vault details do not answer `fetch` for the account's vault root
    pub fn into_domain(
        self,
        known_account_codes: &BTreeMap<Word, AccountCode>,
        fetch: &AccountFetch,
    ) -> Result<AccountDetails, miden_client_core::errors::RpcError> {
        use miden_client_core::errors::RpcError;

        use super::MissingFieldHelper;

        let proto::rpc::get_account_response::AccountDetails {
            header,
            storage_details,
            code,
            vault_details,
        } = self;
        let header: AccountHeader = header
            .ok_or(proto::rpc::get_account_response::AccountDetails::missing_field(stringify!(
                header
            )))?
            .decode_and_verify()?;

        let storage_details: AccountStorageDetails = storage_details
            .ok_or(proto::rpc::get_account_response::AccountDetails::missing_field(stringify!(
                storage_details
            )))?
            .try_into()?;

        storage_details.validate_against_request(fetch.storage())?;

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

        let vault_details = AccountVaultDetails::try_from(
            vault_details
                .ok_or(proto::rpc::AccountVaultDetails::missing_field(stringify!(vault_details)))?,
        )?
        .validate_against_request(header.vault_root(), fetch.vault())?;

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
            .map(TryInto::try_into)
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

// ACCOUNT MAP DETAILS
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
                    .map(TryInto::try_into)
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

    /// Converts the vault details of a `/GetAccount` response.
    ///
    /// The response must set only one of the asset list, the vault proofs, and the too many assets
    /// flag. A response that sets none of them converts to an empty asset list.
    fn try_from(value: proto::rpc::AccountVaultDetails) -> Result<Self, Self::Error> {
        let proto::rpc::AccountVaultDetails { too_many_assets, assets, partial_vault } = value;

        match (too_many_assets, assets.is_empty(), partial_vault) {
            (false, _, None) => Ok(Self::Assets(
                assets
                    .into_iter()
                    .map(DecodeMessageExt::decode_and_verify)
                    .collect::<Result<Vec<Asset>, _>>()?,
            )),
            (true, true, None) => Ok(Self::TooManyAssets),
            (false, true, Some(partial_vault)) => Ok(Self::Partial(partial_vault.try_into()?)),
            _ => Err(RpcError::InvalidResponse(
                "the vault details set more than one of the asset list, the vault proofs, and the \
                 too many assets flag"
                    .into(),
            )),
        }
    }
}

impl TryFrom<proto::rpc::account_vault_details::PartialVault> for PartialVault {
    type Error = RpcError;

    /// Converts the vault proofs of a `/GetAccount` response.
    ///
    /// [`PartialVault::try_from_parts`] rejects duplicate asset IDs and asset IDs that the partial
    /// SMT does not track.
    fn try_from(
        value: proto::rpc::account_vault_details::PartialVault,
    ) -> Result<Self, Self::Error> {
        let asset_ids = value
            .asset_ids
            .into_iter()
            .map(DecodeMessageExt::decode_and_verify)
            .collect::<Result<Vec<AssetId>, _>>()?;

        let partial_smt: PartialSmt = value
            .partial_smt
            .ok_or(proto::rpc::account_vault_details::PartialVault::missing_field(stringify!(
                partial_smt
            )))?
            .decode_and_verify()?;

        PartialVault::try_from_parts(partial_smt, asset_ids).map_err(|err| {
            RpcError::InvalidResponse(format!("rpc api returned a non-valid partial vault: {err}"))
        })
    }
}

// ACCOUNT STORAGE REQUEST
// ================================================================================================

fn storage_map_detail_requests(value: &AccountStorageRequirements) -> Vec<StorageMapDetailRequest> {
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

// GET ACCOUNT REQUEST
// ================================================================================================

/// Encodes the policy as the request's `vault_request`: `None` skips the vault, the empty word
/// (which no real vault root equals) always fetches it, a concrete commitment fetches it only when
/// it differs, and a set of asset IDs requests proofs for those assets.
pub(crate) fn vault_fetch_to_proto(vault: &VaultFetch) -> Option<VaultRequest> {
    let commitment = match vault {
        VaultFetch::Skip => return None,
        VaultFetch::Always => EMPTY_WORD,
        VaultFetch::IfChangedFrom(commitment) => *commitment,
        VaultFetch::Assets(asset_ids) => {
            return Some(VaultRequest::AssetIds(VaultAssetIds {
                asset_ids: asset_ids.iter().copied().map(Into::into).collect(),
            }));
        },
    };
    Some(VaultRequest::AssetVaultCommitment(commitment.into()))
}

pub(crate) fn storage_map_fetch_to_proto(storage: StorageMapFetch) -> Option<StorageRequest> {
    match storage {
        StorageMapFetch::Skip => None,
        StorageMapFetch::All => Some(StorageRequest::AllStorageMaps(true)),
        StorageMapFetch::Slots(reqs) => {
            Some(StorageRequest::StorageMaps(StorageMapDetailRequests {
                storage_maps: storage_map_detail_requests(&reqs),
            }))
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::vec::Vec;

    use miden_protocol::account::AccountId;
    use miden_protocol::asset::{Asset, AssetId, AssetVault, FungibleAsset, PartialVault};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1,
    };

    use super::{
        AccountVaultDetails,
        VaultAssetIds,
        VaultFetch,
        VaultRequest,
        proto,
        vault_fetch_to_proto,
    };

    #[test]
    fn invitation_code_validation_request_debug_hides_code() {
        let code = "secret-invitation-code";
        let request =
            proto::rpc::IsInvitationCodeValidRequest { invitation_code: code.to_string() };

        let debug = format!("{request:?}");
        assert!(debug.contains("IsInvitationCodeValidRequest"));
        assert!(!debug.contains(code));
    }

    /// Returns a vault that holds one asset, that asset, and the ID of an asset that is not in the
    /// vault.
    fn vault_with_one_asset() -> (AssetVault, Asset, AssetId) {
        let held_faucet = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET).unwrap();
        let absent_faucet = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1).unwrap();
        let asset: Asset = FungibleAsset::new(held_faucet, 100).unwrap().into();
        let vault = AssetVault::new(&[asset]).unwrap();
        (vault, asset, AssetId::new_fungible(absent_faucet))
    }

    /// Returns the proofs of `asset_ids` in `vault`.
    fn partial_vault(vault: &AssetVault, asset_ids: &[AssetId]) -> PartialVault {
        PartialVault::with_witnesses(asset_ids.iter().map(|asset_id| vault.open(*asset_id)))
            .unwrap()
    }

    #[test]
    fn asset_request_encodes_the_asset_ids() {
        let (_, asset, absent) = vault_with_one_asset();
        let asset_ids = BTreeSet::from([asset.id(), absent]);

        let request = vault_fetch_to_proto(&VaultFetch::Assets(asset_ids.clone()));

        let expected = asset_ids.into_iter().map(Into::into).collect();
        assert_eq!(request, Some(VaultRequest::AssetIds(VaultAssetIds { asset_ids: expected })));
    }

    #[test]
    fn partial_vault_round_trips_through_proto() {
        let (vault, asset, absent) = vault_with_one_asset();
        let expected = partial_vault(&vault, &[asset.id(), absent]);
        let response = proto::rpc::AccountVaultDetails {
            too_many_assets: false,
            assets: Vec::new(),
            partial_vault: Some(proto::rpc::account_vault_details::PartialVault {
                asset_ids: vec![asset.id().into(), absent.into()],
                partial_smt: Some(expected.partial_smt().clone().into()),
            }),
        };

        let details = AccountVaultDetails::try_from(response).unwrap();

        assert_eq!(details.into_partial_vault(), Some(expected));
    }

    #[test]
    fn partial_vault_with_an_asset_list_is_rejected() {
        let (vault, asset, _) = vault_with_one_asset();
        let partial_vault = partial_vault(&vault, &[asset.id()]);
        let response = proto::rpc::AccountVaultDetails {
            too_many_assets: false,
            assets: vec![asset.into()],
            partial_vault: Some(proto::rpc::account_vault_details::PartialVault {
                asset_ids: vec![asset.id().into()],
                partial_smt: Some(partial_vault.partial_smt().clone().into()),
            }),
        };

        assert!(AccountVaultDetails::try_from(response).is_err());
    }

    #[test]
    fn too_many_assets_flag_converts_to_its_variant() {
        let response = proto::rpc::AccountVaultDetails {
            too_many_assets: true,
            assets: Vec::new(),
            partial_vault: None,
        };

        let details = AccountVaultDetails::try_from(response).unwrap();

        assert!(details.is_too_many_assets());
    }

    #[test]
    fn too_many_assets_flag_with_an_asset_list_is_rejected() {
        let (_, asset, _) = vault_with_one_asset();
        let response = proto::rpc::AccountVaultDetails {
            too_many_assets: true,
            assets: vec![asset.into()],
            partial_vault: None,
        };

        assert!(AccountVaultDetails::try_from(response).is_err());
    }

    #[test]
    fn partial_vault_with_the_too_many_assets_flag_is_rejected() {
        let (vault, asset, _) = vault_with_one_asset();
        let partial_vault = partial_vault(&vault, &[asset.id()]);
        let response = proto::rpc::AccountVaultDetails {
            too_many_assets: true,
            assets: Vec::new(),
            partial_vault: Some(proto::rpc::account_vault_details::PartialVault {
                asset_ids: vec![asset.id().into()],
                partial_smt: Some(partial_vault.partial_smt().clone().into()),
            }),
        };

        assert!(AccountVaultDetails::try_from(response).is_err());
    }
}
