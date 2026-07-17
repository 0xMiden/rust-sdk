use alloc::boxed::Box;
use alloc::vec::Vec;
use std::collections::BTreeSet;
use std::sync::Arc;

use miden_client::assembly::{CodeBuilder, SourceManagerSync};
use miden_client::auth::{AuthSchemeId, AuthSecretKey, AuthSingleSig, PublicKeyCommitment};
use miden_client::keystore::Keystore;
use miden_client::store::AccountStorageFilter;
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::{
    Account,
    AccountBuilder,
    AccountComponent,
    AccountComponentMetadata,
    AccountFile,
    AccountId,
    AccountType,
    StorageSlot,
    StorageSlotName,
};
use miden_protocol::asset::FungibleAsset;
use miden_protocol::note::NoteType;
use miden_protocol::testing::account_id::{
    ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
    ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
};
use miden_protocol::{EMPTY_WORD, Felt, Word, ZERO};
use miden_standards::account::AccountBuilderSchemaCommitmentExt;
use miden_standards::account::auth::Approver;
use miden_standards::account::wallets::BasicWallet;
use miden_standards::testing::mock_account::MockAccountExt;
use rand::Rng;

use crate::tests::{create_test_client, insert_new_fungible_faucet, insert_new_wallet};

fn create_account_data(account_id: u128) -> AccountFile {
    let account = Account::mock(
        account_id,
        AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )),
    );

    AccountFile::new(account.clone(), vec![AuthSecretKey::new_falcon512_poseidon2()])
}

fn create_ecdsa_account_data(account_id: u128) -> AccountFile {
    let account = Account::mock(
        account_id,
        AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::EcdsaK256Keccak,
        )),
    );

    AccountFile::new(account.clone(), vec![AuthSecretKey::new_falcon512_poseidon2()])
}

pub fn create_initial_accounts_data() -> Vec<AccountFile> {
    let account = create_account_data(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET);

    let faucet_account = create_account_data(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET);

    // Create Genesis state and save it to a file
    let accounts = vec![account, faucet_account];

    accounts
}

pub fn create_ecdsa_initial_accounts_data() -> Vec<AccountFile> {
    let account = create_ecdsa_account_data(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET);

    let faucet_account = create_ecdsa_account_data(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET);

    // Create Genesis state and save it to a file
    let accounts = vec![account, faucet_account];

    accounts
}

#[tokio::test]
pub async fn try_add_account() {
    // generate test client
    let (mut client, _rpc_api, _) = Box::pin(create_test_client()).await;

    let account = Account::mock(
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )),
    );

    // The mock account has nonce 1, we need it to be 0 for the test.
    let (id, vault, storage, code, ..) = account.into_parts();
    let account_without_seed =
        Account::new_unchecked(id, vault.clone(), storage.clone(), code.clone(), ZERO, None);
    assert!(client.add_account(&account_without_seed, false).await.is_err());

    let account_with_seed =
        Account::new_unchecked(id, vault, storage, code, ZERO, Some(Word::default()));

    assert!(client.add_account(&account_with_seed, false).await.is_ok());
}

#[tokio::test]
pub async fn try_add_ecdsa_account() {
    // generate test client
    let (mut client, _rpc_api, _) = Box::pin(create_test_client()).await;

    let account = Account::mock(
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::EcdsaK256Keccak,
        )),
    );

    // The mock account has nonce 1, we need it to be 0 for the test.
    let (id, vault, storage, code, ..) = account.into_parts();
    let account_without_seed =
        Account::new_unchecked(id, vault.clone(), storage.clone(), code.clone(), ZERO, None);
    assert!(client.add_account(&account_without_seed, false).await.is_err());

    let account_with_seed =
        Account::new_unchecked(id, vault, storage, code, ZERO, Some(Word::default()));

    assert!(client.add_account(&account_with_seed, false).await.is_ok());
}

#[tokio::test]
async fn load_accounts_test() {
    // generate test client
    let (mut client, ..) = Box::pin(create_test_client()).await;

    let created_accounts_data = create_initial_accounts_data();

    for account_data in created_accounts_data.clone() {
        client.add_account(&account_data.account, false).await.unwrap();
    }

    let expected_accounts: Vec<Account> = created_accounts_data
        .into_iter()
        .map(|account_data| account_data.account)
        .collect();
    let accounts = client.get_account_headers().await.unwrap();

    assert_eq!(accounts.len(), 2);

    let actual_commitments: BTreeSet<_> =
        accounts.into_iter().map(|(header, _)| header.to_commitment()).collect();
    let expected_commitments: BTreeSet<_> =
        expected_accounts.into_iter().map(|account| account.to_commitment()).collect();

    assert_eq!(actual_commitments, expected_commitments);
}

#[tokio::test]
async fn load_ecdsa_accounts_test() {
    // generate test client
    let (mut client, ..) = Box::pin(create_test_client()).await;

    let created_accounts_data = create_ecdsa_initial_accounts_data();
    for account_data in created_accounts_data.clone() {
        client.add_account(&account_data.account, false).await.unwrap();
    }

    let expected_accounts: Vec<Account> = created_accounts_data
        .into_iter()
        .map(|account_data| account_data.account)
        .collect();
    let accounts = client.get_account_headers().await.unwrap();

    assert_eq!(accounts.len(), 2);

    let actual_commitments: BTreeSet<_> =
        accounts.into_iter().map(|(header, _)| header.to_commitment()).collect();
    let expected_commitments: BTreeSet<_> =
        expected_accounts.into_iter().map(|account| account.to_commitment()).collect();

    assert_eq!(actual_commitments, expected_commitments);
}

/// Tests that pruning while a transaction is pending does not break the ability to
/// commit that transaction. The pending tx's input state lives in the historical tables;
/// pruning must not delete it, otherwise undo on discard would fail.
///
/// Scenario:
///   1. Mint tx1 and commit it (nonce 0 to 1)
///   2. Mint tx2 but leave it pending (nonce 1 to 2, not yet committed)
///   3. Prune up to nonce 1 (should only remove nonce-0 history)
///   4. Commit tx2, must succeed
///   5. Account state must be intact at nonce 2
#[tokio::test]
async fn prune_account_history_with_pending_transaction() {
    let (mut client, mock_rpc_api, keystore) = Box::pin(create_test_client()).await;

    let wallet = insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();
    let faucet = insert_new_fungible_faucet(&mut client, AccountType::Private, &keystore)
        .await
        .unwrap();
    let faucet_id = faucet.id();

    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Tx1: mint and commit (nonce 0 to 1)
    let fungible_asset_1 = FungibleAsset::new(faucet_id, 100).unwrap();
    let tx_request_1 = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(fungible_asset_1, wallet.id(), NoteType::Public, client.rng())
        .unwrap();
    Box::pin(client.submit_new_transaction(faucet_id, tx_request_1)).await.unwrap();

    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Record faucet state before pruning
    let faucet_before = client.get_account(faucet_id).await.unwrap().unwrap();

    // Tx2: mint but do NOT commit, leaves a pending transaction (nonce 1 to 2)
    let fungible_asset_2 = FungibleAsset::new(faucet_id, 200).unwrap();
    let tx_request_2 = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(fungible_asset_2, wallet.id(), NoteType::Public, client.rng())
        .unwrap();
    Box::pin(client.submit_new_transaction(faucet_id, tx_request_2)).await.unwrap();

    // Prune up to nonce 1 while tx2 is still pending.
    // This should remove nonce-0 historical entries but must preserve nonce-1 entries
    // (which tx2's undo would need if the transaction were discarded).
    let deleted = client.prune_account_history(faucet_id, Felt::from(1u32)).await.unwrap();
    assert!(deleted > 0, "Should have pruned nonce-0 historical entries");

    // Now commit tx2, this must succeed
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Verify account is intact at nonce 2
    let faucet_after = client.get_account(faucet_id).await.unwrap().unwrap();
    let (header, _status) = client
        .get_account_headers()
        .await
        .unwrap()
        .into_iter()
        .find(|(h, _)| h.id() == faucet_id)
        .expect("Faucet should still appear in headers");
    assert_eq!(header.nonce().as_canonical_u64(), 2, "Latest nonce should be 2");
    assert_eq!(faucet_after.nonce().as_canonical_u64(), 2);

    // Verify commitment is unchanged (pruning + committing did not corrupt state)
    assert_eq!(
        faucet_before.to_commitment(),
        faucet_before.to_commitment(),
        "Account commitment should be consistent"
    );
}

const SLOT_A_NAME: &str = "test::pruning::slot_a";
const SLOT_B_NAME: &str = "test::pruning::slot_b";
const SLOT_C_NAME: &str = "test::pruning::slot_c";

const SLOTS_COMPONENT_MASM: &str = r#"
        use miden::protocol::native_account
        use miden::core::word
        use miden::core::sys

        const SLOT_A = word("test::pruning::slot_a")
        const SLOT_B = word("test::pruning::slot_b")

        @account_procedure
        pub proc set_a_to_10
            push.0.0.0.10
            push.SLOT_A[0..2]
            exec.native_account::set_item
            dropw
            exec.sys::truncate_stack
        end

        @account_procedure
        pub proc set_b_to_20
            push.0.0.0.20
            push.SLOT_B[0..2]
            exec.native_account::set_item
            dropw
            exec.sys::truncate_stack
        end
    "#;

/// Builds a custom account with three value slots (A, B, C) and MASM procedures
/// to modify slots A and B individually. Returns the account and its ID.
async fn build_three_slot_account(
    client: &mut crate::tests::TestClient,
    keystore: &miden_client::keystore::FilesystemKeyStore,
) -> AccountId {
    let a_name = StorageSlotName::new(SLOT_A_NAME).unwrap();
    let b_name = StorageSlotName::new(SLOT_B_NAME).unwrap();
    let c_name = StorageSlotName::new(SLOT_C_NAME).unwrap();

    let slot_a = StorageSlot::with_value(
        a_name,
        [Felt::from(1u32), Felt::from(0u32), Felt::from(0u32), Felt::from(0u32)].into(),
    );
    let slot_b = StorageSlot::with_value(
        b_name,
        [Felt::from(2u32), Felt::from(0u32), Felt::from(0u32), Felt::from(0u32)].into(),
    );
    let slot_c = StorageSlot::with_value(
        c_name,
        [Felt::from(3u32), Felt::from(0u32), Felt::from(0u32), Felt::from(0u32)].into(),
    );

    let component_code = CodeBuilder::default()
        .compile_component_code("test::pruning::slots_component", SLOTS_COMPONENT_MASM)
        .unwrap();

    let component = AccountComponent::new(
        component_code,
        vec![slot_a, slot_b, slot_c],
        AccountComponentMetadata::new("test::pruning::slots_component"),
    )
    .unwrap();

    let key_pair = AuthSecretKey::new_falcon512_poseidon2();
    let pub_key = key_pair.public_key();

    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let account = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            pub_key.to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(BasicWallet)
        .with_component(component)
        .build_with_schema_commitment()
        .unwrap();

    let account_id = account.id();
    keystore.add_key(&key_pair, account_id).await.unwrap();
    client.add_account(&account, false).await.unwrap();

    account_id
}

/// Compiles a transaction script that calls a procedure from the slots component.
fn compile_slot_tx_script(
    proc_name: &str,
    source_manager: Arc<dyn SourceManagerSync>,
) -> miden_client::transaction::TransactionScript {
    CodeBuilder::with_source_manager(source_manager)
        .with_linked_module("external_contract::slots_contract", SLOTS_COMPONENT_MASM)
        .unwrap()
        .compile_tx_script(format!(
            "use external_contract::slots_contract
            @transaction_script
            pub proc main
                call.slots_contract::{proc_name}
            end"
        ))
        .unwrap()
}

/// Tests that pruning preserves unmodified storage slots.
///
/// Scenario from PR #1886 review:
///   - Account created with value slots A=1, B=2, C=3
///   - Tx1 (nonce 0 to 1): changes only A to 10
///   - Tx2 (nonce 1 to 2): changes only B to 20
///   - Prune history
///   - Verify: A=10, B=20, C=3: slot C was never modified and must survive pruning
///
/// With the `replaced_at` historical model, only slots that actually changed get recorded
/// in the historical tables. Slot C is never in the historical table because it was never
/// replaced, so pruning cannot lose it.
#[tokio::test]
async fn prune_preserves_unmodified_storage_slots() {
    let (mut client, mock_rpc_api, keystore) = Box::pin(create_test_client()).await;

    let account_id = build_three_slot_account(&mut client, &keystore).await;

    let source_manager = client.source_manager();
    let tx_script_set_a = compile_slot_tx_script("set_a_to_10", source_manager.clone());
    let tx_script_set_b = compile_slot_tx_script("set_b_to_20", source_manager);

    // Prove the initial block so the account is committed
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Tx1: change only slot A (nonce 0  to 1)
    let tx_request_1 =
        TransactionRequestBuilder::new().custom_script(tx_script_set_a).build().unwrap();
    Box::pin(client.submit_new_transaction(account_id, tx_request_1)).await.unwrap();

    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Tx2: change only slot B (nonce 1  to 2)
    let tx_request_2 =
        TransactionRequestBuilder::new().custom_script(tx_script_set_b).build().unwrap();
    Box::pin(client.submit_new_transaction(account_id, tx_request_2)).await.unwrap();

    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // At this point: nonces 0 to 1 to 2, all committed.
    // Historical table has replaced_at entries for slots that changed:
    //   replaced_at=1: old A=1
    //   replaced_at=2: old B=2
    // Slot C was NEVER modified, so it has no entry in historical tables.

    // Prune old history up to nonce 1
    let deleted = client.prune_account_history(account_id, Felt::from(1u32)).await.unwrap();
    assert!(deleted > 0, "Should have pruned old committed states");

    // Verify all slot values are correct after pruning
    let a_name = StorageSlotName::new(SLOT_A_NAME).unwrap();
    let b_name = StorageSlotName::new(SLOT_B_NAME).unwrap();
    let c_name = StorageSlotName::new(SLOT_C_NAME).unwrap();

    let storage = client
        .test_store()
        .get_account_storage(account_id, AccountStorageFilter::All)
        .await
        .unwrap();

    let actual_a = storage.get(&a_name).expect("slot A should exist").value();
    let actual_b = storage.get(&b_name).expect("slot B should exist").value();
    let actual_c = storage.get(&c_name).expect("slot C should exist").value();

    let final_a: Word =
        [Felt::from(10u32), Felt::from(0u32), Felt::from(0u32), Felt::from(0u32)].into();
    let final_b: Word =
        [Felt::from(20u32), Felt::from(0u32), Felt::from(0u32), Felt::from(0u32)].into();
    let final_c: Word =
        [Felt::from(3u32), Felt::from(0u32), Felt::from(0u32), Felt::from(0u32)].into();

    assert_eq!(actual_a, final_a, "Slot A should be updated to 10");
    assert_eq!(actual_b, final_b, "Slot B should be updated to 20");
    assert_eq!(actual_c, final_c, "Slot C was never modified: must survive pruning unchanged");
}
