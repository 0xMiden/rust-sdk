use anyhow::{Context, Result, anyhow, ensure};
use miden_client::account::component::{AccountComponent, AccountComponentMetadata};
use miden_client::account::standards::wallets::BasicWallet;
use miden_client::account::{
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountId,
    AccountType,
};
use miden_client::assembly::CodeBuilder;
use miden_client::auth::FALCON512_EIDOS_SCHEME_ID;
use miden_client::store::TransactionFilter;
use miden_client::testing::common::{AccountSetup, TestClient, auth_component};
use miden_client::transaction::{
    LogTopic,
    TransactionId,
    TransactionLog,
    TransactionLogDetails,
    TransactionLogs,
    TransactionRequestBuilder,
    TransactionStatus,
};
use miden_client::{Felt, Word};
use rand::Rng;

use crate::ClientConfig;

// HELPERS
// ================================================================================================

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

fn log_content() -> Word {
    Word::from(LOG_CONTENT.map(Felt::from))
}

fn log_component_code() -> String {
    LOG_COMPONENT_CODE.replace("{content}", &log_content().to_hex())
}

/// Returns the logs that one call to `emit_logs` adds for `account_id`, in emission order.
fn emitted_logs(account_id: AccountId) -> Result<Vec<TransactionLog>> {
    let topic = LogTopic::from_name(LOG_TOPIC);
    Ok(vec![
        TransactionLog::new(account_id, topic, vec![log_content()]).map_err(|err| anyhow!(err))?,
        TransactionLog::new(account_id, topic, vec![]).map_err(|err| anyhow!(err))?,
    ])
}

/// Deploys an account with the log component.
async fn deploy_log_account(
    client: &mut TestClient,
    account_type: AccountType,
) -> Result<AccountId> {
    let code = CodeBuilder::default()
        .compile_component_code(LOG_MODULE, log_component_code())
        .context("failed to compile the log component")?;
    let component = AccountComponent::new(code, vec![], AccountComponentMetadata::new(LOG_MODULE))
        .map_err(|err| anyhow!(err))
        .context("failed to create the log component")?;

    let (auth, key) = auth_component(FALCON512_EIDOS_SCHEME_ID)?;
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let account = AccountBuilder::new(init_seed)
        .account_type(account_type)
        .with_component(auth)
        .with_component(BasicWallet)
        .with_component(component)
        .build_with_schema_commitment()
        .context("failed to build the log account")?;

    let (account, _) = client.insert_account(AccountSetup::prebuilt(account, key)).await?;
    client.deploy_account(account.id()).await?;

    Ok(account.id())
}

/// Executes a transaction that calls `emit_logs`, waits for its commit, and returns its ID and the
/// block that holds it.
async fn commit_log_transaction(
    client: &mut TestClient,
    account_id: AccountId,
) -> Result<(TransactionId, u32)> {
    let tx_script = client
        .code_builder()
        .with_linked_module("external_contract::log_contract", log_component_code())?
        .compile_tx_script(
            "use external_contract::log_contract
            @transaction_script
            pub proc main
                call.log_contract::emit_logs
            end",
        )
        .context("failed to compile the log transaction script")?;
    let request = TransactionRequestBuilder::new()
        .custom_script(tx_script)
        .build()
        .context("failed to build the log transaction request")?;

    let transaction_id = client.submit_new_transaction(account_id, request).await?;
    client.wait_for_tx(transaction_id).await?;

    let record = client
        .get_transactions(TransactionFilter::Ids(vec![transaction_id]))
        .await?
        .pop()
        .context("the client should track the log transaction")?;
    let TransactionStatus::Committed { block_number, .. } = record.status else {
        anyhow::bail!("transaction {transaction_id} is not committed");
    };

    Ok((transaction_id, block_number.as_u32()))
}

/// Checks that the stored logs of `transaction_id` hold the logs of `emit_logs`, and returns them.
///
/// On a chain that charges fees, the transaction can emit more logs, so the check compares only the
/// logs with the test topic.
async fn stored_logs(
    client: &TestClient,
    account_id: AccountId,
    transaction_id: TransactionId,
) -> Result<TransactionLogs> {
    let logs = client
        .get_transaction_logs(transaction_id)
        .await?
        .context("the store should hold the logs of the tracked transaction")?;

    let topic = LogTopic::from_name(LOG_TOPIC);
    let test_logs: Vec<TransactionLog> =
        logs.iter().filter(|log| log.topic() == topic).cloned().collect();
    ensure!(
        test_logs == emitted_logs(account_id)?,
        "stored logs {test_logs:?} are not the logs that the transaction emitted"
    );

    Ok(logs)
}

// TESTS
// ================================================================================================

/// A public account emits logs. The store records them, and the node returns the same logs with the
/// position of the transaction. The node knows no logs for another transaction ID, or for the same
/// transaction under another account.
pub async fn test_public_account_transaction_logs(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;
    let account_id = deploy_log_account(&mut client, AccountType::Public).await?;
    let other_account_id = deploy_log_account(&mut client, AccountType::Public).await?;

    let (transaction_id, block_num) = commit_log_transaction(&mut client, account_id).await?;
    let logs = stored_logs(&client, account_id, transaction_id).await?;

    let info = client
        .fetch_transaction_logs(account_id, transaction_id)
        .await?
        .context("the node should return the logs of the committed transaction")?;
    assert_eq!(info.transaction_id, transaction_id);
    assert_eq!(info.account_id, account_id);
    assert_eq!(info.block_num.as_u32(), block_num);
    assert_eq!(info.log_details, TransactionLogDetails::Public(logs.clone()));
    assert_eq!(info.logs_commitment(), logs.commitment());

    let unknown_id = TransactionId::from_raw(Word::from([7u32; 4]));
    assert_eq!(client.fetch_transaction_logs(account_id, unknown_id).await?, None);
    assert_eq!(client.fetch_transaction_logs(other_account_id, transaction_id).await?, None);

    Ok(())
}

/// A private account emits logs. The store records them. The node returns only their commitment.
pub async fn test_private_account_transaction_logs(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;
    let account_id = deploy_log_account(&mut client, AccountType::Private).await?;

    let (transaction_id, block_num) = commit_log_transaction(&mut client, account_id).await?;
    let logs = stored_logs(&client, account_id, transaction_id).await?;

    let info = client
        .fetch_transaction_logs(account_id, transaction_id)
        .await?
        .context("the node should return the logs commitment of the committed transaction")?;
    assert_eq!(info.block_num.as_u32(), block_num);
    assert_eq!(info.log_details, TransactionLogDetails::Private(logs.commitment()));
    assert_eq!(info.logs(), None);

    Ok(())
}
