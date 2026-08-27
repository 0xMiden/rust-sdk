// NOTE FILTER (OUTPUT NOTES)
// ================================================================================================

use std::rc::Rc;

use miden_client::account::AccountId;
use miden_client::note::BlockNumber;
use miden_client::store::{InputNoteCursor, InputNoteState, NoteFilter, OutputNoteState};
use miden_client::utils::Serializable;
use rusqlite::types::{ToSqlOutput, Value};

type NoteQueryParams = Vec<ToSqlOutput<'static>>;

/// Wraps a value list as an `rarray` pointer parameter.
fn array_param(values: Vec<Value>) -> ToSqlOutput<'static> {
    ToSqlOutput::Array(Rc::new(values))
}

/// Returns the output notes query for a specific `NoteFilter`
pub(super) fn note_filter_to_query_output_notes(filter: &NoteFilter) -> (String, NoteQueryParams) {
    let base = "SELECT
                    note.recipient_digest,
                    note.assets,
                    note.metadata,
                    note.expected_height,
                    note.state,
                    note.attachments
                    from output_notes AS note";

    let (condition, params) = note_filter_output_notes_condition(filter);
    let query = format!("{base} WHERE {condition}");

    (query, params)
}

/// Returns the WHERE clause  for a specific `NoteFilter`.
pub(super) fn note_filter_output_notes_condition(filter: &NoteFilter) -> (String, NoteQueryParams) {
    let mut params = Vec::new();
    let condition = match filter {
        NoteFilter::All => "1 = 1".to_string(),
        NoteFilter::Committed => {
            format!(
                "state_discriminant in ({}, {})",
                OutputNoteState::STATE_COMMITTED_PARTIAL,
                OutputNoteState::STATE_COMMITTED_FULL
            )
        },
        NoteFilter::Consumed => {
            format!("state_discriminant = {}", OutputNoteState::STATE_CONSUMED)
        },
        NoteFilter::Expected => {
            format!(
                "state_discriminant in ({}, {})",
                OutputNoteState::STATE_EXPECTED_PARTIAL,
                OutputNoteState::STATE_EXPECTED_FULL
            )
        },
        NoteFilter::Processing | NoteFilter::ScriptRoots(_) | NoteFilter::Unverified => {
            "1 = 0".to_string()
        },
        NoteFilter::Unique(note_id) => {
            let note_ids_list = vec![Value::Blob(note_id.as_word().to_bytes())];
            params.push(array_param(note_ids_list));
            "note.note_id IN rarray(?)".to_string()
        },
        NoteFilter::List(note_ids) => {
            let note_ids_list = note_ids
                .iter()
                .map(|note_id| Value::Blob(note_id.as_word().to_bytes()))
                .collect::<Vec<Value>>();

            params.push(array_param(note_ids_list));
            "note.note_id IN rarray(?)".to_string()
        },
        NoteFilter::DetailsCommitments(commitments) => {
            let commitments_list = commitments
                .iter()
                .map(|commitment| Value::Blob(commitment.to_bytes()))
                .collect::<Vec<Value>>();

            params.push(array_param(commitments_list));
            "note.details_commitment IN rarray(?)".to_string()
        },
        NoteFilter::Nullifiers(nullifiers) => {
            let nullifiers_list = nullifiers
                .iter()
                .map(|nullifier| Value::Blob(nullifier.to_bytes()))
                .collect::<Vec<Value>>();

            params.push(array_param(nullifiers_list));
            "note.nullifier IN rarray(?)".to_string()
        },
        NoteFilter::Unspent => {
            format!(
                "state_discriminant in ({}, {}, {}, {})",
                OutputNoteState::STATE_EXPECTED_PARTIAL,
                OutputNoteState::STATE_EXPECTED_FULL,
                OutputNoteState::STATE_COMMITTED_PARTIAL,
                OutputNoteState::STATE_COMMITTED_FULL,
            )
        },
    };

    (condition, params)
}

// NOTE FILTER (INPUT NOTES)
// ================================================================================================

const INPUT_NOTES_BASE_QUERY: &str = "SELECT
                note.assets,
                note.serial_number,
                note.inputs,
                script.serialized_note_script,
                note.state,
                note.created_at,
                note.attachments
                from input_notes AS note
                LEFT OUTER JOIN notes_scripts AS script
                    ON note.script_root = script.script_root";

pub(super) fn note_filter_to_query_input_notes(filter: &NoteFilter) -> (String, NoteQueryParams) {
    let (condition, params) = note_filter_input_notes_condition(filter);
    let query = if matches!(filter, NoteFilter::Consumed) {
        format!(
            "{INPUT_NOTES_BASE_QUERY} WHERE {condition} \
             ORDER BY note.consumed_block_height ASC, \
                      note.consumed_tx_order IS NULL, note.consumed_tx_order ASC, \
                      note.details_commitment ASC"
        )
    } else {
        format!("{INPUT_NOTES_BASE_QUERY} WHERE {condition}")
    };

    (query, params)
}

/// Returns a query that fetches the input note following `cursor` in the filtered set, restricted
/// to a consumer account and optionally to a block range.
pub(super) fn note_filter_to_query_input_note_after(
    filter: &NoteFilter,
    consumer: AccountId,
    block_start: Option<BlockNumber>,
    block_end: Option<BlockNumber>,
    cursor: Option<InputNoteCursor>,
) -> (String, NoteQueryParams) {
    let (mut condition, mut params) = note_filter_input_notes_condition(filter);

    // `consumer_account_id` is the first column of `idx_input_notes_consumption`. The equality
    // avoids a full sort for the ORDER BY.
    params.push(ToSqlOutput::from(consumer.to_bytes()));
    condition.push_str(" AND note.consumer_account_id = ?");
    condition.push_str(" AND note.consumed_tx_order IS NOT NULL");

    // A cursor at or after `block_start` is the tighter lower bound, and emitting both makes
    // SQLite abandon the row-value seek over `idx_input_notes_consumption`. A cursor before
    // `block_start` excludes nothing that `block_start` does not, so it is dropped.
    let cursor = cursor
        .filter(|cursor| block_start.is_none_or(|start| cursor.consumed_block_height() >= start));

    match cursor {
        Some(cursor) => {
            condition.push_str(
                " AND (note.consumed_block_height, note.consumed_tx_order, \
                 note.details_commitment) > (?, ?, ?)",
            );
            params.push(ToSqlOutput::from(cursor.consumed_block_height().as_u32()));
            params.push(ToSqlOutput::from(cursor.consumed_tx_order()));
            params.push(ToSqlOutput::from(cursor.details_commitment().to_bytes()));
        },
        None => {
            if let Some(start) = block_start {
                condition.push_str(" AND note.consumed_block_height >= ?");
                params.push(ToSqlOutput::from(start.as_u32()));
            }
        },
    }

    if let Some(end) = block_end {
        condition.push_str(" AND note.consumed_block_height <= ?");
        params.push(ToSqlOutput::from(end.as_u32()));
    }

    // `details_commitment` is the primary key of the `WITHOUT ROWID` table, so it trails every
    // index on it. Ordering by it makes the order total and keeps the seek index-served.
    let query = format!(
        "{INPUT_NOTES_BASE_QUERY} WHERE {condition} \
         ORDER BY note.consumed_block_height ASC, note.consumed_tx_order ASC, \
                  note.details_commitment ASC \
         LIMIT 1"
    );

    (query, params)
}

/// Returns the WHERE clause for the input [`NoteFilter`]
pub(super) fn note_filter_input_notes_condition(filter: &NoteFilter) -> (String, NoteQueryParams) {
    let mut params = Vec::new();
    let condition = match filter {
        NoteFilter::All => "(1 = 1)".to_string(),
        NoteFilter::Committed => {
            format!("(state_discriminant = {})", InputNoteState::STATE_COMMITTED)
        },
        NoteFilter::Consumed => {
            format!(
                "(state_discriminant in ({}, {}, {}))",
                InputNoteState::STATE_CONSUMED_AUTHENTICATED_LOCAL,
                InputNoteState::STATE_CONSUMED_UNAUTHENTICATED_LOCAL,
                InputNoteState::STATE_CONSUMED_EXTERNAL
            )
        },
        NoteFilter::Expected => {
            format!("(state_discriminant = {})", InputNoteState::STATE_EXPECTED)
        },
        NoteFilter::Processing => {
            format!(
                "(state_discriminant in ({}, {}))",
                InputNoteState::STATE_PROCESSING_AUTHENTICATED,
                InputNoteState::STATE_PROCESSING_UNAUTHENTICATED
            )
        },
        NoteFilter::Unique(note_id) => {
            let note_ids_list = vec![Value::Blob(note_id.as_word().to_bytes())];
            params.push(array_param(note_ids_list));
            "(note.note_id IN rarray(?))".to_string()
        },
        NoteFilter::List(note_ids) => {
            let note_ids_list = note_ids
                .iter()
                .map(|note_id| Value::Blob(note_id.as_word().to_bytes()))
                .collect::<Vec<Value>>();

            params.push(array_param(note_ids_list));
            "(note.note_id IN rarray(?))".to_string()
        },
        NoteFilter::DetailsCommitments(commitments) => {
            let commitments_list = commitments
                .iter()
                .map(|commitment| Value::Blob(commitment.to_bytes()))
                .collect::<Vec<Value>>();

            params.push(array_param(commitments_list));
            "(note.details_commitment IN rarray(?))".to_string()
        },
        NoteFilter::Nullifiers(nullifiers) => {
            let nullifiers_list = nullifiers
                .iter()
                .map(|nullifier| Value::Blob(nullifier.to_bytes()))
                .collect::<Vec<Value>>();

            params.push(array_param(nullifiers_list));
            "(note.nullifier IN rarray(?))".to_string()
        },
        NoteFilter::ScriptRoots(script_roots) => {
            let script_roots_list = script_roots
                .iter()
                .map(|script_root| Value::Blob(script_root.to_bytes()))
                .collect::<Vec<Value>>();

            params.push(array_param(script_roots_list));
            "(note.script_root IN rarray(?))".to_string()
        },
        NoteFilter::Unverified => {
            format!("(state_discriminant = {})", InputNoteState::STATE_UNVERIFIED)
        },
        NoteFilter::Unspent => {
            let states = InputNoteState::UNSPENT_STATES.map(|state| state.to_string()).join(", ");
            format!("(state_discriminant in ({states}))")
        },
    };

    (condition, params)
}
