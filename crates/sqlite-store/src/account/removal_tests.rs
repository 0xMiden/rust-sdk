use miden_client::store::TransactionFilter;
use miden_client::sync::{NoteTagRecord, NoteTagSource};
use miden_client::transaction::{
    DiscardCause,
    TransactionDetails,
    TransactionRecord,
    TransactionStatus,
};
use miden_client::{Felt, ONE};
use miden_protocol::block::BlockNumber;
use miden_protocol::note::NoteDetailsCommitment;
use miden_protocol::transaction::{RawOutputNotes, TransactionId};

use super::*;
use crate::transaction::upsert_transaction_record;

#[tokio::test]
async fn remove_account_returns_false_when_absent() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let missing = AccountId::try_from(ACCOUNT_ID_REGULAR)?;
    assert!(!store.remove_account(missing).await?);
    Ok(())
}

#[tokio::test]
async fn remove_account_clears_persisted_state_and_allows_reinsertion() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let slot = StorageSlotName::new("test::removal::map")?;
    let mut account = setup_account_with_map(&store, 2, &slot).await?;
    let account_id = account.id();
    let initial_commitment = account.to_commitment();
    apply_single_entry_update(&store, &mut account, &slot, 2).await?;

    let (other, _) = account_with_upgraded_code()?;
    store
        .insert_account(&other, Address::new(other.id()), ClientAccountType::Native)
        .await?;
    store.track_account_witness(account_id).await?;
    store
        .update_account_witness(
            account_id,
            &mock_account_witness(account_id, account.to_commitment()),
        )
        .await?;

    let tag = Address::new(account_id).to_note_tag();
    store.add_note_tag(NoteTagRecord::with_account_source(tag, account_id)).await?;
    let retained_tags = [
        NoteTagRecord { tag, source: NoteTagSource::User },
        NoteTagRecord::with_note_source(
            tag,
            NoteDetailsCommitment::from_raw_commitments(Word::from([ONE; 4]), EMPTY_WORD),
        ),
        NoteTagRecord {
            tag,
            source: NoteTagSource::Subscription(Word::from([ONE; 4])),
        },
    ];
    for record in retained_tags {
        store.add_note_tag(record).await?;
    }

    assert!(store.remove_account(account_id).await?);
    assert!(!store.remove_account(account_id).await?);
    assert!(store.get_account(account_id).await?.is_none());
    assert!(store.get_account_header_by_commitment(initial_commitment).await?.is_none());
    assert!(store.get_addresses_by_account_id(account_id).await?.is_empty());
    assert!(store.tracked_account_witnesses().await?.is_empty());

    let tags = store.get_note_tags().await?;
    assert!(tags.iter().all(|record| !matches!(record.source, NoteTagSource::Account(id) if id == account_id)));
    assert_eq!(tags.len(), retained_tags.len());
    assert!(retained_tags.iter().all(|record| tags.contains(record)));
    assert_eq!(
        Account::try_from(store.get_account(other.id()).await?.unwrap())?.to_bytes(),
        other.to_bytes(),
    );

    store
        .insert_account(&account, Address::new(account_id), ClientAccountType::Native)
        .await?;
    assert_eq!(
        Account::try_from(store.get_account(account_id).await?.unwrap())?.to_bytes(),
        account.to_bytes(),
    );
    Ok(())
}

fn removal_transaction(account: &Account, status: TransactionStatus) -> TransactionRecord {
    TransactionRecord::new(
        TransactionId::from_raw([ONE; 4].into()),
        TransactionDetails {
            account_id: account.id(),
            init_account_state: account.to_commitment(),
            final_account_state: account.to_commitment(),
            input_note_nullifiers: vec![],
            output_notes: RawOutputNotes::new(vec![]).unwrap(),
            block_num: BlockNumber::from(1u32),
            submission_height: BlockNumber::from(1u32),
            expiration_block_num: BlockNumber::from(2u32),
            creation_timestamp: 0,
        },
        None,
        status,
    )
}

#[tokio::test]
async fn remove_account_rejects_pending_transactions_without_changing_state() -> anyhow::Result<()>
{
    let store = create_test_store().await;
    let slot = StorageSlotName::new("test::removal_pending::map")?;
    let account = setup_account_with_map(&store, 2, &slot).await?;
    let account_id = account.id();
    let mut transaction = removal_transaction(&account, TransactionStatus::Pending);
    let record = transaction.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            upsert_transaction_record(&tx, &record)?;
            tx.commit().into_store_error()
        })
        .await?;
    let forest_before = read_forest_trees(&store).await?;
    assert!(matches!(
        store.remove_account(account_id).await,
        Err(StoreError::AccountHasPendingTransactions(id)) if id == account_id
    ));
    assert_eq!(
        Account::try_from(store.get_account(account_id).await?.unwrap())?.to_bytes(),
        account.to_bytes(),
    );
    assert_eq!(read_forest_trees(&store).await?, forest_before);

    let (other, _) = account_with_upgraded_code()?;
    store
        .insert_account(&other, Address::new(other.id()), ClientAccountType::Native)
        .await?;
    let mut pending = removal_transaction(&other, TransactionStatus::Pending);
    pending.id = TransactionId::from_raw([Felt::from(2u32); 4].into());
    let pending_id = pending.id;
    transaction.status = TransactionStatus::Discarded(DiscardCause::Expired);
    let record = transaction.clone();
    store
        .interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            upsert_transaction_record(&tx, &record)?;
            upsert_transaction_record(&tx, &pending)?;
            tx.commit().into_store_error()
        })
        .await?;
    assert!(store.remove_account(account_id).await?);
    let retained = store.get_transactions(TransactionFilter::All).await?;
    assert_eq!(retained.len(), 2);
    let pending = store.get_transactions(TransactionFilter::Uncommitted).await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, pending_id);
    assert_eq!(pending[0].details.account_id, other.id());
    Ok(())
}

#[tokio::test]
async fn remove_account_rolls_back_when_a_delete_fails() -> anyhow::Result<()> {
    let store = create_test_store().await;
    let slot = StorageSlotName::new("test::removal_rollback::map")?;
    let mut account = setup_account_with_map(&store, 2, &slot).await?;
    let old_commitment = account.to_commitment();
    apply_single_entry_update(&store, &mut account, &slot, 2).await?;
    let forest_before = read_forest_trees(&store).await?;
    store
        .interact_with_connection(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER reject_account_removal BEFORE DELETE ON addresses \
             BEGIN SELECT RAISE(ABORT, 'account removal failed'); END",
            )
            .into_store_error()
        })
        .await?;

    assert!(store.remove_account(account.id()).await.is_err());
    assert_eq!(
        Account::try_from(store.get_account(account.id()).await?.unwrap())?.to_bytes(),
        account.to_bytes(),
    );
    assert!(store.get_account_header_by_commitment(old_commitment).await?.is_some());
    assert_eq!(read_forest_trees(&store).await?, forest_before);
    Ok(())
}

#[tokio::test]
async fn remove_account_collects_only_unreferenced_code() -> anyhow::Result<()> {
    for retain_foreign_code in [false, true] {
        let store = create_test_store().await;
        let (mut account, upgraded_code) = account_with_upgraded_code()?;
        let slot = StorageSlotName::new("test::removal_shared::map")?;
        let other = setup_account_with_map(&store, 2, &slot).await?;
        assert_eq!(account.code().commitment(), other.code().commitment());
        store
            .insert_account(&account, Address::new(account.id()), ClientAccountType::Native)
            .await?;
        let old_commitment = account.to_commitment();
        let previous_header = (&account).into();
        let patch = code_upgrade_patch(&account, upgraded_code.clone())?;
        account.apply_patch(&patch)?;
        apply_patch_in_store(&store, previous_header, (&account).into(), patch).await?;
        if retain_foreign_code {
            store.upsert_foreign_account_code(account.id(), upgraded_code.clone()).await?;
        }

        assert_eq!(count_account_codes(&store).await, 2);
        assert!(store.remove_account(account.id()).await?);
        assert!(store.get_account_header_by_commitment(old_commitment).await?.is_none());
        assert_eq!(count_account_codes(&store).await, 1 + usize::from(retain_foreign_code));
        assert_eq!(
            Account::try_from(store.get_account(other.id()).await?.unwrap())?.to_bytes(),
            other.to_bytes(),
        );
        let foreign = store.get_foreign_account_code(vec![account.id()]).await?;
        assert_eq!(foreign.get(&account.id()), retain_foreign_code.then_some(&upgraded_code));
    }
    Ok(())
}
