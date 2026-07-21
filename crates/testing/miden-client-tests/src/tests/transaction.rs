use alloc::boxed::Box;
use alloc::sync::Arc;

use miden_client::assembly::CodeBuilder;
use miden_client::auth::{AuthSchemeId, AuthSecretKey, AuthSingleSig, RPO_FALCON_SCHEME_ID};
use miden_client::keystore::Keystore;
use miden_client::note::{Note, P2idNote};
use miden_client::store::NoteFilter;
use miden_client::transaction::{
    ProvenTransaction,
    TransactionExecutorError,
    TransactionInputs,
    TransactionProver,
    TransactionProverError,
    TransactionRequestBuilder,
};
use miden_client::{ClientError, async_trait};
use miden_protocol::account::{
    AccountBuilder,
    AccountComponent,
    AccountComponentMetadata,
    AccountType,
    StorageMap,
    StorageMapKey,
    StorageSlot,
    StorageSlotName,
};
use miden_protocol::assembly::diagnostics::miette::GraphicalReportHandler;
use miden_protocol::asset::{Asset, FungibleAsset};
use miden_protocol::crypto::rand::FeltRng;
use miden_protocol::note::{NoteRecipient, NoteStorage, NoteType};
use miden_protocol::testing::account_id::{
    ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
    ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
    ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
};
use miden_protocol::{Felt, Word};
use miden_standards::account::AccountBuilderSchemaCommitmentExt;
use miden_standards::account::auth::Approver;
use miden_standards::account::wallets::BasicWallet;

use super::PaymentNoteDescription;
use crate::tests::{create_test_client, setup_wallet_and_faucet};

#[tokio::test]
async fn transaction_creates_two_notes() {
    let (mut client, _, keystore) = Box::pin(create_test_client()).await;
    let asset_1: Asset =
        FungibleAsset::new(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET.try_into().unwrap(), 123)
            .unwrap()
            .into();
    let asset_2: Asset =
        FungibleAsset::new(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET.try_into().unwrap(), 500)
            .unwrap()
            .into();

    let secret_key = AuthSecretKey::new_falcon512_poseidon2();
    let pub_key = secret_key.public_key();

    let account = AccountBuilder::new(Default::default())
        .with_component(BasicWallet)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            pub_key.to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_assets([asset_1, asset_2])
        .build_existing()
        .unwrap();

    keystore.add_key(&secret_key, account.id()).await.unwrap();

    client.add_account(&account, false).await.unwrap();
    client.sync_state().await.unwrap();
    let tx_request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![asset_1, asset_2],
                account.id(),
                ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap(),
            ),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    // Submit transaction
    let _tx_id = Box::pin(client.submit_new_transaction(account.id(), tx_request.clone()))
        .await
        .unwrap();

    // Validate that the request is expected to create two assets in the first note
    let expected_notes = tx_request.expected_output_own_notes();
    assert!(!expected_notes.is_empty());
    assert_eq!(expected_notes[0].assets().num_assets(), 2);

    // Let the client process state changes (mock chain)
    client.sync_state().await.unwrap();
}

#[tokio::test]
async fn transaction_error_reports_source_line() {
    let (mut client, _, keystore) = Box::pin(create_test_client()).await;
    let (wallet, _) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    let failing_script = client
        .code_builder()
        .compile_tx_script("@transaction_script pub proc main push.0 push.2 assert_eq end")
        .unwrap();

    let tx_request =
        TransactionRequestBuilder::new().custom_script(failing_script).build().unwrap();

    let err = Box::pin(client.execute_transaction(wallet.id(), tx_request))
        .await
        .expect_err("transaction should fail for assertion");

    let source_snippet = "push.0 push.2";
    match err {
        ClientError::TransactionExecutorError(
            TransactionExecutorError::TransactionProgramExecutionFailed(exec_err),
        ) => {
            let mut rendered = String::new();
            GraphicalReportHandler::new()
                .render_report(&mut rendered, exec_err.as_ref())
                .unwrap();

            assert!(
                rendered.contains(source_snippet),
                "expected execution error to include script snippet; got:\n{rendered}"
            );
        },
        other => panic!("unexpected error variant: {other:?}"),
    }
}

/// Regression test for #2221: a transaction request whose execution fails must leave the store
/// unchanged — no orphaned input notes and no orphaned output note scripts.
#[tokio::test]
async fn execute_transaction_failure_leaves_store_unchanged() {
    let (mut client, _, keystore) = Box::pin(create_test_client()).await;
    let (wallet, faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    // A note targeting the wallet that is not tracked by the store. Passing it as a request
    // input note is what would trigger an input-note write during preparation.
    let asset = FungibleAsset::new(faucet.id(), 100).unwrap();
    let unauthenticated_note: Note = P2idNote::builder()
        .sender(faucet.id())
        .target(wallet.id())
        .asset(asset)
        .note_type(NoteType::Private)
        .generate_serial_number(client.rng())
        .build()
        .unwrap()
        .into();
    let note_id = unauthenticated_note.id();

    // An expected output recipient with a non-standard script. Declaring it in the request is
    // what would trigger a note-script write during preparation.
    let output_note_script = client
        .code_builder()
        .compile_note_script(
            "@note_script
            pub proc main
                nop
            end",
        )
        .unwrap();
    let script_root = output_note_script.root();
    let serial_num = client.rng().draw_word();
    let output_recipient =
        NoteRecipient::new(serial_num, output_note_script, NoteStorage::new(vec![]).unwrap());

    // A transaction script that always fails, forcing execution to error after preparation has
    // succeeded.
    let failing_script = client
        .code_builder()
        .compile_tx_script("@transaction_script pub proc main push.0 push.2 assert_eq end")
        .unwrap();

    let tx_request = TransactionRequestBuilder::new()
        .input_notes([(unauthenticated_note, None)])
        .expected_output_recipients(vec![output_recipient])
        .custom_script(failing_script)
        .build()
        .unwrap();

    // Neither the note nor the script is tracked before execution.
    assert!(
        client
            .get_input_notes(NoteFilter::List(vec![note_id]))
            .await
            .unwrap()
            .is_empty(),
        "note should not be tracked before execution"
    );
    assert!(
        client.test_store().get_note_script(script_root.into()).await.is_err(),
        "output note script should not be stored before execution"
    );

    Box::pin(client.execute_transaction(wallet.id(), tx_request))
        .await
        .expect_err("transaction execution should fail");

    // The failed execution must leave the store unchanged.
    assert!(
        client
            .get_input_notes(NoteFilter::List(vec![note_id]))
            .await
            .unwrap()
            .is_empty(),
        "execution failure must not persist the request's input notes"
    );
    assert!(
        client.test_store().get_note_script(script_root.into()).await.is_err(),
        "execution failure must not persist the request's output note scripts"
    );
}

// MOCK PROVERS
// ================================================================================================

/// A prover that always fails with a `TransactionProverError`.
/// Used to test the prover fallback pattern.
struct AlwaysFailingProver;

#[async_trait]
impl TransactionProver for AlwaysFailingProver {
    async fn prove(
        &self,
        _inputs: TransactionInputs,
    ) -> Result<ProvenTransaction, TransactionProverError> {
        Err(TransactionProverError::other("simulated remote prover failure"))
    }
}

// PROVER FALLBACK TESTS
// ================================================================================================

/// Tests the prover fallback pattern: when a remote prover fails, the same transaction
/// request can be retried with a different (local) prover.
#[tokio::test]
async fn prover_fallback_pattern_allows_retry_with_different_prover() {
    let (mut client, _, keystore) = Box::pin(create_test_client()).await;
    let (wallet, faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    let fungible_asset = FungibleAsset::new(faucet.id(), 100).unwrap();

    let tx_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(fungible_asset, wallet.id(), NoteType::Private, client.rng())
        .unwrap();

    // First attempt with failing prover
    let failing_prover = Arc::new(AlwaysFailingProver);
    let result = Box::pin(client.submit_new_transaction_with_prover(
        faucet.id(),
        tx_request.clone(),
        failing_prover,
    ))
    .await;

    // Verify first attempt fails with TransactionProvingError
    assert!(
        matches!(result, Err(ClientError::TransactionProvingError(_))),
        "expected TransactionProvingError on first attempt"
    );

    // Retry with the client's default prover (which should work)
    let tx_id = Box::pin(client.submit_new_transaction(faucet.id(), tx_request)).await;

    assert!(tx_id.is_ok(), "fallback to default prover should succeed");
}

// LAZY FOREIGN ACCOUNT LOADING TESTS
// ================================================================================================

/// Tests that the `ClientDataStore` lazy-loads foreign account inputs via RPC when the foreign
/// account is not specified in the `TransactionRequestBuilder`.
#[tokio::test]
async fn lazy_foreign_account_loading() {
    let (mut client, rpc_api, keystore) = Box::pin(create_test_client()).await;

    // Setup: Create and deploy a public foreign account with a storage map.
    let map_key: Word =
        [Felt::from(15u32), Felt::from(15u32), Felt::from(15u32), Felt::from(15u32)].into();
    let map_value: Word =
        [Felt::from(9u32), Felt::from(12u32), Felt::from(18u32), Felt::from(30u32)].into();
    let map_slot_name = StorageSlotName::new("miden::testing::fpi::map").unwrap();

    let mut storage_map = StorageMap::new();
    storage_map.insert(StorageMapKey::new(map_key), map_value).unwrap();
    let map_slot = StorageSlot::with_map(map_slot_name, storage_map);

    let component_code = CodeBuilder::default()
        .compile_component_code(
            "miden::testing::fpi_lazy_component",
            format!(
                r#"
                const STORAGE_MAP_SLOT = word("miden::testing::fpi::map")
                @account_procedure
                pub proc get_map_item
                    push.{map_key}
                    push.STORAGE_MAP_SLOT[0..2]
                    exec.::miden::protocol::active_account::get_map_item
                    swapw dropw
                end"#
            ),
        )
        .unwrap();
    let fpi_component = AccountComponent::new(
        component_code,
        vec![map_slot],
        AccountComponentMetadata::new("miden::testing::fpi_lazy_component"),
    )
    .unwrap();
    let proc_root = fpi_component.mast_forest().procedure_digests().next().unwrap();

    let secret_key = AuthSecretKey::new_falcon512_poseidon2();
    let foreign_account = AccountBuilder::new(Default::default())
        .account_type(AccountType::Public)
        .with_component(fpi_component)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            secret_key.public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .build_with_schema_commitment()
        .unwrap();
    let foreign_account_id = foreign_account.id();

    keystore.add_key(&secret_key, foreign_account_id).await.unwrap();
    client.add_account(&foreign_account, false).await.unwrap();

    // Deploy the foreign account (sets nonce from 0 to 1).
    let deploy_request = TransactionRequestBuilder::new().build().unwrap();
    Box::pin(client.submit_new_transaction(foreign_account_id, deploy_request))
        .await
        .unwrap();

    // Commit the deploy transaction to a block and sync the client.
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Setup: Create a local wallet to execute the FPI transaction.
    let local_wallet = super::insert_new_wallet(&mut client, AccountType::Public, &keystore)
        .await
        .unwrap();

    // Execute FPI transaction WITHOUT specifying foreign account.

    // Verify no foreign account code is cached before the transaction.
    let cached = client
        .test_store()
        .get_foreign_account_code(vec![foreign_account_id])
        .await
        .unwrap();
    assert!(
        cached.is_empty(),
        "foreign account code should not be cached before lazy loading"
    );

    // Build a transaction script that calls the foreign procedure via FPI.
    // The procedure reads from the storage map, triggering lazy loading of map entries.
    let tx_script = client
        .code_builder()
        .compile_tx_script(format!(
            "
            use miden::protocol::tx
            @transaction_script
            pub proc main
                push.{proc_root}
                push.{prefix} push.{suffix}
                exec.tx::execute_foreign_procedure
                push.{map_value} assert_eqw
            end
            ",
            prefix = foreign_account_id.prefix().as_u64(),
            suffix = foreign_account_id.suffix(),
        ))
        .unwrap();

    // Build request WITHOUT specifying foreign accounts, lazy loading should handle it.
    let tx_request = TransactionRequestBuilder::new().custom_script(tx_script).build().unwrap();

    // Execute the transaction. This should succeed because the data store will
    // lazy-load the foreign account via RPC, and then lazy-load the storage map
    // entries when the procedure reads from the map.
    Box::pin(client.submit_new_transaction(local_wallet.id(), tx_request))
        .await
        .unwrap();

    // Verify the foreign account code is now cached in the store.
    let cached = client
        .test_store()
        .get_foreign_account_code(vec![foreign_account_id])
        .await
        .unwrap();
    assert_eq!(cached.len(), 1, "foreign account code should be cached after lazy loading");
}
