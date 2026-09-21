//! Registering an account on the network allowlist.
//!
//! These cover the `RegisterAccount` endpoint itself: which codes it accepts, which it refuses, and
//! what a refusal leaves behind. [`Client::add_account`] is the only caller, and it registers the
//! account before it writes it to the store. What the node then does with a submission is in
//! [`super::enforcement`].

use anyhow::{Context, Result};
use miden_client::rpc::{RegisterAccountError, RpcEndpoint};

use super::invitations::create_invitation_code;
use super::{
    assert_registration_rejected,
    assert_rejected_as_unregistered,
    build_wallet,
    client_error,
    deploy_request,
    insert_undeployed_wallet,
    insert_wallet,
};
use crate::ClientConfig;

/// A code the node was never given. Long enough that it cannot collide with a created code.
const UNKNOWN_INVITATION_CODE: &str = "miden-client-test-invitation-that-was-never-seeded";

/// A code the node does not know is refused, and refusing it consumes nothing.
///
/// The registration runs before the store write, so the refusal also leaves the account untracked.
/// The same account is then added again with a real code and deploys.
pub async fn test_allowlist_unknown_code_is_rejected(client_config: ClientConfig) -> Result<()> {
    let mut client = client_config.into_client().await?;
    client.wait_for_node().await;

    let (account, key) = build_wallet()?;

    let error =
        insert_wallet(&mut client, account.clone(), key.clone(), Some(UNKNOWN_INVITATION_CODE))
            .await
            .expect_err("the node should not know this invitation code");
    assert_registration_rejected(client_error(&error), &RegisterAccountError::InvitationNotFound);

    assert!(
        client.get_account_header(account.id()).await?.is_none(),
        "a refused registration should leave no account tracked"
    );

    // The rejection consumed nothing, so a real code still registers the same account.
    let invitation_code = create_invitation_code().await?;
    insert_wallet(&mut client, account.clone(), key, Some(&invitation_code))
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

    let invitation_code = create_invitation_code().await?;
    insert_undeployed_wallet(&mut client, Some(&invitation_code))
        .await
        .context("failed to register the first account")?;

    let (second, second_key) = build_wallet()?;
    let error =
        insert_wallet(&mut client, second.clone(), second_key.clone(), Some(&invitation_code))
            .await
            .expect_err("a code already bound to an account should not register another");
    assert_registration_rejected(client_error(&error), &RegisterAccountError::AlreadyRegistered);

    // The refusal left the second account untracked, so it is added again without a code. It is
    // still unregistered, so the node refuses to create it on chain.
    let second = insert_wallet(&mut client, second, second_key, None).await?;

    let error = client
        .submit_new_transaction(second.id(), deploy_request()?)
        .await
        .expect_err("the node should refuse to create the unregistered second account");
    assert_rejected_as_unregistered(&error, RpcEndpoint::SubmitProvenTx);

    Ok(())
}
