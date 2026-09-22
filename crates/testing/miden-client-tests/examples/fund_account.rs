//! Funds a new testnet wallet with its first transaction.

use std::sync::Arc;

use miden_client::account::component::{AuthSingleSig, BasicWallet};
use miden_client::account::{
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountId,
    AccountType,
};
use miden_client::asset::AssetId;
use miden_client::auth::{Approver, AuthSchemeId, AuthSecretKey};
use miden_client::builder::ClientBuilder;
use miden_client::funding::FundingOptions;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::protocol_config::ProtocolConfig;
use miden_client_sqlite_store::ClientBuilderSqliteExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let faucet_id = AccountId::from_hex(&std::env::var("MIDEN_FEE_FAUCET_ID")?)?;
    let config = ProtocolConfig::current(AssetId::new_fungible(faucet_id))?;
    let keystore = Arc::new(FilesystemKeyStore::new("funding-keys".into())?);
    let mut client = ClientBuilder::for_testnet()
        .protocol_config(config)
        .sqlite_store("funding.sqlite3".into())
        .authenticator(keystore.clone())
        .build()
        .await?;
    let key = AuthSecretKey::new_falcon512_poseidon2();
    let account = AccountBuilder::new(rand::random())
        .account_type(AccountType::Private)
        .with_component(AuthSingleSig::new(Approver::new(
            key.public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(BasicWallet)
        .build_with_schema_commitment()?;
    keystore.add_key(&key, account.id()).await?;
    client.add_account(&account, false).await?;

    let options = FundingOptions {
        faucet_url: std::env::var("MIDEN_FAUCET_URL").ok(),
        ..FundingOptions::default()
    };
    let funded = client.fund_account(account.id(), &options).await?;
    println!(
        "Note: {}\nTransaction: {}\nBalance: {}",
        funded.note_id, funded.transaction_id, funded.balance
    );
    Ok(())
}
