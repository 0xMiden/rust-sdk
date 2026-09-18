//! Registering an account on the network allowlist.
//!
//! These cover the `RegisterAccount` endpoint itself: which codes it accepts, which it refuses, and
//! what a refusal leaves behind. What the node then does with a submission is in
//! [`super::enforcement`].

use anyhow::{Context, Result};
use miden_client::rpc::RegisterAccountError;

use super::invitations::InvitationPool;
use super::{
    assert_registration_rejected,
    assert_rejected_before_submission,
    deploy_request,
    insert_undeployed_wallet,
};
use crate::ClientConfig;

/// A code the node was never seeded with. Long enough that it cannot collide with the pool.
const UNKNOWN_INVITATION_CODE: &str = "miden-client-test-invitation-that-was-never-seeded";

/// A code the node does not know is refused, and refusing it consumes nothing.
///
/// The account stays registerable afterwards, which is what lets the CLI tell the user to retry
/// with `account --register`.
pub async fn test_allowlist_unknown_code_is_rejected(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let account = insert_undeployed_wallet(&mut client).await?;

    let error = client
        .register_account(UNKNOWN_INVITATION_CODE, account.id())
        .await
        .expect_err("the node should not know this invitation code");
    assert_registration_rejected(&error, &RegisterAccountError::InvitationNotFound);

    // The rejection consumed nothing, so a real code still registers the same account.
    let invitation_code = InvitationPool::from_env()?.claim()?;
    client
        .register_account(&invitation_code, account.id())
        .await
        .context("a rejected registration should leave the account registerable")?;

    let transaction_id = client.submit_new_transaction(account.id(), deploy_request()?).await?;
    client.wait_for_tx(transaction_id).await?;

    Ok(())
}

/// An invitation code binds to one account and cannot be used for another.
pub async fn test_allowlist_code_is_single_use(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let invitation_code = InvitationPool::from_env()?.claim()?;
    let first = insert_undeployed_wallet(&mut client).await?;
    let second = insert_undeployed_wallet(&mut client).await?;

    client
        .register_account(&invitation_code, first.id())
        .await
        .context("failed to register the first account")?;

    let error = client
        .register_account(&invitation_code, second.id())
        .await
        .expect_err("a code already bound to an account should not register another");
    assert_registration_rejected(&error, &RegisterAccountError::AlreadyRegistered);

    // The second account was never registered, so it still cannot be created on chain.
    let error = client
        .submit_new_transaction(second.id(), deploy_request()?)
        .await
        .expect_err("the unregistered second account should not be created");
    assert_rejected_before_submission(&error, &second);

    Ok(())
}
