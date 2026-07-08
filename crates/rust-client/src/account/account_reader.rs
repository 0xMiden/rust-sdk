//! Provides lazy access to account data.

use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::account::{
    AccountHeader,
    AccountId,
    PartialAccount,
    StorageMapKey,
    StorageMapWitness,
    StorageSlotName,
};
use miden_protocol::address::Address;
use miden_protocol::asset::{Asset, AssetCallbackFlag, AssetVaultKey};
use miden_protocol::{Felt, Word};

use crate::errors::ClientError;
use crate::store::{AccountStatus, Store};

/// Provides lazy access to account data.
///
/// `AccountReader` executes queries lazily - each method call fetches fresh data
/// from storage, ensuring you always see the current state.
///
/// # Example
/// ```ignore
/// let reader = client.account_reader(account_id);
///
/// // Each call fetches fresh data
/// let nonce = reader.nonce().await?;
/// let status = reader.status().await?;
/// let commitment = reader.commitment().await?;
///
/// // Vault access
/// let balance = reader.get_balance(faucet_id).await?;
///
/// // Storage access
/// let value = reader.get_storage_item("my_slot").await?;
/// ```
pub struct AccountReader {
    store: Arc<dyn Store>,
    account_id: AccountId,
}

impl AccountReader {
    /// Creates a new `AccountReader` for the given account.
    pub fn new(store: Arc<dyn Store>, account_id: AccountId) -> Self {
        Self { store, account_id }
    }

    /// Returns the account ID (fixed at construction).
    pub fn account_id(&self) -> AccountId {
        self.account_id
    }

    // HEADER ACCESS
    // --------------------------------------------------------------------------------------------

    /// Retrieves the current account nonce.
    pub async fn nonce(&self) -> Result<Felt, ClientError> {
        let (header, _) = self.header().await?;
        Ok(header.nonce())
    }

    /// Retrieves the account commitment (hash of the full state).
    pub async fn commitment(&self) -> Result<Word, ClientError> {
        let (header, _) = self.header().await?;
        Ok(header.to_commitment())
    }

    /// Retrieves the storage commitment (root of the storage tree).
    pub async fn storage_commitment(&self) -> Result<Word, ClientError> {
        let (header, _) = self.header().await?;
        Ok(header.storage_commitment())
    }

    /// Retrieves the vault root (root of the asset vault tree).
    pub async fn vault_root(&self) -> Result<Word, ClientError> {
        let (header, _) = self.header().await?;
        Ok(header.vault_root())
    }

    /// Retrieves the code commitment (hash of the account code).
    pub async fn code_commitment(&self) -> Result<Word, ClientError> {
        let (header, _) = self.header().await?;
        Ok(header.code_commitment())
    }

    /// Retrieves the current account status (New, Tracked, or Locked).
    pub async fn status(&self) -> Result<AccountStatus, ClientError> {
        let (_, status) = self.header().await?;
        Ok(status)
    }

    /// Retrieves the account header and status.
    pub async fn header(&self) -> Result<(AccountHeader, AccountStatus), ClientError> {
        self.store
            .get_account_header(self.account_id)
            .await?
            .ok_or(ClientError::AccountDataNotFound(self.account_id))
    }

    /// Retrieves the minimal partial account representation for this account.
    pub(crate) async fn partial_account(&self) -> Result<PartialAccount, ClientError> {
        self.store
            .get_minimal_partial_account(self.account_id)
            .await?
            .ok_or(ClientError::AccountDataNotFound(self.account_id))?
            .try_into()
    }

    /// Retrieves the addresses associated with this account.
    pub async fn addresses(&self) -> Result<Vec<Address>, ClientError> {
        self.store
            .get_addresses_by_account_id(self.account_id)
            .await
            .map_err(ClientError::StoreError)
    }

    // VAULT ACCESS
    // --------------------------------------------------------------------------------------------

    /// Retrieves the balance of a fungible asset in the account's vault.
    ///
    /// Returns `0` if the asset is not present in the vault or if the asset is not a fungible
    /// asset.
    ///
    /// To load the entire vault, use
    /// [`Client::get_account_vault`](crate::Client::get_account_vault).
    pub async fn get_balance(&self, faucet_id: AccountId) -> Result<u64, ClientError> {
        let mut total = 0u64;
        for callback_flag in [AssetCallbackFlag::Disabled, AssetCallbackFlag::Enabled] {
            let vault_key = AssetVaultKey::new_fungible(faucet_id, callback_flag);
            if let Some((Asset::Fungible(fungible_asset), _)) =
                self.store.get_account_asset(self.account_id, vault_key).await?
            {
                total = total.saturating_add(u64::from(fungible_asset.amount()));
            }
        }

        Ok(total)
    }

    // STORAGE ACCESS
    // --------------------------------------------------------------------------------------------

    /// Retrieves a storage slot value by name.
    ///
    /// This method fetches the requested slot from storage.
    ///
    /// For `Value` slots, returns the stored word.
    /// For `Map` slots, returns the map root.
    pub async fn get_storage_item(
        &self,
        slot_name: impl Into<StorageSlotName>,
    ) -> Result<Word, ClientError> {
        self.store
            .get_account_storage_item(self.account_id, slot_name.into())
            .await
            .map_err(ClientError::StoreError)
    }

    /// Retrieves a value from a storage map slot by name and key.
    ///
    /// This method fetches only the requested slot from storage.
    ///
    /// # Errors
    /// Returns an error if the slot is not found or is not a map.
    pub async fn get_storage_map_item(
        &self,
        slot_name: impl Into<StorageSlotName>,
        key: StorageMapKey,
    ) -> Result<Word, ClientError> {
        let (value, _witness) =
            self.store.get_account_map_item(self.account_id, slot_name.into(), key).await?;
        Ok(value)
    }

    /// Retrieves a value and its Merkle witness from a storage map slot.
    ///
    /// This method fetches the requested slot from storage and it's inclusion proof.
    ///
    /// # Errors
    /// Returns an error if the slot is not found or is not a map.
    pub async fn get_storage_map_witness(
        &self,
        slot_name: impl Into<StorageSlotName>,
        key: StorageMapKey,
    ) -> Result<(Word, StorageMapWitness), ClientError> {
        self.store
            .get_account_map_item(self.account_id, slot_name.into(), key)
            .await
            .map_err(ClientError::StoreError)
    }
}
