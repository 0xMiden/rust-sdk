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
    StorageSlotHeader,
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
                    // Keep the entry list only if it hashes to the slot's root in the storage
                    // header — the node truncates maps with too many entries. Otherwise skip the
                    // map (the header alone carries its root) and let map reads resolve lazily
                    // as per-key witnesses during execution.
                    let slot_root = storage_details
                        .header
                        .slots()
                        .find(|slot| *slot.name() == account_storage_detail.slot_name)
                        .map(StorageSlotHeader::value);
                    let storage_entries_iter = entries.iter().map(|e| (e.key, e.value));
                    match StorageMap::with_entries(storage_entries_iter)
                        .ok()
                        .filter(|map| Some(map.root()) == slot_root)
                    {
                        Some(map) => PartialStorageMap::new_full(map),
                        None => continue,
                    }
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

    use miden_protocol::account::Account;
    use miden_protocol::asset::FungibleAsset;
    use miden_testing::{Auth, MockChainBuilder};

    use super::account_proof_into_inputs;
    use crate::rpc::NodeRpcClient;
    use crate::rpc::domain::account::{AccountStorageRequirements, GetAccountRequest, VaultFetch};
    use crate::test_utils::mock::MockRpcApi;

    fn chain_with_funded_account() -> (Account, Arc<dyn NodeRpcClient>) {
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

#[cfg(all(test, feature = "testing"))]
mod foreign_storage_map_tests {
    use alloc::sync::Arc;

    use miden_protocol::Word;
    use miden_protocol::account::{
        Account,
        StorageMap,
        StorageMapKey,
        StorageSlot,
        StorageSlotName,
    };
    use miden_testing::{Auth, MockChainBuilder};

    use super::account_proof_into_inputs;
    use crate::rpc::NodeRpcClient;
    use crate::rpc::domain::account::{
        AccountStorageRequirements,
        GetAccountRequest,
        StorageMapEntries,
        StorageMapFetch,
    };
    use crate::test_utils::mock::MockRpcApi;

    /// Builds a chain with an account holding a three-entry storage map, returning the account,
    /// the map's slot name and root, and an RPC client over the chain.
    fn chain_with_map_account() -> (Account, StorageSlotName, Word, Arc<dyn NodeRpcClient>) {
        let slot_name = StorageSlotName::new("miden::testing::map").unwrap();
        let mut map = StorageMap::new();
        for i in 1..=3u32 {
            map.insert(StorageMapKey::new(Word::from([i; 4])), Word::from([i * 10; 4]))
                .unwrap();
        }
        let map_root = map.root();

        let mut builder = MockChainBuilder::new();
        let account = builder
            .add_existing_mock_account_with_storage(
                Auth::IncrNonce,
                [StorageSlot::with_map(slot_name.clone(), map)],
            )
            .unwrap();
        (
            account,
            slot_name,
            map_root,
            Arc::new(MockRpcApi::new(builder.build().unwrap())),
        )
    }

    /// An entry list that hashes to the slot's root in the storage header is kept as a full map.
    #[tokio::test]
    async fn matching_map_entries_are_kept_as_a_full_map() {
        let (account, slot_name, map_root, rpc) = chain_with_map_account();

        let requirements =
            AccountStorageRequirements::all_entries(core::slice::from_ref(&slot_name));
        let (_block, proof) = rpc
            .get_account(
                account.id(),
                GetAccountRequest::new().with_storage(StorageMapFetch::Slots(requirements.clone())),
            )
            .await
            .unwrap();

        let inputs = account_proof_into_inputs(proof, &requirements).unwrap();

        let map = inputs
            .storage()
            .maps()
            .next()
            .expect("a verified entry list must be kept in the partial storage");
        assert_eq!(map.root(), map_root);
    }

    /// An entry list that no longer hashes to the slot's root — the node truncates maps with too
    /// many entries — must degrade to a root-only map (absent from the partial storage, served
    /// lazily during execution) rather than fail the conversion.
    #[tokio::test]
    async fn truncated_map_entries_degrade_to_a_root_only_map() {
        let (account, slot_name, _map_root, rpc) = chain_with_map_account();

        let requirements =
            AccountStorageRequirements::all_entries(core::slice::from_ref(&slot_name));
        let (_block, mut proof) = rpc
            .get_account(
                account.id(),
                GetAccountRequest::new().with_storage(StorageMapFetch::Slots(requirements.clone())),
            )
            .await
            .unwrap();

        // Truncate the returned entry list the way the node does for oversized maps.
        let map_details = &mut proof
            .details_mut()
            .expect("public account must carry details")
            .storage_details
            .map_details;
        let StorageMapEntries::AllEntries(entries) = &mut map_details[0].entries else {
            panic!("the mock returns all entries");
        };
        entries.pop();
        map_details[0].too_many_entries = true;

        let inputs = account_proof_into_inputs(proof, &requirements).unwrap();

        assert!(
            inputs.storage().maps().next().is_none(),
            "a truncated entry list must not be carried in the partial storage"
        );
    }
}
