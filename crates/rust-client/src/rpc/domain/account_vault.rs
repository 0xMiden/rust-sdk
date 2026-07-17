use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::AccountVaultPatch;
use miden_protocol::asset::{Asset, AssetId};
use miden_protocol::block::BlockNumber;

use crate::rpc::domain::MissingFieldHelper;
use crate::rpc::{RpcConversionError, RpcError, generated as proto};

// ASSET CONVERSION
// ================================================================================================

impl TryFrom<proto::primitives::Asset> for Asset {
    type Error = RpcConversionError;

    fn try_from(value: proto::primitives::Asset) -> Result<Self, Self::Error> {
        let key_word: Word = value
            .key
            .ok_or(proto::primitives::Asset::missing_field(stringify!(key)))?
            .try_into()?;
        let value_word: Word = value
            .value
            .ok_or(proto::primitives::Asset::missing_field(stringify!(value)))?
            .try_into()?;
        Asset::from_id_and_value_words(key_word, value_word)
            .map_err(|e| RpcConversionError::InvalidField(e.to_string()))
    }
}

// ACCOUNT VAULT INFO
// ================================================================================================

/// The merged result of syncing an account's vault over a block range.
///
/// The node reports per-block asset updates that may repeat a vault key across blocks; these are
/// merged into a single absolute [`AccountVaultPatch`] (latest block wins per key). Also
/// provides the current chain tip observed while processing the request.
pub struct AccountVaultInfo {
    /// Current chain tip.
    pub chain_tip: BlockNumber,
    /// The block number of the last check included in this response.
    pub block_number: BlockNumber,
    /// The absolute vault patch merged from the per-block updates.
    pub vault_patch: AccountVaultPatch,
}

// ACCOUNT VAULT CONVERSION
// ================================================================================================

impl TryFrom<proto::rpc::SyncAccountVaultResponse> for AccountVaultInfo {
    type Error = RpcError;

    fn try_from(value: proto::rpc::SyncAccountVaultResponse) -> Result<Self, Self::Error> {
        let pagination_info =
            value
                .pagination_info
                .ok_or(proto::rpc::SyncAccountVaultResponse::missing_field(stringify!(
                    pagination_info
                )))?;

        let mut updates = value
            .updates
            .into_iter()
            .map(vault_update_from_proto)
            .collect::<Result<Vec<_>, _>>()?;

        // The node may report the same asset ID in more than one block, folding the updates in
        // ascending block order lets the latest block win, with an absent asset (`None`) encoding
        // a removal.
        updates.sort_by_key(|(block_num, ..)| *block_num);
        let mut vault_patch = AccountVaultPatch::default();
        for (_, asset_id, asset) in updates {
            match asset {
                Some(asset) => vault_patch.insert_asset(asset),
                None => vault_patch.remove_asset(asset_id),
            }
        }

        Ok(Self {
            chain_tip: pagination_info.chain_tip.into(),
            block_number: pagination_info.block_num.into(),
            vault_patch,
        })
    }
}

// ACCOUNT VAULT UPDATE
// ================================================================================================

/// Converts a single proto vault update into its block number, asset ID, and new asset (`None` if
/// the asset was removed at that block), validating that a present asset matches the reported key.
fn vault_update_from_proto(
    value: proto::rpc::AccountVaultUpdate,
) -> Result<(BlockNumber, AssetId, Option<Asset>), RpcError> {
    let block_num = BlockNumber::from(value.block_num);

    let vault_id: Word = value
        .vault_key
        .ok_or(proto::rpc::SyncAccountVaultResponse::missing_field(stringify!(vault_id)))?
        .try_into()?;
    let asset_id =
        AssetId::try_from(vault_id).map_err(|e| RpcError::InvalidResponse(e.to_string()))?;

    let asset = value.asset.map(Asset::try_from).transpose()?;

    if let Some(ref asset) = asset
        && asset.id() != asset_id
    {
        return Err(RpcError::InvalidResponse(
            "account vault update returned mismatched asset key".to_string(),
        ));
    }

    Ok((block_num, asset_id, asset))
}
