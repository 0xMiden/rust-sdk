//! Tests the conversion of account proofs from the node into foreign account inputs.

// TESTS
// ================================================================================================

#[cfg(all(test, feature = "testing"))]
mod foreign_vault_tests {
    use alloc::sync::Arc;

    use miden_protocol::Word;
    use miden_protocol::account::{
        Account,
        AccountStorageHeader,
        PartialAccount,
        StorageSlotHeader,
        StorageSlotName,
        StorageSlotType,
    };
    use miden_protocol::asset::FungibleAsset;
    use miden_testing::{Auth, MockChainBuilder};

    use crate::rpc::domain::account::{AccountFetch, GetAccountRequest, VaultFetch};
    use crate::rpc::{NodeRpcClient, RpcError};
    use crate::test_utils::mock::MockRpcApi;

    fn chain_with_funded_account() -> (Account, Arc<dyn NodeRpcClient>) {
        let mut builder = MockChainBuilder::new();
        let account = builder
            .add_existing_wallet_with_assets(Auth::IncrNonce, [FungibleAsset::mock(500)])
            .unwrap();
        (account, Arc::new(MockRpcApi::new(builder.build().unwrap())))
    }

    /// `IfChangedFrom` with a matching root makes the node omit the asset list, which must degrade
    /// to a root-only vault rather than be kept as an empty one.
    #[tokio::test]
    async fn omitted_asset_list_degrades_to_a_root_only_vault() {
        let (account, rpc) = chain_with_funded_account();
        let committed_root = account.vault().root();

        let (_block, proof) = rpc
            .get_account(GetAccountRequest::new(account.id()).with_fetch(
                AccountFetch::new().with_vault(VaultFetch::IfChangedFrom(committed_root)),
            ))
            .await
            .unwrap();

        let details = proof.vault_details().expect("public account must carry vault details");
        assert!(
            details.assets().is_some_and(<[_]>::is_empty),
            "the node omits the asset list when the sent root matches"
        );

        let inputs = proof.into_account_inputs().unwrap();

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
            .get_account(
                GetAccountRequest::new(account.id())
                    .with_fetch(AccountFetch::new().with_vault(VaultFetch::Always)),
            )
            .await
            .unwrap();

        let inputs = proof.into_account_inputs().unwrap();

        assert_eq!(inputs.vault().root(), committed_root);
        assert!(
            inputs.vault().assets().next().is_some(),
            "a verified asset list must be kept in the partial vault"
        );
    }

    #[tokio::test]
    async fn account_details_convert_to_a_partial_account() {
        let (account, rpc) = chain_with_funded_account();
        let (_, proof) = rpc
            .get_account(
                GetAccountRequest::new(account.id())
                    .with_fetch(AccountFetch::new().with_vault(VaultFetch::Always)),
            )
            .await
            .unwrap();
        let partial = PartialAccount::try_from(proof.into_details().unwrap()).unwrap();
        assert_eq!(partial.to_commitment(), account.to_commitment());
        assert_eq!(
            partial.vault().open(FungibleAsset::mock(500).id()).unwrap(),
            account.vault().open(FungibleAsset::mock(500).id())
        );
    }

    #[tokio::test]
    async fn account_details_reject_a_mismatched_storage_header() {
        let (account, rpc) = chain_with_funded_account();
        let (_, proof) = rpc.get_account(GetAccountRequest::new(account.id())).await.unwrap();
        let mut details = proof.into_details().unwrap();
        details.storage_details.header = AccountStorageHeader::new(vec![StorageSlotHeader::new(
            StorageSlotName::new("miden::testing::wrong_storage").unwrap(),
            StorageSlotType::Value,
            Word::from([1u32, 2, 3, 4]),
        )])
        .unwrap();
        assert!(matches!(PartialAccount::try_from(details), Err(RpcError::InvalidResponse(_))));
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

    use crate::rpc::NodeRpcClient;
    use crate::rpc::domain::account::{
        AccountFetch,
        AccountStorageRequirements,
        GetAccountRequest,
        StorageMapEntries,
        StorageMapFetch,
    };
    use crate::test_utils::mock::MockRpcApi;

    /// Builds a chain with an account holding a three-entry storage map, returning the account, the
    /// map's slot name and root, and an RPC client over the chain.
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
        let (_block, proof) =
            rpc.get_account(GetAccountRequest::new(account.id()).with_fetch(
                AccountFetch::new().with_storage(StorageMapFetch::Slots(requirements)),
            ))
            .await
            .unwrap();

        proof.into_account_inputs().unwrap()
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
    /// root-only map (absent from the partial storage, served lazily during execution) rather than
    /// fail the conversion.
    #[tokio::test]
    async fn oversize_map_degrades_to_a_root_only_map() {
        let (account, slot_name, _map_root, rpc) = chain_with_map_account_capped(1);

        let inputs = inputs_for_keys(&rpc, &account, &slot_name, &[]).await;

        assert!(
            inputs.storage().maps().next().is_none(),
            "an oversize map must not be carried in the partial storage"
        );
    }

    /// An entry list that does not hash to the slot's root in the storage header is rejected,
    /// because only an inconsistent response produces it.
    #[tokio::test]
    async fn mismatched_map_entries_are_rejected() {
        let (account, slot_name, _map_root, rpc) = chain_with_map_account();

        let requirements =
            AccountStorageRequirements::all_entries(core::slice::from_ref(&slot_name));
        let (_block, mut proof) =
            rpc.get_account(GetAccountRequest::new(account.id()).with_fetch(
                AccountFetch::new().with_storage(StorageMapFetch::Slots(requirements)),
            ))
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

        assert!(matches!(
            proof.into_account_inputs(),
            Err(crate::rpc::RpcError::InvalidResponse(_))
        ));
    }
}
