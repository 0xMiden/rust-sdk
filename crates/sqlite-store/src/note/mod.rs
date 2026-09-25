#![allow(clippy::items_after_statements)]

use std::collections::BTreeMap;
use std::vec::Vec;

use miden_client::Word;
use miden_client::account::AccountId;
use miden_client::note::{
    BlockNumber,
    NoteDetails,
    NoteRecipient,
    NoteScript,
    NoteUpdateTracker,
    NoteUpdateType,
    Nullifier,
};
use miden_client::store::{
    InputNoteCursor,
    InputNoteRecord,
    NoteFilter,
    OutputNoteRecord,
    StoreError,
    proto,
};
use miden_client::utils::{Deserializable, Serializable};
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
use crate::{column_value_as_u64, u64_to_value, with_write_tx};

mod filters;

// BATCH SIZE CONSTANTS
// ================================================================================================

// SQLite limits statements to 999 parameters. Each batch size is chosen to stay under that limit:
// input notes: 14 columns × 50 = 700, output notes: 11 × 80 = 880, scripts: 2 × 200 = 400.
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

// NOTES STORE METHODS
// ================================================================================================

impl SqliteStore {
    pub(crate) fn get_input_notes(
        conn: &mut Connection,
        filter: &NoteFilter,
    ) -> Result<Vec<InputNoteRecord>, StoreError> {
        let (query, params) = note_filter_to_query_input_notes(filter);
        let mut stmt = conn.prepare(&query).into_store_error()?;
        let mut rows = stmt.query(params_from_iter(params)).into_store_error()?;

        let mut notes = Vec::new();
        while let Some(row) = rows.next().into_store_error()? {
            notes.push(parse_input_note(row)?);
        }

        Ok(notes)
    }

    /// Retrieves the output notes from the database.
    pub(crate) fn get_output_notes(
        conn: &mut Connection,
        filter: &NoteFilter,
    ) -> Result<Vec<OutputNoteRecord>, StoreError> {
        let (query, params) = note_filter_to_query_output_notes(filter);
        let mut stmt = conn.prepare(&query).into_store_error()?;
        let mut rows = stmt.query(params_from_iter(params)).into_store_error()?;

        let mut notes = Vec::new();
        while let Some(row) = rows.next().into_store_error()? {
            notes.push(parse_output_note(row)?);
        }

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
        let mut stmt = conn.prepare_cached(&query).into_store_error()?;
        let mut rows = stmt.query(params_from_iter(params)).into_store_error()?;
        let note = rows.next().into_store_error()?.map(parse_input_note).transpose()?;

        Ok(note)
    }

    pub(crate) fn upsert_input_notes(
        conn: &mut Connection,
        notes: &[InputNoteRecord],
    ) -> Result<(), StoreError> {
        with_write_tx(conn, |tx| {
            let mut scripts: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            let mut serialized = Vec::with_capacity(notes.len());

            for note in notes {
                // A note that carries an inclusion proof makes its block relevant to the client.
                if let Some(inclusion_proof) = note.inclusion_proof() {
                    set_block_header_has_client_notes(
                        tx,
                        inclusion_proof.location().block_num().as_u64(),
                        true,
                    )?;
                }

                let note_data = serialize_input_note(note);
                scripts.insert(note_data.script_root.clone(), note_data.script.clone());
                serialized.push(note_data);
            }

            // Scripts must be written before the notes that reference them by foreign key.
            batch_upsert_scripts(tx, &scripts)?;
            batch_insert_input_notes(tx, &serialized)
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
            Some(bytes) => Ok(proto::decode(&bytes)?),
            None => Err(StoreError::NoteScriptNotFound(script_root.to_hex())),
        }
    }
}

// HELPERS
// ================================================================================================

/// Builds an input note record from one row of the input notes query.
fn parse_input_note(row: &rusqlite::Row<'_>) -> Result<InputNoteRecord, StoreError> {
    let assets: Vec<u8> = row.get("assets").into_store_error()?;
    let serial_number: Vec<u8> = row.get("serial_number").into_store_error()?;
    let inputs: Vec<u8> = row.get("inputs").into_store_error()?;
    let script: Vec<u8> = row.get("serialized_note_script").into_store_error()?;
    let state: Vec<u8> = row.get("state").into_store_error()?;
    let created_at = column_value_as_u64(row, "created_at").into_store_error()?;
    let attachments: Vec<u8> = row.get("attachments").into_store_error()?;

    let assets = proto::decode(&assets)?;
    let serial_number = Word::read_from_bytes(&serial_number)?;
    let script = proto::decode(&script)?;
    let inputs = proto::decode(&inputs)?;
    let recipient = NoteRecipient::new(serial_number, script, inputs);

    let details = NoteDetails::new(assets, recipient);
    let attachments = proto::decode(&attachments)?;
    let state = proto::decode(&state)?;

    Ok(InputNoteRecord::new(details, attachments, Some(created_at), state))
}

/// Serialize the provided input note into database compatible types.
fn serialize_input_note(note: &InputNoteRecord) -> SerializedInputNoteData {
    let details_commitment = note.details_commitment().to_bytes();
    // `note_id` and `nullifier` require metadata, so they're only available when the record carries
    // it. The columns are NULL-able and get populated once metadata arrives (via sync / inclusion
    // proof).
    let id = note.id().map(|id| id.as_word().to_bytes());
    let nullifier = note.nullifier().map(|nullifier| nullifier.to_bytes());
    let created_at = note.created_at().unwrap_or(0);

    let details = note.details();
    let assets = proto::encode(details.assets());
    let attachments = proto::encode(note.attachments());
    let recipient = details.recipient();

    let serial_number = recipient.serial_num().to_bytes();
    let script = proto::encode(recipient.script());
    let inputs = proto::encode(recipient.storage());

    let script_root = recipient.script().root().to_bytes();

    let state_discriminant = note.state().discriminant();
    let state = proto::encode(note.state());

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

/// Builds an output note record from one row of the output notes query.
fn parse_output_note(row: &rusqlite::Row<'_>) -> Result<OutputNoteRecord, StoreError> {
    let recipient_digest: Vec<u8> = row.get("recipient_digest").into_store_error()?;
    let assets: Vec<u8> = row.get("assets").into_store_error()?;
    let metadata: Vec<u8> = row.get("metadata").into_store_error()?;
    let expected_height: u32 = row.get("expected_height").into_store_error()?;
    let state: Vec<u8> = row.get("state").into_store_error()?;
    let attachments: Vec<u8> = row.get("attachments").into_store_error()?;
    let script: Option<Vec<u8>> = row.get("serialized_note_script").into_store_error()?;

    let recipient_digest = Word::read_from_bytes(&recipient_digest)?;
    let assets = proto::decode(&assets)?;
    let metadata = proto::decode(&metadata)?;
    let script = script.map(|script| proto::decode(&script)).transpose()?;
    let state = proto::decode_output_note_state(&state, script)?;
    let attachments = proto::decode(&attachments)?;

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
        state: proto::encode(note.state()),
        attachments: proto::encode(note.attachments()),
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
        state: proto::encode_output_note_state(note.state()),
    }
}

/// Serialize the provided output note into database compatible types.
fn serialize_output_note(note: &OutputNoteRecord) -> SerializedOutputNoteData {
    let details_commitment = note.details_commitment().to_bytes();
    let id = note.id().as_word().to_bytes();
    let assets = proto::encode(note.assets());
    let recipient_digest = note.recipient_digest().to_bytes();
    let metadata = proto::encode(note.metadata());

    let nullifier = note.nullifier().map(|nullifier| nullifier.to_bytes());

    // The script is only known when the note's full details (recipient) are known. It is stored in
    // the shared `notes_scripts` table, with the note row referencing it by root.
    let script_root = note.script_root().map(|root| root.to_bytes());
    let script = note.recipient().map(|recipient| proto::encode(recipient.script()));

    let state_discriminant = note.state().discriminant();
    let state = proto::encode_output_note_state(note.state());

    let attachments = proto::encode(note.attachments());

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

/// Batch-upsert note scripts using a multi-row insert. Multi-row inserts reduce per-statement
/// overhead and show faster insertion times than individual inserts.
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
            param_values.push((*root).clone().into());
            param_values.push((*script).clone().into());
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
            param_values.push(note.details_commitment.clone().into());
            param_values.push(note.id.clone().into());
            param_values.push(note.assets.clone().into());
            param_values.push(note.attachments.clone().into());
            param_values.push(note.serial_number.clone().into());
            param_values.push(note.inputs.clone().into());
            param_values.push(note.script_root.clone().into());
            param_values.push(note.nullifier.clone().into());
            param_values.push(note.state_discriminant.into());
            param_values.push(note.state.clone().into());
            param_values.push(u64_to_value(note.created_at));
            param_values.push(note.consumed_block_height.into());
            param_values.push(note.consumed_tx_order.into());
            param_values.push(note.consumer_account_id.clone().into());
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
            param_values.push(note.details_commitment.clone().into());
            param_values.push(note.id.clone().into());
            param_values.push(note.assets.clone().into());
            param_values.push(note.recipient_digest.clone().into());
            param_values.push(note.metadata.clone().into());
            param_values.push(note.nullifier.clone().into());
            param_values.push(note.expected_height.into());
            param_values.push(note.script_root.clone().into());
            param_values.push(note.state_discriminant.into());
            param_values.push(note.state.clone().into());
            param_values.push(note.attachments.clone().into());
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
        .execute(params![note_script.root().to_bytes(), proto::encode(note_script)])
        .into_store_error()?;

    Ok(())
}
