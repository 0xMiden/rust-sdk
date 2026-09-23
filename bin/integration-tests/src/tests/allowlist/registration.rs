//! Registering an account on the network allowlist.
//!
//! These cover the `RegisterAccount` endpoint itself: which codes it accepts, which it refuses, and
//! what a refusal leaves behind. What the node then does with a submission is in
//! [`super::enforcement`].

use anyhow::{Context, Result};
use miden_client::account::AccountType;
use miden_client::rpc::RegisterAccountError;
use miden_client::testing::common::AccountSetup;
use miden_client::transaction::TransactionRequestBuilder;

use super::invitations::create_invitation_code;
use super::{assert_registration_rejected, assert_rejected_before_submission};
use crate::ClientConfig;

/// A code the node was never given. Long enough that it cannot collide with a created code.
const UNKNOWN_INVITATION_CODE: &str = "miden-client-test-invitation-that-was-never-seeded";

/// A code the node does not know is refused, and refusing it consumes nothing. The same account is
/// then registered with a real code and deploys.
pub async fn test_allowlist_unknown_code_is_rejected(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let account = client.insert_wallet(AccountType::Private).await?;

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

    client.deploy_account(account.id()).await?;

    Ok(())
}

/// An invitation code binds to one account and cannot be used for another.
pub async fn test_allowlist_code_is_single_use(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let invitation_code = create_invitation_code().await?;
    client
        .insert_account(
            AccountSetup::wallet(AccountType::Private).invitation_code(&invitation_code),
        )
        .await
        .context("failed to register the first account")?;

    let second = client.insert_wallet(AccountType::Private).await?;
    let error = client
        .register_account(second.id(), &invitation_code)
        .await
        .expect_err("a code already bound to an account should not register another");
    assert_registration_rejected(&error, &RegisterAccountError::AlreadyRegistered);

    // The second account is still unregistered, so the node refuses to create it on chain.
    let error = client
        .submit_new_transaction(second.id(), TransactionRequestBuilder::new().build()?)
        .await
        .expect_err("the unregistered second account should not be created");
    assert_rejected_before_submission(&error, &second);

    Ok(())
}
