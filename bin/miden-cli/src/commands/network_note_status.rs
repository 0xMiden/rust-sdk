use comfy_table::{Cell, ContentArrangement, presets};
use miden_client::note::NoteId;
use miden_client::rpc::{GrpcClient, NodeRpcClient};

use crate::config::CliConfig;
use crate::errors::CliError;
use crate::utils::{FormattedAsset, ResolvedFaucet};
use crate::{Parser, create_dynamic_table};

#[derive(Debug, Parser, Clone)]
#[command(about = "Query the network for the processing status of a note")]
pub struct NetworkNoteStatusCmd {
    /// The full note ID as a hex string (e.g., 0xabc123...).
    note_id: String,
}

impl NetworkNoteStatusCmd {
    pub async fn execute(&self) -> Result<(), CliError> {
        let note_id = NoteId::try_from_hex(&self.note_id)
            .map_err(|e| CliError::Input(format!("Invalid note ID: {e}")))?;

        let cli_config = CliConfig::load()?;
        let rpc_client =
            GrpcClient::new(&cli_config.rpc.endpoint.clone().into(), cli_config.rpc.timeout_ms);

        let status_info = rpc_client
            .get_network_note_status(note_id)
            .await
            .map_err(|e| CliError::Input(format!("Failed to get network note status: {e}")))?;

        let mut table = create_dynamic_table(&["Network Note Status"]);
        table
            .load_preset(presets::UTF8_HORIZONTAL_ONLY)
            .set_content_arrangement(ContentArrangement::DynamicFullWidth);

        table.add_row(vec![Cell::new("Note ID"), Cell::new(&self.note_id)]);
        table.add_row(vec![Cell::new("Status"), Cell::new(status_info.status.to_string())]);
        table.add_row(vec![
            Cell::new("Attempt Count"),
            Cell::new(status_info.attempt_count.to_string()),
        ]);
        table.add_row(vec![
            Cell::new("Last Error"),
            Cell::new(status_info.last_error.as_deref().unwrap_or("-")),
        ]);
        table.add_row(vec![
            Cell::new("Last Attempt Block"),
            Cell::new(format_block_num(status_info.last_attempt_block_num)),
        ]);

        println!("{table}");

        if !status_info.sponsorships.is_empty() {
            let network_id = cli_config.network_id()?;
            let mut sponsorships_table = create_dynamic_table(&[
                "Fee Sponsorship Note ID",
                "Fee Asset",
                "Reclaim Height",
                "Committed Block",
                "Last Error",
                "Last Attempt Block",
            ]);
            for sponsorship in &status_info.sponsorships {
                // The command does not create a client, so it shows the fee asset in base units
                // with the faucet address.
                let fee_asset = FormattedAsset::new(
                    &sponsorship.fee_asset,
                    &ResolvedFaucet::default(),
                    &network_id,
                );
                sponsorships_table.add_row(vec![
                    sponsorship.note_id.to_hex(),
                    fee_asset.to_string(),
                    format_block_num(sponsorship.reclaim_height),
                    format_block_num(sponsorship.committed_block_num),
                    sponsorship.last_error.clone().unwrap_or_else(|| "-".to_string()),
                    format_block_num(sponsorship.last_attempt_block_num),
                ]);
            }
            println!("{sponsorships_table}");
        }

        Ok(())
    }
}

/// Returns the block number as a string, or `-` when it is `None`.
fn format_block_num(block_num: Option<u32>) -> String {
    block_num.map_or_else(|| "-".to_string(), |block_num| block_num.to_string())
}
