//! Registering an account on the network allowlist.
//!
//! These cover the `RegisterAccount` endpoint itself: which codes it accepts, which it refuses, and
//! what a refusal leaves behind. What the node then does with a submission is in
//! [`super::enforcement`].

use anyhow::{Context, Result, ensure};
use miden_client::rpc::RegisterAccountError;

use super::invitations::create_invitation_code;
use super::{
    assert_registration_rejected,
    assert_rejected_before_submission,
    funding_notes,
    insert_unfunded_wallet,
    registered_deploy_request,
    service_funded_deploy_request,
};
use crate::ClientConfig;

/// A code the node was never given. Long enough that it cannot collide with a created code.
const UNKNOWN_INVITATION_CODE: &str = "miden-client-test-invitation-that-was-never-seeded";

/// A code the node does not know is invalid and is refused, and refusing it consumes nothing. The
/// same account is then registered with a real code and is created on chain.
///
/// This is the flow the whole allowlist feature exists for: claim a code, register the account with
/// it, and deploy. The deploy pays its fee out of the note the node paid the account at
/// registration. The test also checks code validation: validation does not consume a code, and
/// registration makes the code invalid.
pub async fn test_allowlist_unknown_code_is_rejected(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;

    ensure!(
        !client.is_invitation_code_valid(UNKNOWN_INVITATION_CODE).await?,
        "an unknown invitation code must be invalid"
    );

    let account = insert_unfunded_wallet(&mut client, None).await?;

    let error = client
        .register_account(account.id(), UNKNOWN_INVITATION_CODE)
        .await
        .expect_err("the node should not know this invitation code");
    assert_registration_rejected(&error, &RegisterAccountError::InvitationNotFound);

    let invitation_code = create_invitation_code().await?;
    ensure!(
        client.is_invitation_code_valid(&invitation_code).await?,
        "an unused invitation code must be valid"
    );
    ensure!(
        client.is_invitation_code_valid(&invitation_code).await?,
        "validation must not consume an invitation code"
    );

    client
        .register_account(account.id(), &invitation_code)
        .await
        .context("a rejected registration should leave the account registerable")?;
    ensure!(
        !client.is_invitation_code_valid(&invitation_code).await?,
        "a registered invitation code must be invalid"
    );

    // Only the accepted registration is paid. A payment for the refused one would show as a second
    // note.
    let notes = funding_notes(&mut client, &account).await?;
    ensure!(
        notes.len() == 1,
        "only the accepted registration should fund the account, got {} notes",
        notes.len()
    );

    let deploy = registered_deploy_request(&mut client, &account).await?;
    let transaction_id = client
        .submit_new_transaction(account.id(), deploy)
        .await
        .context("the node rejected the deploy of a registered account")?;
    client.wait_for_tx(transaction_id).await?;

    ensure!(
        client.is_deployed(account.id()).await?,
        "a registered account should have been created on chain"
    );

    Ok(())
}

/// An invitation code binds to one account and cannot be used for another. The account that the
/// code does not register cannot be created on chain.
///
/// Without the rejection of the unregistered account, a node that silently stopped enforcing the
/// allowlist would still pass the whole suite.
pub async fn test_allowlist_code_is_single_use(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;

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
    let deploy = service_funded_deploy_request(&client, &second).await?;
    let error = client
        .submit_new_transaction(second.id(), deploy)
        .await
        .expect_err("the unregistered second account should not be created");
    assert_rejected_before_submission(&error, &second);

    ensure!(
        !client.is_deployed(second.id()).await?,
        "a rejected account should not have been created on chain"
    );

    Ok(())
}
