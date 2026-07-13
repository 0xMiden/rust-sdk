use clap::ValueEnum;
use comfy_table::{Attribute, Cell, ContentArrangement, Table, presets};
use miden_client::address::Address;
use miden_client::asset::Asset;
use miden_client::keystore::Keystore;
use miden_client::note::{
    Note,
    NoteConsumability,
    NoteConsumptionStatus,
    NoteMetadata,
    NoteStorage,
    StandardNote,
    get_input_note_with_id_prefix,
};
use miden_client::store::{InputNoteRecord, NoteFilter as ClientNoteFilter, OutputNoteRecord};
use miden_client::{Client, ClientError, IdPrefixFetchError, PrettyPrint};

use crate::errors::CliError;
use crate::utils::{load_faucet_metadata_resolver, parse_account_id};
use crate::{Parser, create_dynamic_table, get_output_note_with_id_prefix};

#[derive(Clone, Debug, ValueEnum)]
pub enum NoteFilter {
    All,
    Expected,
    Committed,
    Consumed,
    Processing,
    Consumable,
}

impl TryInto<ClientNoteFilter> for NoteFilter {
    type Error = String;

    fn try_into(self) -> Result<ClientNoteFilter, Self::Error> {
        match self {
            NoteFilter::All => Ok(ClientNoteFilter::All),
            NoteFilter::Expected => Ok(ClientNoteFilter::Expected),
            NoteFilter::Committed => Ok(ClientNoteFilter::Committed),
            NoteFilter::Consumed => Ok(ClientNoteFilter::Consumed),
            NoteFilter::Processing => Ok(ClientNoteFilter::Processing),
            NoteFilter::Consumable => Err("Consumable filter is not supported".to_string()),
        }
    }
}

#[derive(Debug, Parser, Clone)]
#[command(about = "View and manage notes")]
pub struct NotesCmd {
    /// List notes with the specified filter. If no filter is provided, all notes will be listed.
    #[arg(short, long, group = "action", default_missing_value="all", num_args=0..=1, value_name = "filter")]
    list: Option<NoteFilter>,
    /// Show note with the specified ID.
    #[arg(short, long, group = "action", value_name = "note_id")]
    show: Option<String>,
    /// When using --show, include the note code in the output.
    #[arg(long, requires = "show")]
    with_code: bool,
    /// (only has effect on `--list consumable`) Account ID used to filter list. Only notes
    /// consumable by this account will be shown.
    #[arg(short, long, value_name = "account_id")]
    account_id: Option<String>,
    /// Send a stored private note through the note transport network.
    /// Define both the note ID (as hex string, in full or a prefix) and address (as Bech32 string)
    /// such as: `--send 0xc1234567 mm1qpkdyek2c0ywwvzupakc7zlzty8qn2qnfc`
    #[arg(long, group = "action", num_args = 2, value_names = ["note_id", "address"])]
    send: Option<Vec<String>>,
    /// Fetch notes from the note transport network.
    /// Fetched notes for tracked note tags will be added to the store.
    #[arg(long, group = "action")]
    fetch: bool,
}

impl NotesCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        match self {
            NotesCmd { list: Some(NoteFilter::Consumable), .. } => {
                list_consumable_notes(client, None).await?;
            },
            NotesCmd { list: Some(filter), .. } => {
                list_notes(
                    client,
                    filter.clone().try_into().expect("Filter shouldn't be consumable"),
                )
                .await?;
            },
            NotesCmd { show: Some(id), .. } => {
                show_note(&mut client, id.to_owned(), self.with_code).await?;
            },
            NotesCmd { send: Some(args), .. } => {
                let note_id = &args[0];
                let address = &args[1];
                send(&mut client, note_id, address).await?;
            },
            NotesCmd { fetch: true, .. } => {
                fetch(&mut client).await?;
            },
            _ => {
                list_notes(client, ClientNoteFilter::All).await?;
            },
        }
        Ok(())
    }
}

struct CliNoteSummary {
    id: String,
    script_root: String,
    assets_commitment: String,
    inputs_commitment: String,
    serial_num: String,
    note_type: String,
    state: String,
    tag: String,
    sender: String,
    exportable: bool,
}

// LIST NOTES
// ================================================================================================
async fn list_notes<AUTH: Keystore + Sync>(
    client: Client<AUTH>,
    filter: ClientNoteFilter,
) -> Result<(), CliError> {
    let input_notes = client
        .get_input_notes(filter.clone())
        .await?
        .into_iter()
        .map(|input_note_record| note_summary(Some(&input_note_record), None))
        .collect::<Vec<CliNoteSummary>>();
    let output_notes = client
        .get_output_notes(filter.clone())
        .await?
        .into_iter()
        .map(|output_note_record| note_summary(None, Some(&output_note_record)))
        .collect::<Vec<CliNoteSummary>>();

    print_notes_summary(input_notes, "Input Notes");
    print_notes_summary(output_notes, "Output Notes");

    Ok(())
}

// SHOW NOTE
// ================================================================================================
#[allow(clippy::too_many_lines)]
async fn show_note<AUTH: Keystore + Sync>(
    client: &mut Client<AUTH>,
    note_id: String,
    with_code: bool,
) -> Result<(), CliError> {
    let input_note_record = get_input_note_with_id_prefix(client, &note_id).await;
    let output_note_record = get_output_note_with_id_prefix(client, &note_id).await;

    // If we don't find an input note nor an output note return an error
    if matches!(input_note_record, Err(IdPrefixFetchError::NoMatch(_)))
        && matches!(output_note_record, Err(IdPrefixFetchError::NoMatch(_)))
    {
        return Err(CliError::Import(
            "The specified note ID hex prefix did not match any note".to_string(),
        ));
    }

    // If either one of the two match with multiple notes return an error
    if matches!(input_note_record, Err(IdPrefixFetchError::MultipleMatches(_)))
        || matches!(output_note_record, Err(IdPrefixFetchError::MultipleMatches(_)))
    {
        return Err(CliError::Import(
            "The specified note ID hex prefix matched with more than one note.".to_string(),
        ));
    }

    let input_note_record = input_note_record.ok();
    let output_note_record = output_note_record.ok();

    // If we match one note as the input note and another one as the output note return an error
    match (&input_note_record, &output_note_record) {
        (Some(input_record), Some(output_record))
            if input_record.id() != Some(output_record.id()) =>
        {
            return Err(CliError::Import(
                "The specified note ID hex prefix matched with more than one note.".to_string(),
            ));
        },
        _ => {},
    }

    let mut table = create_dynamic_table(&["Note Information"]);
    table
        .load_preset(presets::UTF8_HORIZONTAL_ONLY)
        .set_content_arrangement(ContentArrangement::DynamicFullWidth);

    let CliNoteSummary {
        id,
        script_root,
        assets_commitment,
        inputs_commitment,
        serial_num,
        note_type,
        state,
        tag,
        sender,
        exportable,
    } = note_summary(input_note_record.as_ref(), output_note_record.as_ref());
    table.add_row(vec![Cell::new("ID"), Cell::new(id)]);

    // Identify if this is a standard note type by script root
    let script_root_word = match (&input_note_record, &output_note_record) {
        (Some(record), _) => Some(record.details().script().root()),
        (_, Some(record)) => record.recipient().map(|r| r.script().root()),
        _ => None,
    };

    if let Some(standard_note) = script_root_word.and_then(StandardNote::from_script_root) {
        table.add_row(vec![Cell::new("Standard Note"), Cell::new(standard_note.name())]);
    }

    table.add_row(vec![Cell::new("Script Root"), Cell::new(script_root)]);
    table.add_row(vec![Cell::new("Assets Commitment"), Cell::new(assets_commitment)]);
    table.add_row(vec![Cell::new("Inputs Commitment"), Cell::new(inputs_commitment)]);
    table.add_row(vec![Cell::new("Serial Number"), Cell::new(serial_num)]);
    table.add_row(vec![Cell::new("Type"), Cell::new(note_type)]);
    table.add_row(vec![Cell::new("State"), Cell::new(state)]);
    table.add_row(vec![Cell::new("Tag"), Cell::new(tag)]);
    table.add_row(vec![Cell::new("Sender"), Cell::new(sender)]);
    table.add_row(vec![Cell::new("Exportable"), Cell::new(if exportable { "✔" } else { "✘" })]);

    println!("{table}");

    let inputs = match (&input_note_record, &output_note_record) {
        (Some(record), _) => {
            let details = record.details();
            Some(details.storage().items().to_vec())
        },
        (_, Some(record)) => {
            record.recipient().map(|recipient| recipient.storage().items().to_vec())
        },
        (None, None) => {
            panic!("One of the two records should be Some")
        },
    };

    let assets = input_note_record
        .clone()
        .map(|record| record.assets().clone())
        .or(output_note_record.clone().map(|record| record.assets().clone()))
        .expect("One of the two records should be Some");

    // print note vault
    let mut table = create_dynamic_table(&["Note Assets"]);
    table
        .load_preset(presets::UTF8_HORIZONTAL_ONLY)
        .set_content_arrangement(ContentArrangement::DynamicFullWidth);

    table.add_row(vec![
        Cell::new("Type").add_attribute(Attribute::Bold),
        Cell::new("Faucet ID").add_attribute(Attribute::Bold),
        Cell::new("Amount").add_attribute(Attribute::Bold),
    ]);
    let resolver = load_faucet_metadata_resolver()?;
    let assets = assets.iter();

    for asset in assets {
        let (asset_type, faucet, amount) = match asset {
            Asset::Fungible(fungible_asset) => {
                let (faucet, amount) =
                    resolver.format_fungible_asset(client, fungible_asset).await?;
                ("Fungible Asset", faucet, amount)
            },
            Asset::NonFungible(non_fungible_asset) => (
                "Non Fungible Asset",
                non_fungible_asset.faucet_id().prefix().to_hex(),
                1.0.to_string(),
            ),
        };
        table.add_row(vec![asset_type, &faucet, &amount.clone()]);
    }
    println!("{table}");

    if let Some(inputs) = inputs {
        let inputs = NoteStorage::new(inputs.clone()).map_err(ClientError::NoteError)?;
        let mut table = create_dynamic_table(&["Note Inputs"]);
        table
            .load_preset(presets::UTF8_HORIZONTAL_ONLY)
            .set_content_arrangement(ContentArrangement::DynamicFullWidth);
        table.add_row(vec![
            Cell::new("Index").add_attribute(Attribute::Bold),
            Cell::new("Value").add_attribute(Attribute::Bold),
        ]);

        inputs.items().iter().enumerate().for_each(|(idx, input)| {
            table.add_row(vec![Cell::new(idx).add_attribute(Attribute::Bold), Cell::new(input)]);
        });
        println!("{table}");
    }

    if with_code {
        let mut table = create_dynamic_table(&["Note Code"]);
        let code = match (&input_note_record, &output_note_record) {
            (Some(record), _) => record.details().script().to_pretty_string(),
            (_, Some(record)) => {
                record.state().recipient().map_or("Code unavailable".to_string(), |recipient| {
                    recipient.script().to_pretty_string()
                })
            },
            (None, None) => {
                panic!("One of the two records should be Some")
            },
        };
        table.add_row(vec![Cell::new(code)]);
        println!("{table}");
    }

    Ok(())
}

// LIST CONSUMABLE INPUT NOTES
// ================================================================================================
async fn list_consumable_notes<AUTH: Keystore + Sync>(
    client: Client<AUTH>,
    account_id: Option<&String>,
) -> Result<(), CliError> {
    let account_id = match account_id {
        Some(id) => Some(parse_account_id(&client, id).await?),
        None => None,
    };
    let notes = client.get_consumable_notes(account_id).await?;
    print_consumable_notes_summary(&notes);
    Ok(())
}

// SEND
// ================================================================================================

/// Send a (stored) note
async fn send<AUTH: Keystore + Sync>(
    client: &mut Client<AUTH>,
    note_id: &str,
    address: &str,
) -> Result<(), CliError> {
    let note_record = get_input_note_with_id_prefix(client, note_id)
        .await
        .map_err(|e| CliError::Input(format!("note not found: {e}")))?;

    let block_hint = note_record.inclusion_proof().map(|proof| proof.location().block_num());
    let note: Note = note_record
        .try_into()
        .map_err(|e| CliError::from(ClientError::NoteRecordConversionError(e)))?;
    let (_netid, address) = Address::decode(address).map_err(|e| CliError::Input(e.to_string()))?;

    match block_hint {
        Some(block_hint) => {
            client.send_private_note_with_block_hint(note, &address, block_hint).await?;
        },
        None => {
            #[allow(deprecated)]
            client.send_private_note(note, &address).await?;
        },
    }

    Ok(())
}

// FETCH
// ================================================================================================

/// Retrieve notes for all tracked tags
///
/// Fetched notes are stored in the store.
async fn fetch<AUTH>(client: &mut Client<AUTH>) -> Result<(), CliError>
where
    AUTH: Keystore + Sync + 'static,
{
    client.fetch_private_notes().await?;

    Ok(())
}

// HELPERS
// ================================================================================================
fn print_notes_summary<I>(notes: I, header: &str)
where
    I: IntoIterator<Item = CliNoteSummary>,
{
    let mut table = Table::new();
    table
        .load_preset(presets::UTF8_NO_BORDERS)
        .set_content_arrangement(ContentArrangement::DynamicFullWidth);
    table.set_header(vec![Cell::new(header).add_attribute(Attribute::Bold)]);
    println!("\n{table}");
    for summary in notes {
        println!(" {} {}", summary.id, summary.state);
    }
}

fn print_consumable_notes_summary<'a, I>(notes: I)
where
    I: IntoIterator<Item = &'a (InputNoteRecord, Vec<NoteConsumability>)>,
{
    let mut table = create_dynamic_table(&["Note ID", "Account ID", "Relevance"]);

    for (note, relevances) in notes {
        // Consumable notes are committed, so they carry metadata and id() is Some.
        let note_id_hex = note.id().map_or_else(|| "<unknown>".to_string(), |id| id.to_hex());
        for relevance in relevances {
            table.add_row(vec![
                note_id_hex.clone(),
                relevance.0.to_string(),
                note_consumption_status_type(&relevance.1),
            ]);
        }
    }

    println!("{table}");
}

fn note_consumption_status_type(note_consumption_status: &NoteConsumptionStatus) -> String {
    match note_consumption_status {
        NoteConsumptionStatus::Consumable => "Consumable".to_string(),
        NoteConsumptionStatus::ConsumableAfter(block_number) => {
            format!("Consumable after block {block_number}")
        },
        NoteConsumptionStatus::ConsumableWithAuthorization => {
            "Consumable with authorization".to_string()
        },
        NoteConsumptionStatus::UnconsumableConditions => {
            "Unconsumable due to conditions".to_string()
        },
        NoteConsumptionStatus::NeverConsumable(error) => format!("Never consumable: {error}"),
    }
    .clone()
}

fn note_record_type(note_record_metadata: Option<&NoteMetadata>) -> String {
    match note_record_metadata {
        Some(metadata) => match metadata.note_type() {
            miden_client::note::NoteType::Private => "Private",
            miden_client::note::NoteType::Public => "Public",
        },
        None => "-",
    }
    .to_string()
}

/// Given that one of the two records is Some, this function will return a summary of the note.
fn note_summary(
    input_note_record: Option<&InputNoteRecord>,
    output_note_record: Option<&OutputNoteRecord>,
) -> CliNoteSummary {
    // Use the NoteId's hex when available; metadata-less input notes have no NoteId, so fall back
    // to the details commitment as the identifier rather than fabricating a NoteId from it.
    let id_str = input_note_record
        .and_then(InputNoteRecord::id)
        .or_else(|| output_note_record.map(OutputNoteRecord::id))
        .map(|id| id.to_hex())
        .or_else(|| input_note_record.map(|record| record.details_commitment().as_word().to_hex()))
        .expect("One of the two records should be Some");

    let assets_commitment_str = input_note_record
        .map(|record| record.assets().commitment().to_string())
        .or(output_note_record.map(|record| record.assets().commitment().to_string()))
        .expect("One of the two records should be Some");

    let (inputs_commitment_str, serial_num, script_root_str) =
        match (input_note_record, output_note_record) {
            (Some(record), _) => {
                let details = record.details();
                (
                    details.storage().commitment().to_string(),
                    details.serial_num().to_string(),
                    details.script().root().to_string(),
                )
            },
            (None, Some(record)) if record.recipient().is_some() => {
                let recipient = record.recipient().expect("output record should have recipient");
                (
                    recipient.storage().commitment().to_string(),
                    recipient.serial_num().to_string(),
                    recipient.script().root().to_string(),
                )
            },
            (None, Some(_record)) => ("-".to_string(), "-".to_string(), "-".to_string()),
            (None, None) => panic!("One of the two records should be Some"),
        };

    let note_type = note_record_type(
        input_note_record
            .and_then(InputNoteRecord::metadata)
            .or(output_note_record.map(OutputNoteRecord::metadata)),
    );

    let state = input_note_record
        .map(|record| record.state().to_string())
        .or(output_note_record.map(|record| record.state().to_string()))
        .expect("One of the two records should be Some");

    let note_metadata = input_note_record
        .map(|record| record.metadata())
        .or(output_note_record.map(|record| Some(record.metadata())))
        .expect("One of the two records should be Some");

    let note_tag_str = note_metadata.map_or("-".to_string(), |metadata| metadata.tag().to_string());

    let note_sender_str =
        note_metadata.map_or("-".to_string(), |metadata| metadata.tag().to_string());

    CliNoteSummary {
        id: id_str,
        script_root: script_root_str,
        assets_commitment: assets_commitment_str,
        inputs_commitment: inputs_commitment_str,
        serial_num,
        note_type,
        state,
        tag: note_tag_str,
        sender: note_sender_str,
        exportable: output_note_record.is_some(),
    }
}
