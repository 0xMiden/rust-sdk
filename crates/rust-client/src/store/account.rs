// ACCOUNT RECORD
// ================================================================================================
use alloc::vec::Vec;
use core::fmt::Display;

use miden_protocol::account::{Account, AccountId, PartialAccount};
use miden_protocol::{Felt, Word};

use crate::ClientError;
use crate::sync::PublicAccountUpdate;

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

/// Contains account changes to apply to the store.
pub struct AccountUpdates {
    /// Updated public accounts, either as full state replacements or incremental deltas.
    updated_public_accounts: Vec<PublicAccountUpdate>,
    /// Network account commitments that don't match the current tracked state for private accounts.
    mismatched_private_accounts: Vec<(AccountId, Word)>,
}

impl AccountUpdates {
    /// Creates a new instance of `AccountUpdates`.
    pub fn new(
        updated_public_accounts: Vec<PublicAccountUpdate>,
        mismatched_private_accounts: Vec<(AccountId, Word)>,
    ) -> Self {
        Self {
            updated_public_accounts,
            mismatched_private_accounts,
        }
    }

    /// Returns the updated public accounts.
    pub fn updated_public_accounts(&self) -> &[PublicAccountUpdate] {
        &self.updated_public_accounts
    }

    /// Returns the mismatched private accounts.
    pub fn mismatched_private_accounts(&self) -> &[(AccountId, Word)] {
        &self.mismatched_private_accounts
    }
}
