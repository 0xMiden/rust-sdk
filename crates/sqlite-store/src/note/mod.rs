#![allow(clippy::items_after_statements)]

use std::collections::BTreeMap;
use std::rc::Rc;
use std::string::{String, ToString};
use std::vec::Vec;

use miden_client::Word;
use miden_client::account::AccountId;
use miden_client::note::{
    BlockNumber,
    NoteAssets,
    NoteAttachments,
    NoteDetails,
    NoteMetadata,
    NoteRecipient,
    NoteScript,
    NoteUpdateTracker,
    NoteUpdateType,
    Nullifier,
};
use miden_client::store::{
    InputNoteRecord,
    InputNoteState,
    NoteFilter,
    OutputNoteRecord,
    OutputNoteState,
    StoreError,
};
use miden_client::utils::{Deserializable, Serializable};
use miden_protocol::note::NoteStorage;
use rusqlite::types::Value;
use rusqlite::{Connection, Transaction, params, params_from_iter};

use super::SqliteStore;
use crate::chain_data::set_block_header_has_client_notes;
use crate::note::filters::{note_filter_to_query_input_notes, note_filter_to_query_output_notes};
use crate::sql_error::SqlResultExt;
use crate::{insert_sql, subst};

mod filters;

// BATCH SIZE CONSTANTS
// ================================================================================================

// SQLite limits statements to 999 parameters. Each batch size is chosen to stay under that
// limit: input notes: 13 columns × 50 = 650, output notes: 8 × 80 = 640, scripts: 2 × 200 = 400.
const INPUT_NOTE_BATCH_SIZE: usize = 50;
const OUTPUT_NOTE_BATCH_SIZE: usize = 80;
const SCRIPT_BATCH_SIZE: usize = 200;

#[cfg(test)]
mod tests;

// TYPES
// ================================================================================================

/// Represents an `InputNoteRecord` serialized to be stored in the database.
struct SerializedInputNoteData {
    pub details_commitment: String,
    pub id: Option<String>,
    pub assets: Vec<u8>,
    pub attachments: Vec<u8>,
    pub serial_number: Vec<u8>,
    pub inputs: Vec<u8>,
    pub script_root: String,
    pub script: Vec<u8>,
    pub nullifier: Option<String>,
    pub state_discriminant: u8,
    pub state: Vec<u8>,
    pub created_at: u64,
    pub consumed_block_height: Option<u32>,
    pub consumed_tx_order: Option<u32>,
    pub consumer_account_id: Option<String>,
}

/// Represents an `OutputNoteRecord` serialized to be stored in the database.
struct SerializedOutputNoteData {
    pub details_commitment: String,
    pub id: String,
    pub assets: Vec<u8>,
    pub metadata: Vec<u8>,
    pub nullifier: Option<String>,
    pub recipient_digest: String,
    pub expected_height: u32,
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
    pub recipient_digest: String,
    pub expected_height: u32,
    pub state: Vec<u8>,
    pub attachments: Vec<u8>,
}

/// Represents the fields needed to update an existing input note's state.
struct SerializedInputNoteStateUpdate {
    pub details_commitment: String,
    pub state_discriminant: u8,
    pub state: Vec<u8>,
    pub consumed_block_height: Option<u32>,
    pub consumed_tx_order: Option<u32>,
    pub consumer_account_id: Option<String>,
}

/// Represents the fields needed to update an existing output note's state.
struct SerializedOutputNoteStateUpdate {
    pub details_commitment: String,
    pub state_discriminant: u8,
    pub state: Vec<u8>,
}

/// Represents the pars retrieved form the database to build a `NoteScript`
struct SerializedNoteScriptPars {
    pub script: Vec<u8>,
}

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
            .expect("no binding parameters used in query")
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
            .expect("no binding parameters used in query")
            .map(|result| Ok(result.into_store_error()?).and_then(parse_output_note))
            .collect::<Result<Vec<OutputNoteRecord>, _>>()?;

        Ok(notes)
    }

    /// Retrieves a single input note at the given offset from the filtered set, restricted to a
    /// consumer account and optionally to a block range.
    pub(crate) fn get_input_note_by_offset(
        conn: &mut Connection,
        filter: &NoteFilter,
        consumer: AccountId,
        block_start: Option<BlockNumber>,
        block_end: Option<BlockNumber>,
        offset: u32,
    ) -> Result<Option<InputNoteRecord>, StoreError> {
        let consumer_hex = consumer.to_hex();
        let (query, params) = filters::note_filter_to_query_input_note_by_offset(
            filter,
            &consumer_hex,
            block_start,
            block_end,
            offset,
        );
        let note = conn
            .prepare(&query)
            .into_store_error()?
            .query_map(params_from_iter(params), parse_input_note_columns)
            .expect("no binding parameters used in query")
            .map(|result| Ok(result.into_store_error()?).and_then(parse_input_note))
            .next()
            .transpose()?;

        Ok(note)
    }

    pub(crate) fn upsert_input_notes(
        conn: &mut Connection,
        notes: &[InputNoteRecord],
    ) -> Result<(), StoreError> {
        let tx = conn.transaction().into_store_error()?;

        for note in notes {
            upsert_input_note_tx(&tx, note)?;

            // Whenever we insert a note, we also update block relevance
            if let Some(inclusion_proof) = note.inclusion_proof() {
                set_block_header_has_client_notes(
                    &tx,
                    inclusion_proof.location().block_num().as_u64(),
                    true,
                )?;
            }
        }

        tx.commit().into_store_error()
    }

    pub(crate) fn get_unspent_input_note_nullifiers(
        conn: &mut Connection,
    ) -> Result<Vec<Nullifier>, StoreError> {
        const QUERY: &str =
            "SELECT nullifier FROM input_notes WHERE state_discriminant NOT IN rarray(?)";
        let unspent_filters = Rc::new(vec![
            Value::from(InputNoteState::STATE_CONSUMED_AUTHENTICATED_LOCAL.to_string()),
            Value::from(InputNoteState::STATE_CONSUMED_UNAUTHENTICATED_LOCAL.to_string()),
            Value::from(InputNoteState::STATE_CONSUMED_EXTERNAL.to_string()),
        ]);
        conn.prepare(QUERY)
            .into_store_error()?
            .query_map([unspent_filters], |row| row.get(0))
            .expect("no binding parameters used in query")
            .map(|result| {
                result
                    .map_err(|err| StoreError::ParsingError(err.to_string()))
                    .and_then(|v: String| Ok(Nullifier::from_hex(&v)?))
            })
            .collect::<Result<Vec<Nullifier>, _>>()
    }

    pub(crate) fn upsert_note_scripts(
        conn: &mut Connection,
        note_scripts: &[NoteScript],
    ) -> Result<(), StoreError> {
        let tx = conn.transaction().into_store_error()?;

        for note_script in note_scripts {
            upsert_note_script_tx(&tx, note_script)?;
        }

        tx.commit().into_store_error()
    }

    /// Retrieves the note scripts from the database.
    pub(crate) fn get_note_script(
        conn: &mut Connection,
        script_root: Word,
    ) -> Result<NoteScript, StoreError> {
        let script_root = script_root.to_hex();
        let query = "SELECT * FROM notes_scripts WHERE script_root = ?";
        let note_script = conn
            .prepare(query)
            .into_store_error()?
            .query_map([script_root.clone()], parse_note_scripts_columns)
            .expect("no binding parameters used in query")
            .map(|result| Ok(result.into_store_error()?).and_then(|s| parse_note_script(&s)))
            .collect::<Result<Vec<NoteScript>, _>>()?
            .first()
            .cloned()
            .ok_or(StoreError::NoteScriptNotFound(script_root))?;

        Ok(note_script)
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

    const SCRIPT_QUERY: &str =
        insert_sql!(notes_scripts { script_root, serialized_note_script } | REPLACE);
    tx.prepare_cached(SCRIPT_QUERY)
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
    let assets: Vec<u8> = row.get(0)?;
    let serial_number: Vec<u8> = row.get(1)?;
    let inputs: Vec<u8> = row.get(2)?;
    let script: Vec<u8> = row.get(3)?;
    let state: Vec<u8> = row.get(4)?;
    let created_at: u64 = row.get(5)?;
    let attachments: Vec<u8> = row.get(6)?;

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
    let details_commitment = note.details_commitment().to_hex();
    // `note_id` and `nullifier` require metadata, so they're only available when the record
    // carries it. The columns are NULL-able and get populated once metadata arrives (via
    // sync / inclusion proof).
    let id = note.id().map(|id| id.as_word().to_string());
    let nullifier = note.metadata().map(|metadata| {
        miden_client::note::Nullifier::from_details_and_metadata(note.details(), metadata).to_hex()
    });
    let created_at = note.created_at().unwrap_or(0);

    let details = note.details();
    let assets = details.assets().to_bytes();
    let attachments = note.attachments().to_bytes();
    let recipient = details.recipient();

    let serial_number = recipient.serial_num().to_bytes();
    let script = recipient.script().to_bytes();
    let inputs = recipient.storage().to_bytes();

    let script_root = recipient.script().root().to_hex();

    let state_discriminant = note.state().discriminant();
    let state = note.state().to_bytes();

    let consumed_block_height = note.state().consumed_block_height().map(|h| h.as_u32());
    let consumed_tx_order = note.state().consumed_tx_order();
    let consumer_account_id = note.consumer_account().map(AccountId::to_hex);

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
    let recipient_digest: String = row.get(0)?;
    let assets: Vec<u8> = row.get(1)?;
    let metadata: Vec<u8> = row.get(2)?;
    let expected_height: u32 = row.get(3)?;
    let state: Vec<u8> = row.get(4)?;
    let attachments: Vec<u8> = row.get(5)?;

    Ok(SerializedOutputNoteParts {
        assets,
        metadata,
        recipient_digest,
        expected_height,
        state,
        attachments,
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
    } = serialized_output_note_parts;

    let recipient_digest = Word::try_from(recipient_digest)?;
    let assets = NoteAssets::read_from_bytes(&assets)?;
    let metadata = NoteMetadata::read_from_bytes(&metadata)?;
    let state = OutputNoteState::read_from_bytes(&state)?;
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
    let consumer_account_id = note.consumer_account().map(AccountId::to_hex);

    SerializedInputNoteStateUpdate {
        details_commitment: note.details_commitment().to_hex(),
        state_discriminant: note.state().discriminant(),
        state: note.state().to_bytes(),
        consumed_block_height,
        consumed_tx_order,
        consumer_account_id,
    }
}

/// Serialize the provided output note state into a lightweight state-only update.
fn serialize_output_note_state(note: &OutputNoteRecord) -> SerializedOutputNoteStateUpdate {
    SerializedOutputNoteStateUpdate {
        details_commitment: note.details_commitment().to_hex(),
        state_discriminant: note.state().discriminant(),
        state: note.state().to_bytes(),
    }
}

/// Serialize the provided output note into database compatible types.
fn serialize_output_note(note: &OutputNoteRecord) -> SerializedOutputNoteData {
    let details_commitment = note.details_commitment().to_hex();
    let id = note.id().as_word().to_string();
    let assets = note.assets().to_bytes();
    let recipient_digest = note.recipient_digest().to_hex();
    let metadata = note.metadata().to_bytes();

    let nullifier = note.nullifier().map(|nullifier| nullifier.to_hex());

    let state_discriminant = note.state().discriminant();
    let state = note.state().to_bytes();

    let attachments = note.attachments().to_bytes();

    SerializedOutputNoteData {
        details_commitment,
        id,
        assets,
        metadata,
        nullifier,
        recipient_digest,
        expected_height: note.expected_height().as_u32(),
        state_discriminant,
        state,
        attachments,
    }
}

pub(crate) fn apply_note_updates_tx(
    tx: &Transaction,
    note_updates: &NoteUpdateTracker,
) -> Result<(), StoreError> {
    // Split input notes into inserts and updates, collecting scripts from new notes.
    let mut input_inserts = Vec::new();
    let mut input_updates = Vec::new();
    let mut scripts: BTreeMap<String, Vec<u8>> = BTreeMap::new();

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

    batch_upsert_scripts(tx, &scripts)?;
    batch_insert_input_notes(tx, &input_inserts)?;
    batch_update_input_note_states(tx, &input_updates)?;

    // Split output notes into inserts and updates.
    let mut output_inserts = Vec::new();
    let mut output_updates = Vec::new();

    for output_note in note_updates.updated_output_notes() {
        match output_note.update_type() {
            // Output notes are never assigned `InsertCommitted`, but it is insert-like for
            // exhaustiveness.
            NoteUpdateType::Insert | NoteUpdateType::InsertCommitted => {
                output_inserts.push(serialize_output_note(output_note.inner()));
            },
            NoteUpdateType::Update => {
                output_updates.push(serialize_output_note_state(output_note.inner()));
            },
            NoteUpdateType::None => {},
        }
    }

    batch_insert_output_notes(tx, &output_inserts)?;
    batch_update_output_note_states(tx, &output_updates)?;

    Ok(())
}

/// Batch-insert note scripts using multi-row INSERT OR REPLACE.
/// Multi-row inserts reduce per-statement overhead and show faster insertion times than
/// individual inserts.
fn batch_upsert_scripts(
    tx: &Transaction,
    scripts: &BTreeMap<String, Vec<u8>>,
) -> Result<(), StoreError> {
    if scripts.is_empty() {
        return Ok(());
    }

    let entries: Vec<_> = scripts.iter().collect();
    for chunk in entries.chunks(SCRIPT_BATCH_SIZE) {
        let placeholders = vec!["(?, ?)"; chunk.len()].join(", ");
        let query = format!(
            "INSERT OR REPLACE INTO `notes_scripts` (`script_root`, `serialized_note_script`) \
             VALUES {placeholders}"
        );
        let mut param_values: Vec<Value> = Vec::with_capacity(chunk.len() * 2);
        for (root, script) in chunk {
            param_values.push(Value::Text((*root).clone()));
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
            param_values.push(Value::Text(note.details_commitment.clone()));
            match &note.id {
                Some(id) => param_values.push(Value::Text(id.clone())),
                None => param_values.push(Value::Null),
            }
            param_values.push(Value::Blob(note.assets.clone()));
            param_values.push(Value::Blob(note.attachments.clone()));
            param_values.push(Value::Blob(note.serial_number.clone()));
            param_values.push(Value::Blob(note.inputs.clone()));
            param_values.push(Value::Text(note.script_root.clone()));
            match &note.nullifier {
                Some(n) => param_values.push(Value::Text(n.clone())),
                None => param_values.push(Value::Null),
            }
            param_values.push(Value::Integer(i64::from(note.state_discriminant)));
            param_values.push(Value::Blob(note.state.clone()));
            #[allow(clippy::cast_possible_wrap)]
            param_values.push(Value::Integer(note.created_at as i64));
            match note.consumed_block_height {
                Some(h) => param_values.push(Value::Integer(i64::from(h))),
                None => param_values.push(Value::Null),
            }
            match note.consumed_tx_order {
                Some(o) => param_values.push(Value::Integer(i64::from(o))),
                None => param_values.push(Value::Null),
            }
            match &note.consumer_account_id {
                Some(id) => param_values.push(Value::Text(id.clone())),
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
            "UPDATE `input_notes` SET state_discriminant = ?, state = ?, \
             consumed_block_height = ?, consumed_tx_order = ?, consumer_account_id = ? \
             WHERE details_commitment = ?",
        )
        .into_store_error()?;

    for update in updates {
        stmt.execute(params![
            update.state_discriminant,
            update.state,
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
        let placeholders = vec!["(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"; chunk.len()].join(", ");
        let query = format!(
            "INSERT OR REPLACE INTO `output_notes` \
             (`details_commitment`, `note_id`, `assets`, `recipient_digest`, `metadata`, \
              `nullifier`, `expected_height`, `state_discriminant`, `state`, `attachments`) \
             VALUES {placeholders}"
        );
        let mut param_values: Vec<Value> = Vec::with_capacity(chunk.len() * 10);
        for note in chunk {
            param_values.push(Value::Text(note.details_commitment.clone()));
            param_values.push(Value::Text(note.id.clone()));
            param_values.push(Value::Blob(note.assets.clone()));
            param_values.push(Value::Text(note.recipient_digest.clone()));
            param_values.push(Value::Blob(note.metadata.clone()));
            match &note.nullifier {
                Some(n) => param_values.push(Value::Text(n.clone())),
                None => param_values.push(Value::Null),
            }
            param_values.push(Value::Integer(i64::from(note.expected_height)));
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
/// replaced.
pub(super) fn upsert_note_script_tx(
    tx: &Transaction<'_>,
    note_script: &NoteScript,
) -> Result<(), StoreError> {
    const QUERY: &str =
        insert_sql!(notes_scripts { script_root, serialized_note_script } | REPLACE);
    tx.prepare_cached(QUERY)
        .into_store_error()?
        .execute(params![note_script.root().to_hex(), note_script.to_bytes()])
        .into_store_error()?;

    Ok(())
}

/// Parse note script columns from the provided row into native types.
fn parse_note_scripts_columns(
    row: &rusqlite::Row<'_>,
) -> Result<SerializedNoteScriptPars, rusqlite::Error> {
    // The script root can be derived from the script itself.
    // There's no need to retrieve it separately.
    // let script_root = row.get(0)?;
    let script = row.get(1)?;

    Ok(SerializedNoteScriptPars { script })
}

/// Parse a note script from the provided parts.
fn parse_note_script(
    serialized_note_script_parts: &SerializedNoteScriptPars,
) -> Result<NoteScript, StoreError> {
    let note_script = NoteScript::from_bytes(&serialized_note_script_parts.script)?;
    Ok(note_script)
}
