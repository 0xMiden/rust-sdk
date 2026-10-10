//! The `account` module provides types and client APIs for managing accounts within the Miden
//! network.
//!
//! Accounts are foundational entities of the Miden protocol. They store assets and define rules for
//! manipulating them. Once an account is registered with the client, its state will be updated
//! accordingly, and validated against the network state on every sync.
//!
//! # Example
//!
//! To add a new account to the client's store, you might use the [`Client::add_account`] method as
//! follows:
//!
//! ```rust
//! # use miden_client::account::{
//! #     AccountBuilder, AccountBuilderSchemaCommitmentExt, AccountType,
//! #     standards::wallets::BasicWallet,
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

pub use miden_objects::account_file::{AccountFile, AccountFileError};
use miden_protocol::account::auth::PublicKey;
pub use miden_protocol::account::{
    Account,
    AccountBuilder,
    AccountCode,
    AccountCodePatch,
    AccountCodeUpgrade,
    AccountComponent,
    AccountComponentCode,
    AccountDelta,
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
use miden_protocol::block::BlockHeader;
pub use miden_protocol::errors::{AccountIdError, AddressError, NetworkIdError};
use miden_protocol::{Felt, Word};
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

/// Display-only metadata for a faucet account, persisted in the client's settings store.
///
/// Populated lazily by the CLI resolver from the on-chain token config of a public faucet and
/// persisted under a `faucet_metadata:<faucet-id>` key.
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

/// Decodes a fungible faucet token config slot value into display metadata.
///
/// Returns `None` when the value does not describe a fungible faucet config the protocol would
/// accept: the symbol must decode as a [`TokenSymbol`], and the decimals must be within
/// [`FungibleFaucet::MAX_DECIMALS`], which is what [`FungibleFaucet`] enforces when the component
/// is built.
fn faucet_metadata_from_token_config(token_config: [Felt; 4]) -> Option<FaucetMetadata> {
    let [_token_supply, _max_supply, decimals, symbol] = token_config;

    let symbol = TokenSymbol::try_from(symbol).ok()?;
    let decimals = u8::try_from(decimals.as_canonical_u64()).ok()?;
    if decimals > FungibleFaucet::MAX_DECIMALS {
        return None;
    }

    Some(FaucetMetadata { symbol: symbol.to_string(), decimals })
}

mod account_reader;
pub use account_reader::AccountReader;
/// Provides built-in account components and their configuration types.
///
/// Use the [`component`] module to define custom account components.
pub use miden_standards::account as standards;
use miden_standards::account::auth::{Approver, AuthSingleSig, NetworkAccount};
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
use crate::rpc::AccountStateAt;
use crate::rpc::domain::account::GetAccountRequest;
use crate::rpc::node::{EndpointError, GetAccountError};
use crate::store::{AccountStatus, AccountStorageFilter, ClientAccountType};
use crate::sync::validate_account_witness;

/// Provides types to define account components and their storage schemas.
///
/// Use the [`standards`] module for built-in account components.
pub mod component {
    pub const MIDEN_PACKAGE_EXTENSION: &str = "masp";

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
        AccountComponentCode,
        AccountComponentMetadata,
        AccountComponentName,
        AccountProcedureRoot,
        RoleSymbol,
    };
    pub use miden_protocol::errors::{
        AccountComponentNameError,
        ComponentMetadataError,
        RoleSymbolError,
    };
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
/// - **Account registration:** On a network that enforces an account allowlist,
///   [`Client::register_account`] binds an invitation code to a new account before its first
///   transaction creates it on chain, and [`Client::is_account_allowed`] asks whether the network
///   accepts the creation of an account.
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

    // ACCOUNT REGISTRATION
    // --------------------------------------------------------------------------------------------

    /// Binds an invitation code to a tracked account on the network allowlist.
    ///
    /// A network that enforces an account allowlist creates an account on chain only when the
    /// account is registered. The first transaction of an account is what creates it, so the
    /// account must be registered before that transaction is submitted.
    /// [`Client::submit_new_transaction`] and [`BatchBuilder::submit`] ask the node first, and fail
    /// with [`ClientError::AccountNotAllowlisted`] for an account the network does not accept. Only
    /// account creation is gated: an account that already exists on chain is never checked, and
    /// network accounts are exempt.
    ///
    /// The account must be tracked by the client, must not be deployed on chain yet, and must not
    /// be a network account. The invitation code must exist on the node and must not be bound to
    /// another account. A registration consumes the code, so the client asks the node first and
    /// does not send the code for an account the node already allows.
    ///
    /// When the network operator runs a funding service, the node pays the registered account a
    /// public P2ID note with the native asset. The node answers once the funding service queues the
    /// note, before the note is committed. The note is not part of the response, and the client
    /// does not see it until a [`Client::sync_state`] runs after the note is committed. The client
    /// tracks the note tag of every account it owns, so that sync imports the note and
    /// [`Client::get_consumable_notes`] lists it. Sync again until the note arrives. The account
    /// then consumes the note in its first transaction. That transaction creates the account on
    /// chain and pays its fee out of the received funds.
    ///
    /// # Errors
    ///
    /// - [`ClientError::AccountDataNotFound`] if the client does not track the account.
    /// - [`ClientError::AccountIsNotNew`] if the account already exists on chain.
    /// - [`ClientError::AccountIsNetworkAccount`] if the account is a network account. The node
    ///   admits network accounts without a code.
    /// - [`ClientError::AccountAlreadyAllowed`] if the node already allows the account, because it
    ///   is registered or because the network does not enforce an allowlist. The code is not sent.
    /// - [`ClientError::RpcError`] carrying a [`RegisterAccountError`] if the node rejects the
    ///   code or the account, or an `Unavailable` status if the funding failed. In the second
    ///   case the account stays registered, so a retry fails with
    ///   [`ClientError::AccountAlreadyAllowed`] and the account has to be funded another way.
    ///
    /// [`BatchBuilder::submit`]: crate::transaction::BatchBuilder::submit
    /// [`RegisterAccountError`]: crate::rpc::RegisterAccountError
    pub async fn register_account(
        &self,
        account_id: AccountId,
        invitation_code: &str,
    ) -> Result<(), ClientError> {
        let (_, status) = self
            .store
            .get_account_header(account_id)
            .await?
            .ok_or(ClientError::AccountDataNotFound(account_id))?;
        if !status.is_new() {
            return Err(ClientError::AccountIsNotNew(account_id));
        }

        let account = self
            .get_account(account_id)
            .await?
            .ok_or(ClientError::AccountDataNotFound(account_id))?;
        // The node admits a network account without a code.
        if NetworkAccount::new(account).is_ok() {
            return Err(ClientError::AccountIsNetworkAccount(account_id));
        }
        // A registration consumes the code, so do not send it when the node already allows the
        // account.
        if self.is_account_allowed(account_id).await? {
            return Err(ClientError::AccountAlreadyAllowed(account_id));
        }

        self.rpc_api.register_account(invitation_code, account_id).await?;

        Ok(())
    }

    /// Returns whether the network lets `account_id` be created on chain.
    ///
    /// The node answers `true` when it does not enforce an account allowlist, or when the account
    /// is registered. See [`Client::register_account`] for how an account gets registered.
    pub async fn is_account_allowed(&self, account_id: AccountId) -> Result<bool, ClientError> {
        Ok(self.rpc_api.is_account_allowed(account_id).await?)
    }

    /// Returns whether a transaction against `account_id` creates an account that the network
    /// allowlist gates.
    ///
    /// Only a new account is gated, and a network account is exempt. The answer is `false` for an
    /// account that the client does not track.
    pub(crate) async fn is_allowlist_gated(
        &self,
        account_id: AccountId,
    ) -> Result<bool, ClientError> {
        let Some((_, status)) = self.store.get_account_header(account_id).await? else {
            return Ok(false);
        };
        if !status.is_new() {
            return Ok(false);
        }

        let Some(account) = self.get_account(account_id).await? else {
            return Ok(false);
        };

        Ok(NetworkAccount::new(account).is_err())
    }

    /// Inserts `account` into the store (or overwrites it if `overwrite` is true).
    ///
    /// If `client_account_type` is [`ClientAccountType::Native`], the client tracks the note tag of
    /// the default address of the account.
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

        let tracked_account = self.store.get_minimal_partial_account(account.id()).await?;

        match tracked_account {
            None => {
                let default_address = Address::new(account.id());

                self.store
                    .insert_account(account, default_address, client_account_type)
                    .await
                    .map_err(ClientError::StoreError)?;

                Ok(())
            },
            Some(tracked_account) => {
                if !overwrite {
                    // Only overwrite the account if the flag is set to `true`
                    return Err(ClientError::AccountAlreadyTracked(account.id()));
                }

                if client_account_type != tracked_account.client_account_type() {
                    // Switching between Watched and Native after the account is tracked is not
                    // supported. The store keeps the mode that the account has at insertion and
                    // does not change it on update.
                    return Err(ClientError::AccountWatchedMismatch(account.id()));
                }

                if tracked_account.nonce().as_canonical_u64() > account.nonce().as_canonical_u64() {
                    // If the new account is older than the one being tracked, return an error
                    return Err(ClientError::AccountNonceTooLow);
                }

                if tracked_account.is_locked() {
                    // If the tracked account is locked, check that the account commitment matches
                    // the one in the network
                    let network_account_commitment =
                        self.fetch_verified_account_commitment(account.id()).await?;
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
    /// [`Self::import_watched_account_by_id`] instead. Switching an already-tracked account between
    /// Native and Watched is not supported.
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
    /// Unlike `import_account_by_id`, the client does not track the note tags of the addresses of
    /// the account. `sync_state` will keep the account's commitment, nonce and storage up to date
    /// but will **not** pull notes targeted at it.
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

    // ACCOUNT WITNESS PREFETCHING
    // --------------------------------------------------------------------------------------------

    /// Registers an account whose account witness [`Client::sync_chain`] keeps up to date, so that
    /// transactions using it as a foreign account resolve the witness locally. This trades one
    /// request per transaction for one per sync.
    ///
    /// A [`ForeignAccount::Private`](crate::transaction::ForeignAccount) needs nothing else, since
    /// the caller supplies the account data. A
    /// [`ForeignAccount::Public`](crate::transaction::ForeignAccount) additionally has to be
    /// tracked by this client, so that its code, storage and vault come from the store as well;
    /// registering an untracked public account costs a request per sync and saves none.
    ///
    /// The witness is fetched by the next sync, not by this call. Transactions assume that a sync
    /// ran after the account was registered.
    ///
    /// The account is not validated against the network here. The sync fails while a registered
    /// account has no witness that the node can return, so an account that is not in the account
    /// tree blocks the sync until it is unregistered.
    ///
    /// Registering an already registered account is a no-op and keeps any cached witness.
    ///
    /// Returns `true` if the account was not registered before this call.
    pub async fn track_account_witness(&self, account_id: AccountId) -> Result<bool, ClientError> {
        self.store.track_account_witness(account_id).await.map_err(Into::into)
    }

    /// Stops keeping the account's witness up to date and drops the cached one.
    ///
    /// Returns `true` if the account was registered. Transactions using it keep working, falling
    /// back to fetching the witness from the node.
    pub async fn untrack_account_witness(
        &self,
        account_id: AccountId,
    ) -> Result<bool, ClientError> {
        self.store.untrack_account_witness(account_id).await.map_err(Into::into)
    }

    /// Returns the IDs of every account registered via [`Client::track_account_witness`].
    pub async fn tracked_account_witnesses(&self) -> Result<Vec<AccountId>, ClientError> {
        self.store.tracked_account_witnesses().await.map_err(Into::into)
    }

    /// Returns the header of the client's sync height, which sync has already authenticated, and
    /// the [`AccountStateAt`] that reads the network at that block.
    ///
    /// Account data fetched from the node is only trusted when its witness opens under this
    /// header's account root.
    async fn sync_height_anchor(&self) -> Result<(BlockHeader, AccountStateAt), ClientError> {
        let sync_height = self.store.get_sync_height().await?;
        let (header, _) = self.store.get_block_header_by_num(sync_height).await?.ok_or_else(|| {
            ClientError::ChainValidationError(format!(
                "block header {sync_height} is not tracked locally; sync the client before it can \
                 verify account data against the chain"
            ))
        })?;

        Ok((header, AccountStateAt::Block(sync_height)))
    }

    /// Fetches the commitment of `account_id` at the sync height, after checking that its witness
    /// opens under the sync height header's account root.
    async fn fetch_verified_account_commitment(
        &self,
        account_id: AccountId,
    ) -> Result<Word, ClientError> {
        let (header, at) = self.sync_height_anchor().await?;
        let (_, proof) =
            self.rpc_api.get_account(account_id, GetAccountRequest::new().at(at)).await?;
        validate_account_witness(proof.account_witness(), account_id, &header)?;

        Ok(proof.account_commitment())
    }

    /// Fetches a public [`Account`] from the network at the sync height, returning a typed error
    /// when the account doesn't exist on chain or is private. The account's witness must open
    /// under the sync height header's account root, and the account must commit to the witness.
    async fn fetch_public_account(&self, account_id: AccountId) -> Result<Account, ClientError> {
        let (header, at) = self.sync_height_anchor().await?;
        let (_, witness, account) = self
            .rpc_api
            .get_account_details_at(account_id, at)
            .await
            .map_err(|err| match err.endpoint_error() {
                Some(EndpointError::GetAccount(GetAccountError::AccountNotFound)) => {
                    ClientError::AccountNotFoundOnChain(account_id)
                },
                _ => ClientError::RpcError(err),
            })?
            .ok_or(ClientError::AccountIsPrivate(account_id))?;

        validate_account_witness(&witness, account_id, &header)?;
        if account.to_commitment() != witness.state_commitment() {
            return Err(ClientError::ChainValidationError(format!(
                "account {account_id} returned by the node does not commit to its witness"
            )));
        }

        Ok(account)
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
    /// - `Err(_)`      — transport-level RPC error, or the node's response does not open under the
    ///   sync height header's account root.
    pub async fn fetch_remote_token_metadata(
        &self,
        faucet_id: AccountId,
    ) -> Result<Option<FaucetMetadata>, ClientError> {
        let (header, at) = self.sync_height_anchor().await?;
        let proof = match self.rpc_api.get_account(faucet_id, GetAccountRequest::new().at(at)).await
        {
            Ok((_, proof)) => proof,
            Err(err) => match err.endpoint_error() {
                Some(EndpointError::GetAccount(
                    GetAccountError::AccountNotFound | GetAccountError::AccountNotPublic,
                )) => return Ok(None),
                _ => return Err(ClientError::RpcError(err)),
            },
        };

        validate_account_witness(proof.account_witness(), faucet_id, &header)?;

        let Some(storage_header) = proof.storage_header() else {
            return Ok(None);
        };

        let Some(slot_header) =
            storage_header.find_slot_header_by_name(FungibleFaucet::token_config_slot())
        else {
            return Ok(None);
        };

        Ok(faucet_metadata_from_token_config(*slot_header.value()))
    }

    /// Adds an [`Address`] to the associated [`AccountId`].
    ///
    /// If the account is native, the client tracks the [`NoteTag`](crate::note::NoteTag) of the
    /// address. If the account is watched, the client does not track the tag.
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

        if self.store.get_minimal_partial_account(account_id).await?.is_none() {
            return Err(ClientError::AccountDataNotFound(account_id));
        }

        self.store.insert_address(address, account_id).await?;
        Ok(())
    }

    /// Removes an [`Address`] from the account it belongs to.
    ///
    /// If the account is native and no other address of the account has the same
    /// [`NoteTag`](crate::note::NoteTag), the client no longer tracks the tag.
    ///
    /// Returns `true` if the address was tracked. If it was not tracked, this is a no-op.
    pub async fn remove_address(&mut self, address: Address) -> Result<bool, ClientError> {
        Ok(self.store.remove_address(address).await?)
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

    /// Returns the [`AccountHeader`] of the account with the specified ID along with its status, or
    /// `None` if the account isn't tracked by the client.
    ///
    /// Said account's state is the state after the last performed sync.
    pub async fn get_account_header(
        &self,
        account_id: AccountId,
    ) -> Result<Option<(AccountHeader, AccountStatus)>, ClientError> {
        self.store.get_account_header(account_id).await.map_err(Into::into)
    }

    /// Retrieves the full [`Account`] object from the store, returning `None` if not found.
    ///
    /// This method loads the complete account state including vault, storage, and code — including
    /// building the vault's Merkle tree. For lazy access that fetches only the data you need
    /// (existence checks, single fields, storage items), use [`Client::account_reader`] instead.
    pub async fn get_account(&self, account_id: AccountId) -> Result<Option<Account>, ClientError> {
        match self.store.get_account(account_id).await? {
            Some(record) => Ok(Some(record.try_into()?)),
            None => Ok(None),
        }
    }

    /// Creates an [`AccountReader`] for lazy access to account data.
    ///
    /// The `AccountReader` provides lazy access to account state - each method call fetches fresh
    /// data from storage, ensuring you always see the current state.
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
    /// Deletes all historical entries with `replaced_at_nonce <= up_to_nonce` and any orphaned
    /// account code.
    ///
    /// Returns the total number of rows deleted, including historical entries and orphaned account
    /// code.
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
        .with_component(auth_component)
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
            .with_component(AuthSingleSig::new(Approver::new(
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

#[cfg(test)]
mod faucet_metadata_tests {
    use miden_protocol::Felt;

    use super::{FungibleFaucet, TokenSymbol, faucet_metadata_from_token_config};

    /// Builds a token config slot value carrying the given decimals and the symbol "TST".
    fn token_config(decimals: u32) -> [Felt; 4] {
        [
            Felt::from(0u32),
            Felt::from(0u32),
            Felt::from(decimals),
            TokenSymbol::new("TST").unwrap().as_element(),
        ]
    }

    #[test]
    fn decodes_a_config_within_the_protocol_bounds() {
        let metadata = faucet_metadata_from_token_config(token_config(8)).unwrap();

        assert_eq!(metadata.symbol, "TST");
        assert_eq!(metadata.decimals, 8);
    }

    #[test]
    fn accepts_the_maximum_supported_decimals() {
        let max = u32::from(FungibleFaucet::MAX_DECIMALS);
        let metadata = faucet_metadata_from_token_config(token_config(max)).unwrap();

        assert_eq!(metadata.decimals, FungibleFaucet::MAX_DECIMALS);
    }

    #[test]
    fn rejects_decimals_above_the_maximum() {
        let above_max = u32::from(FungibleFaucet::MAX_DECIMALS) + 1;

        assert!(faucet_metadata_from_token_config(token_config(above_max)).is_none());
        assert!(faucet_metadata_from_token_config(token_config(200)).is_none());
    }

    #[test]
    fn rejects_decimals_that_do_not_fit_a_u8() {
        assert!(faucet_metadata_from_token_config(token_config(300)).is_none());
    }

    #[test]
    fn rejects_a_symbol_that_is_not_a_token_symbol() {
        let mut config = token_config(8);
        config[3] = Felt::from(0u32);

        assert!(faucet_metadata_from_token_config(config).is_none());
    }
}
