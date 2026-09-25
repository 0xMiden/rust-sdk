//! Registering an account on the network allowlist.
//!
//! These cover the `RegisterAccount` endpoint itself: which codes it accepts, which it refuses, and
//! what a refusal leaves behind. What the node then does with a submission is in
//! [`super::enforcement`].

use anyhow::{Context, Result, ensure};
use miden_client::rpc::RegisterAccountError;

use super::funding::request_funds;
use super::invitations::create_invitation_code;
use super::{
    assert_registration_rejected,
    assert_rejected_before_submission,
    funded_deploy_request,
    funding_notes,
    insert_unfunded_wallet,
};
use crate::ClientConfig;

/// A code the node was never given. Long enough that it cannot collide with a created code.
const UNKNOWN_INVITATION_CODE: &str = "miden-client-test-invitation-that-was-never-seeded";

/// A code the node does not know is refused, and refusing it consumes nothing. The same account is
/// then registered with a real code and deploys.
pub async fn test_allowlist_unknown_code_is_rejected(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let account = insert_unfunded_wallet(&mut client, None).await?;

    let error = client
        .register_account(account.id(), UNKNOWN_INVITATION_CODE)
        .await
        .expect_err("the node should not know this invitation code");
    assert_registration_rejected(&error, &RegisterAccountError::InvitationNotFound);

    let invitation_code = create_invitation_code().await?;
    client
        .register_account(account.id(), &invitation_code)
        .await
        .context("a rejected registration should leave the account registerable")?;

    // Only the accepted registration is paid. A payment for the refused one would show as a second
    // note.
    let notes = funding_notes(&mut client, &account).await?;
    ensure!(
        notes.len() == 1,
        "only the accepted registration should fund the account, got {} notes",
        notes.len()
    );

    let deploy = funded_deploy_request(&mut client, &account).await?;
    let transaction_id = client.submit_new_transaction(account.id(), deploy).await?;
    client.wait_for_tx(transaction_id).await?;

    Ok(())
}

/// An invitation code binds to one account and cannot be used for another.
pub async fn test_allowlist_code_is_single_use(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let invitation_code = create_invitation_code().await?;
    insert_unfunded_wallet(&mut client, Some(&invitation_code))
        .await
        .context("failed to register the first account")?;

    let second = insert_unfunded_wallet(&mut client, None).await?;
    let error = client
        .register_account(second.id(), &invitation_code)
        .await
        .expect_err("a code already bound to an account should not register another");
    assert_registration_rejected(&error, &RegisterAccountError::AlreadyRegistered);

    // The second account is still unregistered, so the node refuses to create it on chain. It is
    // funded, so the only thing that stops the deploy is the allowlist.
    request_funds(&second).await?;
    let deploy = funded_deploy_request(&mut client, &second).await?;
    let error = client
        .submit_new_transaction(second.id(), deploy)
        .await
        .expect_err("the unregistered second account should not be created");
    assert_rejected_before_submission(&error, &second);

    Ok(())
}
