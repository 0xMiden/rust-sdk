//! Contains structures and functions related to FPI (Foreign Procedure Invocation) transactions.
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cmp::Ordering;

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

        // Keep the asset list only if it hashes to the header's vault root; otherwise carry the
        // root alone and let asset reads resolve lazily as per-asset witnesses.
        let vault = AssetVault::new(&vault_details.assets)
            .ok()
            .filter(|vault| vault.root() == account_header.vault_root())
            .map_or_else(|| PartialVault::new(account_header.vault_root()), PartialVault::new_full);

        return Ok(AccountInputs::new(
            PartialAccount::new(
                account_header.id(),
                account_header.nonce(),
                code,
                PartialStorage::new(storage_details.header, storage_map_proofs)?,
                vault,
                None,
            )?,
            witness,
        ));
    }
    Err(TransactionRequestError::ForeignAccountDataMissing)
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

#[cfg(all(test, feature = "testing"))]
mod foreign_vault_tests {
    use alloc::sync::Arc;

    use miden_protocol::asset::FungibleAsset;
    use miden_testing::{Auth, MockChainBuilder};

    use super::account_proof_into_inputs;
    use crate::rpc::NodeRpcClient;
    use crate::rpc::domain::account::{AccountStorageRequirements, GetAccountRequest, VaultFetch};
    use crate::test_utils::mock::MockRpcApi;

    fn chain_with_funded_account() -> (miden_protocol::account::Account, Arc<dyn NodeRpcClient>) {
        let mut builder = MockChainBuilder::new();
        let account = builder
            .add_existing_wallet_with_assets(Auth::IncrNonce, [FungibleAsset::mock(500)])
            .unwrap();
        (account, Arc::new(MockRpcApi::new(builder.build().unwrap())))
    }

    /// `IfChangedFrom` with a matching root makes the node omit the asset list, which must
    /// degrade to a root-only vault rather than be kept as an empty one.
    #[tokio::test]
    async fn omitted_asset_list_degrades_to_a_root_only_vault() {
        let (account, rpc) = chain_with_funded_account();
        let committed_root = account.vault().root();

        let (_block, proof) = rpc
            .get_account(
                account.id(),
                GetAccountRequest::new().with_vault(VaultFetch::IfChangedFrom(committed_root)),
            )
            .await
            .unwrap();

        let details = proof.vault_details().expect("public account must carry vault details");
        assert!(
            details.assets.is_empty(),
            "the node omits the asset list when the sent root matches"
        );

        let inputs =
            account_proof_into_inputs(proof, &AccountStorageRequirements::default()).unwrap();

        assert_eq!(inputs.vault().root(), committed_root);
        assert!(
            inputs.vault().assets().next().is_none(),
            "an omitted list must not be kept as an empty vault"
        );
    }

    /// An asset list that hashes to the header's vault root is kept in full.
    #[tokio::test]
    async fn matching_asset_list_is_kept_as_a_full_vault() {
        let (account, rpc) = chain_with_funded_account();
        let committed_root = account.vault().root();

        let (_block, proof) = rpc
            .get_account(account.id(), GetAccountRequest::new().with_vault(VaultFetch::Always))
            .await
            .unwrap();

        let inputs =
            account_proof_into_inputs(proof, &AccountStorageRequirements::default()).unwrap();

        assert_eq!(inputs.vault().root(), committed_root);
        assert!(
            inputs.vault().assets().next().is_some(),
            "a verified asset list must be kept in the partial vault"
        );
    }
}
