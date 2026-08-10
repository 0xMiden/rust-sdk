//! Tests for transaction fee payment.
//!
//! Fees are charged inside the authentication procedure, so a signature-authenticated account only
//! transacts on a fee-charging chain when the request commits fee conversion info. These tests run
//! against a `MockChain` with a non-zero `verification_base_fee`, which is the only switch that
//! turns fee collection on.

use miden_client::account::component::{FeeConversionInfo, commit_fee_conversion_info};
use miden_client::account::{Account, AccountId};
use miden_client::asset::{Asset, FungibleAsset};
use miden_client::auth::AuthSchemeId;
use miden_client::transaction::{TransactionExecutorError, TransactionRequestBuilder};
use miden_protocol::Word;
use miden_protocol::testing::account_id::ACCOUNT_ID_FEE_FAUCET;
use miden_testing::{Auth, MockChain, MockChainBuilder};

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

/// `TransactionRequestBuilder::fee_conversion_info` produces an auth arg and advice map entry the
/// auth procedure accepts, and the resulting transaction emits the fee note.
#[tokio::test]
async fn fee_conversion_info_pays_the_transaction_fee() {
    let (chain, account, fee_faucet_id) = fee_charging_chain(FEE_ASSET_BALANCE);
    let salt = Word::from([1u32, 2, 3, 4]);
    let conversion_info = FeeConversionInfo::one_to_one(fee_faucet_id);

    let request = TransactionRequestBuilder::new()
        .fee_conversion_info(conversion_info, salt)
        .build()
        .unwrap();

    // The builder commits the conversion info rather than storing it verbatim, so the auth arg is
    // the commitment and the advice map holds its preimage.
    let (expected_auth_arg, preimage) = commit_fee_conversion_info(conversion_info, salt);
    assert_eq!(
        *request.auth_arg(),
        Some(expected_auth_arg),
        "the request should carry the conversion info commitment as its auth arg"
    );

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
/// be executed on a fee-charging chain. This is why `fee_conversion_info` is mandatory rather than
/// optional once a chain charges anything.
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
