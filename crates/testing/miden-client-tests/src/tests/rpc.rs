use std::sync::Arc;

use miden_client::account::{AccountId, AccountType};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::rpc::{
    EndpointError,
    GrpcError,
    NodeRpcClient,
    RegisterAccountError,
    RpcEndpoint,
    RpcError,
};
use miden_client::testing::mock::MockRpcApi;
use miden_client::transaction::{TransactionRequest, TransactionRequestBuilder};
use miden_client::{Client, ClientError, ErrorHint, Word};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::crypto::rand::RandomCoin;
use miden_testing::MockChain;

use super::{ACCOUNT_ID_REGULAR, create_test_client};

const INVITATION_CODE: &str = "Mi-DEN-1234";

/// Builds the request for an account's first transaction, which creates it on chain.
fn deploy_request() -> TransactionRequest {
    TransactionRequestBuilder::new().build().unwrap()
}

fn account_id() -> AccountId {
    AccountId::try_from(ACCOUNT_ID_REGULAR).unwrap()
}

/// Builds an account that was never created on chain, which is the only kind that takes an
/// invitation code.
fn new_account() -> Account {
    let account = Account::mock(
        ACCOUNT_ID_REGULAR,
        [AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        ))],
    );

    // `Account::mock` returns an account at nonce 1. A new account is at nonce 0 and carries a
    // seed.
    let (id, vault, storage, code, ..) = account.into_parts();

    Account::new_unchecked(id, vault, storage, code, ZERO, Some(Word::default()))
}

fn rejection(error_kind: GrpcError, endpoint_error: RegisterAccountError) -> RpcError {
    RpcError::RequestError {
        endpoint: RpcEndpoint::RegisterAccount,
        error_kind,
        endpoint_error: Some(endpoint_error.into()),
        source: None,
    }
}

/// The node matches the exact text, so trimming or case folding on the way out would turn a valid
/// code into an unknown one.
#[tokio::test]
async fn register_account_sends_the_code_unchanged() {
    let rpc_api = MockRpcApi::new(MockChain::new());

    rpc_api.register_account(INVITATION_CODE, account_id()).await.unwrap();

    assert_eq!(
        rpc_api.registered_invitation_code(account_id()).as_deref(),
        Some(INVITATION_CODE)
    );
}

#[tokio::test]
async fn register_account_reports_an_unknown_code() {
    let rpc_api = MockRpcApi::new(MockChain::new());
    rpc_api.fail_next_call(
        RpcEndpoint::RegisterAccount,
        rejection(GrpcError::NotFound, RegisterAccountError::InvitationNotFound),
    );

    let error = rpc_api.register_account(INVITATION_CODE, account_id()).await.unwrap_err();

    assert!(matches!(
        error.endpoint_error(),
        Some(EndpointError::RegisterAccount(RegisterAccountError::InvitationNotFound))
    ));
    assert!(rpc_api.registered_invitation_code(account_id()).is_none());
}

#[tokio::test]
async fn register_account_reports_a_consumed_code() {
    let rpc_api = MockRpcApi::new(MockChain::new());
    rpc_api.fail_next_call(
        RpcEndpoint::RegisterAccount,
        rejection(GrpcError::AlreadyExists, RegisterAccountError::AlreadyRegistered),
    );

    let error = rpc_api.register_account(INVITATION_CODE, account_id()).await.unwrap_err();

    assert!(matches!(
        error.endpoint_error(),
        Some(EndpointError::RegisterAccount(RegisterAccountError::AlreadyRegistered))
    ));
}

// ACCOUNT REGISTRATION THROUGH `Client::register_account`
// ================================================================================================

#[tokio::test]
async fn client_register_account_forwards_the_invitation_code() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    client.add_account(&new_account(), false).await.unwrap();

    client.register_account(account_id(), INVITATION_CODE).await.unwrap();

    assert_eq!(
        rpc_api.registered_invitation_code(account_id()).as_deref(),
        Some(INVITATION_CODE)
    );
}

/// A rejected registration consumes nothing, so the same account is registered again with another
/// code.
#[tokio::test]
async fn client_register_account_can_be_retried_after_a_rejection() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    client.add_account(&new_account(), false).await.unwrap();
    rpc_api.fail_next_call(
        RpcEndpoint::RegisterAccount,
        rejection(GrpcError::NotFound, RegisterAccountError::InvitationNotFound),
    );

    client.register_account(account_id(), "wrong-code").await.unwrap_err();
    client.register_account(account_id(), INVITATION_CODE).await.unwrap();

    assert_eq!(
        rpc_api.registered_invitation_code(account_id()).as_deref(),
        Some(INVITATION_CODE)
    );
}

// REGISTRATION STATUS CODE MAPPING
// ================================================================================================

/// The node reports a registration decision through these three status codes. A wrong mapping here
/// sends the caller the wrong hint and hides the real reason the node refused.
#[test]
fn register_account_error_maps_the_rejection_status_codes() {
    assert_eq!(
        RegisterAccountError::from_grpc_error(&GrpcError::NotFound, "unknown code"),
        Some(RegisterAccountError::InvitationNotFound)
    );
    assert_eq!(
        RegisterAccountError::from_grpc_error(&GrpcError::AlreadyExists, "taken"),
        Some(RegisterAccountError::AlreadyRegistered)
    );
    assert_eq!(
        RegisterAccountError::from_grpc_error(&GrpcError::InvalidArgument, "empty code"),
        Some(RegisterAccountError::InvalidRequest("empty code".to_string()))
    );
}

/// Only `InvalidRequest` carries the node message. The other two variants are self describing, so a
/// node message must not reach the caller through them.
#[test]
fn register_account_error_keeps_the_node_message_only_for_a_malformed_request() {
    let error = RegisterAccountError::from_grpc_error(&GrpcError::InvalidArgument, "code is empty")
        .expect("InvalidArgument maps to a registration error");

    assert_eq!(error.to_string(), "invalid registration request: code is empty");
}

/// Every other status code reports a transport or node side failure, not a decision about the
/// registration. Mapping one of these would tell the caller the code was refused when the node
/// never judged it.
#[test]
fn register_account_error_ignores_transport_status_codes() {
    for error_kind in [
        GrpcError::Unavailable,
        GrpcError::Internal,
        GrpcError::DeadlineExceeded,
        GrpcError::PermissionDenied,
        GrpcError::ResourceExhausted,
        GrpcError::Unauthenticated,
        GrpcError::Unimplemented,
        GrpcError::Aborted,
        GrpcError::Cancelled,
        GrpcError::FailedPrecondition,
    ] {
        assert_eq!(
            RegisterAccountError::from_grpc_error(&error_kind, "transport failure"),
            None,
            "{error_kind:?} must not map to a registration decision"
        );
    }
}

// REGISTRATION HINTS
// ================================================================================================

/// Returns the hint the client offers for a registration the node rejected.
fn hint_for(endpoint_error: RegisterAccountError) -> String {
    let error = ClientError::RpcError(rejection(GrpcError::NotFound, endpoint_error));

    Option::<ErrorHint>::from(&error)
        .expect("a rejected registration carries a hint")
        .into_help_message()
}

/// A code is case sensitive, so the hint must tell the caller to send it verbatim. Advice to trim
/// or lower case it would turn a valid code into an unknown one.
#[test]
fn register_account_hint_explains_an_unknown_code() {
    let hint = hint_for(RegisterAccountError::InvitationNotFound);

    assert!(hint.contains("case-sensitive"), "{hint}");
    assert!(hint.contains("exactly as you received it"), "{hint}");
}

/// A code binds to one account. The hint must cover both readings, because the caller cannot tell
/// from the status code which of the two happened.
#[test]
fn register_account_hint_explains_a_consumed_code() {
    let hint = hint_for(RegisterAccountError::AlreadyRegistered);

    assert!(hint.contains("registered to a different account"), "{hint}");
    assert!(hint.contains("already registered"), "{hint}");
}

#[test]
fn register_account_hint_explains_a_malformed_request() {
    let hint = hint_for(RegisterAccountError::InvalidRequest("code is empty".to_string()));

    assert!(hint.contains("invitation code is not empty"), "{hint}");
    assert!(hint.contains("account ID is correct"), "{hint}");
}

/// Each rejection needs its own hint. A shared message would send the caller to check the wrong
/// thing, and every hint must point at the troubleshooting page.
#[test]
fn register_account_hints_are_distinct_and_link_the_docs() {
    let hints = [
        hint_for(RegisterAccountError::InvitationNotFound),
        hint_for(RegisterAccountError::AlreadyRegistered),
        hint_for(RegisterAccountError::InvalidRequest(String::new())),
    ];

    for hint in &hints {
        assert!(hint.contains("cli-troubleshooting"), "{hint}");
    }

    assert_ne!(hints[0], hints[1]);
    assert_ne!(hints[1], hints[2]);
    assert_ne!(hints[0], hints[2]);
}

// ALLOWLIST INSPECTION
// ================================================================================================

/// The mock answers from the registrations it recorded, so the two endpoints agree with each other.
#[tokio::test]
async fn is_account_allowed_follows_the_registrations() {
    let rpc_api = MockRpcApi::new(MockChain::new());
    rpc_api.enforce_account_allowlist();

    assert!(!rpc_api.is_account_allowed(account_id()).await.unwrap());

    rpc_api.register_account(INVITATION_CODE, account_id()).await.unwrap();

    assert!(rpc_api.is_account_allowed(account_id()).await.unwrap());
}

/// A node that does not enforce the allowlist answers `true` for an account it has never seen.
#[tokio::test]
async fn is_account_allowed_is_true_when_the_node_does_not_enforce() {
    let rpc_api = MockRpcApi::new(MockChain::new());

    assert!(rpc_api.is_account_allowed(account_id()).await.unwrap());
}

// ALLOWLIST CHECK BEFORE SUBMISSION
// ================================================================================================

/// An unregistered account is refused before the transaction is proven.
#[tokio::test]
async fn creating_an_unregistered_account_is_refused_before_proving() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    rpc_api.enforce_account_allowlist();

    let account = client.insert_wallet(AccountType::Private).await.unwrap();

    let error = Box::pin(client.submit_new_transaction(account.id(), deploy_request()))
        .await
        .unwrap_err();

    let ClientError::AccountNotAllowlisted(account_id) = &error else {
        panic!("expected the account creation to be refused, got: {error}");
    };
    assert_eq!(*account_id, account.id());
}

/// A registered account is created as usual.
#[tokio::test]
async fn creating_a_registered_account_is_allowed() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    rpc_api.enforce_account_allowlist();

    let account = client.insert_wallet(AccountType::Private).await.unwrap();
    client.register_account(INVITATION_CODE, account.id()).await.unwrap();

    Box::pin(client.submit_new_transaction(account.id(), deploy_request()))
        .await
        .unwrap();
}

/// The allowlist only gates account creation, so an account that already exists is never asked
/// about. Enforcement is on and the account is not registered, yet its second transaction goes
/// through.
#[tokio::test]
async fn an_existing_account_is_not_checked() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;

    let account = client.insert_wallet(AccountType::Private).await.unwrap();
    Box::pin(client.submit_new_transaction(account.id(), deploy_request()))
        .await
        .unwrap();
    rpc_api.prove_block();
    client.sync_state().await.unwrap();

    rpc_api.enforce_account_allowlist();

    Box::pin(client.submit_new_transaction(account.id(), deploy_request()))
        .await
        .unwrap();
}

/// A node that cannot answer is not an answer about the account, so the transaction is left alone
/// and the node decides at submission. This is what keeps the client working against a node that
/// does not serve the endpoint.
#[tokio::test]
async fn a_failed_check_does_not_block_the_transaction() {
    let (mut client, rpc_api) = Box::pin(create_test_client()).await;
    rpc_api.enforce_account_allowlist();
    rpc_api.fail_next_call(
        RpcEndpoint::IsAccountAllowed,
        RpcError::RequestError {
            endpoint: RpcEndpoint::IsAccountAllowed,
            error_kind: GrpcError::Unimplemented,
            endpoint_error: None,
            source: None,
        },
    );

    let account = client.insert_wallet(AccountType::Private).await.unwrap();

    Box::pin(client.submit_new_transaction(account.id(), deploy_request()))
        .await
        .unwrap();
}
