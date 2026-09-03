//! Tests for transaction fee payment.
//!
//! Fees are charged inside the authentication procedure, so a signature-authenticated account only
//! transacts on a fee-charging chain when the request commits fee conversion info. These tests run
//! against a `MockChain` with a non-zero `verification_base_fee`, which is the only switch that
//! turns fee collection on.

use std::env::temp_dir;
use std::sync::Arc;

use miden_client::ClientError;
use miden_client::account::component::{FeeConversionInfo, commit_fee_conversion_info};
use miden_client::account::{Account, AccountComponentInterface, AccountId};
use miden_client::asset::{Asset, FungibleAsset};
use miden_client::auth::{AuthSchemeId, AuthSecretKey};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::store::NoteFilter;
use miden_client::sync::NoteTagSource;
use miden_client::testing::common::{TestClient, create_test_store_path};
use miden_client::testing::mock::MockRpcApi;
use miden_client::transaction::{
    TransactionExecutorError,
    TransactionRequestBuilder,
    TransactionRequestError,
};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::{AccountBuilder, AccountComponent, AccountType};
use miden_protocol::crypto::rand::RandomCoin;
use miden_protocol::testing::account_id::ACCOUNT_ID_FEE_FAUCET;
use miden_protocol::{Felt, Word};
use miden_standards::account::AccountBuilderSchemaCommitmentExt;
use miden_standards::account::auth::{
    Approver,
    ApproverSet,
    AuthMultisig,
    AuthMultisigConfig,
    AuthSingleSig,
};
use miden_standards::account::wallets::BasicWallet;
use miden_standards::note::TxFeeNote;
use miden_standards::testing::note::NoteBuilder;
use miden_testing::{Auth, MockChain, MockChainBuilder};

use super::seed_mock_transaction_encryption_key;

/// Base fee used by the protocol's own fee-payment tests. Large enough that the computed fee is
/// non-zero, which is what forces the conversion info to be present.
const VERIFICATION_BASE_FEE: u32 = 500;

/// Balance of the fee asset given to the paying account. `pay_fee` withdraws from the vault, so an
/// account that does not hold the fee asset cannot transact at all.
const FEE_ASSET_BALANCE: u64 = 1_000_000;

/// Builds a fee-charging chain holding one singlesig wallet funded with `balance` of the fee asset.
fn fee_charging_chain(balance: u64) -> (MockChain, Account, AccountId) {
    let fee_faucet_id: AccountId = ACCOUNT_ID_FEE_FAUCET.try_into().unwrap();
    let fee_asset: Asset = FungibleAsset::new(fee_faucet_id, balance).unwrap().into();

    let mut builder = MockChainBuilder::new().verification_base_fee(VERIFICATION_BASE_FEE);
    let account = builder
        .add_existing_wallet_with_assets(
            Auth::BasicAuth {
                auth_scheme: AuthSchemeId::Falcon512Poseidon2,
            },
            [fee_asset],
        )
        .unwrap();
    let chain = builder.build().unwrap();

    (chain, account, fee_faucet_id)
}

/// The native conversion info commitment, salted with a caller-declared salt the way
/// `TransactionRequestBuilder::fee_conversion_salt` declares one, is accepted by the auth
/// procedure, and the resulting transaction emits the fee note.
#[tokio::test]
async fn fee_conversion_info_pays_the_transaction_fee() {
    let (chain, account, fee_faucet_id) = fee_charging_chain(FEE_ASSET_BALANCE);
    let salt = Word::from([1u32, 2, 3, 4]);

    // The builder records the declared salt only. The client commits the native conversion info
    // under it when preparing the transaction, which is reproduced by hand here because the
    // `MockChain` executes without a `Client`.
    let request = TransactionRequestBuilder::new().fee_conversion_salt(salt).build().unwrap();
    assert_eq!(
        request.fee_conversion_salt(),
        Some(salt),
        "the request should carry the declared salt for the client to commit under"
    );

    let (expected_auth_arg, preimage) =
        commit_fee_conversion_info(FeeConversionInfo::one_to_one(fee_faucet_id), salt);

    let executed = Box::pin(
        chain
            .build_transaction(account.id())
            .auth_args(expected_auth_arg)
            .add_advice_map_entry(expected_auth_arg, preimage)
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();

    // Paying a non-zero fee creates exactly one output note, the fee note, funded from the vault.
    assert_eq!(
        executed.output_notes().num_notes(),
        1,
        "a fee-paying transaction should emit the fee note"
    );
}

/// Without committed conversion info the auth procedure aborts, so a request that omits it cannot
/// be executed on a fee-charging chain. This is why the client commits it when preparing every
/// transaction once a chain charges anything.
#[tokio::test]
async fn transaction_without_fee_conversion_info_is_rejected() {
    let (chain, account, _) = fee_charging_chain(FEE_ASSET_BALANCE);

    let result = Box::pin(chain.build_transaction(account.id()).build().unwrap().execute()).await;

    let Err(err) = result else {
        panic!("a fee-charging chain should reject an empty auth arg");
    };
    assert!(
        format!("{err:?}").contains("conversion info"),
        "expected a missing-conversion-info abort, got: {err:?}"
    );
}

/// An account holding none of the fee asset can still pay, provided the transaction consumes a note
/// carrying it: note scripts run before the authentication procedure, so the credit lands in the
/// vault before `pay_fee` withdraws from it.
///
/// This is what makes a fee-charging chain usable without pre-funding vaults at genesis: a fresh
/// account's first transaction can consume a mint note and settle its own fee from the proceeds.
#[tokio::test]
async fn fee_can_be_paid_from_a_note_consumed_in_the_same_transaction() {
    let mut builder = MockChainBuilder::new().verification_base_fee(VERIFICATION_BASE_FEE);
    // Deliberately no assets: the account starts unable to pay anything.
    let account = builder
        .add_existing_wallet(Auth::BasicAuth {
            auth_scheme: AuthSchemeId::Falcon512Poseidon2,
        })
        .unwrap();
    let funding_note = builder.add_p2id_note_with_fee(account.id(), FEE_ASSET_BALANCE).unwrap();
    let chain = builder.build().unwrap();

    let salt = Word::from([9u32, 10, 11, 12]);
    let (auth_arg, preimage) =
        commit_fee_conversion_info(FeeConversionInfo::one_to_one(chain.fee_faucet_id()), salt);

    let executed = Box::pin(
        chain
            .build_transaction(account.id())
            .authenticated_input_notes([funding_note.id()])
            .auth_args(auth_arg)
            .add_advice_map_entry(auth_arg, preimage)
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();

    // One output note, the fee note, paid out of the funds the consumed note just delivered.
    assert_eq!(
        executed.output_notes().num_notes(),
        1,
        "the fee should be paid from the assets the consumed note delivered"
    );
}

/// An account that does not hold the fee asset cannot pay, even with correctly committed
/// conversion info: `pay_fee` withdraws the fee from the account vault.
#[tokio::test]
async fn fee_payment_fails_without_fee_asset_balance() {
    let (chain, account, fee_faucet_id) = fee_charging_chain(0);
    let salt = Word::from([5u32, 6, 7, 8]);
    let (auth_arg, preimage) =
        commit_fee_conversion_info(FeeConversionInfo::one_to_one(fee_faucet_id), salt);

    let result = Box::pin(
        chain
            .build_transaction(account.id())
            .auth_args(auth_arg)
            .add_advice_map_entry(auth_arg, preimage)
            .build()
            .unwrap()
            .execute(),
    )
    .await;

    // The withdrawal aborts inside the vault, so the failure surfaces as a kernel assertion rather
    // than as a client-side balance check.
    let TransactionExecutorError::TransactionProgramExecutionFailed(err) = result.unwrap_err()
    else {
        panic!("expected the fee withdrawal to fail while executing the transaction program");
    };
    assert!(
        format!("{err}")
            .contains("amount of the asset in the vault is less than the amount to remove"),
        "expected the vault withdrawal to abort, got: {err:?}"
    );
}

/// Builds a fee-charging chain and a client that can transact on it as `account`.
///
/// The account is built here rather than by the chain builder so that its key can be put in the
/// client's keystore: the client signs with its own authenticator, and a chain-generated key is
/// not reachable from outside the chain.
async fn fee_charging_client() -> (TestClient, Account) {
    let key = AuthSecretKey::new_falcon512_poseidon2();
    let approver =
        Approver::new(key.public_key().to_commitment(), AuthSchemeId::Falcon512Poseidon2);

    Box::pin(fee_charging_client_with_auth(AuthSingleSig::new(approver), key)).await
}

/// Builds a fee-charging chain and a client whose account authenticates through `AuthMultisig`,
/// which reads the auth args as conversion info but will not accept a salt it did not choose.
async fn fee_charging_multisig_client() -> (TestClient, Account) {
    let key = AuthSecretKey::new_falcon512_poseidon2();
    let approvers = ApproverSet::new(
        vec![Approver::new(
            key.public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )],
        1,
    )
    .unwrap();
    let auth = AuthMultisig::new(AuthMultisigConfig::new(approvers)).unwrap();

    Box::pin(fee_charging_client_with_auth(auth, key)).await
}

async fn fee_charging_client_with_auth(
    auth: impl Into<AccountComponent>,
    key: AuthSecretKey,
) -> (TestClient, Account) {
    let fee_faucet_id: AccountId = ACCOUNT_ID_FEE_FAUCET.try_into().unwrap();
    let fee_asset: Asset = FungibleAsset::new(fee_faucet_id, FEE_ASSET_BALANCE).unwrap().into();

    let mut account = AccountBuilder::new([11u8; 32])
        .account_type(AccountType::Public)
        .with_component(auth)
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .unwrap();
    // A new account's vault has to be built empty, and a nonce of zero marks it undeployed, while
    // the chain takes this one as already on it holding enough of the fee asset to pay.
    account.vault_mut().add_asset(fee_asset).unwrap();
    account.set_nonce(Felt::ONE).unwrap();

    let mut builder = MockChainBuilder::new().verification_base_fee(VERIFICATION_BASE_FEE);
    builder.add_account(account.clone()).unwrap();
    let chain = builder.build().unwrap();

    let keystore = FilesystemKeyStore::new(temp_dir()).unwrap();
    keystore.add_key(&key, account.id()).await.unwrap();

    let mut client = TestClient::from(
        ClientBuilder::new()
            .rpc(Arc::new(MockRpcApi::new(chain)))
            .rng(Box::new(RandomCoin::new(Word::from([0xfeeu32, 1, 2, 3]))))
            .sqlite_store(create_test_store_path())
            .authenticator(Arc::new(keystore))
            .tx_discard_delta(None)
            .build()
            .await
            .unwrap(),
    );
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;
    client.add_account(&account, false).await.unwrap();
    client.sync_state().await.unwrap();

    (client, account)
}

/// The kernel's fee note must NOT be tracked as one of the user's own output notes.
///
/// It is a bearer note for whoever builds the batch. Tracking it would put it in the store, return
/// it from `get_output_notes` as a note the user created, list it in `miden-client notes`, and feed
/// its nullifier prefix into `sync_nullifiers` on every sync -- the client asking the node about a
/// note it does not own, once per fee-paying transaction.
///
/// Asserted against the STORE, not against the executed transaction: the raw output list is
/// supposed to contain the fee note, so only what `apply_transaction` persists distinguishes the
/// two.
#[tokio::test]
async fn the_fee_note_is_not_tracked_as_a_client_note() {
    let (mut client, account) = Box::pin(fee_charging_client()).await;

    let output_notes_before = client.get_output_notes(NoteFilter::All).await.unwrap().len();
    let input_notes_before = client.get_input_notes(NoteFilter::All).await.unwrap().len();
    let note_tags_before = client.get_note_tags().await.unwrap();

    let executed = Box::pin(
        client.execute_transaction(account.id(), TransactionRequestBuilder::new().build().unwrap()),
    )
    .await
    .expect("the client should attach conversion info and pay the fee");

    // Precondition: the chain really charges, so a fee note really is emitted. Without this the
    // assertion below would pass just as well on a fee-free chain and prove nothing.
    assert_eq!(
        executed.executed_transaction().output_notes().num_notes(),
        1,
        "precondition: a fee-paying transaction emits the fee note"
    );

    // `apply_transaction` is the store write under test -- it is what builds the
    // `OutputNoteRecord`s from the executed transaction's output notes.
    let height = client.get_sync_height().await.unwrap();
    client.apply_transaction(&executed, height).await.unwrap();

    let tracked = client.get_output_notes(NoteFilter::All).await.unwrap();
    assert_eq!(
        tracked.len(),
        output_notes_before,
        "the kernel fee note must not be recorded as one of our own output notes"
    );

    let tracked_inputs = client.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(
        tracked_inputs.len(),
        input_notes_before,
        "the kernel fee note must not be recorded as an input note"
    );

    let note_tags = client.get_note_tags().await.unwrap();
    assert_eq!(note_tags, note_tags_before, "the kernel fee note must not register a note tag");
    assert!(!note_tags.iter().any(|record| {
        record.tag == TxFeeNote::TAG && matches!(record.source, NoteTagSource::Note(_))
    }));
}

/// A note whose script the consumption checker cannot classify is screened by trial-executing it,
/// which runs the account's auth procedure and therefore pays the fee. Screening has to commit
/// conversion info of its own for that: without it the trial aborts in `fee::pay_fee`, the checker
/// reports unconsumable conditions, and the note is dropped from the sync as irrelevant.
#[tokio::test]
async fn note_screening_finds_a_custom_script_note_consumable_on_a_fee_charging_chain() {
    let (client, account) = Box::pin(fee_charging_client()).await;

    let script = client
        .code_builder()
        .compile_note_script(super::TARGET_BOUND_NOTE_SCRIPT)
        .unwrap();
    let note = NoteBuilder::new(account.id(), RandomCoin::new(Word::from([7u32, 7, 7, 7])))
        .script(script)
        .note_storage([account.id().suffix(), account.id().prefix().as_felt()])
        .unwrap()
        .build()
        .unwrap();

    let consumability = client.note_screener().get_consumability(&note).await.unwrap();

    assert_eq!(
        consumability.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![account.id()],
        "the target account should be able to consume the note it is bound to"
    );
}

/// `check_notes_consumability` trial-executes the notes it is given the same way screening does, so
/// it needs conversion info of its own on a fee-charging chain. This covers the second injection
/// call site, which the screening test above does not reach.
///
/// It asserts on the failure REASON rather than on `successful`. The screener attaches no
/// authenticator on purpose, and this API has no "consumable once authorized" verdict of the kind
/// `can_consume` produces through `handle_epilogue_error`, so a signature-authenticated account
/// never lands a note in `successful` whatever the fee does. What the injected conversion info
/// decides is how far the trial gets: with it, execution reaches the signature request; without it,
/// it aborts earlier in `fee::pay_fee`.
#[tokio::test]
async fn checking_note_consumability_pays_the_fee_on_a_fee_charging_chain() {
    let (client, account) = Box::pin(fee_charging_client()).await;

    let script = client
        .code_builder()
        .compile_note_script(super::TARGET_BOUND_NOTE_SCRIPT)
        .unwrap();
    let note = NoteBuilder::new(account.id(), RandomCoin::new(Word::from([9u32, 9, 9, 9])))
        .script(script)
        .note_storage([account.id().suffix(), account.id().prefix().as_felt()])
        .unwrap()
        .build()
        .unwrap();

    let consumption_info = client
        .note_screener()
        .check_notes_consumability(account.id(), vec![note.clone()])
        .await
        .expect("checking consumability should not error");

    let failed = consumption_info
        .failed()
        .iter()
        .find(|failed| failed.note().id() == note.id())
        .expect("the note should be accounted for");

    let error = failed.error();
    assert!(
        matches!(error, TransactionExecutorError::MissingAuthenticator),
        "the trial should get as far as requesting a signature rather than aborting on the unpaid \
         fee, got: {error:?}"
    );
}

/// An `AuthMultisig` account cannot inherit the client's fixed salt, because `multisig.masm` reuses
/// the auth args as the transaction summary salt and rejects a summary it has already recorded. The
/// client therefore refuses the request up front rather than letting it abort in the VM.
///
/// The unit tests cover `resolve_fee_conversion_info` directly; this one checks the error reaches a
/// caller of `execute_transaction` intact.
#[tokio::test]
async fn a_multisig_account_is_told_to_declare_its_own_conversion_info() {
    let (mut client, account) = Box::pin(fee_charging_multisig_client()).await;

    let err = Box::pin(
        client.execute_transaction(account.id(), TransactionRequestBuilder::new().build().unwrap()),
    )
    .await
    .expect_err("a multisig request declaring no conversion info should be rejected");

    // Matched on the variant rather than on rendered text. `ClientError::TransactionRequestError`
    // displays as a bare "invalid transaction request", so the inner prose is reachable only
    // through `source()`; and a substring of the Debug rendering cannot tell this variant from its
    // sibling `FeeConversionInfoUnsupported`, which is the very distinction under test.
    match err {
        ClientError::TransactionRequestError(
            TransactionRequestError::FeeConversionInfoRequired(auth_component),
        ) => assert_eq!(auth_component, AccountComponentInterface::AuthMultisig.name()),
        other => panic!("expected FeeConversionInfoRequired(Multisig), got {other:?}"),
    }
}
