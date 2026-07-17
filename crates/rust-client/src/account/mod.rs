//! The `account` module provides types and client APIs for managing accounts within the Miden
//! network.
//!
//! Accounts are foundational entities of the Miden protocol. They store assets and define
//! rules for manipulating them. Once an account is registered with the client, its state will
//! be updated accordingly, and validated against the network state on every sync.
//!
//! # Example
//!
//! To add a new account to the client's store, you might use the [`Client::add_account`] method as
//! follows:
//!
//! ```rust
//! # use miden_client::{
//! #   account::{Account, AccountBuilder, AccountBuilderSchemaCommitmentExt, AccountType, component::BasicWallet},
//! #   crypto::FeltRng
//! # };
//! # async fn add_new_account_example<AUTH>(
//! #     client: &mut miden_client::Client<AUTH>
//! # ) -> Result<(), miden_client::ClientError> {
//! #   let random_seed = Default::default();
//! let account = AccountBuilder::new(random_seed)
//!     .account_type(AccountType::Private)
//!     .with_component(BasicWallet)
//!     .build_with_schema_commitment()?;
//!
//! // Add the account to the client. The account already embeds its seed information.
//! client.add_account(&account, false).await?;
//! #   Ok(())
//! # }
//! ```
//!
//! For more details on accounts, refer to the [Account] documentation.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use miden_protocol::Felt;
use miden_protocol::account::auth::PublicKey;
pub use miden_protocol::account::{
    Account,
    AccountBuilder,
    AccountCode,
    AccountComponent,
    AccountComponentCode,
    AccountDelta,
    AccountFile,
    AccountHeader,
    AccountId,
    AccountIdPrefix,
    AccountIdPrefixV1,
    AccountIdV1,
    AccountIdVersion,
    AccountPatch,
    AccountProcedureRoot,
    AccountStorage,
    AccountStoragePatch,
    AccountType,
    AccountUpdateDetails,
    AccountVaultPatch,
    PartialAccount,
    PartialStorage,
    PartialStorageMap,
    RoleSymbol,
    StorageMap,
    StorageMapKey,
    StorageMapKeyHash,
    StorageMapPatch,
    StorageMapPatchEntries,
    StorageMapWitness,
    StorageSlot,
    StorageSlotContent,
    StorageSlotId,
    StorageSlotName,
    StorageSlotPatch,
    StorageSlotType,
    StorageValuePatch,
};
pub use miden_protocol::address::{Address, AddressInterface, AddressType, NetworkId};
use miden_protocol::asset::AssetVault;
pub use miden_protocol::errors::{AccountIdError, AddressError, NetworkIdError};
use miden_protocol::note::NoteTag;
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

/// Display-only metadata for a faucet account, persisted in the client's settings store.
///
/// Populated lazily by the CLI resolver from the on-chain token config of a public faucet
/// and persisted under a `faucet_metadata:<faucet-id>` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaucetMetadata {
    pub symbol: String,
    pub decimals: u8,
}

impl Serializable for FaucetMetadata {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.symbol.write_into(target);
        target.write_u8(self.decimals);
    }
}

impl Deserializable for FaucetMetadata {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let symbol = String::read_from(source)?;
        let decimals = source.read_u8()?;
        Ok(Self { symbol, decimals })
    }
}

mod account_reader;
pub use account_reader::AccountReader;
/// Raw access to `miden-standards` account modules for items not curated by `miden-client`.
pub use miden_standards::account as standards;
use miden_standards::account::auth::{Approver, AuthSingleSig};
use miden_standards::account::faucets::FungibleFaucet;
pub use miden_standards::account::inspection::{
    AccountBuilderSchemaCommitmentExt,
    AccountSchemaCommitment,
};
// RE-EXPORTS
// ================================================================================================
pub use miden_standards::account::interface::{
    AccountComponentInterface,
    AccountComponentInterfaceExt,
    AccountInterface,
    AccountInterfaceExt,
};
use miden_standards::account::wallets::BasicWallet;

use super::Client;
use crate::asset::TokenSymbol;
use crate::errors::ClientError;
use crate::rpc::domain::account::GetAccountRequest;
use crate::rpc::node::{EndpointError, GetAccountError};
use crate::store::{AccountStatus, AccountStorageFilter, ClientAccountType};
use crate::sync::NoteTagRecord;

pub mod component {
    pub const MIDEN_PACKAGE_EXTENSION: &str = "masp";

    pub use miden_protocol::account::auth::*;
    pub use miden_protocol::account::component::{
        FeltSchema,
        InitStorageData,
        InitStorageDataError,
        MapSlotSchema,
        SchemaRequirement,
        SchemaType,
        SchemaTypeError,
        StorageSchema,
        StorageSlotSchema,
        StorageValueName,
        StorageValueNameError,
        ValueSlotSchema,
        WordSchema,
        WordValue,
    };
    pub use miden_protocol::account::{
        AccountComponent,
        AccountComponentMetadata,
        AccountComponentName,
        AccountProcedureRoot,
        RoleSymbol,
    };
    pub use miden_standards::account::access::{
        AccessControl,
        Authority,
        AuthorityError,
        Ownable2Step,
        Ownable2StepError,
        Pausable,
        PausableManager,
        PausableStorage,
        RoleBasedAccessControl,
    };
    pub use miden_standards::account::auth::*;
    pub use miden_standards::account::components::StandardAccountComponent;
    pub use miden_standards::account::faucets::{
        Description,
        ExternalLink,
        FungibleFaucet,
        FungibleFaucetBuilder,
        FungibleFaucetError,
        LogoURI,
        TokenMetadata,
        TokenMetadataError,
        TokenName,
        create_network_fungible_faucet,
        create_singlesig_user_fungible_faucet,
    };
    pub use miden_standards::account::policies::{
        AllowlistOwnerControlled,
        AllowlistStorage,
        BasicAllowlist,
        BasicBlocklist,
        BlocklistOwnerControlled,
        BlocklistStorage,
        BurnAllowAll,
        BurnOwnerOnly,
        BurnPolicy,
        BurnPolicyError,
        MinBurnAmount,
        MintAllowAll,
        MintOwnerOnly,
        MintPolicy,
        MintPolicyError,
        TokenPolicyManager,
        TokenPolicyManagerBuilder,
        TransferAllowAll,
        TransferPolicy,
        TransferPolicyError,
    };
    pub use miden_standards::account::wallets::BasicWallet;
}

// CLIENT METHODS
// ================================================================================================

/// This section of the [Client] contains methods for:
///
/// - **Account creation:** Use the [`AccountBuilder`] to construct new accounts, specifying account
///   visibility (`AccountType::Public` / `AccountType::Private`) and attaching necessary components
///   (e.g., basic wallet or fungible faucet). Prefer
///   [`AccountBuilderSchemaCommitmentExt::build_with_schema_commitment`] so the account includes
///   merged storage schema commitment metadata; use plain [`AccountBuilder::build`] only when you
///   need to opt out. After creation, accounts can be added to the client.
///
/// - **Account tracking:** Accounts added via the client are persisted to the local store, where
///   their state (including nonce, balance, and metadata) is updated upon every synchronization
///   with the network.
///
/// - **Data retrieval:** The module also provides methods to fetch account-related data.
impl<AUTH> Client<AUTH> {
    // ACCOUNT CREATION
    // --------------------------------------------------------------------------------------------

    /// Adds the provided [Account] in the store so it can start being tracked by the client.
    ///
    /// If the account is already being tracked and `overwrite` is set to `true`, the account will
    /// be overwritten. Newly created accounts must embed their seed (`account.seed()` must return
    /// `Some(_)`).
    ///
    /// # Errors
    ///
    /// - If the account is new but it does not contain the seed.
    /// - If the account is already tracked and `overwrite` is set to `false`.
    /// - If `overwrite` is set to `true` and the `account_data` nonce is lower than the one already
    ///   being tracked.
    /// - If `overwrite` is set to `true` and the `account_data` commitment doesn't match the
    ///   network's account commitment.
    pub async fn add_account(
        &mut self,
        account: &Account,
        overwrite: bool,
    ) -> Result<(), ClientError> {
        self.add_account_inner(account, ClientAccountType::Native, overwrite).await
    }

    /// Inserts `account` into the store (or overwrites it if `overwrite` is true) and registers
    /// the per-account note tag if `client_account_type` is [`ClientAccountType::Native`].
    ///
    /// Switching the [`ClientAccountType`] of an already-tracked account is not supported and
    /// returns [`ClientError::AccountWatchedMismatch`].
    async fn add_account_inner(
        &mut self,
        account: &Account,
        client_account_type: ClientAccountType,
        overwrite: bool,
    ) -> Result<(), ClientError> {
        if account.is_new() {
            if account.seed().is_none() {
                return Err(ClientError::AddNewAccountWithoutSeed);
            }
        } else {
            // Ignore the seed since it's not a new account
            if account.seed().is_some() {
                tracing::warn!(
                    "Added an existing account and still provided a seed when it is not needed. It's possible that the account's file was incorrectly generated. The seed will be ignored."
                );
            }
        }

        let tracked_account = self.store.get_account(account.id()).await?;

        match tracked_account {
            None => {
                let default_address = Address::new(account.id());

                self.store
                    .insert_account(account, default_address.clone(), client_account_type)
                    .await
                    .map_err(ClientError::StoreError)?;

                if matches!(client_account_type, ClientAccountType::Native) {
                    // Set the default address note tag so sync pulls notes.
                    let default_address_note_tag = default_address.to_note_tag();
                    let note_tag_record =
                        NoteTagRecord::with_account_source(default_address_note_tag, account.id());
                    self.store.add_note_tag(note_tag_record).await?;
                }

                Ok(())
            },
            Some(tracked_account) => {
                if !overwrite {
                    // Only overwrite the account if the flag is set to `true`
                    return Err(ClientError::AccountAlreadyTracked(account.id()));
                }

                if client_account_type != tracked_account.client_account_type() {
                    // Switching between Watched and Native after the account is tracked is not
                    // supported: the per-account note tag and any client-side state derived from
                    // that mode are set up at insertion time and not migrated on the fly.
                    return Err(ClientError::AccountWatchedMismatch(account.id()));
                }

                if tracked_account.nonce().as_canonical_u64() > account.nonce().as_canonical_u64() {
                    // If the new account is older than the one being tracked, return an error
                    return Err(ClientError::AccountNonceTooLow);
                }

                if tracked_account.is_locked() {
                    // If the tracked account is locked, check that the account commitment matches
                    // the one in the network
                    let network_account_commitment = self
                        .rpc_api
                        .get_account(account.id(), GetAccountRequest::new())
                        .await?
                        .1
                        .account_commitment();
                    if network_account_commitment != account.to_commitment() {
                        return Err(ClientError::AccountCommitmentMismatch(
                            network_account_commitment,
                        ));
                    }
                }

                self.store.update_account(account).await?;

                Ok(())
            },
        }
    }

    /// Imports an account from the network to the client's store. The account needs to be public
    /// and be tracked by the network, it will be fetched by its ID. If the account was already
    /// being tracked by the client, its state will be overwritten.
    ///
    /// To import an account as watched (state-tracking only, no note sync), use
    /// [`Self::import_watched_account_by_id`] instead. Switching an already-tracked account
    /// between Native and Watched is not supported.
    ///
    /// # Errors
    /// - If the account is not found on the network.
    /// - If the account is private.
    /// - If the account is already tracked as watched.
    /// - There was an error sending the request to the network.
    pub async fn import_account_by_id(&mut self, account_id: AccountId) -> Result<(), ClientError> {
        let account = self.fetch_public_account(account_id).await?;
        self.add_account_inner(&account, ClientAccountType::Native, true).await
    }

    /// Starts watching an on-chain account ([`ClientAccountType::Watched`]).
    ///
    /// Like [`Self::import_account_by_id`], the account is fetched from the network by its ID.
    /// Unlike `import_account_by_id`, the account is added without registering its derived note
    /// tag: `sync_state` will keep the account's commitment, nonce and storage up to date but
    /// will **not** pull notes targeted at it.
    ///
    /// If the account is already being tracked as watched its state is overwritten. Switching an
    /// already-tracked native account to watched is not supported.
    ///
    /// # Errors
    /// - If the account is not found on the network.
    /// - If the account is private.
    /// - If the account is already tracked as native.
    /// - There was an error sending the request to the network.
    pub async fn import_watched_account_by_id(
        &mut self,
        account_id: AccountId,
    ) -> Result<(), ClientError> {
        let account = self.fetch_public_account(account_id).await?;
        self.add_account_inner(&account, ClientAccountType::Watched, true).await
    }

    /// Fetches a public [`Account`] from the network, returning a typed error when the account
    /// doesn't exist on chain or is private.
    async fn fetch_public_account(&self, account_id: AccountId) -> Result<Account, ClientError> {
        let fetched_account =
            self.rpc_api.get_account_details(account_id).await.map_err(|err| {
                match err.endpoint_error() {
                    Some(EndpointError::GetAccount(GetAccountError::AccountNotFound)) => {
                        ClientError::AccountNotFoundOnChain(account_id)
                    },
                    _ => ClientError::RpcError(err),
                }
            })?;

        fetched_account.ok_or(ClientError::AccountIsPrivate(account_id))
    }

    /// Fetches a public faucet's display metadata from the network.
    ///
    /// Uses [`get_account`](crate::rpc::NodeRpcClient::get_account) with a minimal request so that
    /// the node does not return vault data. The faucet's token config lives in a single value slot,
    /// which is always present in the returned storage header.
    ///
    /// Returns:
    /// - `Ok(Some(_))` — the account is public and its token config storage slot decoded.
    /// - `Ok(None)`    — the account is private, not on chain, or the storage slot does not parse
    ///   as a token config. Caller should fall back to a raw display.
    /// - `Err(_)`      — transport-level RPC error.
    pub async fn fetch_remote_token_metadata(
        &self,
        faucet_id: AccountId,
    ) -> Result<Option<FaucetMetadata>, ClientError> {
        let proof = match self.rpc_api.get_account(faucet_id, GetAccountRequest::new()).await {
            Ok((_, proof)) => proof,
            Err(err) => match err.endpoint_error() {
                Some(EndpointError::GetAccount(
                    GetAccountError::AccountNotFound | GetAccountError::AccountNotPublic,
                )) => return Ok(None),
                _ => return Err(ClientError::RpcError(err)),
            },
        };

        let Some(storage_header) = proof.storage_header() else {
            return Ok(None);
        };

        let Some(slot_header) =
            storage_header.find_slot_header_by_name(FungibleFaucet::token_config_slot())
        else {
            return Ok(None);
        };

        let [_token_supply, _max_supply, decimals, symbol] = *slot_header.value();
        let Ok(symbol) = TokenSymbol::try_from(symbol) else {
            return Ok(None);
        };
        let Ok(decimals) = u8::try_from(decimals.as_canonical_u64()) else {
            return Ok(None);
        };
        Ok(Some(FaucetMetadata { symbol: symbol.to_string(), decimals }))
    }

    /// Adds an [`Address`] to the associated [`AccountId`], alongside its derived [`NoteTag`]. If
    /// the account is tracked as watched, the note tag is not registered.
    ///
    /// # Errors
    /// - If the account is not found on the network.
    /// - If the address is already being tracked.
    pub async fn add_address(
        &mut self,
        address: Address,
        account_id: AccountId,
    ) -> Result<(), ClientError> {
        let network_id = self.rpc_api.get_network_id().await?;
        let address_bench32 = address.encode(network_id);
        if self.store.get_addresses_by_account_id(account_id).await?.contains(&address) {
            return Err(ClientError::AddressAlreadyTracked(address_bench32));
        }

        let tracked_account = self.store.get_account(account_id).await?;
        match tracked_account {
            None => Err(ClientError::AccountDataNotFound(account_id)),
            Some(tracked_account) => {
                self.store.insert_address(address.clone(), account_id).await?;
                // Watched accounts intentionally have no derived note tag registered to avoid sync
                // state pulling notes for them.
                if !tracked_account.is_watched() {
                    let derived_note_tag: NoteTag = address.to_note_tag();
                    let note_tag_record =
                        NoteTagRecord::with_account_source(derived_note_tag, account_id);
                    self.store.add_note_tag(note_tag_record).await?;
                }
                Ok(())
            },
        }
    }

    /// Removes an [`Address`] from the associated [`AccountId`], alongside its derived [`NoteTag`].
    /// If no address was tracked for the given account, this is a no-op.
    pub async fn remove_address(
        &mut self,
        address: Address,
        account_id: AccountId,
    ) -> Result<(), ClientError> {
        let derived_note_tag = address.to_note_tag();
        let note_tag_record = NoteTagRecord::with_account_source(derived_note_tag, account_id);
        self.store.remove_address(address).await?;
        // Remove the note tag if no other address are associated with it.
        let addresses = self.store.get_addresses_by_account_id(account_id).await?;
        if addresses.iter().all(|address| address.to_note_tag() != derived_note_tag) {
            self.store.remove_note_tag(note_tag_record).await?;
        }
        Ok(())
    }

    // ACCOUNT DATA RETRIEVAL
    // --------------------------------------------------------------------------------------------

    /// Retrieves the asset vault for a specific account.
    ///
    /// To check the balance for a single asset, use [`Client::account_reader`] instead.
    pub async fn get_account_vault(
        &self,
        account_id: AccountId,
    ) -> Result<AssetVault, ClientError> {
        self.store.get_account_vault(account_id).await.map_err(ClientError::StoreError)
    }

    /// Retrieves the whole account storage for a specific account.
    ///
    /// To only load a specific slot, use [`Client::account_reader`] instead.
    pub async fn get_account_storage(
        &self,
        account_id: AccountId,
    ) -> Result<AccountStorage, ClientError> {
        self.store
            .get_account_storage(account_id, AccountStorageFilter::All)
            .await
            .map_err(ClientError::StoreError)
    }

    /// Retrieves the account code for a specific account.
    ///
    /// Returns `None` if the account is not found.
    pub async fn get_account_code(
        &self,
        account_id: AccountId,
    ) -> Result<Option<AccountCode>, ClientError> {
        self.store.get_account_code(account_id).await.map_err(ClientError::StoreError)
    }

    /// Returns a list of [`AccountHeader`] of all accounts stored in the database along with their
    /// statuses.
    ///
    /// Said accounts' state is the state after the last performed sync.
    pub async fn get_account_headers(
        &self,
    ) -> Result<Vec<(AccountHeader, AccountStatus)>, ClientError> {
        self.store.get_account_headers().await.map_err(Into::into)
    }

    /// Retrieves the full [`Account`] object from the store, returning `None` if not found.
    ///
    /// This method loads the complete account state including vault, storage, and code.
    ///
    /// For lazy access that fetches only the data you need, use
    /// [`Client::account_reader`] instead.
    ///
    /// Use [`Client::try_get_account`] if you want to error when the account is not found.
    pub async fn get_account(&self, account_id: AccountId) -> Result<Option<Account>, ClientError> {
        match self.store.get_account(account_id).await? {
            Some(record) => Ok(Some(record.try_into()?)),
            None => Ok(None),
        }
    }

    /// Retrieves the full [`Account`] object from the store, erroring if not found.
    ///
    /// This method loads the complete account state including vault, storage, and code.
    ///
    /// Use [`Client::get_account`] if you want to handle missing accounts gracefully.
    pub async fn try_get_account(&self, account_id: AccountId) -> Result<Account, ClientError> {
        self.get_account(account_id)
            .await?
            .ok_or(ClientError::AccountDataNotFound(account_id))
    }

    /// Creates an [`AccountReader`] for lazy access to account data.
    ///
    /// The `AccountReader` provides lazy access to account state - each method call
    /// fetches fresh data from storage, ensuring you always see the current state.
    ///
    /// For loading the full [`Account`] object, use [`Client::get_account`] instead.
    ///
    /// # Example
    /// ```ignore
    /// let reader = client.account_reader(account_id);
    ///
    /// // Each call fetches fresh data
    /// let nonce = reader.nonce().await?;
    /// let balance = reader.get_balance(faucet_id).await?;
    ///
    /// // Storage access is integrated
    /// let value = reader.get_storage_item("my_slot").await?;
    /// let (map_value, witness) = reader.get_storage_map_witness("balances", key).await?;
    /// ```
    pub fn account_reader(&self, account_id: AccountId) -> AccountReader {
        AccountReader::new(self.store.clone(), account_id)
    }

    /// Prunes historical account states for the specified account up to the given nonce.
    ///
    /// Deletes all historical entries with `replaced_at_nonce <= up_to_nonce` and any
    /// orphaned account code.
    ///
    /// Returns the total number of rows deleted, including historical entries and orphaned
    /// account code.
    pub async fn prune_account_history(
        &self,
        account_id: AccountId,
        up_to_nonce: Felt,
    ) -> Result<usize, ClientError> {
        Ok(self.store.prune_account_history(account_id, up_to_nonce).await?)
    }
}

// UTILITY FUNCTIONS
// ================================================================================================

/// Builds an regular account ID from the provided parameters. The ID may be used along
/// `Client::import_account_by_id` to import a public account from the network (provided that the
/// used seed is known).
///
/// This function currently supports accounts composed of the [`BasicWallet`] component and one of
/// the supported authentication schemes ([`AuthSingleSig`]).
///
/// # Arguments
/// - `init_seed`: Initial seed used to create the account. This is the seed passed to
///   [`AccountBuilder::new`].
/// - `public_key`: Public key of the account used for the authentication component.
/// - `account_visibility`: Public/private visibility of the account.
///
/// # Errors
/// - If the account cannot be built.
pub fn build_wallet_id(
    init_seed: [u8; 32],
    public_key: &PublicKey,
    account_visibility: AccountType,
) -> Result<AccountId, ClientError> {
    let auth_scheme = public_key.auth_scheme();
    let auth_component: AccountComponent =
        AuthSingleSig::new(Approver::new(public_key.to_commitment(), auth_scheme)).into();

    let account = AccountBuilder::new(init_seed)
        .account_type(account_visibility)
        .with_auth_component(auth_component)
        .with_component(BasicWallet)
        .build_with_schema_commitment()?;

    Ok(account.id())
}

#[cfg(test)]
mod schema_commitment_tests {
    use miden_protocol::EMPTY_WORD;
    use miden_protocol::account::auth::AuthSecretKey;
    use miden_standards::account::inspection::AccountSchemaCommitment;

    use super::{
        AccountBuilder,
        AccountBuilderSchemaCommitmentExt,
        AccountType,
        Approver,
        AuthSingleSig,
        BasicWallet,
    };
    use crate::auth::AuthSchemeId;

    #[test]
    fn wallet_build_includes_schema_commitment_metadata_slot() {
        let key = AuthSecretKey::new_falcon512_poseidon2();
        let account = AccountBuilder::new([2u8; 32])
            .account_type(AccountType::Private)
            .with_auth_component(AuthSingleSig::new(Approver::new(
                key.public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            )))
            .with_component(BasicWallet)
            .build_with_schema_commitment()
            .expect("build_with_schema_commitment");

        let commitment = account
            .storage()
            .get_item(AccountSchemaCommitment::schema_commitment_slot())
            .expect("schema commitment slot");
        assert_ne!(commitment, EMPTY_WORD);
    }
}
