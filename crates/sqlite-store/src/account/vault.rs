//! Vault/asset-related database operations for accounts.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::vec::Vec;

use miden_client::Word;
use miden_client::account::{AccountDelta, AccountHeader, AccountId};
use miden_client::asset::{Asset, AssetWitness, FungibleAsset, NonFungibleDeltaAction};
use miden_client::store::{AccountSmtForest, StoreError};
use miden_protocol::asset::AssetVaultKey;
use miden_protocol::crypto::merkle::MerkleError;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::sql_error::SqlResultExt;
use crate::{SqliteStore, insert_sql, subst, u64_to_value};

impl SqliteStore {
    // READER METHODS
    // --------------------------------------------------------------------------------------------

    /// Fetches the relevant fungible assets of an account that will be updated by the account
    /// delta.
    pub(crate) fn get_account_fungible_assets_for_delta(
        conn: &Connection,
        account_id: AccountId,
        delta: &AccountDelta,
    ) -> Result<BTreeMap<AssetVaultKey, FungibleAsset>, StoreError> {
        let vault_keys = delta
            .vault()
            .fungible()
            .iter()
            .map(|(vault_key, _)| Value::Text(vault_key.to_string()))
            .collect::<Vec<Value>>();

        const QUERY: &str = "SELECT vault_key, asset FROM latest_account_assets WHERE account_id = ? AND vault_key IN rarray(?)";

        Ok(conn
            .prepare(QUERY)
            .into_store_error()?
            .query_map(params![account_id.to_hex(), Rc::new(vault_keys)], |row| {
                let vault_key: String = row.get(0)?;
                let asset: String = row.get(1)?;
                Ok((vault_key, asset))
            })
            .into_store_error()?
            .map(|result| {
                let (vault_key_str, asset_str): (String, String) = result.into_store_error()?;
                let key_word = Word::try_from(vault_key_str)?;
                let value_word = Word::try_from(asset_str)?;
                Ok(Asset::from_key_value_words(key_word, value_word)?)
            })
            .collect::<Result<Vec<Asset>, StoreError>>()?
            .into_iter()
            // SAFETY: all retrieved assets should be fungible
            .map(|asset| (asset.vault_key(), asset.unwrap_fungible()))
            .collect())
    }

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
        let account_id_hex = account_id.to_hex();

        for asset in assets {
            let vault_key_hex = asset.vault_key().to_string();
            let asset_hex = asset.to_value_word().to_hex();

            latest_stmt
                .execute(params![&account_id_hex, &vault_key_hex, &asset_hex])
                .into_store_error()?;
        }

        Ok(())
    }

    /// Applies vault delta changes to the account state, updating fungible and non-fungible assets.
    ///
    /// The function updates the SMT forest with all asset changes and verifies that the resulting
    /// vault root matches the expected final state. It archives old values from latest to
    /// historical, deletes removed assets from latest, then inserts updated assets.
    pub(crate) fn apply_account_vault_delta(
        tx: &Transaction<'_>,
        smt_forest: &mut AccountSmtForest,
        account_id: AccountId,
        init_account_state: &AccountHeader,
        final_account_state: &AccountHeader,
        updated_fungible_assets: BTreeMap<AssetVaultKey, FungibleAsset>,
        delta: &AccountDelta,
    ) -> Result<(), StoreError> {
        let nonce = final_account_state.nonce().as_canonical_u64();
        let account_id_hex = account_id.to_hex();
        let nonce_val = u64_to_value(nonce);

        let (updated_assets_values, removed_vault_keys) =
            Self::compute_vault_asset_updates(updated_fungible_assets, delta)?;

        Self::persist_vault_delta(
            tx,
            &account_id_hex,
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

    /// Computes the absolute asset values and removed vault keys produced by applying `delta`'s
    /// vault changes, given the account's current fungible balances for the affected keys.
    ///
    /// Fungible changes are signed amounts, so they are resolved against the supplied current
    /// balances (`current_balance ± delta_amount`); non-fungible changes are absolute add/remove.
    fn compute_vault_asset_updates(
        mut current_fungible_assets: BTreeMap<AssetVaultKey, FungibleAsset>,
        delta: &AccountDelta,
    ) -> Result<(Vec<Asset>, Vec<AssetVaultKey>), StoreError> {
        // This map will contain all updated assets (indexed by vault key), both fungible and
        // non-fungible.
        let mut updated_assets: BTreeMap<AssetVaultKey, Asset> = BTreeMap::new();
        let mut removed_vault_keys: Vec<AssetVaultKey> = Vec::new();

        // We first process the fungible assets, adding or subtracting them from the current
        // balance as requested.
        for (vault_key, delta) in delta.vault().fungible().iter() {
            let delta_asset = FungibleAsset::new(vault_key.faucet_id(), delta.unsigned_abs())?
                .with_callbacks(vault_key.callback_flag());

            let asset = match current_fungible_assets.remove(vault_key) {
                Some(asset) => {
                    // If the asset exists, update it accordingly.
                    if *delta >= 0 {
                        asset.add(delta_asset)?
                    } else {
                        asset.sub(delta_asset)?
                    }
                },
                None => {
                    // If the asset doesn't exist, we add it to the map to be inserted.
                    delta_asset
                },
            };

            if asset.amount().as_u64() > 0 {
                updated_assets.insert(asset.vault_key(), Asset::Fungible(asset));
            } else {
                removed_vault_keys.push(asset.vault_key());
            }
        }

        // Process non-fungible assets. Here additions or removals don't depend on previous state as
        // each asset is unique.
        let (added_nonfungible_assets, removed_nonfungible_assets) =
            delta.vault().non_fungible().iter().partition::<Vec<_>, _>(|(_, action)| {
                matches!(action, NonFungibleDeltaAction::Add)
            });

        updated_assets.extend(
            added_nonfungible_assets
                .into_iter()
                .map(|(asset, _)| (asset.vault_key(), Asset::NonFungible(*asset))),
        );

        removed_vault_keys
            .extend(removed_nonfungible_assets.iter().map(|(asset, _)| asset.vault_key()));

        Ok((updated_assets.into_values().collect(), removed_vault_keys))
    }

    /// Serves vault asset witnesses for `vault_keys` against the in-batch vault state obtained by
    /// applying `delta` to the account's committed vault, *without* persisting the change.
    ///
    /// The delta is staged on the in-memory [`AccountSmtForest`] (which already holds the
    /// committed vault tree), the witnesses are read at the staged root, and the staged tree is
    /// dropped. `vault_root` is the in-batch root the caller expects; a mismatch means the
    /// supplied delta is inconsistent with the committed state.
    pub(crate) fn vault_asset_witnesses_after_delta(
        conn: &mut Connection,
        smt_forest: &Arc<RwLock<AccountSmtForest>>,
        account_id: AccountId,
        delta: &AccountDelta,
        vault_root: Word,
        vault_keys: BTreeSet<AssetVaultKey>,
    ) -> Result<Vec<AssetWitness>, StoreError> {
        let updated_fungible_assets =
            Self::get_account_fungible_assets_for_delta(conn, account_id, delta)?;
        let (updated_assets, removed_vault_keys) =
            Self::compute_vault_asset_updates(updated_fungible_assets, delta)?;

        let committed_vault_root = Self::get_account_header(conn, account_id)?
            .ok_or(StoreError::AccountDataNotFound(account_id))?
            .0
            .vault_root();

        let mut smt_forest = smt_forest
            .write()
            .map_err(|_| StoreError::DatabaseError("smt_forest write lock poisoned".to_string()))?;

        let (staged_root, witnesses) = smt_forest.staged_vault_asset_witnesses(
            committed_vault_root,
            updated_assets.into_iter(),
            removed_vault_keys.into_iter(),
            vault_keys,
        )?;

        if staged_root != vault_root {
            return Err(StoreError::MerkleStoreError(MerkleError::ConflictingRoots {
                expected_root: vault_root,
                actual_root: staged_root,
            }));
        }

        Ok(witnesses)
    }

    /// Persists vault delta changes: archives old values from latest to historical,
    /// then updates latest (deletes removed assets, inserts/updates changed assets).
    fn persist_vault_delta(
        tx: &Transaction<'_>,
        account_id_hex: &str,
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
                .query_row(READ_OLD_ASSET, params![account_id_hex, &vault_key_hex], |row| {
                    row.get(0)
                })
                .optional()
                .into_store_error()?
                .flatten();

            // Archive old value to historical
            hist_stmt
                .execute(params![account_id_hex, nonce_val, &vault_key_hex, old_asset,])
                .into_store_error()?;
        }

        // Batch delete removed assets from latest
        if !removed_vault_keys.is_empty() {
            const DELETE_LATEST_QUERY: &str =
                "DELETE FROM latest_account_assets WHERE account_id = ? AND vault_key IN rarray(?)";
            tx.execute(
                DELETE_LATEST_QUERY,
                params![
                    account_id_hex,
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
                .query_row(READ_OLD_ASSET, params![account_id_hex, &vault_key_hex], |row| {
                    row.get(0)
                })
                .optional()
                .into_store_error()?
                .flatten();

            // Archive old value to historical (NULL old_asset = asset was new)
            hist_stmt
                .execute(params![account_id_hex, nonce_val, &vault_key_hex, old_asset,])
                .into_store_error()?;

            // Insert/update in latest
            latest_stmt
                .execute(params![account_id_hex, &vault_key_hex, &asset_hex])
                .into_store_error()?;
        }

        Ok(())
    }
}
