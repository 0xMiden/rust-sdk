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
//! - An invitation code binds to one account and cannot be reused for another.

use anyhow::{Context, Result};
use assert_matches::assert_matches;
use miden_client::account::{Account, AccountType};
use miden_client::rpc::{EndpointError, RegisterAccountError, RpcEndpoint, RpcError};
use miden_client::testing::common::*;
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

/// Inserts a funded wallet that has not been created on chain yet.
async fn insert_undeployed_wallet(client: &mut TestClient) -> Result<Account> {
    let (account, _) = client
        .insert_account(AccountSetup::wallet(AccountType::Private))
        .await
        .context("failed to insert the wallet account")?;

    Ok(account)
}

/// Asserts that `error` is the client refusing to create an unregistered account.
///
/// The client asks the node before it proves the transaction, so the account is never submitted.
fn assert_rejected_before_submission(error: &ClientError, account: &Account) {
    assert_matches!(
        error,
        ClientError::AccountNotAllowlisted(account_id) if *account_id == account.id(),
        "expected the client to refuse to create the unregistered account, got: {error}"
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
