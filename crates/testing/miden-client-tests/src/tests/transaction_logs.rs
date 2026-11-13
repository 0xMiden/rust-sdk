use alloc::boxed::Box;

use miden_client::account::standards::auth::AuthSingleSig;
use miden_client::account::{AccountBuilderSchemaCommitmentExt, AccountId, AccountType};
use miden_client::assembly::CodeBuilder;
use miden_client::auth::{AuthSchemeId, AuthSecretKey};
use miden_client::keystore::Keystore;
use miden_client::rpc::{GrpcError, NodeRpcClient, RpcEndpoint, RpcError};
use miden_client::testing::common::TestClient;
use miden_client::testing::mock::MockRpcApi;
use miden_client::transaction::{
    LogTopic,
    TransactionId,
    TransactionLog,
    TransactionLogDetails,
    TransactionLogs,
    TransactionLogsInfo,
    TransactionRequestBuilder,
};
use miden_client::{ClientError, Felt, Word};
use miden_protocol::account::{AccountBuilder, AccountComponent, AccountComponentMetadata};
use miden_standards::account::auth::Approver;
use miden_standards::account::wallets::BasicWallet;
use rand::Rng;

use super::{create_test_client, open_store_file};

const LOG_MODULE: &str = "miden::testing::log_component";
const LOG_TOPIC: &str = "miden::testing::log_component::greeting";
const LOG_CONTENT: [u32; 4] = [1, 2, 3, 4];

/// An account component that emits two logs: one with one content word and one without content.
const LOG_COMPONENT_CODE: &str = r#"
    use miden::protocol::tx

    const LOG_TOPIC = word("miden::testing::log_component::greeting")

    @account_procedure
    @locals(4)
    pub proc emit_logs
        push.{content} loc_storew_le.0 dropw

        locaddr.0 push.1 push.LOG_TOPIC[0..2]
        # => [topic_suffix, topic_prefix, num_words, content_ptr]
        exec.tx::add_log_from_memory

        locaddr.0 push.0 push.LOG_TOPIC[0..2]
        # => [topic_suffix, topic_prefix, num_words, content_ptr]
        exec.tx::add_log_from_memory
    end
"#;

fn log_component_code() -> String {
    LOG_COMPONENT_CODE.replace("{content}", &log_content().to_hex())
}

fn log_content() -> Word {
    Word::from(LOG_CONTENT.map(Felt::from))
}

/// Returns the logs that one call to `emit_logs` produces for `account_id`.
fn expected_logs(account_id: AccountId) -> TransactionLogs {
    let topic = LogTopic::from_name(LOG_TOPIC);
    TransactionLogs::new(vec![
        TransactionLog::new(account_id, topic, vec![log_content()]).unwrap(),
        TransactionLog::new(account_id, topic, vec![]).unwrap(),
    ])
    .unwrap()
}

/// Adds an account with the log component to the client.
async fn insert_log_account(client: &mut TestClient, account_type: AccountType) -> AccountId {
    let component_code = CodeBuilder::default()
        .compile_component_code(LOG_MODULE, log_component_code())
        .unwrap();
    let component =
        AccountComponent::new(component_code, vec![], AccountComponentMetadata::new(LOG_MODULE))
            .unwrap();

    let key_pair = AuthSecretKey::new_falcon512_eidos();
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let account = AccountBuilder::new(init_seed)
        .account_type(account_type)
        .with_component(AuthSingleSig::new(Approver::new(
            key_pair.public_key().to_commitment(),
            AuthSchemeId::Falcon512Eidos,
        )))
        .with_component(BasicWallet)
        .with_component(component)
        .build_with_schema_commitment()
        .unwrap();

    client.keystore().add_key(&key_pair, account.id()).await.unwrap();
    client.add_account(&account, false).await.unwrap();

    account.id()
}

/// Submits a transaction that calls `emit_logs`, and commits it in a block.
async fn submit_log_transaction(
    client: &mut TestClient,
    rpc_api: &MockRpcApi,
    account_id: AccountId,
) -> TransactionId {
    let tx_script = CodeBuilder::new()
        .with_linked_module("external_contract::log_contract", log_component_code())
        .unwrap()
        .compile_tx_script(
            "use external_contract::log_contract
            @transaction_script
            pub proc main
                call.log_contract::emit_logs
            end",
        )
        .unwrap();
    let request = TransactionRequestBuilder::new().custom_script(tx_script).build().unwrap();

    let transaction_id =
        Box::pin(client.submit_new_transaction(account_id, request)).await.unwrap();
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    transaction_id
}

/// Returns a transaction ID that no transaction has.
fn unknown_transaction_id() -> TransactionId {
    TransactionId::from_raw(Word::from([Felt::from(7u32); 4]))
}

#[tokio::test]
async fn store_records_the_logs_of_a_transaction() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    let account_id = insert_log_account(&mut client, AccountType::Private).await;

    let transaction_id = submit_log_transaction(&mut client, &rpc_api, account_id).await;

    let logs = client.get_transaction_logs(transaction_id).await.unwrap();
    assert_eq!(logs, Some(expected_logs(account_id)));
}

#[tokio::test]
async fn store_records_empty_logs_for_a_transaction_without_logs() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    let account_id = insert_log_account(&mut client, AccountType::Public).await;

    // The first transaction of the account deploys it and emits no logs.
    let request = TransactionRequestBuilder::new().build().unwrap();
    let transaction_id =
        Box::pin(client.submit_new_transaction(account_id, request)).await.unwrap();
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let logs = client.get_transaction_logs(transaction_id).await.unwrap().unwrap();
    assert!(logs.is_empty());

    let info = client
        .fetch_transaction_logs(account_id, transaction_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.log_details, TransactionLogDetails::Public(TransactionLogs::default()));
}

#[tokio::test]
async fn untracked_transaction_has_no_stored_logs() {
    let (client, _rpc_api) = Box::pin(create_test_client()).await;

    let logs = client.get_transaction_logs(unknown_transaction_id()).await.unwrap();
    assert_eq!(logs, None);
}

#[tokio::test]
async fn node_returns_the_logs_of_a_public_account() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    let account_id = insert_log_account(&mut client, AccountType::Public).await;

    let transaction_id = submit_log_transaction(&mut client, &rpc_api, account_id).await;

    let info = client
        .fetch_transaction_logs(account_id, transaction_id)
        .await
        .unwrap()
        .unwrap();
    let committed_at = rpc_api.get_chain_tip_block_num();

    assert_eq!(
        info,
        TransactionLogsInfo {
            transaction_id,
            account_id,
            block_num: committed_at,
            transaction_index: 0,
            log_details: TransactionLogDetails::Public(expected_logs(account_id)),
        }
    );
    assert_eq!(info.logs(), Some(&expected_logs(account_id)));
    assert_eq!(info.logs_commitment(), expected_logs(account_id).commitment());
}

#[tokio::test]
async fn node_returns_only_the_commitment_for_a_private_account() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    let account_id = insert_log_account(&mut client, AccountType::Private).await;

    let transaction_id = submit_log_transaction(&mut client, &rpc_api, account_id).await;

    let info = client
        .fetch_transaction_logs(account_id, transaction_id)
        .await
        .unwrap()
        .unwrap();
    let commitment = expected_logs(account_id).commitment();

    assert_eq!(info.log_details, TransactionLogDetails::Private(commitment));
    assert_eq!(info.logs(), None);
    assert_eq!(info.logs_commitment(), commitment);
}

#[tokio::test]
async fn node_has_no_logs_for_an_unknown_transaction_or_another_account() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    let account_id = insert_log_account(&mut client, AccountType::Public).await;
    let other_account_id = insert_log_account(&mut client, AccountType::Public).await;

    let transaction_id = submit_log_transaction(&mut client, &rpc_api, account_id).await;

    let unknown = client
        .fetch_transaction_logs(account_id, unknown_transaction_id())
        .await
        .unwrap();
    assert_eq!(unknown, None);

    let wrong_account =
        client.fetch_transaction_logs(other_account_id, transaction_id).await.unwrap();
    assert_eq!(wrong_account, None);
}

#[tokio::test]
async fn pending_transaction_has_stored_logs_but_no_committed_logs() {
    let (mut client, _rpc_api) = Box::pin(create_test_client()).await;
    let account_id = insert_log_account(&mut client, AccountType::Public).await;

    let request = TransactionRequestBuilder::new().build().unwrap();
    let transaction_id =
        Box::pin(client.submit_new_transaction(account_id, request)).await.unwrap();

    assert!(client.get_transaction_logs(transaction_id).await.unwrap().is_some());
    assert_eq!(client.fetch_transaction_logs(account_id, transaction_id).await.unwrap(), None);
}

#[tokio::test]
async fn fetch_rejects_a_commitment_that_differs_from_the_stored_logs() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    let account_id = insert_log_account(&mut client, AccountType::Public).await;

    let transaction_id = submit_log_transaction(&mut client, &rpc_api, account_id).await;

    // Remove the stored logs, so the store holds an empty collection for the transaction.
    let deleted_rows = open_store_file(&mut client)
        .execute(
            "DELETE FROM transaction_logs WHERE transaction_id = ?",
            [transaction_id.as_bytes()],
        )
        .unwrap();
    assert_eq!(deleted_rows, 2);
    let stored = client.get_transaction_logs(transaction_id).await.unwrap().unwrap();
    assert!(stored.is_empty());

    let err = client.fetch_transaction_logs(account_id, transaction_id).await.unwrap_err();
    assert!(matches!(err, ClientError::RpcError(RpcError::InvalidResponse(_))), "{err:?}");
}

/// A stored log that the store cannot decode must not hide the response of the node.
#[tokio::test]
async fn fetch_returns_the_node_logs_when_the_stored_logs_cannot_be_read() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    let account_id = insert_log_account(&mut client, AccountType::Public).await;

    let transaction_id = submit_log_transaction(&mut client, &rpc_api, account_id).await;

    let updated_rows = open_store_file(&mut client)
        .execute(
            "UPDATE transaction_logs SET topic = x'ff' WHERE transaction_id = ?",
            [transaction_id.as_bytes()],
        )
        .unwrap();
    assert_eq!(updated_rows, 2);
    assert!(client.get_transaction_logs(transaction_id).await.is_err());

    let info = client
        .fetch_transaction_logs(account_id, transaction_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.logs(), Some(&expected_logs(account_id)));
}

#[tokio::test]
async fn fetch_reports_a_request_failure() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    let account_id = insert_log_account(&mut client, AccountType::Public).await;

    rpc_api.fail_next_call(
        RpcEndpoint::GetTransactionLogs,
        RpcError::RequestError {
            endpoint: RpcEndpoint::GetTransactionLogs,
            error_kind: GrpcError::Unavailable,
            endpoint_error: None,
            source: None,
        },
    );

    let err = client
        .fetch_transaction_logs(account_id, unknown_transaction_id())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ClientError::RpcError(RpcError::RequestError {
                endpoint: RpcEndpoint::GetTransactionLogs,
                ..
            })
        ),
        "{err:?}"
    );

    // The failure is served once.
    let _ = NodeRpcClient::get_transaction_logs(&rpc_api, account_id, unknown_transaction_id())
        .await
        .unwrap();
}
