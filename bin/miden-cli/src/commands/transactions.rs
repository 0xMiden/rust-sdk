use chrono::{Local, TimeZone};
use clap::ValueEnum;
use comfy_table::{Cell, ContentArrangement, presets};
use miden_client::Client;
use miden_client::block::BlockNumber;
use miden_client::keystore::Keystore;
use miden_client::store::{TransactionFilter, TransactionFilterQuery};
use miden_client::transaction::{
    ExpirationTransactionScript,
    SendNotesTransactionScript,
    TransactionRecord,
    TransactionScript,
    TransactionScriptRoot,
    TransactionStatus,
    TransactionStatusVariant,
};

use crate::errors::CliError;
use crate::notes::display::{print_input_notes, print_output_notes};
use crate::utils::{NO_VALUE, load_faucet_metadata_resolver, parse_account_id};
use crate::{Parser, create_dynamic_table, get_transaction_with_id_prefix};

#[derive(Clone, Debug, ValueEnum)]
pub enum TransactionStatusFilter {
    Pending,
    Committed,
    Discarded,
}

impl From<&TransactionStatusFilter> for TransactionStatusVariant {
    fn from(status: &TransactionStatusFilter) -> Self {
        match status {
            TransactionStatusFilter::Pending => TransactionStatusVariant::Pending,
            TransactionStatusFilter::Committed => TransactionStatusVariant::Committed,
            TransactionStatusFilter::Discarded => TransactionStatusVariant::Discarded,
        }
    }
}

#[derive(Default, Debug, Parser, Clone)]
#[command(about = "Manage and view transactions. Defaults to `list` command")]
pub struct TransactionCmd {
    /// List currently tracked transactions.
    #[arg(short, long, group = "action")]
    list: bool,
    /// Show details of the transaction with the specified ID or hex prefix.
    #[arg(short, long, group = "action", value_name = "ID")]
    show: Option<String>,
    /// Only list transactions executed by this account ID (or hex prefix).
    #[arg(short, long, value_name = "ID", conflicts_with = "show")]
    account_id: Option<String>,
    /// Only list transactions in this status.
    #[arg(long, value_name = "status", conflicts_with = "show")]
    status: Option<TransactionStatusFilter>,
    /// Only list the most recently created transactions, at most this many.
    #[arg(long, value_name = "count", conflicts_with = "show")]
    limit: Option<u32>,
}

impl TransactionCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        client: Client<AUTH>,
    ) -> Result<(), CliError> {
        match &self.show {
            Some(transaction_id) => show_transaction(&client, transaction_id).await,
            None => {
                list_transactions(
                    &client,
                    self.account_id.as_deref(),
                    self.status.as_ref(),
                    self.limit,
                )
                .await
            },
        }
    }
}

// LIST TRANSACTIONS
// ================================================================================================
async fn list_transactions<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    account_id: Option<&str>,
    status: Option<&TransactionStatusFilter>,
    limit: Option<u32>,
) -> Result<(), CliError> {
    let account_id = match account_id {
        Some(account_id) => Some(parse_account_id(client, account_id).await?),
        None => None,
    };

    let transactions = client
        .get_transactions(TransactionFilter::Query(TransactionFilterQuery {
            account_id,
            status: status.map(TransactionStatusVariant::from),
            limit,
        }))
        .await?;

    print_transactions_summary(&transactions);
    Ok(())
}

// SHOW TRANSACTION
// ================================================================================================
async fn show_transaction<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    transaction_id_prefix: &str,
) -> Result<(), CliError> {
    let transaction = get_transaction_with_id_prefix(client, transaction_id_prefix)
        .await
        .map_err(|err| CliError::Input(err.to_string()))?;
    let resolver = load_faucet_metadata_resolver()?;

    print_transaction_details(&transaction);
    print_input_notes(client, &resolver, &transaction).await?;
    print_output_notes(client, &resolver, &transaction.details.output_notes, true).await?;

    Ok(())
}

/// Prints the transaction's own metadata.
fn print_transaction_details(transaction: &TransactionRecord) {
    let details = &transaction.details;

    let mut table = create_dynamic_table(&["Transaction Information"]);
    table
        .load_preset(presets::UTF8_HORIZONTAL_ONLY)
        .set_content_arrangement(ContentArrangement::DynamicFullWidth);

    table.add_row(vec![Cell::new("ID"), Cell::new(transaction.id.to_hex())]);
    table.add_row(vec![Cell::new("Status"), Cell::new(transaction.status.to_string())]);

    // The expiration block only bounds a transaction that isn't in a block yet.
    if transaction.status == TransactionStatus::Pending {
        table.add_row(vec![
            Cell::new("Expiration Block"),
            Cell::new(format_expiration_block(details.expiration_block_num)),
        ]);
    }

    table.add_row(vec![Cell::new("Account ID"), Cell::new(details.account_id.to_string())]);

    let script_root = transaction.script.as_ref().map(TransactionScript::root);
    table.add_row(vec![
        Cell::new("Script Root"),
        Cell::new(script_root.map_or(NO_VALUE.to_string(), |root| root.to_string())),
    ]);
    if let Some(name) = script_root.and_then(standard_transaction_script_name) {
        table.add_row(vec![Cell::new("Standard Script"), Cell::new(name)]);
    }

    table.add_row(vec![Cell::new("Reference Block"), Cell::new(details.block_num.to_string())]);
    table.add_row(vec![
        Cell::new("Submission Height"),
        Cell::new(details.submission_height.to_string()),
    ]);
    table.add_row(vec![
        Cell::new("Creation Time"),
        Cell::new(format_timestamp(details.creation_timestamp)),
    ]);
    table.add_row(vec![
        Cell::new("Account State Before"),
        Cell::new(details.init_account_state.to_hex()),
    ]);
    table.add_row(vec![
        Cell::new("Account State After"),
        Cell::new(details.final_account_state.to_hex()),
    ]);

    println!("{table}");
}

// HELPERS
// ================================================================================================
fn print_transactions_summary<'a, I>(executed_transactions: I)
where
    I: IntoIterator<Item = &'a TransactionRecord>,
{
    let mut table = create_dynamic_table(&[
        "ID",
        "Status",
        "Account ID",
        "Script Root",
        "Input Notes Count",
        "Output Notes Count",
    ]);

    for tx in executed_transactions {
        table.add_row(vec![
            tx.id.to_string(),
            tx.status.to_string(),
            tx.details.account_id.to_string(),
            tx.script.as_ref().map_or(NO_VALUE.to_string(), |x| x.root().to_string()),
            tx.details.input_note_nullifiers.len().to_string(),
            tx.details.output_notes.num_notes().to_string(),
        ]);
    }

    println!("{table}");
}

/// Returns the display name of the standard transaction script with the given root, if any.
fn standard_transaction_script_name(root: TransactionScriptRoot) -> Option<&'static str> {
    if SendNotesTransactionScript::script_roots().contains(&root) {
        return Some("send_notes");
    }
    if root == ExpirationTransactionScript::script_root() {
        return Some("expiration");
    }
    None
}

/// Formats the expiration block; [`BlockNumber::MAX`] marks a transaction that can't expire, and is
/// shown as the empty-value placeholder.
fn format_expiration_block(expiration_block_num: BlockNumber) -> String {
    if expiration_block_num == BlockNumber::MAX {
        NO_VALUE.to_string()
    } else {
        expiration_block_num.to_string()
    }
}

/// Formats a Unix timestamp in the local time zone, falling back to the raw value for a timestamp
/// that doesn't map to a date.
fn format_timestamp(timestamp: u64) -> String {
    i64::try_from(timestamp)
        .ok()
        .and_then(|seconds| Local.timestamp_opt(seconds, 0).single())
        .map_or_else(|| timestamp.to_string(), |datetime| datetime.to_string())
}

#[cfg(test)]
mod tests {
    use miden_client::Word;
    use miden_client::block::BlockNumber;
    use miden_client::transaction::{
        DiscardCause,
        ExpirationTransactionScript,
        SendNotesTransactionScript,
        TransactionScriptRoot,
        TransactionStatus,
        TransactionStatusVariant,
    };

    use super::{
        NO_VALUE,
        TransactionStatusFilter,
        format_expiration_block,
        format_timestamp,
        standard_transaction_script_name,
    };

    #[test]
    fn transaction_status_filter_selects_the_variant_of_its_status() {
        let cases = [
            (TransactionStatusFilter::Pending, TransactionStatus::Pending),
            (
                TransactionStatusFilter::Committed,
                TransactionStatus::Committed {
                    block_number: BlockNumber::from(7u32),
                    commit_timestamp: 0,
                },
            ),
            (
                TransactionStatusFilter::Discarded,
                TransactionStatus::Discarded(DiscardCause::Expired),
            ),
        ];

        for (filter, status) in cases {
            assert_eq!(TransactionStatusVariant::from(&filter), status.variant());
        }
    }

    #[test]
    fn standard_transaction_scripts_are_named_by_their_root() {
        for root in SendNotesTransactionScript::script_roots() {
            assert_eq!(standard_transaction_script_name(root), Some("send_notes"));
        }
        assert_eq!(
            standard_transaction_script_name(ExpirationTransactionScript::script_root()),
            Some("expiration")
        );
        assert_eq!(
            standard_transaction_script_name(TransactionScriptRoot::from_raw(Word::default())),
            None
        );
    }

    #[test]
    fn format_timestamp_falls_back_to_the_raw_value() {
        assert_eq!(format_timestamp(u64::MAX), u64::MAX.to_string());
    }

    #[test]
    fn format_expiration_block_reports_the_sentinel_as_no_value() {
        assert_eq!(format_expiration_block(BlockNumber::MAX), NO_VALUE);
        assert_eq!(format_expiration_block(BlockNumber::from(7u32)), "7");
    }
}
