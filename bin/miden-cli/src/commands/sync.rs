use clap::Parser;
use miden_client::Client;
use miden_client::keystore::Keystore;

use crate::errors::CliError;

#[derive(Debug, Parser, Clone)]
#[command(about = "Sync this client with the latest state of the Miden network")]
pub struct SyncCmd {}

impl SyncCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let new_details = client.sync_state().await?;

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
