// ACCOUNT RECORD
// ================================================================================================
use alloc::vec::Vec;
use core::fmt::Display;

use miden_protocol::account::{
    Account,
    AccountHeader,
    AccountId,
    AccountStorage,
    AccountStoragePatch,
    AccountVaultPatch,
    PartialAccount,
};
use miden_protocol::asset::Asset;
use miden_protocol::{Felt, Word};

use crate::ClientError;

// ACCOUNT RECORD DATA
// ================================================================================================

/// Represents types of records retrieved from the store
#[derive(Debug)]
pub enum AccountRecordData {
    Full(Account),
    Partial(PartialAccount),
}

impl AccountRecordData {
    pub fn nonce(&self) -> Felt {
        match self {
            AccountRecordData::Full(account) => account.nonce(),
            AccountRecordData::Partial(partial_account) => partial_account.nonce(),
        }
    }
}

// CLIENT ACCOUNT TYPE
// ================================================================================================

/// How the client tracks a given account.
///
/// This drives two pieces of behavior:
///
/// - **Note sync:** native accounts have their derived note tag registered so `sync_state` pulls
///   notes targeted at them. Watched accounts do not.
/// - **Transaction execution:** native accounts can be used as the source of a transaction; watched
///   accounts cannot, because the client doesn't hold the keys / authority for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAccountType {
    /// Account is fully owned by this client: notes are synced and transactions can be executed.
    Native,
    /// Account state is mirrored from the network for observability only: no note sync, no
    /// transaction execution.
    Watched,
}

// ACCOUNT RECORD
// ================================================================================================

/// Represents a stored account state along with its status.
///
/// The account should be stored in the database with its parts normalized. Meaning that the account
/// header, vault, storage and code are stored separately. This is done to avoid data duplication as
/// the header can reference the same elements if they have equal roots.
#[derive(Debug)]
pub struct AccountRecord {
    /// Full account object.
    account_data: AccountRecordData,
    /// Status of the tracked account.
    status: AccountStatus,
    /// How the client tracks this account.
    client_account_type: ClientAccountType,
}

impl AccountRecord {
    pub fn new(
        account_data: AccountRecordData,
        status: AccountStatus,
        client_account_type: ClientAccountType,
    ) -> Self {
        // TODO: remove this?
        #[cfg(debug_assertions)]
        {
            let account_seed = match &account_data {
                AccountRecordData::Full(acc) => acc.seed(),
                AccountRecordData::Partial(acc) => acc.seed(),
            };
            debug_assert_eq!(account_seed, status.seed().copied(), "account seed mismatch");
        }

        Self {
            account_data,
            status,
            client_account_type,
        }
    }

    pub fn is_locked(&self) -> bool {
        self.status.is_locked()
    }

    pub fn client_account_type(&self) -> ClientAccountType {
        self.client_account_type
    }

    pub fn is_watched(&self) -> bool {
        self.client_account_type == ClientAccountType::Watched
    }

    pub fn nonce(&self) -> Felt {
        self.account_data.nonce()
    }
}

impl TryFrom<AccountRecord> for Account {
    type Error = ClientError;

    fn try_from(value: AccountRecord) -> Result<Self, Self::Error> {
        match value.account_data {
            AccountRecordData::Full(acc) => Ok(acc),
            AccountRecordData::Partial(acc) => Err(ClientError::AccountRecordNotFull(acc.id())),
        }
    }
}

impl TryFrom<AccountRecord> for PartialAccount {
    type Error = ClientError;

    fn try_from(value: AccountRecord) -> Result<Self, Self::Error> {
        match value.account_data {
            AccountRecordData::Partial(acc) => Ok(acc),
            AccountRecordData::Full(acc) => Err(ClientError::AccountRecordNotPartial(acc.id())),
        }
    }
}

// ACCOUNT STATUS
// ================================================================================================

/// Represents the status of an account tracked by the client.
///
/// The status of an account may change by local or external factors.
#[derive(Debug, Clone)]
pub enum AccountStatus {
    /// The account is new and hasn't been used yet. The seed used to create the account is stored
    /// in this state.
    New { seed: Word },
    /// The account is tracked by the node and was used at least once.
    Tracked,
    /// The local account state doesn't match the node's state, rendering it unusable. Only used for
    /// private accounts. The seed is preserved for private accounts with nonce=0 that need
    /// reconstruction via `Account::new()`.
    Locked { seed: Option<Word> },
}

impl AccountStatus {
    pub fn is_new(&self) -> bool {
        matches!(self, AccountStatus::New { .. })
    }

    pub fn is_locked(&self) -> bool {
        matches!(self, AccountStatus::Locked { .. })
    }

    pub fn seed(&self) -> Option<&Word> {
        match self {
            AccountStatus::New { seed } => Some(seed),
            AccountStatus::Locked { seed } => seed.as_ref(),
            AccountStatus::Tracked => None,
        }
    }
}

impl Display for AccountStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            AccountStatus::New { .. } => write!(f, "New"),
            AccountStatus::Tracked => write!(f, "Tracked"),
            AccountStatus::Locked { .. } => write!(f, "Locked"),
        }
    }
}

// ACCOUNT UPDATES
// ================================================================================================

/// Update to a single account's state: the new header plus how the storage and the vault reach it.
#[derive(Debug, Clone)]
pub struct AccountStateUpdate {
    /// The new account header after applying the update.
    new_header: AccountHeader,
    /// The storage update to apply.
    storage: StorageUpdate,
    /// The vault update to apply.
    vault: VaultUpdate,
}

impl AccountStateUpdate {
    /// Creates a new update that advances the account to `new_header`.
    pub fn new(new_header: AccountHeader, storage: StorageUpdate, vault: VaultUpdate) -> Self {
        Self { new_header, storage, vault }
    }

    /// Returns the account ID for this update.
    pub fn id(&self) -> AccountId {
        self.new_header.id()
    }

    /// Returns the account nonce that this update advances the local state to.
    pub fn nonce(&self) -> Felt {
        self.new_header.nonce()
    }

    /// Returns the new account header after applying the update.
    pub fn new_header(&self) -> &AccountHeader {
        &self.new_header
    }

    /// Returns the storage update.
    pub fn storage(&self) -> &StorageUpdate {
        &self.storage
    }

    /// Returns the vault update.
    pub fn vault(&self) -> &VaultUpdate {
        &self.vault
    }
}

/// Storage part of a [`AccountStateUpdate`].
#[derive(Debug, Clone)]
pub enum StorageUpdate {
    /// The complete storage. The store replaces every local slot with it.
    Full(AccountStorage),
    /// The absolute changes to the storage, layered onto the local one. Maps the node returned in
    /// full are `Create` patches, which replace the slot.
    Patch(AccountStoragePatch),
}

/// Vault part of a [`AccountStateUpdate`].
#[derive(Debug, Clone)]
pub enum VaultUpdate {
    /// The complete vault contents. The store replaces the local vault with them.
    Full(Vec<Asset>),
    /// The absolute changes to the vault, layered onto the local one.
    Patch(AccountVaultPatch),
}

/// Contains account changes to apply to the store after a sync request.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_field_names)]
pub struct AccountUpdates {
    /// Updated public accounts.
    updated_public_accounts: Vec<AccountStateUpdate>,
    /// Account commitments received from the network that don't match the currently locally-tracked
    /// state of the private accounts.
    ///
    /// These updates may represent a stale account commitment (meaning that the latest local state
    /// hasn't been committed). If this is not the case, the account may be locked until the state
    /// is restored manually.
    mismatched_private_accounts: Vec<(AccountId, Word)>,
}

impl AccountUpdates {
    /// Creates a new instance of `AccountUpdates`.
    pub fn new(
        updated_public_accounts: Vec<AccountStateUpdate>,
        mismatched_private_accounts: Vec<(AccountId, Word)>,
    ) -> Self {
        Self {
            updated_public_accounts,
            mismatched_private_accounts,
        }
    }

    /// Returns the updated public accounts.
    pub fn updated_public_accounts(&self) -> &[AccountStateUpdate] {
        &self.updated_public_accounts
    }

    /// Returns the mismatched private accounts.
    pub fn mismatched_private_accounts(&self) -> &[(AccountId, Word)] {
        &self.mismatched_private_accounts
    }

    /// Appends the public account updates and the private account mismatches of `other`.
    pub fn extend(&mut self, other: AccountUpdates) {
        self.updated_public_accounts.extend(other.updated_public_accounts);
        self.mismatched_private_accounts.extend(other.mismatched_private_accounts);
    }
}
