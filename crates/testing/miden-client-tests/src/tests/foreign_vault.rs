use alloc::sync::Arc;
use std::collections::{BTreeMap, BTreeSet};
use std::env::temp_dir;

use miden_client::assembly::CodeBuilder;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::rpc::domain::account::{AccountStorageRequirements, AccountVaultDetails};
use miden_client::testing::common::create_test_store_path;
use miden_client::testing::mock::{MockClient, MockRpcApi};
use miden_client::testing::{ClientDataStore, DataStore};
use miden_client::transaction::{AdviceInputs, ForeignAccount};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::{
    Account,
    AccountBuilder,
    AccountComponent,
    AccountComponentMetadata,
    AccountId,
    AccountType,
};
use miden_protocol::asset::{Asset, AssetId, FungibleAsset};
use miden_protocol::testing::account_id::{ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET, AccountIdBuilder};
use miden_protocol::{Felt, Word};
use miden_testing::{AccountState, Auth, MockChainBuilder};

use crate::tests::seed_mock_transaction_encryption_key;

// FIXTURE
// ================================================================================================

/// A chain with a public foreign account that holds one asset, and a wallet that executes against
/// it. The mock node marks every vault with an asset as oversized, so it never sends the asset list
/// of the foreign account.
struct OversizedVaultFixture {
    rpc_api: MockRpcApi,
    client: MockClient<FilesystemKeyStore>,
    wallet_id: AccountId,
    foreign_account: Account,
    get_asset_root: Word,
    asset: Asset,
}

async fn oversized_vault_fixture() -> OversizedVaultFixture {
    let component_code = CodeBuilder::default()
        .compile_component_code(
            "miden::testing::foreign_vault",
            "
            use miden::protocol::active_account
            @account_procedure
            pub proc get_asset
                # => [ASSET_ID, pad(12)]
                exec.active_account::get_asset
            end",
        )
        .unwrap();
    let component = AccountComponent::new(
        component_code,
        vec![],
        AccountComponentMetadata::new("miden::testing::foreign_vault"),
    )
    .unwrap();
    let get_asset_root = component.mast_forest().procedure_digests().next().unwrap();

    let faucet_id = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET).unwrap();
    let asset: Asset = FungibleAsset::new(faucet_id, 100).unwrap().into();

    let mut builder = MockChainBuilder::new();
    let foreign_account = builder
        .add_account_from_builder(
            Auth::IncrNonce,
            AccountBuilder::new([7; 32])
                .account_type(AccountType::Public)
                .with_component(component)
                .with_assets([asset]),
            AccountState::Exists,
        )
        .unwrap();
    let wallet = builder.add_existing_wallet(Auth::IncrNonce).unwrap();
    let rpc_api = MockRpcApi::new(builder.build().unwrap()).with_oversize_threshold(0);

    let mut client = ClientBuilder::new()
        .rpc(Arc::new(rpc_api.clone()))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(FilesystemKeyStore::new(temp_dir()).unwrap()))
        .tx_discard_delta(None)
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;
    client.add_account(&wallet, false).await.unwrap();
    client.sync_state().await.unwrap();

    OversizedVaultFixture {
        rpc_api,
        client,
        wallet_id: wallet.id(),
        foreign_account,
        get_asset_root,
        asset,
    }
}

// TESTS
// ================================================================================================

/// The data store fetches vault proofs for the requested assets only, when the local store does not
/// hold the vault. The node splits the request into groups of
/// [`AccountVaultDetails::MAX_PARTIAL_VAULT_ASSETS`] assets, and the size of the vault does not
/// cause a vault sync.
#[tokio::test]
async fn vault_witness_fallback_fetches_proofs_for_the_requested_assets() {
    let mut fixture = Box::pin(oversized_vault_fixture()).await;
    let foreign_account = &fixture.foreign_account;

    // The held asset plus enough absent assets to need a second request.
    let asset_ids: BTreeSet<AssetId> =
        core::iter::once(fixture.asset.id())
            .chain((1..=64u8).map(|seed| {
                AssetId::new_fungible(AccountIdBuilder::new().build_with_seed([seed; 32]))
            }))
            .collect();
    assert_eq!(asset_ids.len(), AccountVaultDetails::MAX_PARTIAL_VAULT_ASSETS + 1);

    let store = fixture.client.test_store().clone();
    let rpc_api = fixture.client.test_rpc_api().clone();
    let data_store = ClientDataStore::new(store, rpc_api);

    let calls_before = fixture.rpc_api.get_account_call_count();
    let witnesses = data_store
        .get_vault_asset_witnesses(
            foreign_account.id(),
            foreign_account.vault().root(),
            asset_ids.clone(),
        )
        .await
        .unwrap();

    let expected: Vec<_> = asset_ids
        .iter()
        .map(|asset_id| foreign_account.vault().open(*asset_id))
        .collect();
    assert_eq!(witnesses, expected);
    assert_eq!(
        fixture.rpc_api.get_account_call_count() - calls_before,
        2,
        "65 assets need two requests"
    );
    assert_eq!(fixture.rpc_api.sync_account_vault_call_count(), 0);
}

/// An FPI read from the vault of an untracked foreign account gets a proof for the read asset only.
/// The node does not send the asset list of the oversized vault, so the foreign inputs carry only
/// the vault root, and the read reaches the RPC fallback of the data store during execution.
#[tokio::test]
async fn fpi_asset_read_from_an_oversized_vault_fetches_one_proof() {
    let fixture = Box::pin(oversized_vault_fixture()).await;
    let foreign_account_id = fixture.foreign_account.id();

    let tx_script = fixture
        .client
        .code_builder()
        .compile_tx_script(format!(
            "
            use miden::protocol::tx
            use miden::core::sys
            @transaction_script
            pub proc main
                padw padw padw
                push.{asset_id}
                push.{get_asset_root}
                push.{prefix} push.{suffix}
                exec.tx::execute_foreign_procedure
                # => [ASSET_VALUE, pad(12)]
                exec.sys::truncate_stack
            end
            ",
            asset_id = Word::from(fixture.asset.id()),
            get_asset_root = fixture.get_asset_root,
            prefix = foreign_account_id.prefix().as_u64(),
            suffix = foreign_account_id.suffix(),
        ))
        .unwrap();
    let foreign_accounts = BTreeMap::from([(
        foreign_account_id,
        ForeignAccount::public(foreign_account_id, AccountStorageRequirements::default()).unwrap(),
    )]);

    let calls_before = fixture.rpc_api.get_account_call_count();
    let output_stack = Box::pin(fixture.client.execute_program(
        fixture.wallet_id,
        tx_script,
        AdviceInputs::default(),
        foreign_accounts,
    ))
    .await
    .unwrap();

    let mut expected_stack = [Felt::ZERO; 16];
    expected_stack[..4].copy_from_slice(fixture.asset.to_value_word().as_elements());
    assert_eq!(output_stack, expected_stack);
    assert_eq!(
        fixture.rpc_api.get_account_call_count() - calls_before,
        2,
        "one request loads the foreign account and one request proves the asset"
    );
    assert_eq!(fixture.rpc_api.sync_account_vault_call_count(), 0);
}
