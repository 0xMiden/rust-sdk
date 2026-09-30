use alloc::boxed::Box;
use alloc::sync::Arc;
use std::env::temp_dir;

use miden_client::ClientError;
use miden_client::account::component::{
    AccessControl,
    AccountComponentMetadata,
    Authority,
    BasicWallet,
    UpgradeManager,
};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountCode,
    AccountCodeUpgrade,
    AccountComponent,
    AccountId,
    AccountType,
    StorageMapKey,
    StorageSlot,
    StorageSlotName,
};
use miden_client::assembly::CodeBuilder;
use miden_client::asset::{Asset, FungibleAsset};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::{Note, NoteType};
use miden_client::testing::common::create_test_store_path;
use miden_client::testing::mock::{MockClient, MockRpcApi};
use miden_client::transaction::{
    PaymentNoteDescription,
    TransactionRequest,
    TransactionRequestBuilder,
    TransactionScript,
};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::crypto::rand::RandomCoin;
use miden_protocol::testing::account_id::{
    ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
    ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
};
use miden_protocol::{Felt, Word};
use miden_testing::{Auth, MockChainBuilder};
use rstest::rstest;

use crate::tests::seed_mock_transaction_encryption_key;

// HELPERS
// ================================================================================================

const STATE_MODULE: &str = "miden::testing::upgrade_state";

fn state_value_slot() -> StorageSlotName {
    StorageSlotName::new("miden::testing::upgrade_state::value").unwrap()
}

fn state_map_slot() -> StorageSlotName {
    StorageSlotName::new("miden::testing::upgrade_state::map").unwrap()
}

fn state_map_key() -> StorageMapKey {
    StorageMapKey::new(Word::from([1u32, 0, 0, 0]))
}

/// Returns the code of a component whose `bump` procedure increments a value slot and a map entry.
fn state_component_code() -> String {
    format!(
        r#"
        use miden::protocol::active_account
        use miden::protocol::native_account
        use miden::core::sys

        const VALUE_SLOT = word("miden::testing::upgrade_state::value")
        const MAP_SLOT = word("miden::testing::upgrade_state::map")

        @account_procedure
        pub proc bump
            push.VALUE_SLOT[0..2] exec.active_account::get_item
            add.1
            push.VALUE_SLOT[0..2] exec.native_account::set_item
            dropw
            # => []

            push.{map_key} push.MAP_SLOT[0..2] exec.active_account::get_map_item
            add.1
            push.{map_key} push.MAP_SLOT[0..2] exec.native_account::set_map_item
            exec.sys::truncate_stack
        end
        "#,
        map_key = Word::from(state_map_key()),
    )
}

/// Returns the components of an upgradeable account, without a wallet.
///
/// The auth component does not need signatures, so the client can execute transactions without
/// keys. `Authority::AuthControlled` lets the auth component alone authorize an upgrade. The state
/// component lets a transaction change the storage together with the code.
fn upgradeable_components() -> Vec<AccountComponent> {
    let state_component = AccountComponent::new(
        CodeBuilder::default()
            .compile_component_code(STATE_MODULE, state_component_code())
            .unwrap(),
        vec![
            StorageSlot::with_empty_value(state_value_slot()),
            StorageSlot::with_empty_map(state_map_slot()),
        ],
        AccountComponentMetadata::new(STATE_MODULE),
    )
    .unwrap();

    let (mut components, _) = Auth::IncrNonce.build_components();
    components.push(Authority::AuthControlled.into());
    components.push(UpgradeManager.into());
    components.push(state_component);
    components
}

/// Returns a deployed public account that holds one asset but has no wallet, and the code that adds
/// [`BasicWallet`] to it.
fn upgradeable_account() -> (Account, AccountCode) {
    let asset: Asset =
        FungibleAsset::new(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET.try_into().unwrap(), 100)
            .unwrap()
            .into();

    let account = upgradeable_account_builder().with_assets([asset]).build_existing().unwrap();

    let mut upgraded_components = upgradeable_components();
    upgraded_components.push(BasicWallet.into());
    let upgraded_code = AccountCode::from_components(&upgraded_components).unwrap();
    assert_ne!(account.code().commitment(), upgraded_code.commitment());

    (account, upgraded_code)
}

/// Returns a new public account that has the components of [`upgradeable_account`] but is not
/// deployed yet.
fn new_upgradeable_account() -> Account {
    let account = upgradeable_account_builder().build().unwrap();
    assert!(account.is_new());
    account
}

fn upgradeable_account_builder() -> AccountBuilder {
    AccountBuilder::new([3; 32])
        .account_type(AccountType::Public)
        .with_components(upgradeable_components())
}

/// Returns a mock RPC API over a chain that contains `account`.
fn rpc_api_with(account: &Account) -> MockRpcApi {
    rpc_api_with_accounts(&[account])
}

/// Returns a mock RPC API over a chain that contains `accounts`.
fn rpc_api_with_accounts(accounts: &[&Account]) -> MockRpcApi {
    let mut builder = MockChainBuilder::new();
    for account in accounts {
        builder.add_account((*account).clone()).unwrap();
    }
    MockRpcApi::new(builder.build().unwrap())
}

/// Returns a client over `rpc_api` that tracks `account`.
async fn client_tracking(rpc_api: MockRpcApi, account: &Account) -> MockClient<FilesystemKeyStore> {
    let rng =
        RandomCoin::new(rand::random::<[u64; 4]>().map(|v| Felt::new_unchecked(v >> 1)).into());
    let mut client = ClientBuilder::new()
        .rpc(Arc::new(rpc_api))
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(FilesystemKeyStore::new(temp_dir()).unwrap()))
        .tx_discard_delta(None)
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;
    client.add_account(account, false).await.unwrap();
    client
}

/// Returns a script that initializes each upgrade in `upgrades`, given as the new code commitment
/// and the storage upgrade commitment. If `bump_state` is set, the script then also changes the
/// storage of the account.
fn upgrade_script(
    client: &MockClient<FilesystemKeyStore>,
    upgrades: &[(Word, Word)],
    bump_state: bool,
) -> TransactionScript {
    let upgrade_calls = upgrades
        .iter()
        .map(|(new_code_commitment, storage_upgrade_commitment)| {
            format!(
                "push.{storage_upgrade_commitment} push.{new_code_commitment}
                # => [NEW_CODE_COMMITMENT, STORAGE_UPGRADE_COMMITMENT]
                call.account_upgrade::upgrade
                dropw dropw"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let bump_call = if bump_state { "call.upgrade_state::bump" } else { "" };

    client
        .code_builder()
        .with_linked_module(STATE_MODULE, state_component_code())
        .unwrap()
        .compile_tx_script(format!(
            "use miden::standards::account_upgrade
            use miden::testing::upgrade_state

            @transaction_script
            pub proc main
                {upgrade_calls}
                {bump_call}
            end"
        ))
        .unwrap()
}

/// Returns a request with the script of [`upgrade_script`]. `advice_entry` is the advice map entry
/// that gives the new code to the transaction, if any.
fn upgrade_request(
    client: &MockClient<FilesystemKeyStore>,
    upgrades: &[(Word, Word)],
    bump_state: bool,
    advice_entry: Option<(Word, Vec<Felt>)>,
) -> TransactionRequest {
    TransactionRequestBuilder::new()
        .custom_script(upgrade_script(client, upgrades, bump_state))
        .extend_advice_map(advice_entry)
        .build()
        .unwrap()
}

/// Returns a request that upgrades the account code to `code`. If `bump_state` is set, the
/// transaction also changes the storage of the account, so the request needs a custom script.
fn valid_upgrade_request(
    client: &MockClient<FilesystemKeyStore>,
    code: &AccountCode,
    bump_state: bool,
) -> TransactionRequest {
    if !bump_state {
        return TransactionRequestBuilder::new()
            .build_account_code_upgrade(code.clone())
            .unwrap();
    }

    TransactionRequestBuilder::new()
        .custom_script(upgrade_script(client, &[(code.commitment(), Word::empty())], true))
        .account_code_upgrade(code.clone())
        .build()
        .unwrap()
}

/// Returns a deployed public account with a basic wallet. The auth component does not need
/// signatures.
fn wallet_account(seed: u8) -> Account {
    let (mut components, _) = Auth::IncrNonce.build_components();
    components.push(BasicWallet.into());
    AccountBuilder::new([seed; 32])
        .account_type(AccountType::Public)
        .with_components(components)
        .build_existing()
        .unwrap()
}

/// Returns a deployed public account that `owner` owns through `Ownable2Step`, and the code that
/// adds [`BasicWallet`] to it. The `Authority` of the account only accepts an upgrade from `owner`.
fn owned_upgradeable_account(owner: AccountId) -> (Account, AccountCode) {
    let components = || {
        let (mut components, _) = Auth::IncrNonce.build_components();
        components.extend(AccessControl::Ownable2Step { owner });
        components.push(UpgradeManager.into());
        components
    };

    let account = AccountBuilder::new([5; 32])
        .account_type(AccountType::Public)
        .with_components(components())
        .build_existing()
        .unwrap();

    let mut upgraded_components = components();
    upgraded_components.push(BasicWallet.into());
    let upgraded_code = AccountCode::from_components(&upgraded_components).unwrap();
    assert_ne!(account.code().commitment(), upgraded_code.commitment());

    (account, upgraded_code)
}

/// Returns the message of `error` and of all of its sources.
fn error_chain(error: &ClientError) -> String {
    let mut messages = vec![error.to_string()];
    let mut source = core::error::Error::source(error);
    while let Some(err) = source {
        messages.push(err.to_string());
        source = err.source();
    }
    messages.join(": ")
}

/// Returns a request that sends the asset of `account` to another account. The request needs the
/// basic wallet interface.
fn send_request(
    client: &mut MockClient<FilesystemKeyStore>,
    account: &Account,
) -> TransactionRequest {
    let asset = account.vault().assets().next().expect("the account holds one asset");
    TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![asset],
                account.id(),
                ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap(),
            ),
            NoteType::Public,
            client.rng(),
        )
        .unwrap()
}

/// Executes a transaction in which `sender` sends an upgrade note that upgrades the code of
/// `target` to `code`, and returns the note.
async fn send_upgrade_note(
    client: &mut MockClient<FilesystemKeyStore>,
    sender: &Account,
    target: &Account,
    code: &AccountCode,
) -> Note {
    let request = TransactionRequestBuilder::new()
        .build_upgrade_note(sender.id(), target.id(), code.clone(), client.rng())
        .unwrap();
    let [note]: [Note; 1] = request.expected_output_own_notes().try_into().unwrap();

    let result = Box::pin(client.execute_transaction(sender.id(), request)).await.unwrap();
    assert!(
        result
            .executed_transaction()
            .output_notes()
            .iter()
            .any(|output| output.id() == note.id()),
        "the transaction should create the upgrade note"
    );

    note
}

async fn stored_account(client: &mut MockClient<FilesystemKeyStore>, account: &Account) -> Account {
    client
        .test_store()
        .get_account(account.id())
        .await
        .unwrap()
        .expect("the account should be tracked")
        .try_into()
        .unwrap()
}

// TESTS
// ================================================================================================

/// A local transaction that upgrades the code stores the new code. The next transaction runs
/// against the new code, so the account can use the wallet that only the new code has.
#[tokio::test]
async fn local_code_upgrade_replaces_account_code() {
    let (account, upgraded_code) = upgradeable_account();
    let rpc_api = rpc_api_with(&account);
    let mut client = client_tracking(rpc_api.clone(), &account).await;

    // The current code has no wallet, so the account cannot send its asset.
    let request = send_request(&mut client, &account);
    let error = Box::pin(client.execute_transaction(account.id(), request)).await.unwrap_err();
    assert!(
        matches!(error, ClientError::TransactionRequestError(_)),
        "unexpected error: {error:?}"
    );

    let request = valid_upgrade_request(&client, &upgraded_code, false);
    let result = Box::pin(client.execute_transaction(account.id(), request)).await.unwrap();
    let executed_tx = result.executed_transaction();
    assert_eq!(executed_tx.final_account().code_commitment(), upgraded_code.commitment());
    assert_eq!(executed_tx.account_patch().code().as_code(), Some(&upgraded_code));
    assert!(executed_tx.account_patch().storage().is_empty());

    client
        .apply_transaction(&result, rpc_api.get_chain_tip_block_num())
        .await
        .unwrap();

    let stored = stored_account(&mut client, &account).await;
    assert_eq!(stored.code(), &upgraded_code);
    assert_eq!(stored.to_commitment(), executed_tx.final_account().to_commitment());

    // The client reads the new code from the store, so it builds and runs the send.
    let request = send_request(&mut client, &account);
    let result = Box::pin(client.execute_transaction(account.id(), request)).await.unwrap();
    assert_eq!(
        result.executed_transaction().initial_account().code().commitment(),
        upgraded_code.commitment()
    );
}

/// A transaction can upgrade the code and change the storage of the account at the same time. The
/// patch carries both changes, and the store applies both. The next transaction runs against the
/// new code and the new storage.
#[tokio::test]
async fn local_code_upgrade_with_storage_changes() {
    let (account, upgraded_code) = upgradeable_account();
    let rpc_api = rpc_api_with(&account);
    let mut client = client_tracking(rpc_api.clone(), &account).await;

    let request = valid_upgrade_request(&client, &upgraded_code, true);
    let result = Box::pin(client.execute_transaction(account.id(), request)).await.unwrap();
    let patch = result.executed_transaction().account_patch();
    assert_eq!(patch.code().as_code(), Some(&upgraded_code));
    assert!(patch.storage().updated_value(&state_value_slot()).is_some());
    assert!(patch.storage().updated_map(&state_map_slot()).is_some());

    let mut expected = account.clone();
    expected.apply_patch(patch).unwrap();

    client
        .apply_transaction(&result, rpc_api.get_chain_tip_block_num())
        .await
        .unwrap();

    let stored = stored_account(&mut client, &account).await;
    assert_eq!(stored, expected);
    assert_eq!(stored.code(), &upgraded_code);
    assert_ne!(stored.storage().to_commitment(), account.storage().to_commitment());

    // A storage change without an upgrade runs against the new code and the new storage.
    let request = upgrade_request(&client, &[], true, None);
    let result = Box::pin(client.execute_transaction(account.id(), request)).await.unwrap();
    let executed_tx = result.executed_transaction();
    assert_eq!(executed_tx.initial_account().code().commitment(), upgraded_code.commitment());
    assert!(executed_tx.account_patch().code().is_empty());
}

/// An invalid upgrade fails during execution, and the stored account does not change. The kernel
/// and the host enforce these rules:
/// - an upgrade cannot change the storage layout, so the storage upgrade commitment must be empty.
/// - a new account cannot upgrade its code.
/// - a transaction can initialize only one upgrade.
/// - the transaction must get the new code, and the code must match the requested commitment.
#[rstest]
#[case::storage_upgrade(
    InvalidUpgrade::StorageUpgrade,
    "account storage upgrades are not supported"
)]
#[case::new_account(InvalidUpgrade::NewAccount, "a new account cannot be upgraded")]
#[case::second_upgrade(InvalidUpgrade::SecondUpgrade, "an account code upgrade is already pending")]
#[case::missing_code(InvalidUpgrade::MissingCode, "did not provide the new code")]
#[case::mismatched_code(InvalidUpgrade::MismatchedCode, "but the advice map provides code")]
#[tokio::test]
async fn invalid_code_upgrade_is_rejected(
    #[case] invalid_upgrade: InvalidUpgrade,
    #[case] expected_error: &str,
) {
    let (existing_account, upgraded_code) = upgradeable_account();
    // A new account is not on chain yet. The client creates it with its first transaction.
    let (account, rpc_api) = match invalid_upgrade {
        InvalidUpgrade::NewAccount => (
            new_upgradeable_account(),
            MockRpcApi::new(MockChainBuilder::new().build().unwrap()),
        ),
        _ => (existing_account.clone(), rpc_api_with(&existing_account)),
    };
    let mut client = client_tracking(rpc_api, &account).await;

    let new_code_commitment = upgraded_code.commitment();
    let code_entry = AccountCodeUpgrade::new(upgraded_code).to_advice_map_entry();
    let (upgrades, advice_entry) = match invalid_upgrade {
        InvalidUpgrade::NewAccount => {
            (vec![(new_code_commitment, Word::empty())], Some(code_entry))
        },
        InvalidUpgrade::MissingCode => (vec![(new_code_commitment, Word::empty())], None),
        // The entry is under the key of the requested commitment but carries the current code.
        InvalidUpgrade::MismatchedCode => (
            vec![(new_code_commitment, Word::empty())],
            Some((
                AccountCodeUpgrade::advice_map_key(new_code_commitment),
                AccountCodeUpgrade::new(account.code().clone()).to_elements(),
            )),
        ),
        InvalidUpgrade::StorageUpgrade => {
            (vec![(new_code_commitment, Word::from([1u32, 2, 3, 4]))], Some(code_entry))
        },
        InvalidUpgrade::SecondUpgrade => (
            vec![(new_code_commitment, Word::empty()), (new_code_commitment, Word::empty())],
            Some(code_entry),
        ),
    };

    let request = upgrade_request(&client, &upgrades, false, advice_entry);
    let error = Box::pin(client.execute_transaction(account.id(), request)).await.unwrap_err();
    assert!(
        matches!(error, ClientError::TransactionExecutorError(_)),
        "unexpected error: {error:?}"
    );
    let messages = error_chain(&error);
    assert!(messages.contains(expected_error), "unexpected error: {messages}");

    assert_eq!(stored_account(&mut client, &account).await, account);
}

#[derive(Debug, Clone, Copy)]
enum InvalidUpgrade {
    StorageUpgrade,
    NewAccount,
    SecondUpgrade,
    MissingCode,
    MismatchedCode,
}

/// A client that tracks the account receives the code upgrade during sync and stores the new code,
/// also when the same transaction changes the storage. The default threshold syncs the full account
/// state. A threshold of zero makes the vault and the storage map oversized, so the client builds
/// the update from incremental patches.
#[rstest]
#[tokio::test]
async fn synced_code_upgrade_stores_new_code(
    #[values(None, Some(0))] oversize_threshold: Option<usize>,
    #[values(false, true)] bump_state: bool,
) {
    let (account, upgraded_code) = upgradeable_account();
    let rpc_api = rpc_api_with(&account);
    let owner = client_tracking(rpc_api.clone(), &account).await;

    let observer_rpc_api = match oversize_threshold {
        Some(threshold) => rpc_api.clone().with_oversize_threshold(threshold),
        None => rpc_api.clone(),
    };
    let mut observer = client_tracking(observer_rpc_api, &account).await;

    let request = valid_upgrade_request(&owner, &upgraded_code, bump_state);
    let result = Box::pin(owner.execute_transaction(account.id(), request)).await.unwrap();
    let executed_tx = result.executed_transaction();
    rpc_api.add_pending_executed_transaction(executed_tx);
    rpc_api.prove_block();

    let mut expected = account.clone();
    expected.apply_patch(executed_tx.account_patch()).unwrap();
    assert_eq!(expected.to_commitment(), executed_tx.final_account().to_commitment());

    observer.sync_state().await.unwrap();

    let stored = stored_account(&mut observer, &account).await;
    assert_eq!(stored, expected);
    assert_eq!(stored.code(), &upgraded_code);
}

/// The standard upgrade request calls the `upgrade` procedure of `UpgradeManager`. An account
/// without this component cannot execute the request, and the stored account does not change.
#[tokio::test]
async fn account_code_upgrade_without_upgrade_manager_is_rejected() {
    let account = wallet_account(8);
    let (_, other_code) = upgradeable_account();
    let rpc_api = rpc_api_with(&account);
    let mut client = client_tracking(rpc_api, &account).await;

    let request = TransactionRequestBuilder::new().build_account_code_upgrade(other_code).unwrap();
    let error = Box::pin(client.execute_transaction(account.id(), request)).await.unwrap_err();
    assert!(
        matches!(error, ClientError::TransactionExecutorError(_)),
        "unexpected error: {error:?}"
    );
    let expected_error = format!(
        "account procedure with procedure root {} is not in the account procedure index map",
        UpgradeManager::upgrade_root()
    );
    let messages = error_chain(&error);
    assert!(messages.contains(&expected_error), "unexpected error: {messages}");

    assert_eq!(stored_account(&mut client, &account).await, account);
}

/// The owner of an account sends an upgrade note to the account. The account consumes the note, and
/// the store saves the new code.
#[tokio::test]
async fn upgrade_note_from_owner_upgrades_target_code() {
    let owner = wallet_account(6);
    let (target, upgraded_code) = owned_upgradeable_account(owner.id());
    let rpc_api = rpc_api_with_accounts(&[&owner, &target]);
    let mut client = client_tracking(rpc_api.clone(), &target).await;
    client.add_account(&owner, false).await.unwrap();

    let note = send_upgrade_note(&mut client, &owner, &target, &upgraded_code).await;

    let request = TransactionRequestBuilder::new().build_consume_notes(vec![note]).unwrap();
    let result = Box::pin(client.execute_transaction(target.id(), request)).await.unwrap();
    let executed_tx = result.executed_transaction();
    assert_eq!(executed_tx.final_account().code_commitment(), upgraded_code.commitment());
    assert_eq!(executed_tx.account_patch().code().as_code(), Some(&upgraded_code));

    client
        .apply_transaction(&result, rpc_api.get_chain_tip_block_num())
        .await
        .unwrap();

    let stored = stored_account(&mut client, &target).await;
    assert_eq!(stored.code(), &upgraded_code);
    assert_eq!(stored.to_commitment(), executed_tx.final_account().to_commitment());
}

/// An upgrade note from an account that is not the owner fails when the target consumes it, and the
/// stored account does not change.
#[tokio::test]
async fn upgrade_note_from_other_sender_is_rejected() {
    let owner = wallet_account(6);
    let other_sender = wallet_account(7);
    let (target, upgraded_code) = owned_upgradeable_account(owner.id());
    let rpc_api = rpc_api_with_accounts(&[&other_sender, &target]);
    let mut client = client_tracking(rpc_api, &target).await;
    client.add_account(&other_sender, false).await.unwrap();

    let note = send_upgrade_note(&mut client, &other_sender, &target, &upgraded_code).await;

    let request = TransactionRequestBuilder::new().build_consume_notes(vec![note]).unwrap();
    let error = Box::pin(client.execute_transaction(target.id(), request)).await.unwrap_err();
    assert!(
        matches!(error, ClientError::TransactionExecutorError(_)),
        "unexpected error: {error:?}"
    );
    let messages = error_chain(&error);
    assert!(
        messages.contains("note sender is not the owner"),
        "unexpected error: {messages}"
    );

    assert_eq!(stored_account(&mut client, &target).await, target);
}
