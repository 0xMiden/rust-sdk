use std::sync::Arc;

use miden_client::account::AccountId;
use miden_client::auth::{AuthSchemeId, AuthSingleSig, PublicKeyCommitment};
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
use miden_client::testing::common::create_test_store_path;
use miden_client::testing::mock::MockRpcApi;
use miden_client::{Client, ClientError, ErrorHint, Word};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::Account;
use miden_protocol::crypto::rand::RandomCoin;
use miden_protocol::{EMPTY_WORD, ZERO};
use miden_standards::account::auth::Approver;
use miden_standards::testing::mock_account::MockAccountExt;
use miden_testing::MockChain;

use super::ACCOUNT_ID_REGULAR;

const INVITATION_CODE: &str = "Mi-DEN-1234";

fn account_id() -> AccountId {
    AccountId::try_from(ACCOUNT_ID_REGULAR).unwrap()
}

/// Builds an account that was never created on chain, which is the only kind `add_account` accepts
/// an invitation code for.
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

/// The node treats a repeat with the same code and account as a no-op, so a caller that registers
/// the same binding twice gets a success both times.
#[tokio::test]
async fn register_account_accepts_a_repeated_binding() {
    let rpc_api = MockRpcApi::new(MockChain::new());

    rpc_api.register_account(INVITATION_CODE, account_id()).await.unwrap();
    rpc_api.register_account(INVITATION_CODE, account_id()).await.unwrap();

    assert_eq!(
        rpc_api.registered_invitation_code(account_id()).as_deref(),
        Some(INVITATION_CODE)
    );
}

// ACCOUNT REGISTRATION THROUGH `add_account`
// ================================================================================================

/// Builds a client whose RPC layer is `rpc_api`.
async fn client_with_rpc(rpc_api: Arc<MockRpcApi>) -> Client<FilesystemKeyStore> {
    let keystore = FilesystemKeyStore::new(std::env::temp_dir()).unwrap();

    ClientBuilder::new()
        .rpc(rpc_api)
        .rng(Box::new(RandomCoin::new(Word::from([0xfeeu32, 1, 2, 3]))))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn add_account_forwards_the_invitation_code() {
    let rpc_api = Arc::new(MockRpcApi::new(MockChain::new()));
    let mut client = client_with_rpc(rpc_api.clone()).await;

    client.add_account(&new_account(), false, Some(INVITATION_CODE)).await.unwrap();

    assert_eq!(
        rpc_api.registered_invitation_code(account_id()).as_deref(),
        Some(INVITATION_CODE)
    );
    assert!(client.get_account_header(account_id()).await.unwrap().is_some());
}

/// Without a code the account is stored and the registration endpoint is never called, which is
/// what leaves every caller that passes `None` unaffected.
#[tokio::test]
async fn add_account_without_an_invitation_code_does_not_register() {
    let rpc_api = Arc::new(MockRpcApi::new(MockChain::new()));
    let mut client = client_with_rpc(rpc_api.clone()).await;

    client.add_account(&new_account(), false, None).await.unwrap();

    assert!(rpc_api.registered_invitation_code(account_id()).is_none());
    assert!(client.get_account_header(account_id()).await.unwrap().is_some());
}

/// The registration runs before the store write, so a refused code leaves no account behind.
#[tokio::test]
async fn add_account_reports_an_unknown_code_and_stores_nothing() {
    let rpc_api = Arc::new(MockRpcApi::new(MockChain::new()));
    rpc_api.fail_next_call(
        RpcEndpoint::RegisterAccount,
        rejection(GrpcError::NotFound, RegisterAccountError::InvitationNotFound),
    );
    let mut client = client_with_rpc(rpc_api.clone()).await;

    let error = client
        .add_account(&new_account(), false, Some(INVITATION_CODE))
        .await
        .unwrap_err();

    let ClientError::RpcError(rpc_error) = &error else {
        panic!("expected an RPC error, got {error:?}");
    };
    assert!(matches!(
        rpc_error.endpoint_error(),
        Some(EndpointError::RegisterAccount(RegisterAccountError::InvitationNotFound))
    ));
    assert!(rpc_api.registered_invitation_code(account_id()).is_none());
    assert!(client.get_account_header(account_id()).await.unwrap().is_none());
}

/// A rejected call leaves no trace, so the same account is added again with another code and
/// without the `overwrite` flag. This is the whole reason the registration precedes the store
/// write.
#[tokio::test]
async fn add_account_can_be_retried_after_a_rejection() {
    let rpc_api = Arc::new(MockRpcApi::new(MockChain::new()));
    rpc_api.fail_next_call(
        RpcEndpoint::RegisterAccount,
        rejection(GrpcError::NotFound, RegisterAccountError::InvitationNotFound),
    );
    let mut client = client_with_rpc(rpc_api.clone()).await;

    client.add_account(&new_account(), false, Some("wrong-code")).await.unwrap_err();
    client.add_account(&new_account(), false, Some(INVITATION_CODE)).await.unwrap();

    assert_eq!(
        rpc_api.registered_invitation_code(account_id()).as_deref(),
        Some(INVITATION_CODE)
    );
    assert!(client.get_account_header(account_id()).await.unwrap().is_some());
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
