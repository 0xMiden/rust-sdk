//! Tables of the input notes and the output notes of a transaction.

use std::collections::BTreeMap;

use miden_client::keystore::Keystore;
use miden_client::note::{
    NoteAssets,
    NoteId,
    NoteMetadata,
    NoteScript,
    NoteType,
    Nullifier,
    StandardNote,
};
use miden_client::store::{InputNoteRecord, NoteFilter, OutputNoteRecord};
use miden_client::transaction::{
    InputNote,
    InputNotes,
    RawOutputNote,
    RawOutputNotes,
    TransactionRecord,
};
use miden_client::{Client, Felt};

use super::decoding::format_note_storage;
use crate::create_dynamic_table;
use crate::errors::CliError;
use crate::utils::{FaucetMetadataResolver, FormattedAsset, NO_VALUE};

// NOTE CELLS
// ================================================================================================

/// Returns the name of the note type, or the placeholder if the metadata is not known.
pub(crate) fn format_note_type(metadata: Option<&NoteMetadata>) -> String {
    match metadata {
        Some(metadata) => match metadata.note_type() {
            NoteType::Private => "Private",
            NoteType::Public => "Public",
        },
        None => NO_VALUE,
    }
    .to_string()
}

/// The cells that the tables of input notes and output notes share.
struct NoteCells {
    standard_note: String,
    note_type: String,
    storage: String,
    assets: String,
}

/// Builds the cells that the tables of input notes and output notes share.
///
/// `contents` is the script and the storage items of the note. A partial output note has none.
async fn note_cells<AUTH: Keystore + Sync>(
    client: &Client<AUTH>,
    resolver: &FaucetMetadataResolver,
    contents: Option<(&NoteScript, &[Felt])>,
    metadata: Option<&NoteMetadata>,
    assets: &NoteAssets,
) -> Result<NoteCells, CliError> {
    let standard_note =
        contents.and_then(|(script, _)| StandardNote::from_script_root(script.root()));
    let storage = match contents {
        Some((_, items)) => format_note_storage(client, resolver, standard_note, items).await?,
        None => NO_VALUE.to_string(),
    };

    let mut formatted_assets = Vec::with_capacity(assets.num_assets());
    for asset in assets.iter() {
        let resolved_faucet = resolver.resolve_asset(client, asset).await?;
        formatted_assets
            .push(FormattedAsset::new(asset, &resolved_faucet, resolver.network_id()).to_string());
    }
    let assets = if formatted_assets.is_empty() {
        NO_VALUE.to_string()
    } else {
        formatted_assets.join("\n")
    };

    Ok(NoteCells {
        standard_note: standard_note.map_or(NO_VALUE, |note| note.name()).to_string(),
        note_type: format_note_type(metadata),
        storage,
        assets,
    })
}

// INPUT NOTES
// ================================================================================================

/// Placeholder shown instead of the ID of a consumed note the client does not track. A consumed
/// note is recorded only by its nullifier. If the client does not track the note, its ID cannot be
/// recovered from the nullifier.
const UNTRACKED_NOTE: &str = "<untracked>";

/// Prints the table of the notes that the transaction consumed.
pub(crate) async fn print_input_notes<AUTH: Keystore + Sync>(
    client: &Client<AUTH>,
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

    let mut table = create_dynamic_table(&[
        "ID",
        "Nullifier",
        "Standard Note",
        "Type",
        "State",
        "Storage",
        "Assets",
    ]);
    for nullifier in &nullifiers {
        let Some(record) = records.get(nullifier) else {
            table.add_row(vec![
                UNTRACKED_NOTE,
                &nullifier.to_hex(),
                NO_VALUE,
                NO_VALUE,
                NO_VALUE,
                NO_VALUE,
                NO_VALUE,
            ]);
            continue;
        };

        let details = record.details();
        let cells = note_cells(
            client,
            resolver,
            Some((details.script(), details.storage().items())),
            record.metadata(),
            record.assets(),
        )
        .await?;
        table.add_row(input_note_row(record, nullifier, cells));
    }

    println!("{table}");
    Ok(())
}

/// Returns the cells of the row of a consumed note that the client tracks.
fn input_note_row(
    record: &InputNoteRecord,
    nullifier: &Nullifier,
    cells: NoteCells,
) -> Vec<String> {
    vec![
        record.id().map_or_else(|| NO_VALUE.to_string(), |id| id.to_hex()),
        nullifier.to_hex(),
        cells.standard_note,
        cells.note_type,
        record.state().to_string(),
        cells.storage,
        cells.assets,
    ]
}

/// Prints the table of the notes that an executed transaction consumes.
pub(crate) async fn print_consumed_notes<AUTH: Keystore + Sync>(
    client: &Client<AUTH>,
    resolver: &FaucetMetadataResolver,
    input_notes: &InputNotes<InputNote>,
) -> Result<(), CliError> {
    println!("Input Notes:");
    let mut table = create_dynamic_table(&[
        "ID",
        "Standard Note",
        "Type",
        "Tag",
        "Authenticated",
        "Storage",
        "Assets",
    ]);
    for input_note in input_notes.iter() {
        let note = input_note.note();
        let recipient = note.recipient();
        let cells = note_cells(
            client,
            resolver,
            Some((recipient.script(), recipient.storage().items())),
            Some(note.metadata()),
            note.assets(),
        )
        .await?;
        table.add_row(consumed_note_row(input_note, cells));
    }

    println!("{table}");
    Ok(())
}

/// Returns the cells of the row of a note that an executed transaction consumes.
fn consumed_note_row(input_note: &InputNote, cells: NoteCells) -> Vec<String> {
    let authenticated = match input_note {
        InputNote::Authenticated { .. } => "yes",
        InputNote::Unauthenticated { .. } => "no",
    };
    vec![
        input_note.id().to_hex(),
        cells.standard_note,
        cells.note_type,
        input_note.note().metadata().tag().to_string(),
        authenticated.to_string(),
        cells.storage,
        cells.assets,
    ]
}

// OUTPUT NOTES
// ================================================================================================

/// Prints the table of the notes that the transaction created.
///
/// With `with_state`, the table also shows the state of each note in the store.
pub(crate) async fn print_output_notes<AUTH: Keystore + Sync>(
    client: &Client<AUTH>,
    resolver: &FaucetMetadataResolver,
    output_notes: &RawOutputNotes,
    with_state: bool,
) -> Result<(), CliError> {
    println!("\nOutput Notes:");
    if output_notes.is_empty() {
        println!("The transaction created no notes.");
        return Ok(());
    }

    // The transaction keeps the notes as it created them. The store keeps their current state.
    let mut records = BTreeMap::<NoteId, OutputNoteRecord>::new();
    if with_state {
        let note_ids = output_notes.iter().map(RawOutputNote::id).collect::<Vec<_>>();
        records = client
            .get_output_notes(NoteFilter::List(note_ids))
            .await?
            .into_iter()
            .map(|record| (record.id(), record))
            .collect();
    }

    let mut headers = vec!["ID", "Standard Note", "Type", "Tag"];
    if with_state {
        headers.extend(["State", "Expected Height"]);
    }
    headers.extend(["Storage", "Assets"]);
    let mut table = create_dynamic_table(&headers);
    for note in output_notes.iter() {
        let contents = note
            .recipient()
            .map(|recipient| (recipient.script(), recipient.storage().items()));
        let cells =
            note_cells(client, resolver, contents, Some(note.metadata()), note.assets()).await?;
        table.add_row(output_note_row(note, cells, with_state, records.get(&note.id())));
    }

    println!("{table}");
    Ok(())
}

/// Returns the cells of the row of an output note.
///
/// With `with_state`, the row has the state and the expected height of the store `record`. Both are
/// a placeholder if the store has no record of the note.
fn output_note_row(
    note: &RawOutputNote,
    cells: NoteCells,
    with_state: bool,
    record: Option<&OutputNoteRecord>,
) -> Vec<String> {
    let mut row = vec![
        note.id().to_hex(),
        cells.standard_note,
        cells.note_type,
        note.metadata().tag().to_string(),
    ];
    if with_state {
        row.push(record.map_or_else(|| NO_VALUE.to_string(), |record| record.state().to_string()));
        row.push(
            record.map_or_else(
                || NO_VALUE.to_string(),
                |record| record.expected_height().to_string(),
            ),
        );
    }
    row.push(cells.storage);
    row.push(cells.assets);
    row
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client::Word;
    use miden_client::account::AccountId;
    use miden_client::block::BlockNumber;
    use miden_client::note::{
        Note,
        NoteAssets,
        NoteAttachments,
        NoteRecipient,
        NoteStorage,
        NoteTag,
        NoteType,
        PartialNote,
        PartialNoteMetadata,
        StandardNote,
    };
    use miden_client::store::{InputNoteRecord, OutputNoteRecord};
    use miden_client::testing::account_id::ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE;
    use miden_client::transaction::{InputNote, RawOutputNote};

    use super::{NoteCells, consumed_note_row, input_note_row, output_note_row};

    #[test]
    fn input_note_row_puts_the_cells_in_column_order() {
        let sender =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
        let note = Note::new(
            NoteAssets::new(vec![]).unwrap(),
            PartialNoteMetadata::new(sender, NoteType::Public).with_tag(NoteTag::new(42)),
            NoteRecipient::new(
                Word::default(),
                StandardNote::P2ID.script(),
                NoteStorage::new(vec![]).unwrap(),
            ),
        );
        let note_id = note.id();
        let record = InputNoteRecord::from(note);
        let nullifier = record.nullifier().unwrap();
        let cells = NoteCells {
            standard_note: "P2ID".to_string(),
            note_type: "Public".to_string(),
            storage: "target: 0x01".to_string(),
            assets: "100 TST".to_string(),
        };

        assert_eq!(
            input_note_row(&record, &nullifier, cells),
            vec![
                note_id.to_hex(),
                nullifier.to_hex(),
                "P2ID".to_string(),
                "Public".to_string(),
                "Expected (after block 0)".to_string(),
                "target: 0x01".to_string(),
                "100 TST".to_string(),
            ]
        );
    }

    #[test]
    fn consumed_note_row_marks_an_unauthenticated_note() {
        let sender =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
        let tag = NoteTag::new(42);
        let note = Note::new(
            NoteAssets::new(vec![]).unwrap(),
            PartialNoteMetadata::new(sender, NoteType::Public).with_tag(tag),
            NoteRecipient::new(
                Word::default(),
                StandardNote::P2ID.script(),
                NoteStorage::new(vec![]).unwrap(),
            ),
        );
        let note_id = note.id();
        let input_note = InputNote::unauthenticated(note);
        let cells = NoteCells {
            standard_note: "P2ID".to_string(),
            note_type: "Public".to_string(),
            storage: "target: 0x01".to_string(),
            assets: "100 TST".to_string(),
        };

        assert_eq!(
            consumed_note_row(&input_note, cells),
            vec![
                note_id.to_hex(),
                "P2ID".to_string(),
                "Public".to_string(),
                tag.to_string(),
                "no".to_string(),
                "target: 0x01".to_string(),
                "100 TST".to_string(),
            ]
        );
    }

    #[test]
    fn output_note_row_without_state_has_no_state_cells() {
        let sender =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
        let tag = NoteTag::new(42);
        let digest = Word::default();
        let note = RawOutputNote::Partial(PartialNote::new(
            PartialNoteMetadata::new(sender, NoteType::Public).with_tag(tag),
            digest,
            NoteAssets::new(vec![]).unwrap(),
            NoteAttachments::empty(),
        ));
        let cells = NoteCells {
            standard_note: "-".to_string(),
            note_type: "Public".to_string(),
            storage: "-".to_string(),
            assets: "100 TST".to_string(),
        };

        assert_eq!(
            output_note_row(&note, cells, false, None),
            vec![
                note.id().to_hex(),
                "-".to_string(),
                "Public".to_string(),
                tag.to_string(),
                "-".to_string(),
                "100 TST".to_string(),
            ]
        );
    }

    #[test]
    fn output_note_row_of_a_full_note_keeps_its_storage() {
        let sender =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
        let tag = NoteTag::new(7);
        let note = RawOutputNote::Full(Note::new(
            NoteAssets::new(vec![]).unwrap(),
            PartialNoteMetadata::new(sender, NoteType::Private).with_tag(tag),
            NoteRecipient::new(
                Word::default(),
                StandardNote::P2ID.script(),
                NoteStorage::new(vec![]).unwrap(),
            ),
        ));
        let cells = NoteCells {
            standard_note: "P2ID".to_string(),
            note_type: "Private".to_string(),
            storage: "target: 0x01".to_string(),
            assets: "-".to_string(),
        };

        assert_eq!(
            output_note_row(&note, cells, true, None),
            vec![
                note.id().to_hex(),
                "P2ID".to_string(),
                "Private".to_string(),
                tag.to_string(),
                "-".to_string(),
                "-".to_string(),
                "target: 0x01".to_string(),
                "-".to_string(),
            ]
        );
    }

    #[test]
    fn output_note_row_shows_the_state_of_a_stored_note() {
        let sender =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
        let tag = NoteTag::new(7);
        let full_note = Note::new(
            NoteAssets::new(vec![]).unwrap(),
            PartialNoteMetadata::new(sender, NoteType::Public).with_tag(tag),
            NoteRecipient::new(
                Word::default(),
                StandardNote::P2ID.script(),
                NoteStorage::new(vec![]).unwrap(),
            ),
        );
        let record =
            OutputNoteRecord::from_full_note(full_note.clone(), BlockNumber::from(19604u32));
        let note = RawOutputNote::Full(full_note);
        let cells = NoteCells {
            standard_note: "P2ID".to_string(),
            note_type: "Public".to_string(),
            storage: "target: 0x01".to_string(),
            assets: "100 TST".to_string(),
        };

        assert_eq!(
            output_note_row(&note, cells, true, Some(&record)),
            vec![
                note.id().to_hex(),
                "P2ID".to_string(),
                "Public".to_string(),
                tag.to_string(),
                "Expected Full".to_string(),
                "19604".to_string(),
                "target: 0x01".to_string(),
                "100 TST".to_string(),
            ]
        );
    }
}
