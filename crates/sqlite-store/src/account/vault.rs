//! Vault/asset-related database operations for accounts.

use std::rc::Rc;
use std::vec::Vec;

use miden_client::Serializable;
use miden_client::account::{AccountHeader, AccountId, AccountVaultPatch};
use miden_client::asset::Asset;
use miden_client::store::{AccountSmtForest, StoreError};
use miden_protocol::asset::AssetVaultKey;
use miden_protocol::crypto::merkle::MerkleError;
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction, params};

use crate::sql_error::SqlResultExt;
use crate::{SqliteStore, insert_sql, subst, u64_to_value};

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
            insert_sql!(latest_account_assets { account_id, vault_key, asset } | REPLACE);

        let mut latest_stmt = tx.prepare_cached(LATEST_QUERY).into_store_error()?;
        let account_id_bytes = account_id.to_bytes();

        for asset in assets {
            let vault_key_hex = asset.vault_key().to_string();
            let asset_hex = asset.to_value_word().to_hex();

            latest_stmt
                .execute(params![&account_id_bytes, &vault_key_hex, &asset_hex])
                .into_store_error()?;
        }

        Ok(())
    }

    /// Applies vault delta changes to the account state, updating fungible and non-fungible assets.
    ///
    /// The function updates the SMT forest with all asset changes and verifies that the resulting
    /// vault root matches the expected final state. It archives old values from latest to
    /// historical, deletes removed assets from latest, then inserts updated assets.
    pub(crate) fn apply_account_vault_patch(
        tx: &Transaction<'_>,
        smt_forest: &mut AccountSmtForest,
        account_id: AccountId,
        init_account_state: &AccountHeader,
        final_account_state: &AccountHeader,
        vault_patch: &AccountVaultPatch,
    ) -> Result<(), StoreError> {
        let nonce = final_account_state.nonce().as_canonical_u64();
        let account_id_bytes = account_id.to_bytes();
        let nonce_val = u64_to_value(nonce);

        // The patch carries the absolute final value of every changed entry, so updated assets are
        // inserted verbatim and removed entries (empty value) are deleted. No prior balance lookup
        // or signed-amount arithmetic is needed, and the asset value word already encodes the
        // callback flag for both fungible and non-fungible assets.
        let updated_assets_values: Vec<Asset> = vault_patch.updated_assets().collect();
        let removed_vault_keys: Vec<AssetVaultKey> =
            vault_patch.removed_asset_keys().copied().collect();

        Self::persist_vault_delta(
            tx,
            &account_id_bytes,
            &nonce_val,
            &removed_vault_keys,
            &updated_assets_values,
        )?;

        let new_vault_root = smt_forest.update_asset_nodes(
            init_account_state.vault_root(),
            updated_assets_values.iter().copied(),
            removed_vault_keys.iter().copied(),
        )?;
        if new_vault_root != final_account_state.vault_root() {
            return Err(StoreError::MerkleStoreError(MerkleError::ConflictingRoots {
                expected_root: final_account_state.vault_root(),
                actual_root: new_vault_root,
            }));
        }

        Ok(())
    }

    /// Persists vault delta changes: archives old values from latest to historical,
    /// then updates latest (deletes removed assets, inserts/updates changed assets).
    fn persist_vault_delta(
        tx: &Transaction<'_>,
        account_id_bytes: &[u8],
        nonce_val: &rusqlite::types::Value,
        removed_vault_keys: &[AssetVaultKey],
        updated_assets: &[Asset],
    ) -> Result<(), StoreError> {
        const READ_OLD_ASSET: &str =
            "SELECT asset FROM latest_account_assets WHERE account_id = ? AND vault_key = ?";
        const HISTORICAL_INSERT: &str = insert_sql!(
            historical_account_assets {
                account_id,
                replaced_at_nonce,
                vault_key,
                old_asset
            } | REPLACE
        );
        const LATEST_INSERT: &str =
            insert_sql!(latest_account_assets { account_id, vault_key, asset } | REPLACE);

        let mut hist_stmt = tx.prepare_cached(HISTORICAL_INSERT).into_store_error()?;
        let mut latest_stmt = tx.prepare_cached(LATEST_INSERT).into_store_error()?;

        // Archive and delete removed assets
        for vault_key in removed_vault_keys {
            let vault_key_hex = vault_key.to_string();

            // Read old asset value from latest (should exist since we're removing it)
            let old_asset: Option<String> = tx
                .query_row(READ_OLD_ASSET, params![account_id_bytes, &vault_key_hex], |row| {
                    row.get(0)
                })
                .optional()
                .into_store_error()?
                .flatten();

            // Archive old value to historical
            hist_stmt
                .execute(params![account_id_bytes, nonce_val, &vault_key_hex, old_asset,])
                .into_store_error()?;
        }

        // Batch delete removed assets from latest
        if !removed_vault_keys.is_empty() {
            const DELETE_LATEST_QUERY: &str =
                "DELETE FROM latest_account_assets WHERE account_id = ? AND vault_key IN rarray(?)";
            tx.execute(
                DELETE_LATEST_QUERY,
                params![
                    account_id_bytes,
                    Rc::new(
                        removed_vault_keys
                            .iter()
                            .map(|k| Value::from(k.to_string()))
                            .collect::<Vec<Value>>(),
                    ),
                ],
            )
            .into_store_error()?;
        }

        // Archive old values and insert updated assets
        for asset in updated_assets {
            let vault_key_hex = asset.vault_key().to_string();
            let asset_hex = asset.to_value_word().to_hex();

            // Read old asset value from latest (NULL if asset is new)
            let old_asset: Option<String> = tx
                .query_row(READ_OLD_ASSET, params![account_id_bytes, &vault_key_hex], |row| {
                    row.get(0)
                })
                .optional()
                .into_store_error()?
                .flatten();

            // Archive old value to historical (NULL old_asset = asset was new)
            hist_stmt
                .execute(params![account_id_bytes, nonce_val, &vault_key_hex, old_asset,])
                .into_store_error()?;

            // Insert/update in latest
            latest_stmt
                .execute(params![account_id_bytes, &vault_key_hex, &asset_hex])
                .into_store_error()?;
        }

        Ok(())
    }
}
