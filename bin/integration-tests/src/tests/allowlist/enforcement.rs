//! Enforcement of the allowlist when an account is created on chain.
//!
//! These cover what the node does with a submission that creates an account: it is accepted when
//! the account is registered or is a network account, and refused otherwise. Registering an account
//! in the first place is in [`super::registration`].

use anyhow::{Context, Result};
use miden_client::rpc::RpcEndpoint;

use super::invitations::create_invitation_code;
use super::{
    assert_rejected_as_unregistered,
    deploy_request,
    insert_undeployed_wallet,
    is_deployed,
};
use crate::ClientConfig;
use crate::tests::network_transaction::deploy_network_counter_contract;

/// A registered account can be created on chain.
///
/// This is the flow the whole allowlist feature exists for: claim a code, register the account with
/// it, and deploy.
pub async fn test_allowlist_registered_account_can_deploy(
    client_config: ClientConfig,
) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let invitation_code = create_invitation_code().await?;
    let account = insert_undeployed_wallet(&mut client).await?;

    client
        .register_account(&invitation_code, account.id())
        .await
        .context("failed to register the account on the network allowlist")?;

    let transaction_id = client
        .submit_new_transaction(account.id(), deploy_request()?)
        .await
        .context("the node rejected the deploy of a registered account")?;
    client.wait_for_tx(transaction_id).await?;

    assert!(
        is_deployed(&client, &account).await?,
        "a registered account should have been created on chain"
    );

    Ok(())
}

/// An account that was never registered cannot be created on chain.
///
/// The negative half of the test above. Without it a node that silently stopped enforcing would
/// still pass the whole suite.
pub async fn test_allowlist_unregistered_account_is_rejected(
    client_config: ClientConfig,
) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let account = insert_undeployed_wallet(&mut client).await?;

    let error = client
        .submit_new_transaction(account.id(), deploy_request()?)
        .await
        .expect_err("the node should refuse to create an unregistered account");
    assert_rejected_as_unregistered(&error, RpcEndpoint::SubmitProvenTx);

    assert!(
        !is_deployed(&client, &account).await?,
        "a rejected account should not have been created on chain"
    );

    Ok(())
}

/// A network account is created without any registration.
///
/// The node classifies the account before it exists on chain and exempts network accounts, so this
/// covers the branch that keeps the allowlist from blocking the node's own accounts.
pub async fn test_allowlist_network_account_needs_no_registration(
    client_config: ClientConfig,
) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let account = deploy_network_counter_contract(&mut client, &[])
        .await
        .context("a network account should deploy without being registered")?;

    assert!(
        is_deployed(&client, &account).await?,
        "a network account should have been created on chain without registration"
    );

    Ok(())
}

/// Every transaction in a batch is checked, not only the first.
///
/// The node checks each transaction of a submitted batch separately, so a batch that pairs a
/// registered account with an unregistered one is rejected as a whole. Without this the batch
/// endpoint could stop enforcing and only the single-transaction tests would notice.
pub async fn test_allowlist_is_enforced_per_batch_transaction(
    client_config: ClientConfig,
) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let invitation_code = create_invitation_code().await?;
    let registered = insert_undeployed_wallet(&mut client).await?;
    let unregistered = insert_undeployed_wallet(&mut client).await?;

    client
        .register_account(&invitation_code, registered.id())
        .await
        .context("failed to register the account")?;

    // The funding notes are folded in before the batch borrows the client.
    let registered_request = client.fund_request(registered.id(), deploy_request()?);
    let unregistered_request = client.fund_request(unregistered.id(), deploy_request()?);

    let mut batch = client.new_transaction_batch();
    batch.push(registered.id(), registered_request).await?;
    batch.push(unregistered.id(), unregistered_request).await?;

    let error = batch
        .submit()
        .await
        .expect_err("a batch creating an unregistered account should be rejected");
    assert_rejected_as_unregistered(&error, RpcEndpoint::SubmitProvenBatch);

    assert!(
        !is_deployed(&client, &unregistered).await?,
        "the unregistered account should not have been created on chain"
    );
    // The batch is refused as a whole, so the registered account is not created either.
    assert!(
        !is_deployed(&client, &registered).await?,
        "the rejected batch should not have created the registered account"
    );

    Ok(())
}
