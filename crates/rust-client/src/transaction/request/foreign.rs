//! Contains structures and functions related to FPI (Foreign Procedure Invocation) transactions.
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt::Write as _;

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
use miden_protocol::crypto::merkle::smt::PartialSmt;
use miden_protocol::transaction::{AccountInputs, TransactionScript};
use miden_protocol::vm::MIN_STACK_DEPTH;
use miden_protocol::{Felt, Word};
use miden_standards::code_builder::CodeBuilder;
use miden_tx::utils::serde::{Deserializable, DeserializationError, Serializable};

use super::TransactionRequestError;
use crate::rpc::domain::account::{
    AccountDetails,
    AccountProof,
    AccountStorageRequirements,
    StorageMapEntries,
};

// FPI SCRIPT
// ================================================================================================

/// Builds a transaction script that invokes the procedure with the given root on a foreign
/// account.
///
/// `args` are the procedure's inputs, pushed so that `args[0]` ends up on top of the stack. The
/// kernel reads them as a fixed window of [`MIN_STACK_DEPTH`] felts, so no more may be passed.
///
/// The script leaves the procedure's outputs on top of the stack and drops the rest.
pub fn build_fpi_script(
    code_builder: CodeBuilder,
    foreign_account_id: AccountId,
    procedure_root: Word,
    args: &[Felt],
) -> Result<TransactionScript, TransactionRequestError> {
    if args.len() > MIN_STACK_DEPTH {
        return Err(TransactionRequestError::ForeignProcedureInputsTooLong {
            max: MIN_STACK_DEPTH,
            actual: args.len(),
        });
    }

    let mut script = String::from(
        "use miden::protocol::tx\nuse miden::core::sys\n\n@transaction_script\npub proc main\n",
    );

    // Fill the unused input slots with zeros, then push the args on top of them.
    let pad_count = MIN_STACK_DEPTH - args.len();
    for _ in 0..pad_count / 4 {
        script.push_str("    padw\n");
    }
    for _ in 0..pad_count % 4 {
        script.push_str("    push.0\n");
    }
    for arg in args.iter().rev() {
        writeln!(script, "    push.{arg}").expect("writing to a string never fails");
    }

    writeln!(script, "    push.{}", procedure_root.to_hex())
        .expect("writing to a string never fails");
    writeln!(script, "    push.{}", foreign_account_id.prefix().as_u64())
        .expect("writing to a string never fails");
    writeln!(script, "    push.{}", foreign_account_id.suffix())
        .expect("writing to a string never fails");

    script.push_str("    exec.tx::execute_foreign_procedure\n");
    script.push_str("    exec.sys::truncate_stack\n");
    script.push_str("end\n");

    Ok(code_builder.compile_tx_script(&script)?)
}

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
pub(crate) fn account_proof_into_inputs(
    account_proof: AccountProof,
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
                    // header. Otherwise skip the map (the header alone carries its root) and let
                    // map reads resolve lazily as per-key witnesses during execution, rather than
                    // carry a map at a root the account never committed.
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
                StorageMapEntries::PartialMap { map_keys, partial_smt } => {
                    partial_map_into_partial_storage(&map_keys, &partial_smt)?
                },
                // The node carries no entries for an oversize map, so only the slot's root in the
                // storage header is known. Reads resolve lazily as per-key witnesses.
                StorageMapEntries::LimitExceeded => continue,
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

/// Rebuilds a [`PartialStorageMap`] from the raw keys the node covered and the partial SMT
/// covering them.
///
/// An empty key list keeps the root alone, since there is no opening to derive it from.
fn partial_map_into_partial_storage(
    map_keys: &[StorageMapKey],
    partial_smt: &PartialSmt,
) -> Result<PartialStorageMap, TransactionRequestError> {
    if map_keys.is_empty() {
        return Ok(PartialStorageMap::new(partial_smt.root()));
    }

    let witnesses = map_keys
        .iter()
        .map(|key| {
            let proof = partial_smt.open(&key.hash().as_word())?;
            StorageMapWitness::new(proof, [*key]).map_err(TransactionRequestError::StorageMapError)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(PartialStorageMap::with_witnesses(witnesses)?)
}

// TESTS
// ================================================================================================

#[cfg(all(test, feature = "testing"))]
mod foreign_vault_tests {
    use alloc::sync::Arc;

    use miden_protocol::account::Account;
    use miden_protocol::asset::FungibleAsset;
    use miden_testing::{Auth, MockChainBuilder};

    use super::account_proof_into_inputs;
    use crate::rpc::NodeRpcClient;
    use crate::rpc::domain::account::{GetAccountRequest, VaultFetch};
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

        let inputs = account_proof_into_inputs(proof).unwrap();

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

        let inputs = account_proof_into_inputs(proof).unwrap();

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
    use miden_protocol::transaction::AccountInputs;
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
        chain_with_map_account_capped(usize::MAX)
    }

    /// Same as [`chain_with_map_account`], with the mock node reporting any map larger than
    /// `oversize_threshold` as oversize.
    fn chain_with_map_account_capped(
        oversize_threshold: usize,
    ) -> (Account, StorageSlotName, Word, Arc<dyn NodeRpcClient>) {
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
        let rpc =
            MockRpcApi::new(builder.build().unwrap()).with_oversize_threshold(oversize_threshold);
        (account, slot_name, map_root, Arc::new(rpc))
    }

    /// Requests the given keys of the account's map slot and returns the resulting inputs.
    async fn inputs_for_keys(
        rpc: &Arc<dyn NodeRpcClient>,
        account: &Account,
        slot_name: &StorageSlotName,
        keys: &[StorageMapKey],
    ) -> AccountInputs {
        let requirements = AccountStorageRequirements::new([(slot_name.clone(), keys.iter())]);
        let (_block, proof) = rpc
            .get_account(
                account.id(),
                GetAccountRequest::new().with_storage(StorageMapFetch::Slots(requirements)),
            )
            .await
            .unwrap();

        account_proof_into_inputs(proof).unwrap()
    }

    /// An entry list that hashes to the slot's root in the storage header is kept as a full map.
    #[tokio::test]
    async fn matching_map_entries_are_kept_as_a_full_map() {
        let (account, slot_name, map_root, rpc) = chain_with_map_account();

        let inputs = inputs_for_keys(&rpc, &account, &slot_name, &[]).await;

        let map = inputs
            .storage()
            .maps()
            .next()
            .expect("a verified entry list must be kept in the partial storage");
        assert_eq!(map.root(), map_root);
    }

    /// The requested keys come back as a partial map, which must be carried with the slot's root
    /// and every requested value readable from it.
    #[tokio::test]
    async fn requested_keys_are_kept_as_a_partial_map() {
        let (account, slot_name, map_root, rpc) = chain_with_map_account();
        let present_key = StorageMapKey::new(Word::from([2u32; 4]));
        let absent_key = StorageMapKey::new(Word::from([99u32; 4]));

        let inputs = inputs_for_keys(&rpc, &account, &slot_name, &[present_key, absent_key]).await;

        let map = inputs
            .storage()
            .maps()
            .next()
            .expect("a partial map must be carried in the partial storage");
        assert_eq!(map.root(), map_root, "the partial map must prove the committed slot root");
        assert_eq!(map.get(&present_key), Some(Word::from([20u32; 4])));
        assert_eq!(
            map.get(&absent_key),
            Some(Word::empty()),
            "a requested key that is absent from the map must be proven absent, not untracked"
        );
    }

    /// A map the node reports as oversize carries no entries at all, so it must degrade to a
    /// root-only map (absent from the partial storage, served lazily during execution) rather
    /// than fail the conversion.
    #[tokio::test]
    async fn oversize_map_degrades_to_a_root_only_map() {
        let (account, slot_name, _map_root, rpc) = chain_with_map_account_capped(1);

        let inputs = inputs_for_keys(&rpc, &account, &slot_name, &[]).await;

        assert!(
            inputs.storage().maps().next().is_none(),
            "an oversize map must not be carried in the partial storage"
        );
    }

    /// An entry list that does not hash to the slot's root in the storage header must likewise
    /// degrade to a root-only map instead of being carried at a root the account never committed.
    #[tokio::test]
    async fn mismatched_map_entries_degrade_to_a_root_only_map() {
        let (account, slot_name, _map_root, rpc) = chain_with_map_account();

        let requirements =
            AccountStorageRequirements::all_entries(core::slice::from_ref(&slot_name));
        let (_block, mut proof) = rpc
            .get_account(
                account.id(),
                GetAccountRequest::new().with_storage(StorageMapFetch::Slots(requirements)),
            )
            .await
            .unwrap();

        let map_details = &mut proof
            .details_mut()
            .expect("public account must carry details")
            .storage_details
            .map_details;
        let StorageMapEntries::AllEntries(entries) = &mut map_details[0].entries else {
            panic!("the mock returns all entries when none are named");
        };
        entries.pop();

        let inputs = account_proof_into_inputs(proof).unwrap();

        assert!(
            inputs.storage().maps().next().is_none(),
            "an entry list that disagrees with the slot root must not be carried"
        );
    }
}

#[cfg(test)]
mod tests {
    use miden_protocol::testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET;

    use super::*;

    /// The inputs are read as a fixed window, so a longer argument list is rejected.
    #[test]
    fn build_fpi_script_rejects_more_args_than_the_input_window() {
        let foreign_id: AccountId = ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET.try_into().unwrap();
        let arg = Felt::new(1).expect("one is a valid field element");
        let args = vec![arg; MIN_STACK_DEPTH + 1];

        let err = build_fpi_script(CodeBuilder::new(), foreign_id, Word::empty(), &args)
            .expect_err("a longer argument list must be rejected");

        assert!(matches!(
            err,
            TransactionRequestError::ForeignProcedureInputsTooLong { max, actual }
                if max == MIN_STACK_DEPTH && actual == MIN_STACK_DEPTH + 1
        ));
    }
}
