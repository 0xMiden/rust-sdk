use alloc::boxed::Box;
use alloc::sync::Arc;
use std::collections::BTreeSet;
use std::net::TcpListener;
use std::time::Duration;

use miden_client::assembly::CodeBuilder;
use miden_client::auth::{AuthSchemeId, AuthSecretKey, AuthSingleSig, RPO_FALCON_SCHEME_ID};
use miden_client::keystore::Keystore;
use miden_client::note::{Note, P2idNote};
use miden_client::store::{NoteFilter, TransactionFilter};
use miden_client::transaction::{
    ChainAnchor,
    ChainAnchorError,
    ProvenTransaction,
    TransactionExecutorError,
    TransactionInputs,
    TransactionProver,
    TransactionProverError,
    TransactionRequestBuilder,
};
use miden_client::{ClientError, Deserializable, Serializable, async_trait};
use miden_debug::{DapClient, DapConfig, DapStopReason};
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
async fn dap_transaction_execution_records_replay_data() {
    let (mut client, _, keystore) = Box::pin(create_test_client()).await;
    let (wallet, _) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen_addr = listener.local_addr().unwrap();
    drop(listener);

    let snapshot_dir = tempfile::tempdir().unwrap();
    let snapshot_path = snapshot_dir.path().join("transaction.replay");

    let mut config = DapConfig::new(listen_addr.to_string());
    let event_recorder = config.record_event_mutations();
    let snapshot_recorder = config.record_snapshot(snapshot_path.clone());
    DapConfig::set_global(config);

    let dap_session = std::thread::spawn(move || {
        let mut dap_client =
            DapClient::connect_with_retry(&listen_addr.to_string(), Duration::from_secs(10))
                .expect("failed to connect to transaction DAP session");
        dap_client.handshake().expect("DAP handshake failed");

        loop {
            match dap_client.continue_().expect("DAP continue failed") {
                DapStopReason::Stopped(_) => {},
                DapStopReason::Terminated => {
                    dap_client.disconnect().expect("DAP disconnect failed");
                    break;
                },
                DapStopReason::Restarting => panic!("unexpected DAP restart"),
            }
        }
    });

    let transaction_request = TransactionRequestBuilder::new().build().unwrap();
    let transaction_result =
        Box::pin(client.execute_transaction_with_dap(wallet.id(), transaction_request))
            .await
            .expect("DAP transaction execution failed");
    assert_eq!(transaction_result.account_patch().id(), wallet.id());
    dap_session.join().expect("DAP client thread panicked");

    let event_log = event_recorder.take();
    assert!(!event_log.is_empty(), "transaction host events were not recorded");

    let snapshot_write = snapshot_recorder
        .take()
        .expect("replay snapshot status was not reported")
        .expect("replay snapshot write failed");
    assert_eq!(snapshot_write.event_count, event_log.len());
    assert!(snapshot_path.is_file(), "replay snapshot was not written");
}

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
        .with_component(AuthSingleSig::new(Approver::new(
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

/// A prover that discards the transaction it is asked to prove and always hands back a
/// pre-baked, independently valid proof of a completely different transaction.
/// Used to test that the client rejects a prover response unrelated to its request.
struct SwapProver {
    swapped: ProvenTransaction,
}

#[async_trait]
impl TransactionProver for SwapProver {
    async fn prove(
        &self,
        _inputs: TransactionInputs,
    ) -> Result<ProvenTransaction, TransactionProverError> {
        Ok(self.swapped.clone())
    }
}

// PROVER RESPONSE VALIDATION TESTS
// ================================================================================================

/// A prover that returns a valid proof of a transaction other than
/// the one it was asked to prove must be rejected, instead of having its answer submitted and
/// the local store updated as if the requested transaction had gone through.
#[tokio::test]
async fn submit_rejects_proven_transaction_unrelated_to_the_request() {
    let (mut client, _, keystore) = Box::pin(create_test_client()).await;
    let (wallet, faucet_a) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();
    let (_, faucet_b) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    // Transaction B: a mint from a different faucet, executed and proven on its own. This is
    // what the rogue prover hands back regardless of what it is asked to prove.
    let request_b = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet_b.id(), 50).unwrap(),
            wallet.id(),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    let result_b = Box::pin(client.execute_transaction(faucet_b.id(), request_b)).await.unwrap();
    let proven_b = Box::pin(client.prove_transaction(&result_b)).await.unwrap();
    let tx_id_b = proven_b.id();

    // Transaction A: the mint the client is actually asked to submit.
    let request_a = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet_a.id(), 100).unwrap(),
            wallet.id(),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    // Local state before the rejected submission, to check nothing is written for a transaction
    // that never reached the network.
    let tracked_before: BTreeSet<_> = client
        .get_transactions(TransactionFilter::All)
        .await
        .unwrap()
        .into_iter()
        .map(|tx| tx.id)
        .collect();
    let faucet_a_commitment_before =
        client.account_reader(faucet_a.id()).commitment().await.unwrap();

    let swap_prover = Arc::new(SwapProver { swapped: proven_b });
    let result =
        Box::pin(client.submit_new_transaction_with_prover(faucet_a.id(), request_a, swap_prover))
            .await;

    let err = match result {
        Ok(id) => panic!(
            "submitting a proven transaction unrelated to the requested one must be rejected, but \
             the call succeeded reporting {id} while the network received {tx_id_b}"
        ),
        Err(err) => err,
    };
    match err {
        ClientError::MismatchedProvenTransaction { returned, .. } => {
            assert_eq!(
                returned, tx_id_b,
                "the error must report the transaction the prover returned"
            );
        },
        other => panic!("unexpected error variant: {other:?}"),
    }

    let tracked_after: BTreeSet<_> = client
        .get_transactions(TransactionFilter::All)
        .await
        .unwrap()
        .into_iter()
        .map(|tx| tx.id)
        .collect();
    assert_eq!(
        tracked_before, tracked_after,
        "a rejected prover response must not record a transaction locally"
    );

    let faucet_a_commitment_after =
        client.account_reader(faucet_a.id()).commitment().await.unwrap();
    assert_eq!(
        faucet_a_commitment_before, faucet_a_commitment_after,
        "a rejected prover response must not advance the requesting account's local state"
    );
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
        .with_component(AuthSingleSig::new(Approver::new(
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

#[tokio::test]
async fn chain_anchor_pins_execution_to_an_older_reference_block() {
    let (mut client, rpc_api, keystore) = Box::pin(create_test_client()).await;
    let (wallet, faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();
    client.sync_state().await.unwrap();

    let transaction_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            wallet.id(),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    // Capture the anchor at the current tip. The mint consumes no notes, so nothing beyond the
    // reference block needs tracking.
    let anchor = client.chain_anchor_for_request(&transaction_request).await.unwrap();
    let anchor_block = anchor.block_num();

    // The anchor round-trips through serialization, as it would inside a proposal payload. This
    // chain came from the store via `build_partial_mmr_with_paths`, so unlike the synthetic
    // fixtures in the unit tests it exercises the real capture path's output.
    let deserialized = ChainAnchor::read_from_bytes(&anchor.to_bytes()).unwrap();
    assert_eq!(anchor, deserialized, "a captured anchor must survive serialization unchanged");
    let anchor = deserialized;
    assert_eq!(anchor.block_num(), anchor_block);

    // Advance the chain past the anchor and sync, so the local tip no longer matches it.
    for _ in 0..3 {
        rpc_api.prove_block();
    }
    client.sync_state().await.unwrap();
    let tip = client.get_sync_height().await.unwrap();
    assert!(tip > anchor_block, "the chain must have advanced past the anchor");

    // Anchored execution references the anchor block, not the tip.
    let anchored_result =
        Box::pin(client.execute_transaction_at(faucet.id(), transaction_request.clone(), anchor))
            .await
            .unwrap();
    assert_eq!(
        anchored_result.executed_transaction().block_header().block_num(),
        anchor_block,
        "anchored execution must reference the anchor block"
    );

    // The default path still references the tip.
    let tip_result = Box::pin(client.execute_transaction(faucet.id(), transaction_request))
        .await
        .unwrap();
    assert_eq!(
        tip_result.executed_transaction().block_header().block_num(),
        tip,
        "default execution must reference the sync height"
    );
}

/// The reason the anchor exists: a transaction executed against it must come out identical no
/// matter how far the local chain has moved on, because signatures collected over a slow approval
/// round are bound to the transaction summary and stop applying the moment it changes.
///
/// A literal `TransactionSummary` cannot be built here — it is raised by the `Unauthorized` kernel
/// event, which only a multisig-style auth component triggers, and no such flow exists in this
/// workspace. Its inputs are reachable, though, and this test stands in for it with the final
/// account commitment and the input, output and reference block commitments. Of those, only the
/// block commitment moves with the chain — the expiration delta the summary binds is relative, so
/// it comes from the request rather than the reference block — which is why the assertion on it,
/// and the unanchored control below, are what stop this test passing if the anchor did nothing.
#[tokio::test]
async fn chain_anchor_execution_reproduces_the_same_transaction_at_a_later_height() {
    let (mut client, rpc_api, keystore) = Box::pin(create_test_client()).await;
    let (wallet, faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();
    client.sync_state().await.unwrap();

    // Consuming a note gives the transaction a non-empty input notes commitment and a real account
    // delta, so the assertions below are not comparing empty values.
    let mint_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            wallet.id(),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    let note_id = mint_request.expected_output_own_notes().pop().unwrap().id();
    Box::pin(client.submit_new_transaction(faucet.id(), mint_request))
        .await
        .unwrap();
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let note = client.get_input_note(note_id).await.unwrap().unwrap();
    let consume_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.try_into().unwrap()])
        .unwrap();

    // Capture once, as a proposer would, and round-trip it as the proposal payload would.
    let anchor = client.chain_anchor_for_request(&consume_request).await.unwrap();
    let anchor = ChainAnchor::read_from_bytes(&anchor.to_bytes()).unwrap();

    let before = Box::pin(client.execute_transaction_at(
        wallet.id(),
        consume_request.clone(),
        anchor.clone(),
    ))
    .await
    .unwrap();
    let (before_account, before_inputs, before_outputs, before_block) = {
        let tx = before.executed_transaction();
        (
            tx.final_account().to_commitment(),
            tx.input_notes().commitment(),
            tx.output_notes().commitment(),
            tx.block_header().commitment(),
        )
    };
    assert!(!before.executed_transaction().input_notes().is_empty());

    // Time passes while approvals are gathered.
    for _ in 0..4 {
        rpc_api.prove_block();
    }
    client.sync_state().await.unwrap();
    assert!(client.get_sync_height().await.unwrap() > anchor.block_num());

    let after =
        Box::pin(client.execute_transaction_at(wallet.id(), consume_request.clone(), anchor))
            .await
            .unwrap();
    let after_tx = after.executed_transaction();

    assert_eq!(
        before_account,
        after_tx.final_account().to_commitment(),
        "the account delta must not depend on when the anchored transaction was executed"
    );
    assert_eq!(
        before_inputs,
        after_tx.input_notes().commitment(),
        "the input notes commitment must not depend on when the anchored transaction was executed"
    );
    assert_eq!(
        before_outputs,
        after_tx.output_notes().commitment(),
        "the output notes commitment must not depend on when the anchored transaction was executed"
    );
    assert_eq!(
        before_block,
        after_tx.block_header().commitment(),
        "the summary's block commitment must not depend on when the anchored transaction was run"
    );

    // The control, and the reason this test asserts on the block commitment at all: the account
    // delta and both note commitments come out identical with or without an anchor for a request
    // this simple, so the block commitment is the whole difference between a summary the collected
    // signatures still verify against and one they do not. Without this assertion the three above
    // would pass even if `execute_transaction_at` ignored the anchor entirely.
    let unanchored = Box::pin(client.execute_transaction(wallet.id(), consume_request))
        .await
        .unwrap();
    assert_ne!(
        before_block,
        unanchored.executed_transaction().block_header().commitment(),
        "the unanchored control must bind a different block, or this test proves nothing"
    );
}

/// An anchor collected days before it is executed can expire while signatures are gathered. The
/// protocol rejects a transaction that can no longer be included in the next block
/// (`ProposedBatchError::ExpiredTransaction`), so the client has to fail at execution with a
/// diagnosable error rather than hand back a transaction that cannot be submitted.
#[tokio::test]
async fn chain_anchor_execution_rejects_an_already_expired_transaction() {
    let (mut client, rpc_api, keystore) = Box::pin(create_test_client()).await;
    let (wallet, faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();
    client.sync_state().await.unwrap();

    // The shortest expiry the builder accepts, so a handful of blocks is enough to pass it.
    let transaction_request = TransactionRequestBuilder::new()
        .expiration_delta(1)
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            wallet.id(),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    let anchor = client.chain_anchor_for_request(&transaction_request).await.unwrap();
    let anchor_block = anchor.block_num();

    // Advance to exactly the expiration block, not past it. The guard is `expiration <=
    // sync_height`, and a transaction expiring at the tip can no longer be included, so this is
    // the case that separates the correct boundary from a `<`, which would let it through. The
    // well-past case below then covers the other direction, so neither mutation survives.
    rpc_api.prove_block();
    client.sync_state().await.unwrap();
    let tip = client.get_sync_height().await.unwrap();
    assert_eq!(
        tip,
        anchor_block + 1,
        "the chain must have reached exactly the expiration block"
    );

    let err = Box::pin(client.execute_transaction_at(
        faucet.id(),
        transaction_request.clone(),
        anchor.clone(),
    ))
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            ClientError::ChainAnchorError(ChainAnchorError::AnchoredTransactionExpired { .. })
        ),
        "expected an expiration error at exactly the expiration block, got {err:?}"
    );

    // And well past it, which is the ordinary case of a signing round that ran long. Asserting
    // only this one would leave `<` alive; asserting only the boundary above would leave `==`.
    for _ in 0..4 {
        rpc_api.prove_block();
    }
    client.sync_state().await.unwrap();
    let err =
        Box::pin(client.execute_transaction_at(faucet.id(), transaction_request.clone(), anchor))
            .await
            .unwrap_err();
    assert!(
        matches!(
            err,
            ClientError::ChainAnchorError(ChainAnchorError::AnchoredTransactionExpired { .. })
        ),
        "expected an expiration error well past the expiration block, got {err:?}"
    );

    // The same request still executes against the tip, so the failure is the anchor's staleness
    // and not something wrong with the request itself.
    Box::pin(client.execute_transaction(faucet.id(), transaction_request))
        .await
        .unwrap();
}

/// The capture path removes the reference block from the set of blocks it authenticates, because
/// the reference block is not a leaf of its own MMR. A note created in that very block is the case
/// that exercises the removal; without it the anchor would ask for a nonexistent path.
#[tokio::test]
async fn chain_anchor_for_request_handles_a_note_created_in_the_reference_block() {
    let (mut client, rpc_api, keystore) = Box::pin(create_test_client()).await;
    let (wallet, faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();
    client.sync_state().await.unwrap();

    let mint_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            wallet.id(),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    let note_id = mint_request.expected_output_own_notes().pop().unwrap().id();
    Box::pin(client.submit_new_transaction(faucet.id(), mint_request))
        .await
        .unwrap();
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Unlike the test below, do not advance the chain again: the note's inclusion block stays the
    // tip, which is exactly the block the anchor will pin as its reference.
    let note = client.get_input_note(note_id).await.unwrap().unwrap();
    let note_block = note.inclusion_proof().unwrap().location().block_num();
    let tip = client.get_sync_height().await.unwrap();
    assert_eq!(note_block, tip, "the note must have been created in the block the anchor pins");

    let consume_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.try_into().unwrap()])
        .unwrap();
    let anchor = client.chain_anchor_for_request(&consume_request).await.unwrap();

    assert_eq!(anchor.block_num(), tip);

    Box::pin(client.execute_transaction_at(wallet.id(), consume_request, anchor))
        .await
        .unwrap();
}

#[tokio::test]
async fn chain_anchor_for_request_tracks_consumed_note_blocks() {
    let (mut client, rpc_api, keystore) = Box::pin(create_test_client()).await;
    let (wallet, faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();
    client.sync_state().await.unwrap();

    // Mint a note for the wallet and let it commit on chain.
    let mint_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            wallet.id(),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    let note_id = mint_request.expected_output_own_notes().pop().unwrap().id();
    Box::pin(client.submit_new_transaction(faucet.id(), mint_request))
        .await
        .unwrap();
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let note = client.get_input_note(note_id).await.unwrap().unwrap();
    let note_block = note.inclusion_proof().unwrap().location().block_num();

    // Advance one block so the note's creation block is older than the anchor's reference
    // block — otherwise the note block IS the reference block and needs no tracking.
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Capture the anchor from the consume request itself: the note's creation block must be
    // tracked without the caller having to know it.
    let consume_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.try_into().unwrap()])
        .unwrap();
    let anchor = client.chain_anchor_for_request(&consume_request).await.unwrap();
    let anchor_block = anchor.block_num();
    assert!(
        anchor.partial_blockchain().contains_block(note_block),
        "the anchor must track the consumed note's creation block"
    );

    // Advance the chain past the anchor and sync.
    for _ in 0..3 {
        rpc_api.prove_block();
    }
    client.sync_state().await.unwrap();
    assert!(client.get_sync_height().await.unwrap() > anchor_block);

    // The consume executes against the anchor block, and the result reports the same anchor.
    let result = Box::pin(client.execute_transaction_at(wallet.id(), consume_request, anchor))
        .await
        .unwrap();
    assert_eq!(result.executed_transaction().block_header().block_num(), anchor_block);
}

/// Note screening runs against the anchored data store, so it must use the anchor's reference
/// block. Using the sync height instead makes every anchored execution of a request built with
/// `ignore_invalid_input_notes` fail once the chain moves past the anchor — including, as here,
/// one where screening drops nothing and the flag is therefore inert.
#[tokio::test]
async fn chain_anchor_note_screening_uses_the_anchor_block() {
    let (mut client, rpc_api, keystore) = Box::pin(create_test_client()).await;
    let (wallet, faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();
    client.sync_state().await.unwrap();

    // Mint a note for the wallet and let it commit on chain.
    let mint_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            wallet.id(),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    let note_id = mint_request.expected_output_own_notes().pop().unwrap().id();
    Box::pin(client.submit_new_transaction(faucet.id(), mint_request))
        .await
        .unwrap();
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let note = client.get_input_note(note_id).await.unwrap().unwrap();

    // Advance one block so the note's creation block is older than the anchor's reference block.
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // The invalid-note trial must run at the anchor block, not the sync height.
    let consume_request = TransactionRequestBuilder::new()
        .ignore_invalid_input_notes()
        .build_consume_notes(vec![note.try_into().unwrap()])
        .unwrap();
    let anchor = client.chain_anchor_for_request(&consume_request).await.unwrap();
    let anchor_block = anchor.block_num();

    // Advance the chain past the anchor and sync.
    for _ in 0..3 {
        rpc_api.prove_block();
    }
    client.sync_state().await.unwrap();
    assert!(client.get_sync_height().await.unwrap() > anchor_block);

    let result = Box::pin(client.execute_transaction_at(wallet.id(), consume_request, anchor))
        .await
        .unwrap();
    assert_eq!(result.executed_transaction().block_header().block_num(), anchor_block);
}

#[tokio::test]
async fn chain_anchor_untracked_note_block_fails_with_typed_error() {
    let (mut client, rpc_api, keystore) = Box::pin(create_test_client()).await;
    let (wallet, faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();
    client.sync_state().await.unwrap();

    let mint_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            wallet.id(),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    let note_id = mint_request.expected_output_own_notes().pop().unwrap().id();
    Box::pin(client.submit_new_transaction(faucet.id(), mint_request))
        .await
        .unwrap();
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let note = client.get_input_note(note_id).await.unwrap().unwrap();
    let note_block = note.inclusion_proof().unwrap().location().block_num();

    // Advance so the note's creation block is older than the anchor block and needs tracking.
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Capture the anchor from a request without input notes, so it doesn't track the note block.
    let unrelated_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            wallet.id(),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    let anchor = client.chain_anchor_for_request(&unrelated_request).await.unwrap();
    assert!(!anchor.partial_blockchain().contains_block(note_block));

    // Consuming the note against that anchor fails with the typed error, so callers can react by
    // recapturing a wider anchor.
    let consume_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.try_into().unwrap()])
        .unwrap();
    let result =
        Box::pin(client.execute_transaction_at(wallet.id(), consume_request, anchor)).await;
    assert!(matches!(
        result,
        Err(ClientError::ChainAnchorError(ChainAnchorError::BlockNotTracked { block_num }))
            if block_num == note_block
    ));
}
