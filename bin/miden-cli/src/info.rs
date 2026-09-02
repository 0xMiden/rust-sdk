use std::fs;

use miden_client::Client;
use miden_client::account::AccountId;
use miden_client::block::BlockNumber;
use miden_client::keystore::Keystore;
use miden_client::rpc::{GrpcClient, RpcStatusInfo};
use miden_client::store::NoteFilter;

use super::config::CliConfig;
use crate::commands::account::DEFAULT_ACCOUNT_ID_KEY;
use crate::errors::CliError;

pub async fn print_client_info<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    show_rpc_status: bool,
) -> Result<(), CliError> {
    let config = CliConfig::load()?;

    println!("Client version: {}", env!("CARGO_PKG_VERSION"));

    if let Some(config_dir) = &config.config_dir {
        println!("Config directory: {config_dir}");
    }

    // Get and display local genesis commitment
    if let Ok(Some((genesis_header, _))) =
        client.get_block_header_by_num(BlockNumber::GENESIS).await
    {
        println!("Genesis commitment: {}", genesis_header.commitment().to_hex());
    }

    print_config_stats(&config)?;
    print_client_stats(client).await?;

    if show_rpc_status {
        print_rpc_status(&config).await?;
    }

    Ok(())
}

// HELPERS
// ================================================================================================
async fn print_client_stats<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
) -> Result<(), CliError> {
    println!("Block number: {}", client.get_sync_height().await?);
    println!("Tracked accounts: {}", client.get_account_headers().await?.len());
    println!("Expected notes: {}", client.get_input_notes(NoteFilter::Expected).await?.len());
    println!(
        "Default account: {}",
        client
            .get_setting(DEFAULT_ACCOUNT_ID_KEY.to_string())
            .await?
            .map_or("-".to_string(), AccountId::to_hex)
    );
    Ok(())
}

fn print_config_stats(config: &CliConfig) -> Result<(), CliError> {
    println!("Node address: {}", config.rpc.endpoint.0.host());
    let store_len = fs::metadata(config.store_filepath.clone())?.len();
    println!("Store size: {} kB", store_len / 1024);
    Ok(())
}

async fn print_rpc_status(config: &CliConfig) -> Result<(), CliError> {
    println!("\n--- RPC Node Status ---");
    let rpc_client = GrpcClient::new(&config.rpc.endpoint.clone().into(), config.rpc.timeout_ms);
    match rpc_client.get_status_unversioned().await {
        Ok(status) => {
            print_status_info(&status);
        },
        Err(e) => {
            println!("Failed to fetch RPC status: {e}");
        },
    }
    Ok(())
}

fn print_status_info(status: &RpcStatusInfo) {
    println!("Node version: {}", status.version);
    if let Some(genesis) = status.genesis_commitment {
        println!("Node genesis: {}", genesis.to_hex());
    }
    println!("Chain tip: {}", status.chain_tip);
    if let Some(ref bp) = status.block_producer {
        println!("Block producer: {} (chain tip: {})", bp.status, bp.chain_tip);
    }
}
