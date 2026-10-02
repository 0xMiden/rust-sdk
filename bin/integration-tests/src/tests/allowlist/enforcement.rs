//! Enforcement of the allowlist when an account is created on chain.
//!
//! These cover what the node does with a submission that creates an account: it is accepted when
//! the account is registered or is a network account, and refused otherwise. Registering an account
//! in the first place is in [`super::registration`].

use anyhow::{Context, Result, bail};

use super::invitations::create_invitation_code;
use super::{
    assert_rejected_before_submission,
    insert_unfunded_wallet,
    is_deployed,
    registered_deploy_request,
    service_funded_deploy_request,
};
use crate::ClientConfig;
use crate::tests::network_transaction::add_network_counter_contract;

/// A registered account can be created on chain.
///
/// This is the flow the whole allowlist feature exists for: claim a code, add the account with it,
/// and deploy. The deploy pays its fee out of the note the node paid the account at registration.
pub async fn test_allowlist_registered_account_can_deploy(
    client_config: ClientConfig,
) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let invitation_code = create_invitation_code().await?;
    let account = insert_unfunded_wallet(&mut client, Some(&invitation_code))
        .await
        .context("failed to register the account on the network allowlist")?;

    let deploy = registered_deploy_request(&mut client, &account).await?;
    let transaction_id = client
        .submit_new_transaction(account.id(), deploy)
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

    let account = insert_unfunded_wallet(&mut client, None).await?;
    // The account can pay the fee, so the only thing that stops the deploy is the allowlist.
    let deploy = service_funded_deploy_request(&client, &account).await?;
    let error = client
        .submit_new_transaction(account.id(), deploy)
        .await
        .expect_err("an unregistered account should not be created");
    assert_rejected_before_submission(&error, &account);

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
///
/// A network account cannot register, so the node does not pay it. The fee funder of the test
/// client pays it, and the deploy consumes that note.
pub async fn test_allowlist_network_account_needs_no_registration(
    client_config: ClientConfig,
) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let account = add_network_counter_contract(&mut client, &[]).await?;

    let deploy = service_funded_deploy_request(&client, &account).await?;
    let transaction_id = client
        .submit_new_transaction(account.id(), deploy)
        .await
        .context("a network account should deploy without being registered")?;
    client.wait_for_tx(transaction_id).await?;

    assert!(
        is_deployed(&client, &account).await?,
        "a network account should have been created on chain without registration"
    );

    Ok(())
}

/// Every account a batch creates is checked, not only the first.
///
/// A batch that pairs a registered account with an unregistered one is refused at submission. The
/// registered account is pushed first, so the test fails if the check only ever looks at the
/// opening transaction of a batch.
pub async fn test_allowlist_is_enforced_per_batch(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let invitation_code = create_invitation_code().await?;
    let registered = insert_unfunded_wallet(&mut client, Some(&invitation_code))
        .await
        .context("failed to register the account")?;
    let unregistered = insert_unfunded_wallet(&mut client, None).await?;

    // The requests are built before the batch borrows the client, because building the request of
    // the registered account syncs.
    let registered_request = registered_deploy_request(&mut client, &registered).await?;
    let unregistered_request = service_funded_deploy_request(&client, &unregistered).await?;

    let mut batch = client.new_transaction_batch();
    batch.push(registered.id(), registered_request).await?;
    batch.push(unregistered.id(), unregistered_request).await?;

    let Err(error) = batch.submit().await else {
        bail!("a batch creating an unregistered account should be refused")
    };
    assert_rejected_before_submission(&error, &unregistered);

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
