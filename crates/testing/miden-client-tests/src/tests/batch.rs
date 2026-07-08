use std::collections::BTreeSet;
use std::sync::Arc;

use miden_client::account::AccountType;
use miden_client::asset::{Asset, FungibleAsset};
use miden_client::auth::RPO_FALCON_SCHEME_ID;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::{NoteType, NoteUpdateTracker};
use miden_client::rpc::NodeRpcClient;
use miden_client::store::{StoreError, TransactionFilter};
use miden_client::testing::common::{
    MINT_AMOUNT,
    TRANSFER_AMOUNT,
    create_test_store_path,
    insert_new_fungible_faucet,
    mint_and_consume,
    mint_note,
    setup_two_wallets_and_faucet,
};
use miden_client::testing::mock::MockRpcApi;
use miden_client::transaction::{
    BatchBuilderError,
    LocalTransactionProver,
    PaymentNoteDescription,
    TransactionRequestBuilder,
    TransactionStoreUpdate,
};
use miden_client::{ClientError, DebugMode};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::Felt;
use miden_protocol::crypto::rand::RandomCoin;
use miden_testing::{Auth, MockChainBuilder, TxContextInput};

use crate::tests::create_test_client;

/// Exercises the mock `submit_proven_batch` path end-to-end: build a real
/// `ProvenBatch` from a proven transaction produced against a `MockChain`, submit it via
/// `MockRpcApi`, and verify the returned block number equals the chain tip. The mock
/// ignores `proposed_batch` and `transaction_inputs`, so we pass a cloned
/// `ProposedBatch` and an empty inputs vector — good enough to exercise the trait wiring.
#[tokio::test]
async fn submit_proven_batch_returns_chain_tip() {
    let (_client, rpc_api, _keystore) = Box::pin(create_test_client()).await;

    // Pick the first account recorded in the prebuilt mock chain.
    let account_id = rpc_api
        .mock_chain
        .read()
        .proven_blocks()
        .iter()
        .flat_map(|block| block.body().updated_accounts())
        .next()
        .unwrap()
        .account_id();

    // Execute and prove a trivial transaction against that account.
    let tx_context = rpc_api
        .mock_chain
        .read()
        .build_tx_context(TxContextInput::AccountId(account_id), &[], &[])
        .unwrap()
        .build()
        .unwrap();
    let executed_tx = Box::pin(tx_context.execute()).await.unwrap();

    let proven_tx = LocalTransactionProver::default().prove_dummy(executed_tx).unwrap();

    // Wrap the proven transaction into a ProvenBatch using MockChain helpers.
    // ProposedBatch is Clone, so we clone it before consuming the original to produce the
    // ProvenBatch.
    let (proven_batch, proposed_for_submit) = {
        let chain = rpc_api.mock_chain.read();
        let proposed_batch = chain.propose_transaction_batch(vec![proven_tx]).unwrap();
        let proposed_for_submit = proposed_batch.clone();
        let proven_batch = chain.prove_transaction_batch(proposed_batch).unwrap();
        (proven_batch, proposed_for_submit)
    };

    let expected_tip = rpc_api.get_chain_tip_block_num();
    let returned = Box::pin(rpc_api.submit_proven_batch(proven_batch, proposed_for_submit, vec![]))
        .await
        .unwrap();

    assert_eq!(returned, expected_tip);
}

/// Build a 2-tx batch on one local account through `Client::new_transaction_batch`,
/// submit it and verify the returned block number matches the mock chain's tip and
/// both transactions land in the local store.
#[tokio::test]
async fn batch_builder_submits_two_txs_on_one_account() {
    let (mut client, rpc_api, _keystore) = Box::pin(create_test_client()).await;

    // Pick the first tracked account in the mock chain (same pattern as the existing test above).
    let account_id = rpc_api
        .mock_chain
        .read()
        .proven_blocks()
        .iter()
        .flat_map(|block| block.body().updated_accounts())
        .next()
        .unwrap()
        .account_id();

    // Retrieve the committed account state from the mock chain and register it with the client
    // store so that `new_transaction_batch` can find it.
    let account = rpc_api.mock_chain.read().committed_account(account_id).unwrap().clone();
    client.add_account(&account, false).await.unwrap();

    // Sync so the client's store reflects the on-chain state.
    client.sync_state().await.unwrap();

    // Build two minimal no-op TransactionRequests for the same account.
    // The mock account uses IncrNonce auth which requires no signing key — a bare
    // TransactionRequestBuilder::new().build() is sufficient.
    let req1 = TransactionRequestBuilder::new().build().unwrap();
    let req2 = TransactionRequestBuilder::new().build().unwrap();

    let block_num = Box::pin(async {
        client
            .new_transaction_batch()
            .push(account_id, req1)
            .await?
            .push(account_id, req2)
            .await?
            .submit()
            .await
    })
    .await
    .expect("batch submit should succeed");

    let expected_tip = rpc_api.get_chain_tip_block_num();
    assert_eq!(block_num, expected_tip);

    // Assert both transactions are in the local store.
    let transactions = client
        .get_transactions(TransactionFilter::All)
        .await
        .expect("transactions fetched");
    assert!(
        transactions.len() >= 2,
        "expected >= 2 transactions in the store after submitting a 2-tx batch, got {}",
        transactions.len()
    );
}

/// Verifies that `Store::apply_transaction_batch` is atomic across the SQL store AND the
/// in-memory `AccountSmtForest`: if any per-tx update in the batch fails, no earlier update
/// is persisted, and a follow-up `Store::update_account` on the affected account still
/// works.
#[tokio::test]
async fn apply_transaction_batch_rolls_back_on_mid_batch_failure() {
    // Build a fresh mock chain with two existing accounts.
    let mut chain_builder = MockChainBuilder::new();
    let account_a = chain_builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
    let account_b = chain_builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
    let a_id = account_a.id();
    let b_id = account_b.id();
    let mock_chain = chain_builder.build().unwrap();

    // Build a client backed by the mock chain.
    let rng =
        RandomCoin::new(rand::random::<[u64; 4]>().map(|v| Felt::new_unchecked(v >> 1)).into());
    let keystore = FilesystemKeyStore::new(std::env::temp_dir()).unwrap();
    let rpc_api = MockRpcApi::new(mock_chain);
    let mut client = ClientBuilder::new()
        .rpc(Arc::new(rpc_api.clone()))
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .in_debug_mode(DebugMode::Enabled)
        .tx_discard_delta(None)
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();

    // Register ONLY account A. Account B stays unknown to the client store, so
    // `smt_forest.get_roots(B)` will return None during `apply_account_delta`.
    client.add_account(&account_a, false).await.unwrap();

    // Execute a trivial transaction against A and another against B, both via the mock chain.
    // Both produce valid `ExecutedTransaction`s; the failure happens only at store-apply time.
    // Build each `TxContext` in its own statement so the mock-chain read guard is dropped
    // before `.execute().await` is reached (otherwise clippy flags await_holding_lock).
    let tx_ctx_a = rpc_api
        .mock_chain
        .read()
        .build_tx_context(TxContextInput::AccountId(a_id), &[], &[])
        .unwrap()
        .build()
        .unwrap();
    let executed_a = Box::pin(tx_ctx_a.execute()).await.unwrap();

    let tx_ctx_b = rpc_api
        .mock_chain
        .read()
        .build_tx_context(TxContextInput::AccountId(b_id), &[], &[])
        .unwrap()
        .build()
        .unwrap();
    let executed_b = Box::pin(tx_ctx_b.execute()).await.unwrap();

    let chain_tip = rpc_api.get_chain_tip_block_num();
    let update_a = TransactionStoreUpdate::new(
        executed_a,
        chain_tip,
        NoteUpdateTracker::default(),
        vec![],
        vec![],
    );
    let update_b = TransactionStoreUpdate::new(
        executed_b,
        chain_tip,
        NoteUpdateTracker::default(),
        vec![],
        vec![],
    );

    // Snapshot A's stored state pre-batch so we can assert it didn't move.
    let a_before = client.get_account(a_id).await.unwrap().expect("A was registered");
    let a_commitment_before = a_before.to_commitment();

    let store = client.test_store().clone();
    let result = store.apply_transaction_batch(vec![update_a, update_b]).await;

    match result {
        Err(StoreError::AccountDataNotFound(id)) if id == b_id => {},
        other => panic!("expected StoreError::AccountDataNotFound({b_id:?}), got {other:?}"),
    }

    // Rollback check: neither update's transaction record is visible.
    let transactions = client.get_transactions(TransactionFilter::All).await.unwrap();
    assert!(
        transactions.is_empty(),
        "expected 0 transactions after atomic rollback, got {}",
        transactions.len()
    );

    // Rollback check: A's commitment is still at the pre-batch value (update_a's final state
    // was not applied).
    let a_after = client.get_account(a_id).await.unwrap().expect("A still registered");
    assert_eq!(
        a_after.to_commitment(),
        a_commitment_before,
        "account A state must be unchanged after atomic rollback"
    );

    // Forest rollback check. `update_account_state` calls `replace_roots`, which asserts
    // that the forest has no staged-but-uncommitted roots for the account. Without the
    // forest rollback, the failed batch above would have left A's previous roots sitting
    // in `pending_old_roots`, and this call would trip the assertion.
    store
        .update_account(&account_a)
        .await
        .expect("update_account on A must succeed after the failed batch was rolled back");
}

/// `BatchBuilder::push` must validate each transaction against the in-batch (stacked)
/// account state, not the persisted pre-batch state — otherwise a tx that depends on
/// state created by a prior push in the same batch is wrongly rejected at validation
/// time even though the executor would accept it.
///
/// Setup: A starts with `MINT_AMOUNT` (consumed, also puts A on-chain). A second
/// mint note also worth `MINT_AMOUNT` is left UNCONSUMED.
///
/// - Push 1 consumes the second note → in-batch balance becomes `2 * MINT_AMOUNT`.
/// - Push 2 sends `MINT_AMOUNT + 1` to B → invalid against pre-batch (`MINT_AMOUNT`) but valid
///   against in-batch (`2 * MINT_AMOUNT`).
#[tokio::test]
async fn batch_builder_push_succeeds_when_balance_depends_on_prior_push() {
    let (mut client, rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await
        .unwrap();

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // Pre-batch: give A `MINT_AMOUNT` (also gets A on-chain so its first
    // batch-tx delta is partial, not full-state).
    mint_and_consume(&mut client, from_account_id, faucet_account_id, NoteType::Private).await;
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Mint a second note worth `MINT_AMOUNT` for A — left UNCONSUMED, so push 1 can claim it.
    let (_mint_tx_id, second_note) =
        mint_note(&mut client, from_account_id, faucet_account_id, NoteType::Private).await;
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Push 1 consumes the second note → in-batch balance = 2 * MINT_AMOUNT.
    let push1 = TransactionRequestBuilder::new().build_consume_notes(vec![second_note]).unwrap();

    // Push 2 sends MINT_AMOUNT + 1 → exceeds pre-batch balance (MINT_AMOUNT)
    // but valid against in-batch balance (2 * MINT_AMOUNT).
    let oversend = FungibleAsset::new(faucet_account_id, MINT_AMOUNT + 1).unwrap();
    let push2 = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(oversend)],
                from_account_id,
                to_account_id,
            ),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    let block_num = Box::pin(async {
        client
            .new_transaction_batch()
            .push(from_account_id, push1)
            .await?
            .push(from_account_id, push2)
            .await?
            .submit()
            .await
    })
    .await
    .expect("submit should succeed because validation uses in-batch state");

    assert!(block_num.as_u32() > 0);
}

/// A later transaction in a batch may touch a vault key that an earlier transaction in the same
/// batch never touched. That key is absent from the earlier transaction's execution advice, so the
/// batch data store must serve its witness by staging the accumulated in-batch delta onto the
/// store's committed Merkle forest — not fail. Regression test for the in-batch "untouched key"
/// witness path.
///
/// Setup: `from` holds a balance of a "held" faucet (committed); a note from a *different*
/// "consumed" faucet is left unconsumed.
/// - Push 1 consumes the note → touches only the consumed faucet's vault key.
/// - Push 2 sends the held asset to `to` → touches the held faucet's vault key, which push 1 never
///   loaded.
#[tokio::test]
async fn batch_builder_serves_witness_for_untouched_vault_key() {
    let (mut client, rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let (from_account, to_account, consumed_faucet) = setup_two_wallets_and_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();

    let from_id = from_account.id();
    let to_id = to_account.id();
    let consumed_faucet_id = consumed_faucet.id();

    // A second, independent faucet whose balance `from` holds but never touches in push 1.
    let (held_faucet, _) = insert_new_fungible_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();
    let held_faucet_id = held_faucet.id();
    client.sync_state().await.unwrap();

    // Give `from` a committed balance of the held faucet. It is part of the committed vault but is
    // NOT touched by the first in-batch transaction.
    mint_and_consume(&mut client, from_id, held_faucet_id, NoteType::Private).await;
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Mint a note from the consumed faucet for `from`, left UNCONSUMED so push 1 can claim it.
    let (_mint_tx_id, consumed_note) =
        mint_note(&mut client, from_id, consumed_faucet_id, NoteType::Private).await;
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Push 1 consumes the note → touches only the consumed faucet's vault key.
    let push1 = TransactionRequestBuilder::new()
        .build_consume_notes(vec![consumed_note])
        .unwrap();

    // Push 2 sends the held asset to `to` → touches the held faucet's vault key, absent from
    // push 1's execution advice.
    let held_asset = FungibleAsset::new(held_faucet_id, TRANSFER_AMOUNT).unwrap();
    let push2 = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(vec![Asset::Fungible(held_asset)], from_id, to_id),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    let block_num = Box::pin(async {
        client
            .new_transaction_batch()
            .push(from_id, push1)
            .await?
            .push(from_id, push2)
            .await?
            .submit()
            .await
    })
    .await
    .expect("submit should succeed: the untouched G vault key is served via the store forest");

    assert!(block_num.as_u32() > 0);
}

/// Verify that submitting an empty batch (no pushes) returns `BatchBuilderError::Empty`.
#[tokio::test]
async fn batch_builder_empty_submit_returns_empty_error() {
    let (client, rpc_api, _keystore) = Box::pin(create_test_client()).await;

    // Pick the first tracked account in the mock chain.
    let _account_id = rpc_api
        .mock_chain
        .read()
        .proven_blocks()
        .iter()
        .flat_map(|block| block.body().updated_accounts())
        .next()
        .unwrap()
        .account_id();

    let batch = client.new_transaction_batch();
    assert_eq!(batch.len(), 0);
    assert!(batch.is_empty());

    let result = batch.submit().await;

    // Verify we got the Empty error variant specifically.
    match result {
        Err(ClientError::BatchBuilder(BatchBuilderError::Empty)) => {},
        other => panic!("expected BatchBuilderError::Empty, got {other:?}"),
    }
}

/// Verify that pushing two transactions that consume the same input note in one batch
/// fails the second push with `BatchBuilderError::DuplicateInputNote(note_id)`.
#[tokio::test]
async fn batch_builder_push_rejects_duplicate_input_note() {
    let (mut client, rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let (first_regular_account, _second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await
        .unwrap();

    let from_account_id = first_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // Get the account on-chain so its first batch-tx delta is partial, not full-state.
    mint_and_consume(&mut client, from_account_id, faucet_account_id, NoteType::Private).await;
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Mint a single note for `from_account` — left UNCONSUMED.
    let (_mint_tx_id, note) =
        mint_note(&mut client, from_account_id, faucet_account_id, NoteType::Private).await;
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let note_id = note.id();

    // Two requests that both reference the SAME note as their input.
    let req1 = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.clone()])
        .unwrap();
    let req2 = TransactionRequestBuilder::new().build_consume_notes(vec![note]).unwrap();

    // First push must succeed; second must fail with DuplicateInputNote(note_id).
    let result = Box::pin(async {
        client
            .new_transaction_batch()
            .push(from_account_id, req1)
            .await?
            .push(from_account_id, req2)
            .await
    })
    .await;

    match result {
        Err(ClientError::BatchBuilder(BatchBuilderError::DuplicateInputNote(id))) => {
            assert_eq!(id, note_id, "DuplicateInputNote should carry the duplicated note id");
        },
        Err(other) => {
            panic!("expected BatchBuilderError::DuplicateInputNote({note_id}), got {other:?}")
        },
        Ok(_) => {
            panic!("expected BatchBuilderError::DuplicateInputNote({note_id}), got Ok(_)")
        },
    }
}

/// Build a 2-account batch (1 tx per account, both pushing trivial no-op `TransactionRequests`)
/// and verify both transactions reach the local store and the returned block number matches
/// the mock chain's tip.
#[tokio::test]
async fn batch_builder_submits_txs_across_multiple_accounts() {
    // Build a fresh mock chain with two existing IncrNonce accounts so we can execute a
    // trivial transaction against each without needing signing keys.
    let mut chain_builder = MockChainBuilder::new();
    let account_a = chain_builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
    let account_b = chain_builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
    let account_id_a = account_a.id();
    let account_id_b = account_b.id();
    let mock_chain = chain_builder.build().unwrap();

    let rng = RandomCoin::new(rand::random::<[u64; 4]>().map(Felt::new_unchecked).into());
    let keystore = FilesystemKeyStore::new(std::env::temp_dir()).unwrap();
    let rpc_api = MockRpcApi::new(mock_chain);
    let mut client = ClientBuilder::new()
        .rpc(Arc::new(rpc_api.clone()))
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .in_debug_mode(DebugMode::Enabled)
        .tx_discard_delta(None)
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();

    // Register both accounts with the client.
    client.add_account(&account_a, false).await.unwrap();
    client.add_account(&account_b, false).await.unwrap();

    client.sync_state().await.unwrap();

    let req_a = TransactionRequestBuilder::new().build().unwrap();
    let req_b = TransactionRequestBuilder::new().build().unwrap();

    let block_num = Box::pin(async {
        client
            .new_transaction_batch()
            .push(account_id_a, req_a)
            .await?
            .push(account_id_b, req_b)
            .await?
            .submit()
            .await
    })
    .await
    .expect("multi-account batch submit should succeed");

    let expected_tip = rpc_api.get_chain_tip_block_num();
    assert_eq!(block_num, expected_tip);

    let transactions = client
        .get_transactions(TransactionFilter::All)
        .await
        .expect("transactions fetched");
    assert!(
        transactions.len() >= 2,
        "expected >= 2 transactions in the store after a 2-account batch, got {}",
        transactions.len()
    );

    let touched: BTreeSet<_> = transactions.iter().map(|tx| tx.details.account_id).collect();
    assert!(touched.contains(&account_id_a), "tx for account A not recorded");
    assert!(touched.contains(&account_id_b), "tx for account B not recorded");
}

/// Verify that pushing a transaction for an account that's not tracked by the client's store
/// fails with `ClientError::AccountDataNotFound`.
#[tokio::test]
async fn batch_builder_push_for_unknown_account_returns_error() {
    let (client, rpc_api, _keystore) = Box::pin(create_test_client()).await;

    // Pick an account that EXISTS on the mock chain but is NOT registered with the client
    // store (we never call `client.add_account` for it).
    let account_id = rpc_api
        .mock_chain
        .read()
        .proven_blocks()
        .iter()
        .flat_map(|block| block.body().updated_accounts())
        .next()
        .unwrap()
        .account_id();

    // Build a no-op request; we never get to submission — the push itself must fail.
    let req = TransactionRequestBuilder::new().build().unwrap();

    match client.new_transaction_batch().push(account_id, req).await {
        Err(ClientError::AccountDataNotFound(id)) => {
            assert_eq!(id, account_id, "AccountDataNotFound should carry the requested id");
        },
        Err(other) => {
            panic!("expected ClientError::AccountDataNotFound({account_id}), got {other:?}")
        },
        Ok(_) => {
            panic!("expected ClientError::AccountDataNotFound({account_id}), got Ok(_)")
        },
    }
}

/// A tx in the batch can consume a note produced by an earlier tx in the same batch when
/// each tx targets a different account. The expected output note is extracted from the
/// producer's `TransactionRequest::expected_output_own_notes` before pushing.
#[tokio::test]
async fn batch_builder_cross_account_note_flow() {
    let (mut client, rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await
        .unwrap();

    let account_id_a = first_regular_account.id();
    let account_id_b = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // Pre-batch: get both A and B on-chain (each with MINT_AMOUNT) so their first batch-tx
    // deltas are partial, not full-state — the batch apply path requires partial deltas.
    mint_and_consume(&mut client, account_id_a, faucet_account_id, NoteType::Private).await;
    rpc_api.prove_block();
    client.sync_state().await.unwrap();
    mint_and_consume(&mut client, account_id_b, faucet_account_id, NoteType::Private).await;
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // tx1 (account A): send MINT_AMOUNT to B via P2ID. Pre-extract the note we expect to
    // create so tx2 can consume it.
    let asset = FungibleAsset::new(faucet_account_id, MINT_AMOUNT).unwrap();
    let req_send = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(vec![Asset::Fungible(asset)], account_id_a, account_id_b),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    let in_batch_note = req_send
        .expected_output_own_notes()
        .pop()
        .expect("pay_to_id should produce exactly one note");

    // tx2 (account B): consume the just-created note.
    let req_consume = TransactionRequestBuilder::new()
        .build_consume_notes(vec![in_batch_note])
        .unwrap();

    let block_num = Box::pin(async {
        client
            .new_transaction_batch()
            .push(account_id_a, req_send)
            .await?
            .push(account_id_b, req_consume)
            .await?
            .submit()
            .await
    })
    .await
    .expect("cross-account in-batch note flow should succeed");

    assert!(block_num.as_u32() > 0, "expected a positive block number");

    let transactions = client
        .get_transactions(TransactionFilter::All)
        .await
        .expect("transactions fetched");
    let touched: BTreeSet<_> = transactions.iter().map(|tx| tx.details.account_id).collect();
    assert!(touched.contains(&account_id_a), "send tx not recorded");
    assert!(touched.contains(&account_id_b), "consume tx not recorded");

    // After the batch: A sent its MINT_AMOUNT → 0. B started with MINT_AMOUNT (pre-batch
    // mint above) and received another MINT_AMOUNT from A → 2 * MINT_AMOUNT.
    let a_balance = client
        .account_reader(account_id_a)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    let b_balance = client
        .account_reader(account_id_b)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    assert_eq!(a_balance, 0, "A should have sent all its balance");
    assert_eq!(b_balance, 2 * MINT_AMOUNT, "B should hold its prior MINT_AMOUNT + A's transfer");
}

/// The duplicate-input-note check is global to the batch: a note consumed by `tx_a` (account A)
/// cannot also appear as an input to `tx_b` (account B). Second push fails with
/// `DuplicateInputNote(note_id)`.
#[tokio::test]
async fn batch_builder_dedup_rejects_duplicate_input_note_across_accounts() {
    let (mut client, rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await
        .unwrap();

    let account_id_a = first_regular_account.id();
    let account_id_b = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // Get account A on-chain so its first batch-tx delta is partial, not full-state.
    mint_and_consume(&mut client, account_id_a, faucet_account_id, NoteType::Private).await;
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Mint a single shared note (created with A's recipient, but we'll try to feed the same
    // note to both pushes).
    let (_mint_tx_id, note) =
        mint_note(&mut client, account_id_a, faucet_account_id, NoteType::Private).await;
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let note_id = note.id();

    let req_a = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.clone()])
        .unwrap();
    let req_b = TransactionRequestBuilder::new().build_consume_notes(vec![note]).unwrap();

    let result = Box::pin(async {
        client
            .new_transaction_batch()
            .push(account_id_a, req_a)
            .await?
            .push(account_id_b, req_b)
            .await
    })
    .await;

    match result {
        Err(ClientError::BatchBuilder(BatchBuilderError::DuplicateInputNote(id))) => {
            assert_eq!(id, note_id, "DuplicateInputNote should carry the duplicated note id");
        },
        Err(other) => {
            panic!("expected BatchBuilderError::DuplicateInputNote({note_id}), got {other:?}")
        },
        Ok(_) => {
            panic!("expected BatchBuilderError::DuplicateInputNote({note_id}), got Ok(_)")
        },
    }
}
