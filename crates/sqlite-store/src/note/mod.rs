#![allow(clippy::items_after_statements)]

use std::collections::BTreeMap;
use std::vec::Vec;

use miden_client::account::AccountId;
use miden_client::note::{
    BlockNumber,
    NoteAssets,
    NoteAttachments,
    NoteDetails,
    NoteInclusionProof,
    NoteMetadata,
    NoteRecipient,
    NoteScript,
    NoteUpdateTracker,
    NoteUpdateType,
    Nullifier,
};
use miden_client::store::{
    InputNoteCursor,
    InputNoteRecord,
    InputNoteState,
    NoteFilter,
    OutputNoteRecord,
    OutputNoteState,
    StoreError,
};
use miden_client::utils::{Deserializable, DeserializationError, Serializable};
use miden_client::{SliceReader, Word};
use miden_protocol::note::NoteStorage;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};

use super::SqliteStore;
use crate::chain_data::set_block_header_has_client_notes;
use crate::note::filters::{
    note_filter_input_notes_condition,
    note_filter_to_query_input_notes,
    note_filter_to_query_output_notes,
};
use crate::sql_error::SqlResultExt;
use crate::{column_value_as_u64, insert_sql, subst, u64_to_value, with_write_tx};

mod filters;

// BATCH SIZE CONSTANTS
// ================================================================================================

// SQLite limits statements to 999 parameters. Each batch size is chosen to stay under that
// limit: input notes: 14 columns × 50 = 700, output notes: 11 × 80 = 880, scripts: 2 × 200 = 400.
const INPUT_NOTE_BATCH_SIZE: usize = 50;
const OUTPUT_NOTE_BATCH_SIZE: usize = 80;
const SCRIPT_BATCH_SIZE: usize = 200;

// NOTE SCRIPT UPSERT
// ================================================================================================

// `input_notes.script_root` references `notes_scripts.script_root`, so replacing a script row
// deletes the parent and forces a foreign key check against every referencing note. Updating the
// row in place keeps the parent alive, so no check runs at all.
const UPSERT_NOTE_SCRIPT_QUERY: &str = "INSERT INTO `notes_scripts` \
     (`script_root`, `serialized_note_script`) VALUES (?, ?) \
     ON CONFLICT(`script_root`) DO UPDATE SET \
     `serialized_note_script` = excluded.`serialized_note_script`";

#[cfg(test)]
mod tests;

// TYPES
// ================================================================================================

/// Represents an `InputNoteRecord` serialized to be stored in the database.
struct SerializedInputNoteData {
    pub details_commitment: Vec<u8>,
    pub id: Option<Vec<u8>>,
    pub assets: Vec<u8>,
    pub attachments: Vec<u8>,
    pub serial_number: Vec<u8>,
    pub inputs: Vec<u8>,
    pub script_root: Vec<u8>,
    pub script: Vec<u8>,
    pub nullifier: Option<Vec<u8>>,
    pub state_discriminant: u8,
    pub state: Vec<u8>,
    pub created_at: u64,
    pub consumed_block_height: Option<u32>,
    pub consumed_tx_order: Option<u32>,
    pub consumer_account_id: Option<Vec<u8>>,
}

/// Represents an `OutputNoteRecord` serialized to be stored in the database.
struct SerializedOutputNoteData {
    pub details_commitment: Vec<u8>,
    pub id: Vec<u8>,
    pub assets: Vec<u8>,
    pub metadata: Vec<u8>,
    pub nullifier: Option<Vec<u8>>,
    pub recipient_digest: Vec<u8>,
    pub expected_height: u32,
    pub script_root: Option<Vec<u8>>,
    pub script: Option<Vec<u8>>,
    pub state_discriminant: u8,
    pub state: Vec<u8>,
    pub attachments: Vec<u8>,
}

/// Represents the parts retrieved from the database to build an `InputNoteRecord`.
struct SerializedInputNoteParts {
    pub assets: Vec<u8>,
    pub serial_number: Vec<u8>,
    pub inputs: Vec<u8>,
    pub script: Vec<u8>,
    pub state: Vec<u8>,
    pub created_at: u64,
    pub attachments: Vec<u8>,
}

/// Represents the parts retrieved from the database to build an `OutputNoteRecord`.
struct SerializedOutputNoteParts {
    pub assets: Vec<u8>,
    pub metadata: Vec<u8>,
    pub recipient_digest: Vec<u8>,
    pub expected_height: u32,
    pub state: Vec<u8>,
    pub attachments: Vec<u8>,
    pub script: Option<Vec<u8>>,
}

/// Represents the fields needed to update an existing input note's state.
struct SerializedInputNoteStateUpdate {
    pub details_commitment: Vec<u8>,
    pub state_discriminant: u8,
    pub state: Vec<u8>,
    pub attachments: Vec<u8>,
    pub consumed_block_height: Option<u32>,
    pub consumed_tx_order: Option<u32>,
    pub consumer_account_id: Option<Vec<u8>>,
}

/// Represents the fields needed to update an existing output note's state.
struct SerializedOutputNoteStateUpdate {
    pub details_commitment: Vec<u8>,
    pub state_discriminant: u8,
    pub state: Vec<u8>,
}

// COLUMN LISTS
// ================================================================================================

// Each SELECT list below is the single source of truth for its row mapper: the filter modules
// build their queries from these constants and the mappers read the same columns by name, so the
// two cannot drift apart.

/// Columns read by [`parse_input_note_columns`].
pub(super) const INPUT_NOTE_COLUMNS: &str = "note.assets AS assets, \
     note.serial_number AS serial_number, \
     note.inputs AS inputs, \
     script.serialized_note_script AS serialized_note_script, \
     note.state AS state, \
     note.created_at AS created_at, \
     note.attachments AS attachments";

/// Columns read by [`parse_output_note_columns`].
pub(super) const OUTPUT_NOTE_COLUMNS: &str = "note.recipient_digest AS recipient_digest, \
     note.assets AS assets, \
     note.metadata AS metadata, \
     note.expected_height AS expected_height, \
     note.state AS state, \
     note.attachments AS attachments, \
     script.serialized_note_script AS serialized_note_script";

// NOTES STORE METHODS
// ================================================================================================

impl SqliteStore {
    pub(crate) fn get_input_notes(
        conn: &mut Connection,
        filter: &NoteFilter,
    ) -> Result<Vec<InputNoteRecord>, StoreError> {
        let (query, params) = note_filter_to_query_input_notes(filter);
        let notes = conn
            .prepare(query.as_str())
            .into_store_error()?
            .query_map(params_from_iter(params), parse_input_note_columns)
            .into_store_error()?
            .map(|result| Ok(result.into_store_error()?).and_then(parse_input_note))
            .collect::<Result<Vec<InputNoteRecord>, _>>()?;

        Ok(notes)
    }

    /// Retrieves the output notes from the database.
    pub(crate) fn get_output_notes(
        conn: &mut Connection,
        filter: &NoteFilter,
    ) -> Result<Vec<OutputNoteRecord>, StoreError> {
        let (query, params) = note_filter_to_query_output_notes(filter);
        let notes = conn
            .prepare(&query)
            .into_store_error()?
            .query_map(params_from_iter(params), parse_output_note_columns)
            .into_store_error()?
            .map(|result| Ok(result.into_store_error()?).and_then(parse_output_note))
            .collect::<Result<Vec<OutputNoteRecord>, _>>()?;

        Ok(notes)
    }

    /// Retrieves the input note following `cursor` in the filtered set, restricted to a consumer
    /// account and optionally to a block range.
    pub(crate) fn get_input_note_after(
        conn: &mut Connection,
        filter: &NoteFilter,
        consumer: AccountId,
        block_start: Option<BlockNumber>,
        block_end: Option<BlockNumber>,
        cursor: Option<InputNoteCursor>,
    ) -> Result<Option<InputNoteRecord>, StoreError> {
        let (query, params) = filters::note_filter_to_query_input_note_after(
            filter,
            consumer,
            block_start,
            block_end,
            cursor,
        );
        let note = conn
            .prepare_cached(&query)
            .into_store_error()?
            .query_map(params_from_iter(params), parse_input_note_columns)
            .into_store_error()?
            .map(|result| Ok(result.into_store_error()?).and_then(parse_input_note))
            .next()
            .transpose()?;

        Ok(note)
    }

    pub(crate) fn upsert_input_notes(
        conn: &mut Connection,
        notes: &[InputNoteRecord],
    ) -> Result<(), StoreError> {
        with_write_tx(conn, |tx| {
            for note in notes {
                upsert_input_note_tx(tx, note)?;

                // Whenever we insert a note, we also update block relevance
                if let Some(inclusion_proof) = note.inclusion_proof() {
                    set_block_header_has_client_notes(
                        tx,
                        inclusion_proof.location().block_num().as_u64(),
                        true,
                    )?;
                }
            }
            Ok(())
        })
    }

    pub(crate) fn get_unspent_input_note_nullifiers(
        conn: &mut Connection,
    ) -> Result<Vec<Nullifier>, StoreError> {
        let (unspent_condition, _) = note_filter_input_notes_condition(&NoteFilter::Unspent);
        let query = format!(
            "SELECT nullifier FROM input_notes \
             WHERE {unspent_condition} AND nullifier IS NOT NULL"
        );
        conn.prepare(&query)
            .into_store_error()?
            .query_map([], |row| row.get(0))
            .expect("no binding parameters used in query")
            .map(|result| {
                let v: Vec<u8> = result.into_store_error()?;
                Ok(Nullifier::read_from_bytes(&v)?)
            })
            .collect::<Result<Vec<Nullifier>, _>>()
    }

    pub(crate) fn upsert_note_scripts(
        conn: &mut Connection,
        note_scripts: &[NoteScript],
    ) -> Result<(), StoreError> {
        with_write_tx(conn, |tx| {
            for note_script in note_scripts {
                upsert_note_script_tx(tx, note_script)?;
            }
            Ok(())
        })
    }

    /// Retrieves a note script by its root from the database.
    pub(crate) fn get_note_script(
        conn: &mut Connection,
        script_root: Word,
    ) -> Result<NoteScript, StoreError> {
        const QUERY: &str =
            "SELECT serialized_note_script FROM notes_scripts WHERE script_root = ?";
        let script_bytes: Option<Vec<u8>> = conn
            .prepare_cached(QUERY)
            .into_store_error()?
            .query_row([script_root.to_bytes()], |row| row.get(0))
            .optional()
            .into_store_error()?;

        match script_bytes {
            Some(bytes) => Ok(NoteScript::from_bytes(&bytes)?),
            None => Err(StoreError::NoteScriptNotFound(script_root.to_hex())),
        }
    }
}

// HELPERS
// ================================================================================================

/// Inserts the provided input note into the database, if the note already exists, it will be
/// replaced.
pub(super) fn upsert_input_note_tx(
    tx: &Transaction<'_>,
    note: &InputNoteRecord,
) -> Result<(), StoreError> {
    let SerializedInputNoteData {
        details_commitment,
        id,
        assets,
        attachments,
        serial_number,
        inputs,
        script_root,
        script,
        nullifier,
        state_discriminant,
        state,
        created_at,
        consumed_block_height,
        consumed_tx_order,
        consumer_account_id,
    } = serialize_input_note(note);

    tx.prepare_cached(UPSERT_NOTE_SCRIPT_QUERY)
        .into_store_error()?
        .execute(params![script_root, script])
        .into_store_error()?;

    const NOTE_QUERY: &str = insert_sql!(
        input_notes {
            details_commitment,
            note_id,
            assets,
            attachments,
            serial_number,
            inputs,
            script_root,
            nullifier,
            state_discriminant,
            state,
            created_at,
            consumed_block_height,
            consumed_tx_order,
            consumer_account_id,
        } | REPLACE
    );

    tx.prepare_cached(NOTE_QUERY)
        .into_store_error()?
        .execute(params![
            details_commitment,
            id,
            assets,
            attachments,
            serial_number,
            inputs,
            script_root,
            nullifier,
            state_discriminant,
            state,
            created_at,
            consumed_block_height,
            consumed_tx_order,
            consumer_account_id,
        ])
        .into_store_error()?;

    Ok(())
}

/// Parse input note columns from the provided row into native types.
fn parse_input_note_columns(
    row: &rusqlite::Row<'_>,
) -> Result<SerializedInputNoteParts, rusqlite::Error> {
    let assets: Vec<u8> = row.get("assets")?;
    let serial_number: Vec<u8> = row.get("serial_number")?;
    let inputs: Vec<u8> = row.get("inputs")?;
    let script: Vec<u8> = row.get("serialized_note_script")?;
    let state: Vec<u8> = row.get("state")?;
    let created_at: u64 = column_value_as_u64(row, "created_at")?;
    let attachments: Vec<u8> = row.get("attachments")?;

    Ok(SerializedInputNoteParts {
        assets,
        serial_number,
        inputs,
        script,
        state,
        created_at,
        attachments,
    })
}

/// Parse a note from the provided parts.
fn parse_input_note(
    serialized_input_note_parts: SerializedInputNoteParts,
) -> Result<InputNoteRecord, StoreError> {
    let SerializedInputNoteParts {
        assets,
        serial_number,
        inputs,
        script,
        state,
        created_at,
        attachments,
    } = serialized_input_note_parts;

    let assets = NoteAssets::read_from_bytes(&assets)?;

    let serial_number = Word::read_from_bytes(&serial_number)?;
    let script = NoteScript::read_from_bytes(&script)?;
    let inputs = NoteStorage::read_from_bytes(&inputs)?;
    let recipient = NoteRecipient::new(serial_number, script, inputs);

    let details = NoteDetails::new(assets, recipient);
    let attachments = NoteAttachments::read_from_bytes(&attachments)?;

    let state = InputNoteState::read_from_bytes(&state)?;

    Ok(InputNoteRecord::new(details, attachments, Some(created_at), state))
}

/// Serialize the provided input note into database compatible types.
fn serialize_input_note(note: &InputNoteRecord) -> SerializedInputNoteData {
    let details_commitment = note.details_commitment().to_bytes();
    // `note_id` and `nullifier` require metadata, so they're only available when the record
    // carries it. The columns are NULL-able and get populated once metadata arrives (via
    // sync / inclusion proof).
    let id = note.id().map(|id| id.as_word().to_bytes());
    let nullifier = note.metadata().map(|metadata| {
        miden_client::note::Nullifier::from_details_and_metadata(note.details(), metadata)
            .to_bytes()
    });
    let created_at = note.created_at().unwrap_or(0);

    let details = note.details();
    let assets = details.assets().to_bytes();
    let attachments = note.attachments().to_bytes();
    let recipient = details.recipient();

    let serial_number = recipient.serial_num().to_bytes();
    let script = recipient.script().to_bytes();
    let inputs = recipient.storage().to_bytes();

    let script_root = recipient.script().root().to_bytes();

    let state_discriminant = note.state().discriminant();
    let state = note.state().to_bytes();

    let consumed_block_height = note.state().consumed_block_height().map(|h| h.as_u32());
    let consumed_tx_order = note.state().consumed_tx_order();
    let consumer_account_id = note.consumer_account().map(|id| id.to_bytes());

    SerializedInputNoteData {
        details_commitment,
        id,
        assets,
        attachments,
        serial_number,
        inputs,
        script_root,
        script,
        nullifier,
        state_discriminant,
        state,
        created_at,
        consumed_block_height,
        consumed_tx_order,
        consumer_account_id,
    }
}

/// Parse output note columns from the provided row into native types.
fn parse_output_note_columns(
    row: &rusqlite::Row<'_>,
) -> Result<SerializedOutputNoteParts, rusqlite::Error> {
    let recipient_digest: Vec<u8> = row.get("recipient_digest")?;
    let assets: Vec<u8> = row.get("assets")?;
    let metadata: Vec<u8> = row.get("metadata")?;
    let expected_height: u32 = row.get("expected_height")?;
    let state: Vec<u8> = row.get("state")?;
    let attachments: Vec<u8> = row.get("attachments")?;
    let script: Option<Vec<u8>> = row.get("serialized_note_script")?;

    Ok(SerializedOutputNoteParts {
        assets,
        metadata,
        recipient_digest,
        expected_height,
        state,
        attachments,
        script,
    })
}

/// Parse a note from the provided parts.
fn parse_output_note(
    serialized_output_note_parts: SerializedOutputNoteParts,
) -> Result<OutputNoteRecord, StoreError> {
    let SerializedOutputNoteParts {
        recipient_digest,
        assets,
        metadata,
        expected_height,
        state,
        attachments,
        script,
    } = serialized_output_note_parts;

    let recipient_digest = Word::read_from_bytes(&recipient_digest)?;
    let assets = NoteAssets::read_from_bytes(&assets)?;
    let metadata = NoteMetadata::read_from_bytes(&metadata)?;
    let script = script.map(|script| NoteScript::read_from_bytes(&script)).transpose()?;
    let state = decode_output_note_state(&state, script)?;
    let attachments = NoteAttachments::read_from_bytes(&attachments)?;

    Ok(OutputNoteRecord::new(
        recipient_digest,
        assets,
        metadata,
        state,
        BlockNumber::from(expected_height),
        attachments,
    ))
}

/// Serialize the provided input note state into a lightweight update.
fn serialize_input_note_state(note: &InputNoteRecord) -> SerializedInputNoteStateUpdate {
    let consumed_block_height = note.state().consumed_block_height().map(|h| h.as_u32());
    let consumed_tx_order = note.state().consumed_tx_order();
    let consumer_account_id = note.consumer_account().map(|id| id.to_bytes());

    SerializedInputNoteStateUpdate {
        details_commitment: note.details_commitment().to_bytes(),
        state_discriminant: note.state().discriminant(),
        state: note.state().to_bytes(),
        attachments: note.attachments().to_bytes(),
        consumed_block_height,
        consumed_tx_order,
        consumer_account_id,
    }
}

/// Serialize the provided output note state into a lightweight state-only update.
fn serialize_output_note_state(note: &OutputNoteRecord) -> SerializedOutputNoteStateUpdate {
    SerializedOutputNoteStateUpdate {
        details_commitment: note.details_commitment().to_bytes(),
        state_discriminant: note.state().discriminant(),
        state: encode_output_note_state(note.state()),
    }
}

/// Serialize the provided output note into database compatible types.
fn serialize_output_note(note: &OutputNoteRecord) -> SerializedOutputNoteData {
    let details_commitment = note.details_commitment().to_bytes();
    let id = note.id().as_word().to_bytes();
    let assets = note.assets().to_bytes();
    let recipient_digest = note.recipient_digest().to_bytes();
    let metadata = note.metadata().to_bytes();

    let nullifier = note.nullifier().map(|nullifier| nullifier.to_bytes());

    // The script is only known when the note's full details (recipient) are known. It is stored
    // in the shared `notes_scripts` table, with the note row referencing it by root.
    let script_root = note.script_root().map(|root| root.to_bytes());
    let script = note.recipient().map(|recipient| recipient.script().to_bytes());

    let state_discriminant = note.state().discriminant();
    let state = encode_output_note_state(note.state());

    let attachments = note.attachments().to_bytes();

    SerializedOutputNoteData {
        details_commitment,
        id,
        assets,
        metadata,
        nullifier,
        recipient_digest,
        expected_height: note.expected_height().as_u32(),
        script_root,
        script,
        state_discriminant,
        state,
        attachments,
    }
}

// OUTPUT NOTE STATE CODEC
// ================================================================================================

/// Serializes an output note state for storage, leaving the recipient's script out.
///
/// The script lives in the shared `notes_scripts` table, referenced by the row's `script_root`
/// column.
fn encode_output_note_state(state: &OutputNoteState) -> Vec<u8> {
    let mut target = Vec::new();
    state.discriminant().write_into(&mut target);
    match state {
        OutputNoteState::ExpectedPartial => {},
        OutputNoteState::ExpectedFull { recipient } => {
            write_recipient_without_script(recipient, &mut target);
        },
        OutputNoteState::CommittedPartial { inclusion_proof } => {
            inclusion_proof.write_into(&mut target);
        },
        OutputNoteState::CommittedFull { recipient, inclusion_proof } => {
            write_recipient_without_script(recipient, &mut target);
            inclusion_proof.write_into(&mut target);
        },
        OutputNoteState::Consumed { block_height, recipient } => {
            block_height.write_into(&mut target);
            write_recipient_without_script(recipient, &mut target);
        },
    }
    target
}

/// Deserializes an output note state written by [`encode_output_note_state`], completing the
/// recipient with the script read from the `notes_scripts` table.
fn decode_output_note_state(
    bytes: &[u8],
    script: Option<NoteScript>,
) -> Result<OutputNoteState, StoreError> {
    let mut source = SliceReader::new(bytes);
    let state = match u8::read_from(&mut source)? {
        OutputNoteState::STATE_EXPECTED_PARTIAL => OutputNoteState::ExpectedPartial,
        OutputNoteState::STATE_EXPECTED_FULL => OutputNoteState::ExpectedFull {
            recipient: read_recipient_with_script(&mut source, script)?,
        },
        OutputNoteState::STATE_COMMITTED_PARTIAL => OutputNoteState::CommittedPartial {
            inclusion_proof: NoteInclusionProof::read_from(&mut source)?,
        },
        OutputNoteState::STATE_COMMITTED_FULL => {
            let recipient = read_recipient_with_script(&mut source, script)?;
            let inclusion_proof = NoteInclusionProof::read_from(&mut source)?;
            OutputNoteState::CommittedFull { recipient, inclusion_proof }
        },
        OutputNoteState::STATE_CONSUMED => {
            let block_height = BlockNumber::read_from(&mut source)?;
            let recipient = read_recipient_with_script(&mut source, script)?;
            OutputNoteState::Consumed { block_height, recipient }
        },
        discriminant => {
            return Err(DeserializationError::InvalidValue(format!(
                "unknown output note state discriminant {discriminant}"
            ))
            .into());
        },
    };

    Ok(state)
}

/// Writes the parts of a recipient that the state blob carries: everything except the script.
fn write_recipient_without_script(recipient: &NoteRecipient, target: &mut Vec<u8>) {
    recipient.serial_num().write_into(target);
    recipient.storage().write_into(target);
}

/// Reads a recipient written by [`write_recipient_without_script`], completing it with `script`.
fn read_recipient_with_script(
    source: &mut SliceReader<'_>,
    script: Option<NoteScript>,
) -> Result<NoteRecipient, StoreError> {
    let serial_num = Word::read_from(source)?;
    let storage = NoteStorage::read_from(source)?;
    let script = script.ok_or_else(|| {
        StoreError::DatabaseError("output note state has a recipient but no script row".into())
    })?;

    Ok(NoteRecipient::new(serial_num, script, storage))
}

pub(crate) fn apply_note_updates_tx(
    tx: &Transaction,
    note_updates: &NoteUpdateTracker,
) -> Result<(), StoreError> {
    // Split input notes into inserts and updates, collecting scripts from new notes.
    let mut input_inserts = Vec::new();
    let mut input_updates = Vec::new();
    let mut scripts: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    for input_note in note_updates.updated_input_notes() {
        match input_note.update_type() {
            // `InsertCommitted` is a previously-expected note that just gained its metadata, so it
            // needs a full-row insert (to write `note_id`/`nullifier`), same as `Insert`.
            NoteUpdateType::Insert | NoteUpdateType::InsertCommitted => {
                let serialized = serialize_input_note(input_note.inner());
                scripts.insert(serialized.script_root.clone(), serialized.script.clone());
                input_inserts.push(serialized);
            },
            NoteUpdateType::Update => {
                input_updates.push(serialize_input_note_state(input_note.inner()));
            },
            NoteUpdateType::None => {},
        }
    }

    // Split output notes into inserts and updates, collecting scripts from new notes whose full
    // details are known.
    let mut output_inserts = Vec::new();
    let mut output_updates = Vec::new();

    for output_note in note_updates.updated_output_notes() {
        match output_note.update_type() {
            // Output notes are never assigned `InsertCommitted`, but it is insert-like for
            // exhaustiveness.
            NoteUpdateType::Insert | NoteUpdateType::InsertCommitted => {
                let serialized = serialize_output_note(output_note.inner());
                if let (Some(root), Some(script)) = (&serialized.script_root, &serialized.script) {
                    scripts.insert(root.clone(), script.clone());
                }
                output_inserts.push(serialized);
            },
            NoteUpdateType::Update => {
                output_updates.push(serialize_output_note_state(output_note.inner()));
            },
            NoteUpdateType::None => {},
        }
    }

    // Scripts must be inserted before the notes that reference them via foreign key.
    batch_upsert_scripts(tx, &scripts)?;
    batch_insert_input_notes(tx, &input_inserts)?;
    batch_update_input_note_states(tx, &input_updates)?;
    batch_insert_output_notes(tx, &output_inserts)?;
    batch_update_output_note_states(tx, &output_updates)?;

    Ok(())
}

/// Batch-upsert note scripts using a multi-row insert.
/// Multi-row inserts reduce per-statement overhead and show faster insertion times than
/// individual inserts.
fn batch_upsert_scripts(
    tx: &Transaction,
    scripts: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<(), StoreError> {
    if scripts.is_empty() {
        return Ok(());
    }

    let entries: Vec<_> = scripts.iter().collect();
    for chunk in entries.chunks(SCRIPT_BATCH_SIZE) {
        let placeholders = vec!["(?, ?)"; chunk.len()].join(", ");
        let query = format!(
            "INSERT INTO `notes_scripts` (`script_root`, `serialized_note_script`) \
             VALUES {placeholders} \
             ON CONFLICT(`script_root`) DO UPDATE SET \
             `serialized_note_script` = excluded.`serialized_note_script`"
        );
        let mut param_values: Vec<Value> = Vec::with_capacity(chunk.len() * 2);
        for (root, script) in chunk {
            param_values.push(Value::Blob((*root).clone()));
            param_values.push(Value::Blob((*script).clone()));
        }
        tx.execute(&query, params_from_iter(param_values)).into_store_error()?;
    }

    Ok(())
}

/// Batch-insert new input notes using multi-row INSERT OR REPLACE.
fn batch_insert_input_notes(
    tx: &Transaction,
    notes: &[SerializedInputNoteData],
) -> Result<(), StoreError> {
    if notes.is_empty() {
        return Ok(());
    }

    for chunk in notes.chunks(INPUT_NOTE_BATCH_SIZE) {
        let placeholders =
            vec!["(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"; chunk.len()].join(", ");
        let query = format!(
            "INSERT OR REPLACE INTO `input_notes` \
             (`details_commitment`, `note_id`, `assets`, `attachments`, `serial_number`, \
              `inputs`, `script_root`, `nullifier`, `state_discriminant`, `state`, `created_at`, \
              `consumed_block_height`, `consumed_tx_order`, `consumer_account_id`) \
             VALUES {placeholders}"
        );
        let mut param_values: Vec<Value> = Vec::with_capacity(chunk.len() * 14);
        for note in chunk {
            param_values.push(Value::Blob(note.details_commitment.clone()));
            match &note.id {
                Some(id) => param_values.push(Value::Blob(id.clone())),
                None => param_values.push(Value::Null),
            }
            param_values.push(Value::Blob(note.assets.clone()));
            param_values.push(Value::Blob(note.attachments.clone()));
            param_values.push(Value::Blob(note.serial_number.clone()));
            param_values.push(Value::Blob(note.inputs.clone()));
            param_values.push(Value::Blob(note.script_root.clone()));
            match &note.nullifier {
                Some(n) => param_values.push(Value::Blob(n.clone())),
                None => param_values.push(Value::Null),
            }
            param_values.push(Value::Integer(i64::from(note.state_discriminant)));
            param_values.push(Value::Blob(note.state.clone()));
            param_values.push(u64_to_value(note.created_at));
            match note.consumed_block_height {
                Some(h) => param_values.push(Value::Integer(i64::from(h))),
                None => param_values.push(Value::Null),
            }
            match note.consumed_tx_order {
                Some(o) => param_values.push(Value::Integer(i64::from(o))),
                None => param_values.push(Value::Null),
            }
            match &note.consumer_account_id {
                Some(id) => param_values.push(Value::Blob(id.clone())),
                None => param_values.push(Value::Null),
            }
        }
        tx.execute(&query, params_from_iter(param_values)).into_store_error()?;
    }

    Ok(())
}

/// Batch-update input note states using a prepared cached statement.
fn batch_update_input_note_states(
    tx: &Transaction,
    updates: &[SerializedInputNoteStateUpdate],
) -> Result<(), StoreError> {
    if updates.is_empty() {
        return Ok(());
    }

    let mut stmt = tx
        .prepare_cached(
            "UPDATE `input_notes` SET state_discriminant = ?, state = ?, attachments = ?, \
             consumed_block_height = ?, consumed_tx_order = ?, consumer_account_id = ? \
             WHERE details_commitment = ?",
        )
        .into_store_error()?;

    for update in updates {
        stmt.execute(params![
            update.state_discriminant,
            update.state,
            update.attachments,
            update.consumed_block_height,
            update.consumed_tx_order,
            update.consumer_account_id,
            update.details_commitment,
        ])
        .into_store_error()?;
    }

    Ok(())
}

/// Batch-insert new output notes using multi-row INSERT OR REPLACE.
fn batch_insert_output_notes(
    tx: &Transaction,
    notes: &[SerializedOutputNoteData],
) -> Result<(), StoreError> {
    if notes.is_empty() {
        return Ok(());
    }

    for chunk in notes.chunks(OUTPUT_NOTE_BATCH_SIZE) {
        let placeholders = vec!["(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"; chunk.len()].join(", ");
        let query = format!(
            "INSERT OR REPLACE INTO `output_notes` \
             (`details_commitment`, `note_id`, `assets`, `recipient_digest`, `metadata`, \
              `nullifier`, `expected_height`, `script_root`, `state_discriminant`, `state`, \
              `attachments`) \
             VALUES {placeholders}"
        );
        let mut param_values: Vec<Value> = Vec::with_capacity(chunk.len() * 11);
        for note in chunk {
            param_values.push(Value::Blob(note.details_commitment.clone()));
            param_values.push(Value::Blob(note.id.clone()));
            param_values.push(Value::Blob(note.assets.clone()));
            param_values.push(Value::Blob(note.recipient_digest.clone()));
            param_values.push(Value::Blob(note.metadata.clone()));
            match &note.nullifier {
                Some(n) => param_values.push(Value::Blob(n.clone())),
                None => param_values.push(Value::Null),
            }
            param_values.push(Value::Integer(i64::from(note.expected_height)));
            match &note.script_root {
                Some(root) => param_values.push(Value::Blob(root.clone())),
                None => param_values.push(Value::Null),
            }
            param_values.push(Value::Integer(i64::from(note.state_discriminant)));
            param_values.push(Value::Blob(note.state.clone()));
            param_values.push(Value::Blob(note.attachments.clone()));
        }
        tx.execute(&query, params_from_iter(param_values)).into_store_error()?;
    }

    Ok(())
}

/// Batch-update output note states using a prepared cached statement.
fn batch_update_output_note_states(
    tx: &Transaction,
    updates: &[SerializedOutputNoteStateUpdate],
) -> Result<(), StoreError> {
    if updates.is_empty() {
        return Ok(());
    }

    let mut stmt = tx
        .prepare_cached(
            "UPDATE `output_notes` SET state_discriminant = ?, state = ? WHERE details_commitment = ?",
        )
        .into_store_error()?;

    for update in updates {
        stmt.execute(params![update.state_discriminant, update.state, update.details_commitment])
            .into_store_error()?;
    }

    Ok(())
}

/// Inserts the provided note script into the database, if the script already exists, it will be
/// updated.
pub(super) fn upsert_note_script_tx(
    tx: &Transaction<'_>,
    note_script: &NoteScript,
) -> Result<(), StoreError> {
    tx.prepare_cached(UPSERT_NOTE_SCRIPT_QUERY)
        .into_store_error()?
        .execute(params![note_script.root().to_bytes(), note_script.to_bytes()])
        .into_store_error()?;

    Ok(())
}
