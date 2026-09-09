use miden_client::account::AccountType;
use miden_client::asset::AssetId;
use miden_client::protocol_config::ProtocolConfig;
use miden_client::store::{SettingScope, StoreError};
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{ClientError, Serializable, Word};
use miden_protocol::testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2;

use super::{create_test_client, insert_new_wallet};

#[tokio::test]
async fn protocol_configs_are_selected_by_commitment() {
    let (client, rpc, _) = create_test_client().await;
    let original = rpc.protocol_config();
    let other = ProtocolConfig::current(AssetId::new_fungible(
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2.try_into().unwrap(),
    ))
    .unwrap();
    client.add_protocol_config(other.clone()).await.unwrap();
    assert_eq!(client.get_protocol_config(original.to_commitment()).await.unwrap(), original);
    assert_eq!(client.get_protocol_config(other.to_commitment()).await.unwrap(), other);
    assert!(matches!(
        client.get_protocol_config(Word::empty()).await,
        Err(ClientError::StoreError(StoreError::ProtocolConfigNotFound(_)))
    ));
    assert!(
        client
            .list_setting_keys()
            .await
            .unwrap()
            .iter()
            .all(|key| !key.starts_with("protocol_config:"))
    );
}

#[tokio::test]
async fn protocol_config_rejects_a_substituted_preimage() {
    let (mut client, rpc, _) = create_test_client().await;
    let commitment = rpc.protocol_config().to_commitment();
    let other = ProtocolConfig::current(AssetId::new_fungible(
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2.try_into().unwrap(),
    ))
    .unwrap();
    assert_ne!(other.to_commitment(), commitment);
    client
        .test_store()
        .set_setting(
            SettingScope::Client,
            format!("protocol_config:{commitment}"),
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
    let (mut client, rpc, keystore) = create_test_client().await;
    client.sync_state().await.unwrap();
    let wallet = insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();
    let config = rpc.protocol_config();
    let commitment = config.to_commitment();
    client
        .test_store()
        .remove_setting(SettingScope::Client, format!("protocol_config:{commitment}"))
        .await
        .unwrap();
    let other = ProtocolConfig::current(AssetId::new_fungible(
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2.try_into().unwrap(),
    ))
    .unwrap();
    assert_ne!(other.to_commitment(), commitment);
    client.add_protocol_config(other).await.unwrap();

    let request = TransactionRequestBuilder::new().build().unwrap();
    assert!(matches!(
        client.execute_transaction(wallet.id(), request.clone()).await,
        Err(ClientError::StoreError(StoreError::ProtocolConfigNotFound(missing)))
            if missing == commitment
    ));

    client.add_protocol_config(config).await.unwrap();
    client.execute_transaction(wallet.id(), request).await.unwrap();
}
