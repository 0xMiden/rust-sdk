use miden_client::account::AccountType;
use miden_client::asset::AssetId;
use miden_client::block::BlockNumber;
use miden_client::protocol_config::{ProtocolConfig, protocol_config_setting_key};
use miden_client::rpc::NodeRpcClient;
use miden_client::store::{SettingScope, StoreError};
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{ClientError, Serializable, Word};
use miden_protocol::testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2;

use super::{TestClient, create_test_client, create_test_client_builder};

/// Returns the current protocol configuration for `faucet`.
fn protocol_config_for(faucet: u128) -> ProtocolConfig {
    ProtocolConfig::current(AssetId::new_fungible(faucet.try_into().unwrap())).unwrap()
}

#[tokio::test]
async fn the_first_sync_stores_the_protocol_config() {
    let (builder, rpc) = create_test_client_builder().await;
    let mut client = TestClient::from(builder.build().await.unwrap());
    let config = rpc.protocol_config();

    // Nothing seeded this client, so the store starts without the configuration.
    assert!(matches!(
        client.get_protocol_config(config.to_commitment()).await,
        Err(ClientError::StoreError(StoreError::ProtocolConfigNotFound(_)))
    ));

    client.sync_state().await.unwrap();

    assert_eq!(client.get_protocol_config(config.to_commitment()).await.unwrap(), config);
}

#[tokio::test]
async fn seed_genesis_stores_the_genesis_header_and_protocol_config() {
    let (builder, rpc) = create_test_client_builder().await;
    let (genesis, _) =
        rpc.get_block_header_by_number(Some(BlockNumber::GENESIS), false).await.unwrap();
    let protocol_config = rpc.protocol_config();

    let client = builder
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
    let (builder, rpc) = create_test_client_builder().await;
    let (genesis, _) =
        rpc.get_block_header_by_number(Some(BlockNumber::GENESIS), false).await.unwrap();
    let wrong_protocol_config = protocol_config_for(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2);
    assert_ne!(wrong_protocol_config.to_commitment(), genesis.protocol_config_commitment());

    let result = builder.seed_genesis(genesis, wrong_protocol_config).build().await;

    assert!(matches!(result, Err(ClientError::ChainValidationError(_))));
}

#[tokio::test]
async fn storing_a_configuration_does_not_replace_another() {
    let (builder, rpc) = create_test_client_builder().await;
    let mut client = TestClient::from(builder.build().await.unwrap());

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
    client
        .test_store()
        .set_setting(
            SettingScope::Client,
            protocol_config_setting_key(commitment),
            other.to_bytes(),
        )
        .await
        .unwrap();
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
    client
        .test_store()
        .remove_setting(SettingScope::Client, protocol_config_setting_key(commitment))
        .await
        .unwrap();
    let other = protocol_config_for(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2);
    assert_ne!(other.to_commitment(), commitment);
    client.seed_protocol_config(other).await.unwrap();

    let request = TransactionRequestBuilder::new().build().unwrap();
    assert!(matches!(
        client.execute_transaction(wallet.id(), request.clone()).await,
        Err(ClientError::StoreError(StoreError::ProtocolConfigNotFound(missing)))
            if missing == commitment
    ));

    client.seed_protocol_config(config).await.unwrap();
    client.execute_transaction(wallet.id(), request).await.unwrap();
}
