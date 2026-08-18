//! Contains structures and functions related to FPI (Foreign Procedure Invocation) transactions.
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cmp::Ordering;

use miden_protocol::Word;
use miden_protocol::account::{
    AccountId,
    PartialAccount,
    PartialStorage,
    PartialStorageMap,
    StorageMap,
    StorageMapKey,
    StorageMapWitness,
};
use miden_protocol::asset::{AssetVault, PartialVault};
use miden_protocol::crypto::merkle::smt::SmtProof;
use miden_protocol::transaction::AccountInputs;
use miden_tx::utils::serde::{Deserializable, DeserializationError, Serializable};

use super::TransactionRequestError;
use crate::rpc::domain::account::{
    AccountDetails,
    AccountProof,
    AccountStorageRequirements,
    AccountVaultDetails,
    StorageMapEntries,
};

// FOREIGN ACCOUNT
// ================================================================================================

/// Account types for foreign procedure invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum ForeignAccount {
    /// Account with public visibility whose state and
    /// code will be retrieved from the network at execution time. Declaring it upfront lets you
    /// specify [`AccountStorageRequirements`] so the correct storage map entries are fetched in a
    /// single RPC call. If not declared, the account is lazily loaded with empty storage
    /// requirements, and any storage map accesses will trigger additional RPC calls during
    /// execution.
    Public(AccountId, AccountStorageRequirements),
    /// Private account that requires a [`PartialAccount`] to be provided by the caller. An
    /// account witness will be retrieved from the network at execution time so that it can be
    /// used as inputs to the transaction kernel.
    Private(PartialAccount),
}

impl ForeignAccount {
    /// Creates a new [`ForeignAccount::Public`]. The account's components (code, storage header and
    /// inclusion proof) will be retrieved at execution time, alongside particular storage slot
    /// maps correspondent to keys passed in `indices`.
    pub fn public(
        account_id: AccountId,
        storage_requirements: AccountStorageRequirements,
    ) -> Result<Self, TransactionRequestError> {
        if !account_id.is_public() {
            return Err(TransactionRequestError::InvalidForeignAccountId(account_id));
        }

        Ok(Self::Public(account_id, storage_requirements))
    }

    /// Creates a new [`ForeignAccount::Private`]. A proof of the account's inclusion will be
    /// retrieved at execution time.
    pub fn private(account: impl Into<PartialAccount>) -> Result<Self, TransactionRequestError> {
        let partial_account: PartialAccount = account.into();
        if partial_account.id().is_public() {
            return Err(TransactionRequestError::InvalidForeignAccountId(partial_account.id()));
        }

        Ok(Self::Private(partial_account))
    }

    pub fn storage_slot_requirements(&self) -> AccountStorageRequirements {
        match self {
            ForeignAccount::Public(_, account_storage_requirements) => {
                account_storage_requirements.clone()
            },
            ForeignAccount::Private(_) => AccountStorageRequirements::default(),
        }
    }

    /// Returns the foreign account's [`AccountId`].
    pub fn account_id(&self) -> AccountId {
        match self {
            ForeignAccount::Public(account_id, _) => *account_id,
            ForeignAccount::Private(partial_account) => partial_account.id(),
        }
    }
}

impl Ord for ForeignAccount {
    fn cmp(&self, other: &Self) -> Ordering {
        self.account_id().cmp(&other.account_id())
    }
}

impl PartialOrd for ForeignAccount {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Serializable for ForeignAccount {
    fn write_into<W: miden_tx::utils::serde::ByteWriter>(&self, target: &mut W) {
        match self {
            ForeignAccount::Public(account_id, storage_requirements) => {
                target.write(0u8);
                account_id.write_into(target);
                storage_requirements.write_into(target);
            },
            ForeignAccount::Private(partial_account) => {
                target.write(1u8);
                partial_account.write_into(target);
            },
        }
    }
}

impl Deserializable for ForeignAccount {
    fn read_from<R: miden_tx::utils::serde::ByteReader>(
        source: &mut R,
    ) -> Result<Self, miden_tx::utils::serde::DeserializationError> {
        let account_type: u8 = source.read_u8()?;
        match account_type {
            0 => {
                let account_id = AccountId::read_from(source)?;
                let storage_requirements = AccountStorageRequirements::read_from(source)?;
                Ok(ForeignAccount::Public(account_id, storage_requirements))
            },
            1 => {
                let foreign_inputs = PartialAccount::read_from(source)?;
                Ok(ForeignAccount::Private(foreign_inputs))
            },
            _ => Err(DeserializationError::InvalidValue("Invalid account type".to_string())),
        }
    }
}

/// Converts an [`AccountProof`] to [`AccountInputs`].
///
/// The `storage_requirements` are needed to reassociate raw keys with the SMT proofs returned
/// by the node (the node only sends hashed leaf keys, not the original raw keys).
pub(crate) fn account_proof_into_inputs(
    account_proof: AccountProof,
    storage_requirements: &AccountStorageRequirements,
    known_vault: Option<AssetVault>,
) -> Result<AccountInputs, TransactionRequestError> {
    let (witness, account_details) = account_proof.into_parts();

    if let Some(AccountDetails {
        header: account_header,
        code,
        storage_details,
        vault_details,
    }) = account_details
    {
        // discard slot indices - not needed for execution
        let account_storage_map_details = storage_details.map_details;
        let mut storage_map_proofs = Vec::with_capacity(account_storage_map_details.len());
        for account_storage_detail in account_storage_map_details {
            let partial_storage = match account_storage_detail.entries {
                StorageMapEntries::AllEntries(entries) => {
                    // Full map available - create from all entries
                    let storage_entries_iter = entries.iter().map(|e| (e.key, e.value));
                    PartialStorageMap::new_full(
                        StorageMap::with_entries(storage_entries_iter)
                            .map_err(TransactionRequestError::StorageMapError)?,
                    )
                },
                StorageMapEntries::EntriesWithProofs(proofs) => {
                    // Reassociate the proofs with the keys from storage requirements.
                    let keys =
                        storage_requirements.keys_for_slot(&account_storage_detail.slot_name);
                    let witnesses = proofs_to_witnesses(proofs, keys)?;
                    PartialStorageMap::with_witnesses(witnesses)?
                },
            };
            storage_map_proofs.push(partial_storage);
        }

        let vault =
            foreign_account_vault(&vault_details, account_header.vault_root(), known_vault)?;
        return Ok(AccountInputs::new(
            PartialAccount::new(
                account_header.id(),
                account_header.nonce(),
                code,
                PartialStorage::new(storage_details.header, storage_map_proofs)?,
                PartialVault::new_full(vault),
                None,
            )?,
            witness,
        ));
    }
    Err(TransactionRequestError::ForeignAccountDataMissing)
}

/// Builds a foreign account's vault, tolerating the node's "vault unchanged" optimization.
///
/// When the client requests a vault with [`VaultFetch::IfChangedFrom`], the node OMITS the asset
/// list if the account's vault root already equals the root the client sent. The response then
/// carries an empty asset list, which is byte-identical to a genuinely empty vault — so rebuilding
/// from it unconditionally produces an empty vault and a wrong account commitment. The kernel
/// rejects that with `ERR_FOREIGN_ACCOUNT_INVALID_COMMITMENT`, and it does so deterministically:
/// the better-synced the client, the more reliably the node omits, so every subsequent FPI against
/// that account fails. While the foreign vault is empty the bad reconstruction is accidentally
/// correct, which is why this stays hidden until the first asset lands in the foreign vault.
///
/// The omission happens *only* when the foreign vault matches the one the client already has, so on
/// that path the locally-known vault is not a workaround — it is exactly the right vault. Checking
/// both candidates against the header's vault root keeps the bandwidth optimization while making it
/// impossible to hand the kernel a vault that does not hash to the committed root.
fn foreign_account_vault(
    vault_details: &AccountVaultDetails,
    expected_root: Word,
    known_vault: Option<AssetVault>,
) -> Result<AssetVault, TransactionRequestError> {
    let from_response = AssetVault::new(&vault_details.assets)?;
    if from_response.root() == expected_root {
        return Ok(from_response);
    }

    if let Some(known_vault) = known_vault
        && known_vault.root() == expected_root
    {
        return Ok(known_vault);
    }

    Err(TransactionRequestError::ForeignAccountVaultMismatch {
        expected: expected_root,
        actual: from_response.root(),
    })
}

/// Pairs each [`SmtProof`] with its corresponding key to produce [`StorageMapWitness`]es.
///
/// Proofs and keys are matched by position (the node returns proofs in the same order as
/// the requested keys). [`StorageMapWitness::new`] validates each pair by hashing the key
/// and checking that the proof's leaf covers it, so a mismatch will surface as a
/// `StorageMapError::MissingKey` error.
fn proofs_to_witnesses(
    proofs: Vec<SmtProof>,
    keys: &[StorageMapKey],
) -> Result<Vec<StorageMapWitness>, TransactionRequestError> {
    proofs
        .into_iter()
        .zip(keys)
        .map(|(proof, key)| {
            StorageMapWitness::new(proof, [*key]).map_err(TransactionRequestError::StorageMapError)
        })
        .collect()
}

#[cfg(test)]
mod foreign_vault_tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use miden_protocol::account::AccountId;
    use miden_protocol::asset::{Asset, AssetVault, FungibleAsset};
    use miden_protocol::testing::account_id::ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET;

    use super::{AccountVaultDetails, TransactionRequestError, foreign_account_vault};

    fn asset(amount: u64) -> Asset {
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        Asset::Fungible(FungibleAsset::new(faucet_id, amount).unwrap())
    }

    fn details(assets: Vec<Asset>) -> AccountVaultDetails {
        AccountVaultDetails { too_many_assets: false, assets }
    }

    #[test]
    fn uses_the_response_assets_when_the_node_sent_them() {
        let vault = AssetVault::new(&[asset(100)]).unwrap();

        let built = foreign_account_vault(&details(vec![asset(100)]), vault.root(), None).unwrap();

        assert_eq!(built.root(), vault.root());
    }

    /// The regression. The node omits the asset list when the caller's vault root already matches,
    /// so the response carries NO assets while the header still commits to a non-empty vault.
    /// Rebuilding from the response alone yields an empty vault and a wrong commitment, which the
    /// kernel rejects with `ERR_FOREIGN_ACCOUNT_INVALID_COMMITMENT` — deterministically, and only
    /// once the foreign vault stops being empty.
    #[test]
    fn falls_back_to_the_local_vault_when_the_node_omitted_the_assets() {
        let local = AssetVault::new(&[asset(100)]).unwrap();
        let committed_root = local.root();

        // Response with an omitted (hence empty) asset list.
        let built = foreign_account_vault(&details(vec![]), committed_root, Some(local)).unwrap();

        assert_eq!(
            built.root(),
            committed_root,
            "an omitted asset list must not be reconstructed as an empty vault"
        );
        assert_ne!(
            built.root(),
            AssetVault::new(&[]).unwrap().root(),
            "the pre-fix behaviour built an empty vault here"
        );
    }

    #[test]
    fn accepts_a_genuinely_empty_foreign_vault() {
        let empty_root = AssetVault::new(&[]).unwrap().root();

        // No local vault to fall back on, and none needed: the header commits to an empty vault.
        let built = foreign_account_vault(&details(vec![]), empty_root, None).unwrap();

        assert_eq!(built.root(), empty_root);
    }

    /// A stale local copy must be rejected rather than trusted — handing the kernel a vault that
    /// does not hash to the committed root is what this whole path exists to prevent.
    #[test]
    fn rejects_when_neither_candidate_matches_the_committed_root() {
        let stale_local = AssetVault::new(&[asset(100)]).unwrap();
        let committed_root = AssetVault::new(&[asset(250)]).unwrap().root();

        let err = foreign_account_vault(&details(vec![]), committed_root, Some(stale_local))
            .expect_err("a vault that does not match the committed root must not be used");

        assert!(matches!(err, TransactionRequestError::ForeignAccountVaultMismatch { .. }));
    }
}
