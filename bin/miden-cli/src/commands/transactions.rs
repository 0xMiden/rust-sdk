use std::collections::BTreeMap;

use chrono::{Local, TimeZone};
use clap::ValueEnum;
use comfy_table::{Cell, ContentArrangement, presets};
use miden_client::Client;
use miden_client::asset::Asset;
use miden_client::block::BlockNumber;
use miden_client::keystore::Keystore;
use miden_client::note::{NoteAssets, Nullifier, StandardNote};
use miden_client::store::{InputNoteRecord, NoteFilter, TransactionFilter};
use miden_client::transaction::{
    ExpirationTransactionScript,
    SendNotesTransactionScript,
    TransactionRecord,
    TransactionScript,
    TransactionScriptRoot,
    TransactionStatus,
};

use crate::commands::notes::note_record_type;
use crate::errors::CliError;
use crate::utils::{FaucetMetadataResolver, load_faucet_metadata_resolver, parse_account_id};
use crate::{Parser, create_dynamic_table};

/// Placeholder shown for a field that the client can't fill in for the transaction at hand.
const NO_VALUE: &str = "-";

/// Placeholder shown instead of the ID of a consumed note the client can't resolve.
const PRIVATE_NOTE: &str = "<private>";

/// Shown instead of the expiration block of a transaction that can't expire.
const NO_EXPIRATION: &str = "never";

#[derive(Clone, Debug, ValueEnum)]
pub enum TransactionStatusFilter {
    Pending,
    Committed,
    Discarded,
}

impl TransactionStatusFilter {
    fn matches(&self, status: &TransactionStatus) -> bool {
        matches!(
            (self, status),
            (TransactionStatusFilter::Pending, TransactionStatus::Pending)
                | (TransactionStatusFilter::Committed, TransactionStatus::Committed { .. })
                | (TransactionStatusFilter::Discarded, TransactionStatus::Discarded(_))
        )
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
    /// (only has effect on `--list`) Only list transactions executed by this account ID (or hex
    /// prefix).
    #[arg(short, long, value_name = "ID", conflicts_with = "show")]
    account_id: Option<String>,
    /// (only has effect on `--list`) Only list transactions in this status.
    #[arg(long, value_name = "status", conflicts_with = "show")]
    status: Option<TransactionStatusFilter>,
    /// (only has effect on `--list`) Only list the most recently created transactions, at most
    /// this many.
    #[arg(long, value_name = "count", conflicts_with = "show")]
    limit: Option<usize>,
}

impl TransactionCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        match &self.show {
            Some(transaction_id) => show_transaction(&mut client, transaction_id).await,
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
    limit: Option<usize>,
) -> Result<(), CliError> {
    let account_id = match account_id {
        Some(account_id) => Some(parse_account_id(client, account_id).await?),
        None => None,
    };

    let mut transactions = client.get_transactions(TransactionFilter::All).await?;
    transactions.retain(|transaction| {
        account_id.is_none_or(|account_id| transaction.details.account_id == account_id)
            && status.is_none_or(|status| status.matches(&transaction.status))
    });

    // The store returns transactions in an unspecified order, so sort them by creation time to
    // make the listing chronological and give `--limit` a well-defined tail to keep.
    transactions.sort_by_key(|transaction| transaction.details.creation_timestamp);
    if let Some(limit) = limit {
        transactions.drain(..transactions.len().saturating_sub(limit));
    }

    print_transactions_summary(&transactions);
    Ok(())
}

// SHOW TRANSACTION
// ================================================================================================
async fn show_transaction<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
    transaction_id_prefix: &str,
) -> Result<(), CliError> {
    let transaction = get_transaction_with_id_prefix(client, transaction_id_prefix).await?;
    let resolver = load_faucet_metadata_resolver()?;

    print_transaction_summary(&transaction);
    print_input_notes(client, &resolver, &transaction).await?;
    print_output_notes(client, &resolver, &transaction).await?;

    Ok(())
}

/// Returns the tracked transaction whose ID starts with `transaction_id_prefix`.
async fn get_transaction_with_id_prefix<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    transaction_id_prefix: &str,
) -> Result<TransactionRecord, CliError> {
    let mut matches = client
        .get_transactions(TransactionFilter::All)
        .await?
        .into_iter()
        .filter(|transaction| transaction.id.to_hex().starts_with(transaction_id_prefix))
        .collect::<Vec<_>>();

    match matches.len() {
        0 => Err(CliError::Input(format!(
            "The specified transaction ID hex prefix {transaction_id_prefix} did not match any transaction"
        ))),
        1 => Ok(matches.pop().expect("matches has exactly one element")),
        _ => Err(CliError::Input(format!(
            "The specified transaction ID hex prefix {transaction_id_prefix} matched with more than one transaction"
        ))),
    }
}

/// Prints the transaction's own metadata.
fn print_transaction_summary(transaction: &TransactionRecord) {
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

    table.add_row(vec![Cell::new("Block Number"), Cell::new(details.block_num.to_string())]);
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

/// Prints the notes the transaction consumed.
///
/// A note ID can't be derived from a nullifier, so it's recovered by looking the nullifier up
/// among the client's tracked notes; an unresolved one is marked as private.
async fn print_input_notes<AUTH: Keystore + Sync>(
    client: &mut Client<AUTH>,
    resolver: &FaucetMetadataResolver,
    transaction: &TransactionRecord,
) -> Result<(), CliError> {
    let nullifiers = transaction
        .details
        .input_note_nullifiers
        .iter()
        .map(|nullifier| Nullifier::from_raw(*nullifier))
        .collect::<Vec<_>>();

    println!("\nInput Notes:");
    if nullifiers.is_empty() {
        println!("The transaction consumed no notes.");
        return Ok(());
    }

    let records: BTreeMap<Nullifier, InputNoteRecord> = client
        .get_input_notes(NoteFilter::Nullifiers(nullifiers.clone()))
        .await?
        .into_iter()
        .filter_map(|record| record.nullifier().map(|nullifier| (nullifier, record)))
        .collect();

    let mut table = create_dynamic_table(&["ID", "Nullifier", "Standard Note", "Type", "Assets"]);
    for nullifier in &nullifiers {
        let Some(record) = records.get(nullifier) else {
            table.add_row(vec![PRIVATE_NOTE, &nullifier.to_hex(), NO_VALUE, NO_VALUE, NO_VALUE]);
            continue;
        };

        let id = record.id().map_or_else(|| NO_VALUE.to_string(), |id| id.to_hex());
        let standard_note = StandardNote::from_script_root(record.details().script().root())
            .map_or(NO_VALUE, |standard_note| standard_note.name());

        table.add_row(vec![
            id,
            nullifier.to_hex(),
            standard_note.to_string(),
            note_record_type(record.metadata()),
            format_assets(client, resolver, record.assets()).await?,
        ]);
    }

    println!("{table}");
    Ok(())
}

/// Prints the notes the transaction created.
async fn print_output_notes<AUTH: Keystore + Sync>(
    client: &mut Client<AUTH>,
    resolver: &FaucetMetadataResolver,
    transaction: &TransactionRecord,
) -> Result<(), CliError> {
    let output_notes = &transaction.details.output_notes;

    println!("\nOutput Notes:");
    if output_notes.is_empty() {
        println!("The transaction created no notes.");
        return Ok(());
    }

    let mut table = create_dynamic_table(&["ID", "Standard Note", "Type", "Tag", "Assets"]);
    for note in output_notes.iter() {
        // A partial output note carries no recipient, so its script root isn't known.
        let standard_note = note
            .recipient()
            .and_then(|recipient| StandardNote::from_script_root(recipient.script().root()))
            .map_or(NO_VALUE, |standard_note| standard_note.name());

        table.add_row(vec![
            note.id().to_hex(),
            standard_note.to_string(),
            note_record_type(Some(note.metadata())),
            note.metadata().tag().to_string(),
            format_assets(client, resolver, note.assets()).await?,
        ]);
    }

    println!("{table}");
    Ok(())
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
            tx.script.as_ref().map_or("-".to_string(), |x| x.root().to_string()),
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

/// Formats the expiration block; [`BlockNumber::MAX`] marks a transaction that can't expire.
fn format_expiration_block(expiration_block_num: BlockNumber) -> String {
    if expiration_block_num == BlockNumber::MAX {
        NO_EXPIRATION.to_string()
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

/// Renders a note's assets one per line, so they fit a single table cell.
async fn format_assets<AUTH: Keystore + Sync>(
    client: &mut Client<AUTH>,
    resolver: &FaucetMetadataResolver,
    assets: &NoteAssets,
) -> Result<String, CliError> {
    let mut formatted = Vec::with_capacity(assets.num_assets());
    for asset in assets.iter() {
        formatted.push(match asset {
            Asset::Fungible(fungible_asset) => {
                let (faucet, amount) =
                    resolver.format_fungible_asset(client, fungible_asset).await?;
                format!("{amount} {faucet}")
            },
            Asset::NonFungible(non_fungible_asset) => {
                format!("1 {} (non-fungible)", non_fungible_asset.faucet_id().prefix().to_hex())
            },
        });
    }

    if formatted.is_empty() {
        return Ok(NO_VALUE.to_string());
    }

    Ok(formatted.join("\n"))
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
    };

    use super::{
        TransactionStatusFilter,
        format_expiration_block,
        format_timestamp,
        standard_transaction_script_name,
    };

    #[test]
    fn transaction_status_filter_matches_only_its_own_status() {
        let statuses = [
            TransactionStatus::Pending,
            TransactionStatus::Committed {
                block_number: BlockNumber::from(7u32),
                commit_timestamp: 0,
            },
            TransactionStatus::Discarded(DiscardCause::Expired),
        ];
        let filters = [
            TransactionStatusFilter::Pending,
            TransactionStatusFilter::Committed,
            TransactionStatusFilter::Discarded,
        ];

        for (filter_index, filter) in filters.iter().enumerate() {
            for (status_index, status) in statuses.iter().enumerate() {
                assert_eq!(filter.matches(status), filter_index == status_index);
            }
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
    fn format_expiration_block_reports_the_sentinel_as_never() {
        assert_eq!(format_expiration_block(BlockNumber::MAX), "never");
        assert_eq!(format_expiration_block(BlockNumber::from(7u32)), "7");
    }
}
