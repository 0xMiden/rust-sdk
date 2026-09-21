//! Account allowlist tests against a live node that enforces it.
//!
//! These tests need a node started with `MIDEN_ACCOUNT_ALLOWLIST=1`, which enforces the allowlist
//! and binds the administration API they create invitation codes through. Every other integration
//! test needs the opposite, because enforcement rejects the account creations they all do, so these
//! tests run in their own job against their own node and every other target filters them out. Run
//! them with `make integration-test-allowlist`.
//!
//! [`registration`] covers the `RegisterAccount` endpoint, and [`enforcement`] covers what the node
//! does with a submission once an account is registered or not. [`invitations`] creates the codes
//! both of them register with.
//!
//! What the node enforces, and therefore what these tests pin down:
//!
//! - Only account-creating submissions are checked. An account that already exists is not.
//! - Network accounts are exempt.
//! - A rejected submission comes back as `PermissionDenied`.
//! - An invitation code binds to one account and cannot be reused for another.

use anyhow::{Context, Result};
use assert_matches::assert_matches;
use miden_client::account::component::{Approver, BasicWallet};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountType,
};
use miden_client::auth::{AuthSchemeId, AuthSecretKey, AuthSingleSig};
use miden_client::rpc::{EndpointError, GrpcError, RegisterAccountError, RpcEndpoint, RpcError};
use miden_client::testing::common::*;
use miden_client::transaction::{TransactionRequest, TransactionRequestBuilder};
use miden_client::{ClientError, Felt};

pub mod enforcement;
pub mod invitations;
pub mod registration;

// HELPERS
// ================================================================================================

/// Builds the request for an account's first transaction, which creates it on chain.
///
/// This is the submission the allowlist gates. `TestClient::submit_new_transaction` folds in the
/// account's funding note, so the transaction pays its own fee out of the funds it consumes.
fn deploy_request() -> Result<TransactionRequest> {
    TransactionRequestBuilder::new()
        .build()
        .context("failed to build the deploy transaction request")
}

/// Builds a private wallet and the key its authentication component commits to, without inserting
/// it into the client.
fn build_wallet() -> Result<(Account, AuthSecretKey)> {
    let key = AuthSecretKey::new_falcon512_poseidon2();
    let auth = AuthSingleSig::new(Approver::new(
        key.public_key().to_commitment(),
        AuthSchemeId::Falcon512Poseidon2,
    ));

    // Every call generates its own key, so a fixed initial seed still yields a distinct account.
    let account = AccountBuilder::new(Default::default())
        .account_type(AccountType::Private)
        .with_component(auth)
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .context("failed to build the wallet account")?;

    Ok((account, key))
}

/// Inserts and funds an account built by [`build_wallet`], registering it with `invitation_code`
/// when one is given.
async fn insert_wallet(
    client: &mut TestClient,
    account: Account,
    key: AuthSecretKey,
    invitation_code: Option<&str>,
) -> Result<Account> {
    let mut setup = AccountSetup::prebuilt(account, key);
    if let Some(invitation_code) = invitation_code {
        setup = setup.invitation_code(invitation_code);
    }

    let (account, _) = client.insert_account(setup).await?;

    Ok(account)
}

/// Inserts a funded wallet that has not been created on chain yet, registering it with
/// `invitation_code` when one is given.
async fn insert_undeployed_wallet(
    client: &mut TestClient,
    invitation_code: Option<&str>,
) -> Result<Account> {
    let (account, key) = build_wallet()?;

    insert_wallet(client, account, key, invitation_code)
        .await
        .context("failed to insert the wallet account")
}

/// Returns the [`ClientError`] that `error` wraps.
///
/// [`TestClient::insert_account`] reports through `anyhow`, but the allowlist assertions need the
/// typed error the node returned.
fn client_error(error: &anyhow::Error) -> &ClientError {
    error
        .downcast_ref::<ClientError>()
        .unwrap_or_else(|| panic!("expected a client error, got: {error:?}"))
}

/// Asserts that `error` is the node refusing to create an unregistered account.
fn assert_rejected_as_unregistered(error: &ClientError, endpoint: RpcEndpoint) {
    assert_matches!(
        error,
        ClientError::RpcError(RpcError::RequestError {
            endpoint: actual_endpoint,
            error_kind: GrpcError::PermissionDenied,
            ..
        }) if actual_endpoint.proto_name() == endpoint.proto_name(),
        "expected the node to reject the account creation as unregistered, got: {error}"
    );
}

/// Asserts that `error` is the given rejection of a registration request.
fn assert_registration_rejected(error: &ClientError, expected: &RegisterAccountError) {
    assert_matches!(
        error,
        ClientError::RpcError(RpcError::RequestError {
            endpoint: RpcEndpoint::RegisterAccount,
            endpoint_error: Some(EndpointError::RegisterAccount(actual)),
            ..
        }) if actual == expected,
        "expected the registration to be rejected with {expected}, got: {error}"
    );
}

/// Returns whether the account has been created on chain. A zero nonce marks an account that has
/// never transacted.
async fn is_deployed(client: &TestClient, account: &Account) -> Result<bool> {
    let nonce = client
        .account_reader(account.id())
        .nonce()
        .await
        .with_context(|| format!("account {} is not tracked by the client", account.id()))?;

    Ok(nonce != Felt::ZERO)
}
