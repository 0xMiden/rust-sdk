use std::env::temp_dir;
use std::sync::Arc;

use miden_client::account::AccountType;
use miden_client::asset::{AssetId, FungibleAsset};
use miden_client::block::BlockNumber;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::NoteType;
use miden_client::protocol_config::ProtocolConfig;
use miden_client::rpc::NodeRpcClient;
use miden_client::store::{Store, StoreError};
use miden_client::testing::common::{AccountSetup, create_test_store_path};
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{ClientError, Word};
use miden_client_sqlite_store::SqliteStore;
use miden_protocol::testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2;

use super::{TestClient, create_test_client, open_store_file, prebuilt_rpc_api};

/// Returns the current protocol configuration for `faucet`.
fn protocol_config_for(faucet: u128) -> ProtocolConfig {
    ProtocolConfig::current(AssetId::new_fungible(faucet.try_into().unwrap())).unwrap()
}

#[tokio::test]
async fn the_first_sync_stores_the_genesis_protocol_config() {
    let rpc = prebuilt_rpc_api().await;
    // Nothing is seeded into this client's store.
    let mut client = TestClient::from(
        ClientBuilder::new()
            .rpc(Arc::new(rpc.clone()))
            .store(Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap()))
            .authenticator(Arc::new(FilesystemKeyStore::new(temp_dir()).unwrap()))
            .build()
            .await
            .unwrap(),
    );
    let config = rpc.protocol_config();

    // The store starts without the configuration.
    assert!(matches!(
        client.get_protocol_config(config.to_commitment()).await,
        Err(ClientError::StoreError(StoreError::ProtocolConfigNotFound(_)))
    ));

    client.sync_state().await.unwrap();

    assert_eq!(client.get_protocol_config(config.to_commitment()).await.unwrap(), config);
}

/// A store that has the genesis block header but not its protocol configuration gets the
/// configuration when it ensures genesis.
#[tokio::test]
async fn ensure_genesis_fetches_a_missing_genesis_protocol_config() {
    let rpc = prebuilt_rpc_api().await;
    let store = SqliteStore::new(create_test_store_path()).await.unwrap();
    let (genesis, _) =
        rpc.get_block_header_by_number(Some(BlockNumber::GENESIS), false).await.unwrap();
    store.insert_block_header(&genesis, &[], false).await.unwrap();

    let mut client = TestClient::from(
        ClientBuilder::new()
            .rpc(Arc::new(rpc.clone()))
            .store(Arc::new(store))
            .authenticator(Arc::new(FilesystemKeyStore::new(temp_dir()).unwrap()))
            .build()
            .await
            .unwrap(),
    );
    let commitment = genesis.protocol_config_commitment();
    assert!(matches!(
        client.get_protocol_config(commitment).await,
        Err(ClientError::StoreError(StoreError::ProtocolConfigNotFound(_)))
    ));

    client.ensure_genesis_in_place().await.unwrap();

    assert_eq!(client.get_protocol_config(commitment).await.unwrap(), rpc.protocol_config());
}

/// A fresh client without a stored protocol configuration keeps a public note for its account on
/// the first sync.
#[tokio::test]
async fn first_sync_keeps_account_note_without_protocol_config() {
    let (mut sender, rpc) = Box::pin(create_test_client()).await;

    // The receiver uses the same chain but does not seed the protocol configuration.
    let mut receiver = TestClient::from(
        ClientBuilder::new()
            .rpc(Arc::new(rpc.clone()))
            .store(Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap()))
            .authenticator(Arc::new(FilesystemKeyStore::new(temp_dir()).unwrap()))
            .tx_discard_delta(None)
            .build()
            .await
            .unwrap(),
    );
    let (wallet, _) = receiver
        .insert_account(AccountSetup::wallet(AccountType::Private).unfunded())
        .await
        .unwrap();

    let faucet = sender.insert_faucet(AccountType::Private).await.unwrap();
    rpc.prove_block();
    sender.sync_state().await.unwrap();

    let request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            wallet.id(),
            NoteType::Public,
            sender.rng(),
        )
        .unwrap();
    let note_id = request.expected_output_own_notes().pop().unwrap().id();
    Box::pin(sender.submit_new_transaction(faucet.id(), request)).await.unwrap();
    rpc.prove_block();

    receiver.sync_state().await.unwrap();
    assert!(receiver.get_input_note(note_id).await.unwrap().is_some());
}

#[tokio::test]
async fn seed_genesis_stores_the_genesis_header_and_protocol_config() {
    let rpc = prebuilt_rpc_api().await;
    let (genesis, _) =
        rpc.get_block_header_by_number(Some(BlockNumber::GENESIS), false).await.unwrap();
    let protocol_config = rpc.protocol_config();

    let client = ClientBuilder::new()
        .rpc(Arc::new(rpc.clone()))
        .store(Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap()))
        .authenticator(Arc::new(FilesystemKeyStore::new(temp_dir()).unwrap()))
        .seed_genesis(genesis.clone(), protocol_config.clone())
        .build()
        .await
        .unwrap();

    let (stored, _) = client.get_block_header_by_num(BlockNumber::GENESIS).await.unwrap().unwrap();
    assert_eq!(stored.commitment(), genesis.commitment());
    let expected_protocol_config =
        client.get_protocol_config(protocol_config.to_commitment()).await.unwrap();
    assert_eq!(expected_protocol_config, protocol_config);
}

#[tokio::test]
async fn seed_genesis_rejects_a_protocol_config_the_header_does_not_commit_to() {
    let rpc = prebuilt_rpc_api().await;
    let (genesis, _) =
        rpc.get_block_header_by_number(Some(BlockNumber::GENESIS), false).await.unwrap();

    let builder = ClientBuilder::new()
        .rpc(Arc::new(rpc.clone()))
        .store(Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap()))
        .authenticator(Arc::new(FilesystemKeyStore::new(temp_dir()).unwrap()));

    let wrong_protocol_config = protocol_config_for(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2);
    assert_ne!(wrong_protocol_config.to_commitment(), genesis.protocol_config_commitment());

    let result = builder.seed_genesis(genesis, wrong_protocol_config).build().await;

    assert!(matches!(result, Err(ClientError::ChainValidationError(_))));
}

#[tokio::test]
async fn storing_a_configuration_does_not_replace_another() {
    let rpc = prebuilt_rpc_api().await;
    // Nothing is seeded into this client's store.
    let mut client = TestClient::from(
        ClientBuilder::new()
            .rpc(Arc::new(rpc.clone()))
            .store(Arc::new(SqliteStore::new(create_test_store_path()).await.unwrap()))
            .authenticator(Arc::new(FilesystemKeyStore::new(temp_dir()).unwrap()))
            .build()
            .await
            .unwrap(),
    );

    let held = protocol_config_for(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2);
    client.seed_protocol_config(held.clone()).await.unwrap();
    let returned = rpc.protocol_config();
    assert_ne!(returned.to_commitment(), held.to_commitment());

    client.sync_state().await.unwrap();

    // Both are readable: a configuration is keyed by its own commitment, so storing one never
    // replaces another.
    assert_eq!(client.get_protocol_config(returned.to_commitment()).await.unwrap(), returned);
    assert_eq!(client.get_protocol_config(held.to_commitment()).await.unwrap(), held);
}

#[tokio::test]
async fn protocol_configs_are_selected_by_commitment() {
    let (client, rpc) = create_test_client().await;
    let original = rpc.protocol_config();
    let other = protocol_config_for(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2);
    assert_ne!(other.to_commitment(), original.to_commitment());
    client.seed_protocol_config(other.clone()).await.unwrap();
    assert_eq!(client.get_protocol_config(original.to_commitment()).await.unwrap(), original);
    assert_eq!(client.get_protocol_config(other.to_commitment()).await.unwrap(), other);
    assert!(matches!(
        client.get_protocol_config(Word::empty()).await,
        Err(ClientError::StoreError(StoreError::ProtocolConfigNotFound(_)))
    ));
}

#[tokio::test]
async fn protocol_config_rejects_a_substituted_preimage() {
    let (mut client, rpc) = create_test_client().await;
    let commitment = rpc.protocol_config().to_commitment();
    let other = protocol_config_for(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2);
    assert_ne!(other.to_commitment(), commitment);
    // Copy the stored bytes of `other` under `commitment`, so the store encoding does not matter.
    client.seed_protocol_config(other.clone()).await.unwrap();
    let copied = open_store_file(&mut client)
        .execute(
            "INSERT OR REPLACE INTO client_config (name, value) \
             SELECT ?1, value FROM client_config WHERE name = ?2",
            [
                format!("protocol_config:{commitment}"),
                format!("protocol_config:{}", other.to_commitment()),
            ],
        )
        .unwrap();
    assert_eq!(copied, 1);
    assert!(matches!(
        client.get_protocol_config(commitment).await,
        Err(ClientError::StoreError(StoreError::ProtocolConfigCommitmentMismatch(_)))
    ));
}

#[tokio::test]
async fn execution_requires_the_reference_block_protocol_config() {
    let (mut client, rpc) = create_test_client().await;
    client.sync_state().await.unwrap();
    let wallet = client.insert_wallet(AccountType::Private).await.unwrap();
    let config = rpc.protocol_config();
    let commitment = config.to_commitment();
    let removed = open_store_file(&mut client)
        .execute(
            "DELETE FROM client_config WHERE name = ?1",
            [format!("protocol_config:{commitment}")],
        )
        .unwrap();
    assert_eq!(removed, 1);
    let other = protocol_config_for(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2);
    assert_ne!(other.to_commitment(), commitment);
    client.seed_protocol_config(other).await.unwrap();

    let request = TransactionRequestBuilder::new().build().unwrap();
    // `TestClient::execute_transaction` syncs first, and the sync stores the genesis protocol
    // configuration again. Genesis and the reference block commit to the same configuration here.
    assert!(matches!(
        (*client).execute_transaction(wallet.id(), request.clone()).await,
        Err(ClientError::StoreError(StoreError::ProtocolConfigNotFound(missing)))
            if missing == commitment
    ));

    client.seed_protocol_config(config).await.unwrap();
    client.execute_transaction(wallet.id(), request).await.unwrap();
}
