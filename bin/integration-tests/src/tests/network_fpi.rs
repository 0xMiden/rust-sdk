use anyhow::{Context, Result};
use miden_client::account::AccountType;
use miden_client::auth::RPO_FALCON_SCHEME_ID;
use miden_client::testing::common::{execute_tx_and_sync, insert_new_wallet, wait_for_blocks};
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Felt, Word, ZERO};

use super::fpi::{FPI_STORAGE_VALUE, MAP_KEY, MAP_SLOT_NAME, deploy_foreign_account};
use super::network_transaction::{
    COUNTER_SLOT_NAME,
    deploy_network_counter_contract,
    get_network_note_with_script,
    note_script_root,
};
use crate::tests::config::ClientConfig;

// TESTS
// ================================================================================================

/// This test essentially combines the `test_counter_contract_ntx` network transaction test and
/// `test_fpi_execute_program` fpi test in attempt to create a network transaction which performs
/// the FPI.
///
/// This test uses three accounts: public foreign account, network counter account, and sender
/// account as a private wallet (which is needed only for the note creation, so potentially it could
/// be replaced by any account ID).
/// Sender account creates a note, which targets the counter account. This note's script contains
/// the FPI, which obtains the map value from the foreign account. In order to check whether the FPI
/// was successful (note script was executed successfully), note script updates the counter of the
/// network (counter) account.
pub async fn test_network_fpi(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.clone().into_client().await?;
    client.sync_state().await?;

    let (foreign_account, proc_root) = deploy_foreign_account(
        &mut client,
        &keystore,
        AccountType::Public,
        format!(
            r#"
            const MAP_STORAGE_SLOT = word("{MAP_SLOT_NAME}")

            #! Inputs:  [pad(16)]
            #! Outputs: [VALUE, pad(12)]
            @account_procedure
            pub proc get_fpi_map_item
                # map key
                push.{map_key}
                # => [KEY, pad(16)]

                # item slot
                push.MAP_STORAGE_SLOT[0..2]
                # => [slot_id_prefix, slot_id_suffix, KEY, pad(16)]

                exec.::miden::protocol::active_account::get_map_item
                # => [VALUE, pad(16)]

                # truncate the stack
                swapw dropw
                # => [VALUE, pad(12)]
            end
            "#,
            map_key = Word::from(MAP_KEY)
        ),
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let foreign_account_id = foreign_account.id();

    client.sync_state().await?;

    let (mut client2, keystore2) =
        ClientConfig::new(client_config.rpc_endpoint, client_config.rpc_timeout_ms)
            .into_client()
            .await?;

    // NOTE: Syncing the client is important because the client needs to be beyond the account
    // creation block
    client2.sync_state().await?;

    let network_fpi_note_script = format!(
        "
        use miden::protocol::tx
        use external_contract::counter_contract
        use miden::core::sys

        @note_script
        pub proc main
            # push the hash of the `get_fpi_map_item` account procedure
            push.{proc_root}

            # push the foreign account id
            push.{account_id_prefix} push.{account_id_suffix}
            # => [foreign_id_suffix, foreign_id_prefix, FOREIGN_PROC_ROOT, pad(16)]

            exec.tx::execute_foreign_procedure

            push.{fpi_value} assert_eqw

            call.counter_contract::increment_count

            exec.sys::truncate_stack
        end
        ",
        fpi_value = Word::from(FPI_STORAGE_VALUE),
        account_id_prefix = foreign_account_id.prefix().as_u64(),
        account_id_suffix = foreign_account_id.suffix(),
    );

    // The counter account is deployed as a network account that allowlists the FPI note script, so
    // the node routes the note to it and runs the network transaction.
    let network_fpi_note_root =
        note_script_root(&network_fpi_note_script, client2.source_manager())?;
    let target_network_account =
        deploy_network_counter_contract(&mut client2, &[network_fpi_note_root]).await?;

    client2.sync_state().await?;

    let (sender_account, ..) =
        insert_new_wallet(&mut client2, AccountType::Private, &keystore2, RPO_FALCON_SCHEME_ID)
            .await?;

    let network_note = get_network_note_with_script(
        sender_account.id(),
        target_network_account.id(),
        &network_fpi_note_script,
        client2.source_manager(),
        &mut client2.rng(),
    )?;

    let tx_request = TransactionRequestBuilder::new().own_output_notes([network_note]).build()?;

    execute_tx_and_sync(&mut client2, sender_account.id(), tx_request).await?;

    wait_for_blocks(&mut client2, 2).await;

    // get the updated network account to check that the counter value was updated (meaning that the
    // note was executed successfully, so the FPI was successful)
    let updated_network_account = client2
        .test_rpc_api()
        .get_account_details(target_network_account.id())
        .await?
        .with_context(|| "account details not available")?;

    assert_eq!(
        updated_network_account.storage().get_item(&COUNTER_SLOT_NAME)?,
        Word::from([Felt::from(1u32), ZERO, ZERO, ZERO])
    );

    Ok(())
}
