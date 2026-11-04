use clap::Parser;
use miden_client::asset::AssetCallbackFlag;
use miden_client::keystore::Keystore;
use miden_client::sync::SyncSummary;
use miden_client::{Client, ClientError};

use crate::errors::CliError;

#[derive(Debug, Parser, Clone)]
#[command(about = "Sync this client with the latest state of the Miden network")]
pub struct SyncCmd {}

impl SyncCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
        cache_fee_faucet: bool,
    ) -> Result<(), CliError> {
        let new_details = sync_state(&mut client, cache_fee_faucet).await?;

        println!("State synced to block {}", new_details.block_num);
        println!("New public notes: {}", new_details.new_public_notes.len());
        println!("New private notes: {}", new_details.new_private_notes.len());
        println!("Committed notes: {}", new_details.committed_notes.len());
        println!("Tracked notes consumed: {}", new_details.consumed_notes.len());
        println!("Tracked accounts updated: {}", new_details.updated_accounts.len());
        println!("Locked accounts: {}", new_details.locked_accounts.len());
        println!("Committed transactions: {}", new_details.committed_transactions.len());
        Ok(())
    }
}

/// Syncs `client` with the network, then updates the cache of the fee token faucet.
///
/// If `cache_fee_faucet` is true, the client keeps the state and the account witness of the fee
/// token faucet up to date. If it is false, the client stops keeping the witness. A failure of this
/// update does not fail the sync, because the sync itself is complete. The function prints a
/// warning instead.
pub(crate) async fn sync_state<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
    cache_fee_faucet: bool,
) -> Result<SyncSummary, CliError> {
    let summary = client.sync_state().await?;

    if let Err(err) = update_fee_faucet_cache(client, cache_fee_faucet).await {
        eprintln!("Warning: could not update the cache of the fee token faucet: {err}");
    }

    Ok(summary)
}

/// Starts or stops keeping the state and the account witness of the fee token faucet.
///
/// When the fee token faucet has asset callbacks, each transaction that pays a fee calls the
/// faucet. The transaction then needs the faucet state and its account witness. If the client keeps
/// them, each sync fetches the witness one time. Otherwise each transaction fetches the faucet from
/// the node. A faucet without asset callbacks is never called, so the function does not cache it.
///
/// The faucet is added as a watched account, so it shows in the account list. A sync after this one
/// fetches the first witness.
async fn update_fee_faucet_cache<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
    enabled: bool,
) -> Result<(), ClientError> {
    let header = client.get_latest_block_header().await?;
    let protocol_config = client.get_protocol_config(header.protocol_config_commitment()).await?;
    let faucet_id = protocol_config.fee_asset_id().faucet_id();

    if !enabled {
        client.untrack_account_witness(faucet_id).await?;
        return Ok(());
    }

    if faucet_id.asset_callback_flag() == AssetCallbackFlag::Disabled {
        return Ok(());
    }

    if client.get_account_header(faucet_id).await?.is_none() {
        client.import_watched_account_by_id(faucet_id).await?;
    }

    if client.track_account_witness(faucet_id).await? {
        println!(
            "The client now keeps the fee token faucet {faucet_id} up to date. From the next sync, transactions do not fetch it from the node."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use miden_client::Client;
    use miden_client::account::component::{
        Approver,
        AuthSingleSig,
        Authority,
        BasicWallet,
        BurnPolicy,
        Description,
        FungibleFaucet,
        MintPolicy,
        Pausable,
        PausableManager,
        TokenName,
        TokenPolicyManager,
        TransferPolicy,
    };
    use miden_client::account::{Account, AccountBuilder, AccountType};
    use miden_client::asset::{AssetAmount, AssetCallbackFlag, TokenSymbol};
    use miden_client::auth::{AuthSchemeId, AuthSecretKey};
    use miden_client::builder::ClientBuilder;
    use miden_client::keystore::FilesystemKeyStore;
    use miden_client::testing::MockChainBuilder;
    use miden_client::testing::common::create_test_store_path;
    use miden_client::testing::mock::MockRpcApi;
    use miden_client_sqlite_store::ClientBuilderSqliteExt;

    use super::sync_state;

    /// Returns a public fungible faucet. If `callbacks` is true, the faucet has blocklist send and
    /// receive policies, so its account ID has asset callbacks enabled.
    fn faucet(callbacks: bool) -> Account {
        let policies = TokenPolicyManager::builder()
            .active_mint_policy(MintPolicy::allow_all())
            .active_burn_policy(BurnPolicy::allow_all());
        let policies = if callbacks {
            policies
                .active_send_policy(TransferPolicy::empty_basic_blocklist())
                .active_receive_policy(TransferPolicy::empty_basic_blocklist())
                .build()
        } else {
            policies.build()
        };
        let key = AuthSecretKey::new_falcon512_poseidon2();
        let faucet = AccountBuilder::new([9u8; 32])
            .account_type(AccountType::Public)
            .with_component(AuthSingleSig::new(Approver::new(
                key.public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            )))
            .with_component(
                FungibleFaucet::builder()
                    .name(TokenName::new("fee token").unwrap())
                    .symbol(TokenSymbol::try_from("FEE").unwrap())
                    .decimals(6)
                    .max_supply(AssetAmount::from(1_000_000_u32))
                    .description(Description::new("fee token").unwrap())
                    .build()
                    .unwrap(),
            )
            .with_component(BasicWallet)
            .with_component(Authority::AuthControlled)
            .with_components(policies)
            .with_component(Pausable::unpaused())
            .with_component(PausableManager)
            .build_existing()
            .unwrap();

        let expected_flag = if callbacks {
            AssetCallbackFlag::Enabled
        } else {
            AssetCallbackFlag::Disabled
        };
        assert_eq!(faucet.id().asset_callback_flag(), expected_flag);
        faucet
    }

    /// Returns a client on a mock chain that charges fees in the token of `fee_faucet`.
    async fn client_with_fee_faucet(fee_faucet: &Account) -> Client<FilesystemKeyStore> {
        let mut builder = MockChainBuilder::new().fee_faucet_id(fee_faucet.id());
        builder.add_account(fee_faucet.clone()).unwrap();
        let mut chain = builder.build().unwrap();
        chain.prove_next_block().unwrap();
        let rpc = MockRpcApi::new(chain);

        let mut client = ClientBuilder::new()
            .rpc(Arc::new(rpc.clone()))
            .sqlite_store(create_test_store_path())
            .authenticator(Arc::new(FilesystemKeyStore::new(std::env::temp_dir()).unwrap()))
            .build()
            .await
            .unwrap();
        client.ensure_genesis_in_place().await.unwrap();
        client.seed_protocol_config(rpc.protocol_config()).await.unwrap();
        client
    }

    /// A sync adds a fee faucet with asset callbacks as a watched account and keeps its witness.
    /// Later syncs keep it.
    #[tokio::test]
    async fn sync_caches_a_fee_faucet_that_has_callbacks() {
        let fee_faucet = faucet(true);
        let mut client = client_with_fee_faucet(&fee_faucet).await;

        sync_state(&mut client, true).await.unwrap();
        assert!(client.get_account_header(fee_faucet.id()).await.unwrap().is_some());
        assert_eq!(client.tracked_account_witnesses().await.unwrap(), [fee_faucet.id()]);

        sync_state(&mut client, true).await.unwrap();
        assert_eq!(client.tracked_account_witnesses().await.unwrap(), [fee_faucet.id()]);
    }

    /// A sync does not cache a fee faucet without asset callbacks, because no transaction calls it.
    #[tokio::test]
    async fn sync_does_not_cache_a_fee_faucet_without_callbacks() {
        let fee_faucet = faucet(false);
        let mut client = client_with_fee_faucet(&fee_faucet).await;

        sync_state(&mut client, true).await.unwrap();
        assert!(client.get_account_header(fee_faucet.id()).await.unwrap().is_none());
        assert!(client.tracked_account_witnesses().await.unwrap().is_empty());
    }

    /// A sync with the cache disabled stops keeping the witness of the fee faucet.
    #[tokio::test]
    async fn sync_stops_caching_the_fee_faucet_when_the_cache_is_disabled() {
        let fee_faucet = faucet(true);
        let mut client = client_with_fee_faucet(&fee_faucet).await;
        sync_state(&mut client, true).await.unwrap();
        assert_eq!(client.tracked_account_witnesses().await.unwrap(), [fee_faucet.id()]);

        sync_state(&mut client, false).await.unwrap();
        assert!(client.tracked_account_witnesses().await.unwrap().is_empty());
    }
}
