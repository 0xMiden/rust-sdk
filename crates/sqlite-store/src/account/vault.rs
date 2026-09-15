//! Vault/asset-related database operations for accounts.

use std::vec::Vec;

use miden_client::Serializable;
use miden_client::account::{AccountHeader, AccountId, AccountVaultPatch};
use miden_client::asset::Asset;
use miden_client::store::StoreError;
use miden_protocol::asset::AssetId;
use rusqlite::{OptionalExtension, Transaction, params};

use crate::sql_error::SqlResultExt;
use crate::{SqliteStore, blob_array, insert_sql, subst, u64_to_value};

impl SqliteStore {
    // READER METHODS
    // --------------------------------------------------------------------------------------------

    // MUTATOR/WRITER METHODS
    // --------------------------------------------------------------------------------------------

    /// Inserts assets into the latest tables only.
    ///
    /// Historical archival is handled separately by the caller when needed.
    pub(crate) fn insert_assets(
        tx: &Transaction<'_>,
        account_id: AccountId,
        assets: impl Iterator<Item = Asset>,
    ) -> Result<(), StoreError> {
        const LATEST_QUERY: &str =
            insert_sql!(latest_account_assets { account_id, asset_id, asset } | REPLACE);

        let mut latest_stmt = tx.prepare_cached(LATEST_QUERY).into_store_error()?;
        let account_id_bytes = account_id.to_bytes();

        for asset in assets {
            let asset_id_bytes = asset.id().to_bytes();
            let asset_bytes = asset.to_value_word().to_bytes();

            latest_stmt
                .execute(params![&account_id_bytes, &asset_id_bytes, &asset_bytes])
                .into_store_error()?;
        }

        Ok(())
    }

    /// Persists vault patch changes to the asset tables, updating fungible and non-fungible assets.
    /// It archives the old value of every changed entry to the historical table, writes the updated
    /// assets to the latest table, and deletes the removed assets from it.
    ///
    /// The corresponding forest update (and the verification that the resulting vault root matches
    /// the final header) happens in `apply_account_patch`, which applies all of an account's tree
    /// changes in one batch.
    pub(crate) fn apply_account_vault_patch(
        tx: &Transaction<'_>,
        account_id: AccountId,
        final_account_state: &AccountHeader,
        vault_patch: &AccountVaultPatch,
    ) -> Result<(), StoreError> {
        const READ_OLD_ASSET: &str =
            "SELECT asset FROM latest_account_assets WHERE account_id = ? AND asset_id = ?";
        const HISTORICAL_INSERT: &str = insert_sql!(
            historical_account_assets {
                account_id,
                replaced_at_nonce,
                asset_id,
                old_asset
            } | REPLACE
        );
        const LATEST_INSERT: &str =
            insert_sql!(latest_account_assets { account_id, asset_id, asset } | REPLACE);
        const DELETE_LATEST: &str =
            "DELETE FROM latest_account_assets WHERE account_id = ? AND asset_id IN rarray(?)";

        let account_id_bytes = account_id.to_bytes();
        let nonce_val = u64_to_value(final_account_state.nonce().as_canonical_u64());
        let mut hist_stmt = tx.prepare_cached(HISTORICAL_INSERT).into_store_error()?;
        let mut latest_stmt = tx.prepare_cached(LATEST_INSERT).into_store_error()?;

        // The patch carries the absolute final value of every changed entry, so updated assets are
        // inserted verbatim and removed entries (empty value) are deleted. No prior balance lookup
        // or signed-amount arithmetic is needed, and the asset value word already encodes the
        // callback flag for both fungible and non-fungible assets.
        //
        // The patch holds one value per asset id, so an id is either removed or updated, never
        // both. The removed assets can therefore be deleted after the inserts.
        let removed_asset_ids: Vec<AssetId> = vault_patch.removed_asset_ids().copied().collect();
        let removed = removed_asset_ids.iter().map(|asset_id| (asset_id.to_bytes(), None::<Asset>));
        let updated =
            vault_patch.updated_assets().map(|asset| (asset.id().to_bytes(), Some(asset)));

        for (asset_id_bytes, new_asset) in removed.chain(updated) {
            // Read the value the entry held before this nonce. A NULL value marks a new entry.
            let old_asset: Option<Vec<u8>> = tx
                .query_row(READ_OLD_ASSET, params![&account_id_bytes, &asset_id_bytes], |row| {
                    row.get(0)
                })
                .optional()
                .into_store_error()?
                .flatten();

            hist_stmt
                .execute(params![&account_id_bytes, &nonce_val, &asset_id_bytes, old_asset])
                .into_store_error()?;

            if let Some(asset) = new_asset {
                let asset_bytes = asset.to_value_word().to_bytes();
                latest_stmt
                    .execute(params![&account_id_bytes, &asset_id_bytes, &asset_bytes])
                    .into_store_error()?;
            }
        }

        if !removed_asset_ids.is_empty() {
            tx.execute(DELETE_LATEST, params![&account_id_bytes, blob_array(&removed_asset_ids)])
                .into_store_error()?;
        }

        Ok(())
    }
}
