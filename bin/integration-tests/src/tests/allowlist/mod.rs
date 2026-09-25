//! Account allowlist tests against a live node that enforces it.
//!
//! These tests need a node started with `MIDEN_ACCOUNT_ALLOWLIST=1`, which enforces the allowlist,
//! binds the administration API they create invitation codes through, and pays every account that
//! registers through the node's funding service. Every other integration test needs the opposite,
//! because enforcement rejects the account creations they all do, so these tests run in their own
//! job against their own node and every other target filters them out. Run them with `make
//! integration-test-allowlist`.
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
//! - A registered account receives a public note with the native asset, and its deploy consumes that
//!   note to pay its own fee.
//!
//! No account in these tests is paid by the test funder. [`funding`] pays the accounts that do not
//! register, the network account included, through the node's funding service. An unregistered
//! wallet needs the funds to pay the fee of the execution that comes before the allowlist check.

use anyhow::{Context, Result};
use assert_matches::assert_matches;
use miden_client::account::{Account, AccountType};
use miden_client::note::Note;
use miden_client::rpc::{EndpointError, RegisterAccountError, RpcEndpoint, RpcError};
use miden_client::testing::common::*;
use miden_client::transaction::{InputNote, TransactionRequest, TransactionRequestBuilder};
use miden_client::{ClientError, Felt};

pub mod enforcement;
pub mod funding;
pub mod invitations;
pub mod registration;

// CONSTANTS
// ================================================================================================

/// How many blocks a test waits for the funding service to commit a funding note.
const FUNDING_NOTE_MAX_BLOCKS: u32 = 20;

// HELPERS
// ================================================================================================

/// Builds the request for the first transaction of an account, which creates it on chain.
///
/// This is the submission the allowlist gates. The transaction consumes the notes paid to the
/// account, either at registration or through [`funding::request_funds`], so it pays its own fee
/// out of those funds.
async fn funded_deploy_request(
    client: &mut TestClient,
    account: &Account,
) -> Result<TransactionRequest> {
    let notes = funding_notes(client, account).await?;

    TransactionRequestBuilder::new()
        .build_consume_notes(notes)
        .context("failed to build the deploy transaction request")
}

/// Syncs the client until `account` has a committed note it can consume, and returns those notes.
///
/// The funding service answers before the note commits, so the sync repeats until the note is on
/// chain. Panics when no note arrives within [`FUNDING_NOTE_MAX_BLOCKS`] blocks.
///
/// [`Client::add_account`] tracks the note tag of the account, so the sync imports the public notes
/// paid to the account.
///
/// [`Client::add_account`]: miden_client::Client::add_account
async fn funding_notes(client: &mut TestClient, account: &Account) -> Result<Vec<Note>> {
    client
        .wait_for_consumable_notes(account.id(), FUNDING_NOTE_MAX_BLOCKS)
        .await
        .with_context(|| format!("failed to wait for a funding note for account {}", account.id()))?
        .into_iter()
        .map(|(record, _)| {
            let note: InputNote =
                record.try_into().context("a committed note should convert to an input note")?;
            Ok(note.into_note())
        })
        .collect()
}

/// Inserts a private wallet that has not been created on chain yet, registering it with
/// `invitation_code` when one is given.
///
/// The test client does not fund the wallet. A registration makes the node pay the wallet. An
/// unregistered wallet is paid only when a test calls [`funding::request_funds`].
async fn insert_unfunded_wallet(
    client: &mut TestClient,
    invitation_code: Option<&str>,
) -> Result<Account> {
    let mut setup = AccountSetup::wallet(AccountType::Private).unfunded();
    if let Some(invitation_code) = invitation_code {
        setup = setup.invitation_code(invitation_code);
    }

    let (account, _) = client.insert_account(setup).await?;

    Ok(account)
}

/// Asserts that `error` is the client refusing to create an unregistered account.
///
/// The client asks the node before it submits the transaction, so the node never receives it.
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
