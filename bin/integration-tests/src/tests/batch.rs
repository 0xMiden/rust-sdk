use anyhow::{Context, Result};
use miden_client::Felt;
use miden_client::account::AccountType;
use miden_client::asset::{Asset, AssetAmount, FungibleAsset};
use miden_client::auth::RPO_FALCON_SCHEME_ID;
use miden_client::note::NoteType;
use miden_client::store::TransactionFilter;
use miden_client::testing::common::*;
use miden_client::transaction::{
    PaymentNoteDescription,
    TransactionRequestBuilder,
    TransactionStatus,
};
use tracing::info;

use crate::tests::config::ClientConfig;

/// Real-node integration test for the `BatchBuilder` end-to-end path.
///
/// Mints tokens onto a first wallet, then submits two P2ID transfers from that
/// wallet to a second wallet as a single proven batch via
/// `Client::new_transaction_batch`.
///
/// The balance assertion at the end implicitly verifies `InMemoryBatchDataStore`'s
/// account state stacking: if the second push read the pre-batch state instead of
/// the post-push-1 state, both transactions would carry the same
/// `initial_account_state` in their proofs and the node would reject the batch.
/// Successful submission with balance = `MINT_AMOUNT` - 2 * `TRANSFER_AMOUNT` proves
/// that each push saw the state produced by the previous push.
pub async fn test_batch_builder_submits_two_p2id_on_one_account(
    client_config: ClientConfig,
) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;
    wait_for_node(&mut client).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await?;

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // Mint tokens into first_regular_account (covers both transfers).
    let tx_id =
        mint_and_consume(&mut client, from_account_id, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client, tx_id).await?;
    client.sync_state().await.unwrap();

    let nonce_before = client.account_reader(from_account_id).nonce().await?;
    info!(?nonce_before, "Sender nonce before batch");

    // Build two P2ID transfer requests of TRANSFER_AMOUNT each.
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();

    let tx_request_1 = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(asset)],
                from_account_id,
                to_account_id,
            ),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    let tx_request_2 = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(asset)],
                from_account_id,
                to_account_id,
            ),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    info!(
        from = %from_account_id,
        to = %to_account_id,
        amount = TRANSFER_AMOUNT,
        "Submitting 2-tx P2ID batch via BatchBuilder"
    );

    // Submit both requests as a single batch.
    let block_num = client
        .new_transaction_batch()
        .push(from_account_id, tx_request_1)
        .await?
        .push(from_account_id, tx_request_2)
        .await?
        .submit()
        .await?;

    info!(block_num = block_num.as_u32(), "Batch submitted successfully");

    assert!(block_num.as_u32() > 0, "expected a positive block number from batch submit");

    // Poll until at least 3 sender-account transactions are committed (1 from
    // mint-and-consume + 2 from the batch). Give the node a reasonable window
    // to finalize the batch's block.
    let mut committed_count = 0;
    for attempt in 0..30 {
        wait_for_blocks(&mut client, 1).await;
        client.sync_state().await.unwrap();
        let all_transactions = client.get_transactions(TransactionFilter::All).await.unwrap();
        committed_count = all_transactions
            .iter()
            .filter(|tx| tx.details.account_id == from_account_id)
            .filter(|tx| matches!(tx.status, TransactionStatus::Committed { .. }))
            .count();
        info!(attempt, committed_count, "polling for batch txs to commit");
        if committed_count >= 3 {
            break;
        }
    }
    assert!(
        committed_count >= 3,
        "expected at least 3 committed transactions from the sender account \
         (1 mint-and-consume + 2 batch), got {committed_count}"
    );

    // Check that nonce has advanced by exactly 2.
    let nonce_after = client.account_reader(from_account_id).nonce().await?;
    info!(?nonce_before, ?nonce_after, "Sender nonce after batch");
    let expected = nonce_before + Felt::from(2u32);
    assert_eq!(
        nonce_after, expected,
        "sender nonce should advance by exactly 2 after a 2-tx batch \
         (stacking proof: {nonce_before:?} → {nonce_after:?}, expected {expected:?})"
    );

    // check that balance is handled correctly between batch txs
    let sender_balance = client
        .account_reader(from_account_id)
        .get_balance(faucet_account_id)
        .await
        .context("failed to find sender account after transactions")?;

    assert_eq!(
        sender_balance,
        AssetAmount::new(MINT_AMOUNT - (TRANSFER_AMOUNT * 2)).unwrap(),
        "sender balance should have decreased by exactly 2 * TRANSFER_AMOUNT — this proves \
         BatchBuilder stacked account state correctly between pushes"
    );

    Ok(())
}

/// Real-node integration test for in-batch cross-account note flow.
///
/// Mints `MINT_AMOUNT` tokens to wallet A AND `MINT_AMOUNT` to wallet B (both pre-batch,
/// so each account's first batch-tx delta is partial rather than full-state — required by
/// the batch apply path). Then submits a batch with two pushes:
/// - tx1 (A → B): transfer `TRANSFER_AMOUNT` via P2ID.
/// - tx2 (B): consume the just-created P2ID note.
///
/// Asserts both transactions commit, A's balance is `MINT_AMOUNT - TRANSFER_AMOUNT`, B's
/// balance is `MINT_AMOUNT + TRANSFER_AMOUNT`, and both accounts' nonces advanced by
/// exactly 1 during the batch.
pub async fn test_batch_builder_multiple_accounts(client_config: ClientConfig) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;
    wait_for_node(&mut client).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await?;

    let account_id_a = first_regular_account.id();
    let account_id_b = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // Pre-batch: get BOTH A and B on-chain (each with MINT_AMOUNT) so their first batch-tx
    // deltas are partial, not full-state. The batch apply path requires partial deltas.
    let tx_id_a =
        mint_and_consume(&mut client, account_id_a, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client, tx_id_a).await?;
    let tx_id_b =
        mint_and_consume(&mut client, account_id_b, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client, tx_id_b).await?;
    client.sync_state().await.unwrap();

    let nonce_a_before = client.account_reader(account_id_a).nonce().await?;
    let nonce_b_before = client.account_reader(account_id_b).nonce().await?;
    info!(?nonce_a_before, ?nonce_b_before, "Nonces before cross-account batch");

    // Build tx1: A → B for TRANSFER_AMOUNT.
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();
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

    // Build tx2: B consumes the just-created note.
    let req_consume = TransactionRequestBuilder::new()
        .build_consume_notes(vec![in_batch_note])
        .unwrap();

    info!(
        from = %account_id_a,
        to = %account_id_b,
        amount = TRANSFER_AMOUNT,
        "Submitting cross-account batch (A→B P2ID + B consume)"
    );

    let block_num = client
        .new_transaction_batch()
        .push(account_id_a, req_send)
        .await?
        .push(account_id_b, req_consume)
        .await?
        .submit()
        .await?;

    info!(block_num = block_num.as_u32(), "Cross-account batch submitted");
    assert!(block_num.as_u32() > 0, "expected a positive block number");

    // Poll until both txs are committed.
    let mut a_committed = 0;
    let mut b_committed = 0;
    for attempt in 0..30 {
        wait_for_blocks(&mut client, 1).await;
        client.sync_state().await.unwrap();
        let all_transactions = client.get_transactions(TransactionFilter::All).await.unwrap();
        a_committed = all_transactions
            .iter()
            .filter(|tx| tx.details.account_id == account_id_a)
            .filter(|tx| matches!(tx.status, TransactionStatus::Committed { .. }))
            .count();
        b_committed = all_transactions
            .iter()
            .filter(|tx| tx.details.account_id == account_id_b)
            .filter(|tx| matches!(tx.status, TransactionStatus::Committed { .. }))
            .count();
        info!(attempt, a_committed, b_committed, "polling for cross-account batch txs");
        // A needs ≥ 2 commits (mint-and-consume + batch send); B needs ≥ 2 (mint-and-consume +
        // batch consume).
        if a_committed >= 2 && b_committed >= 2 {
            break;
        }
    }
    assert!(a_committed >= 2, "expected ≥ 2 committed txs for A, got {a_committed}");
    assert!(b_committed >= 2, "expected ≥ 2 committed txs for B, got {b_committed}");

    let nonce_a_after = client.account_reader(account_id_a).nonce().await?;
    let nonce_b_after = client.account_reader(account_id_b).nonce().await?;
    assert_eq!(
        nonce_a_after,
        nonce_a_before + Felt::from(1u32),
        "A's nonce should advance by exactly 1 (one batch tx)"
    );
    assert_eq!(
        nonce_b_after,
        nonce_b_before + Felt::from(1u32),
        "B's nonce should advance by exactly 1 (one batch tx)"
    );

    let a_balance = client
        .account_reader(account_id_a)
        .get_balance(faucet_account_id)
        .await
        .context("failed to find A's balance after batch")?;
    let b_balance = client
        .account_reader(account_id_b)
        .get_balance(faucet_account_id)
        .await
        .context("failed to find B's balance after batch")?;

    assert_eq!(
        a_balance,
        AssetAmount::new(MINT_AMOUNT - TRANSFER_AMOUNT).unwrap(),
        "A's balance should be MINT_AMOUNT - TRANSFER_AMOUNT after sending"
    );
    assert_eq!(
        b_balance,
        AssetAmount::new(MINT_AMOUNT + TRANSFER_AMOUNT).unwrap(),
        "B's balance should be MINT_AMOUNT + TRANSFER_AMOUNT after consuming the in-batch note"
    );

    Ok(())
}

/// Integration test for the A → B → A interleaved push path.
///
/// Pre-mints `MINT_AMOUNT` to both A and B so each has on-chain state. Then submits a
/// 3-tx batch with pushes in order `A → B`, `B → A`, `A → B`. The middle B push forces
/// `InMemoryBatchDataStore` to handle a non-A push between two A pushes. The third push must read
/// A's cached post-push-1 state, not re-fetch from the store, otherwise its `initial_account_state`
/// would not match the chain produced by push 1 and the node would reject the batch.
///
/// Asserts A advances by 2 nonces and B by 1, A's balance reflects two outbound notes,
/// and B's reflects one outbound note (all output notes remain pending consumption).
pub async fn test_batch_builder_interleaved_pushes(client_config: ClientConfig) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;
    wait_for_node(&mut client).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await?;

    let account_id_a = first_regular_account.id();
    let account_id_b = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // Pre-batch: fund both A and B on-chain so their first batch-tx deltas are partial.
    let tx_id_a =
        mint_and_consume(&mut client, account_id_a, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client, tx_id_a).await?;
    let tx_id_b =
        mint_and_consume(&mut client, account_id_b, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client, tx_id_b).await?;
    client.sync_state().await.unwrap();

    let nonce_a_before = client.account_reader(account_id_a).nonce().await?;
    let nonce_b_before = client.account_reader(account_id_b).nonce().await?;
    info!(?nonce_a_before, ?nonce_b_before, "Nonces before interleaved batch");

    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();

    let req_a_to_b_first = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(vec![Asset::Fungible(asset)], account_id_a, account_id_b),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    let req_b_to_a = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(vec![Asset::Fungible(asset)], account_id_b, account_id_a),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    let req_a_to_b_second = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(vec![Asset::Fungible(asset)], account_id_a, account_id_b),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    info!("Submitting A→B→A interleaved batch");

    let block_num = client
        .new_transaction_batch()
        .push(account_id_a, req_a_to_b_first)
        .await?
        .push(account_id_b, req_b_to_a)
        .await?
        .push(account_id_a, req_a_to_b_second)
        .await?
        .submit()
        .await?;

    info!(block_num = block_num.as_u32(), "Interleaved batch submitted");
    assert!(block_num.as_u32() > 0, "expected a positive block number");

    // Poll until both accounts have their batch txs committed (A: mint+consume + 2 batch = 3,
    // B: mint+consume + 1 batch = 2).
    let mut a_committed = 0;
    let mut b_committed = 0;
    for attempt in 0..30 {
        wait_for_blocks(&mut client, 1).await;
        client.sync_state().await.unwrap();
        let all_transactions = client.get_transactions(TransactionFilter::All).await.unwrap();
        a_committed = all_transactions
            .iter()
            .filter(|tx| tx.details.account_id == account_id_a)
            .filter(|tx| matches!(tx.status, TransactionStatus::Committed { .. }))
            .count();
        b_committed = all_transactions
            .iter()
            .filter(|tx| tx.details.account_id == account_id_b)
            .filter(|tx| matches!(tx.status, TransactionStatus::Committed { .. }))
            .count();
        info!(attempt, a_committed, b_committed, "polling for interleaved batch txs");
        if a_committed >= 3 && b_committed >= 2 {
            break;
        }
    }
    assert!(a_committed >= 3, "expected ≥ 3 committed txs for A, got {a_committed}");
    assert!(b_committed >= 2, "expected ≥ 2 committed txs for B, got {b_committed}");

    let nonce_a_after = client.account_reader(account_id_a).nonce().await?;
    let nonce_b_after = client.account_reader(account_id_b).nonce().await?;
    assert_eq!(
        nonce_a_after,
        nonce_a_before + Felt::from(2u32),
        "A's nonce should advance by exactly 2 — proves A's cached state was reused on the \
         third push instead of re-fetched from the store"
    );
    assert_eq!(
        nonce_b_after,
        nonce_b_before + Felt::from(1u32),
        "B's nonce should advance by exactly 1 (one batch tx)"
    );

    let a_balance = client
        .account_reader(account_id_a)
        .get_balance(faucet_account_id)
        .await
        .context("failed to find A's balance after batch")?;
    let b_balance = client
        .account_reader(account_id_b)
        .get_balance(faucet_account_id)
        .await
        .context("failed to find B's balance after batch")?;

    assert_eq!(
        a_balance,
        AssetAmount::new(MINT_AMOUNT - (TRANSFER_AMOUNT * 2)).unwrap(),
        "A's balance should reflect two outbound P2ID notes"
    );
    assert_eq!(
        b_balance,
        AssetAmount::new(MINT_AMOUNT - TRANSFER_AMOUNT).unwrap(),
        "B's balance should reflect one outbound P2ID note"
    );

    Ok(())
}
