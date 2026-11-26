use alloc::sync::Arc;
use std::error::Error;

use miden_client::ClientError;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::{Note, P2idNote};
use miden_client::rpc::domain::account::AccountFetch;
use miden_client::testing::mock::{MockClient, MockRpcApi};
use miden_client::transaction::{
    ForeignAccount,
    IesSealSeed,
    OutputNote,
    TransactionRequest,
    TransactionRequestBuilder,
};
use miden_protocol::Word;
use miden_protocol::account::{AccountBuilder, AccountId, AccountType};
use miden_protocol::asset::{AssetAmount, FungibleAsset};
use miden_protocol::crypto::ecdh::ecgfp5::SecretKey;
use miden_protocol::note::NoteType;
use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE;
use miden_standards::account::access::Authority;
use miden_standards::account::faucets::{FungibleFaucet, TokenName};
use miden_standards::account::policies::{BurnPolicy, MintPolicy, SendPolicy, TokenPolicyManager};
use miden_standards::note::{EncryptedAssetsAttachment, EncryptedAssetsIssuerKey};
use miden_testing::{AccountState, Auth, MockChainBuilder};

use crate::tests::mock_client_builder;

// FIXTURE
// ================================================================================================

/// The amount of the asset that the note holds.
const NOTE_AMOUNT: u64 = 100;

/// A chain with a public faucet whose send policy encrypts the assets of a private note to the
/// issuer, and a wallet that holds the asset of the faucet.
struct EncryptedAssetsFixture {
    client: MockClient<FilesystemKeyStore>,
    wallet_id: AccountId,
    faucet_id: AccountId,
    issuer_secret_key: SecretKey,
}

impl EncryptedAssetsFixture {
    async fn new() -> Self {
        let issuer_secret_key = SecretKey::from_bytes(&[7; 40]).unwrap();
        let policy_manager = TokenPolicyManager::builder()
            .active_mint_policy(MintPolicy::allow_all())
            .active_burn_policy(BurnPolicy::allow_all())
            .active_send_policy(SendPolicy::with_encrypted_assets(EncryptedAssetsIssuerKey::new(
                issuer_secret_key.public_key(),
            )))
            .build();
        let faucet = FungibleFaucet::builder()
            .name(TokenName::new("ENC").unwrap())
            .symbol("ENC".try_into().unwrap())
            .decimals(8)
            .max_supply(AssetAmount::new(1_000_000).unwrap())
            .build()
            .unwrap();

        let mut builder = MockChainBuilder::new();
        let faucet = builder
            .add_account_from_builder(
                Auth::IncrNonce,
                AccountBuilder::new([48; 32])
                    .account_type(AccountType::Public)
                    .with_component(faucet)
                    .with_component(Authority::AuthControlled)
                    .with_components(policy_manager),
                AccountState::Exists,
            )
            .unwrap();
        let wallet = builder
            .add_existing_wallet_with_assets(
                Auth::IncrNonce,
                [FungibleAsset::new(faucet.id(), NOTE_AMOUNT).unwrap().into()],
            )
            .unwrap();
        let rpc_api = MockRpcApi::new(builder.build().unwrap());

        let mut client = mock_client_builder(Arc::new(rpc_api)).await.build().await.unwrap();
        client.add_account(&wallet, false).await.unwrap();
        client.sync_state().await.unwrap();

        Self {
            client,
            wallet_id: wallet.id(),
            faucet_id: faucet.id(),
            issuer_secret_key,
        }
    }

    /// Returns a private P2ID note from the wallet that holds the asset of the faucet.
    fn note(&self) -> Note {
        P2idNote::builder()
            .sender(self.wallet_id)
            .target(AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap())
            .assets([FungibleAsset::new(self.faucet_id, NOTE_AMOUNT).unwrap()])
            .note_type(NoteType::Private)
            .serial_number(Word::from([1u32; 4]))
            .build()
            .unwrap()
            .into()
    }

    /// Returns a request that sends `note`, with `seed` as its IES seal seed if it is set.
    fn send_request(&self, note: Note, seed: Option<IesSealSeed>) -> TransactionRequest {
        let mut builder =
            TransactionRequestBuilder::new().own_output_notes([note]).foreign_accounts([
                ForeignAccount::public(self.faucet_id, AccountFetch::new()).unwrap(),
            ]);
        if let Some(seed) = seed {
            builder = builder.ies_seal_seed(seed);
        }
        builder.build().unwrap()
    }
}

// TESTS
// ================================================================================================

/// A transaction that sends an asset of a faucet with the encrypted assets send policy fails when
/// its request has no IES seal seed.
#[tokio::test]
async fn send_of_encrypted_asset_without_ies_seal_seed_fails() {
    let fixture = Box::pin(EncryptedAssetsFixture::new()).await;
    let request = fixture.send_request(fixture.note(), None);

    let error = Box::pin(fixture.client.execute_transaction(fixture.wallet_id, request))
        .await
        .unwrap_err();

    assert!(matches!(error, ClientError::TransactionExecutorError(_)), "{error:?}");
    let mut messages = vec![error.to_string()];
    let mut source = error.source();
    while let Some(err) = source {
        messages.push(err.to_string());
        source = err.source();
    }
    assert!(
        messages
            .iter()
            .any(|message| message.contains("no IES seal seed for an IES seal request")),
        "the error chain does not report the missing seed: {messages:?}"
    );
}

/// A transaction that sends an asset of a faucet with the encrypted assets send policy executes
/// when its request has an IES seal seed. The prover uses the same seed, and the issuer decrypts
/// the attachment of the proven note.
#[tokio::test]
async fn send_of_encrypted_asset_with_ies_seal_seed_executes_and_proves() {
    let fixture = Box::pin(EncryptedAssetsFixture::new()).await;
    let seed = IesSealSeed::new([3; 32]);
    let note = fixture.note();
    let request = fixture.send_request(note.clone(), Some(seed.clone()));

    let result = Box::pin(fixture.client.execute_transaction(fixture.wallet_id, request))
        .await
        .unwrap();
    assert_eq!(result.transaction_arguments().ies_seal_seed(), Some(&seed));

    let proven = Box::pin(fixture.client.prove_transaction(&result)).await.unwrap();
    let OutputNote::Private(output_note) = proven.output_notes().get_note(0) else {
        panic!("output note should be private");
    };
    let plaintext = EncryptedAssetsAttachment::decrypt_and_verify_note(
        output_note.header(),
        output_note.attachments(),
        &fixture.issuer_secret_key,
    )
    .unwrap();
    assert_eq!(plaintext.serial_num(), note.recipient().serial_num());
    assert_eq!(plaintext.assets(), note.assets());
}
