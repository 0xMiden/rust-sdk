// NOTE FILTER QUERIES
// ================================================================================================

use std::rc::Rc;

use miden_client::account::AccountId;
use miden_client::note::{BlockNumber, NoteId};
use miden_client::store::{InputNoteCursor, InputNoteState, NoteFilter, OutputNoteState};
use miden_client::utils::Serializable;
use rusqlite::types::{ToSqlOutput, Value};

use super::{INPUT_NOTE_COLUMNS, OUTPUT_NOTE_COLUMNS};
use crate::blob_array;

type NoteQueryParams = Vec<ToSqlOutput<'static>>;

/// Builds a `column IN rarray(?)` condition, pushing the bound value list onto `params`.
///
/// The list is bound as a single table-valued parameter so the SQL text stays constant no matter
/// how many values the filter carries.
fn in_rarray_condition(
    column: &str,
    values: Rc<Vec<Value>>,
    params: &mut NoteQueryParams,
) -> String {
    params.push(ToSqlOutput::Array(values));
    format!("({column} IN rarray(?))")
}

// NOTE FILTER (OUTPUT NOTES)
// ================================================================================================

fn output_notes_base_query() -> String {
    format!(
        "SELECT {OUTPUT_NOTE_COLUMNS} from output_notes AS note \
         LEFT OUTER JOIN notes_scripts AS script ON note.script_root = script.script_root"
    )
}

/// Returns the output notes query for a specific `NoteFilter`
pub(super) fn note_filter_to_query_output_notes(filter: &NoteFilter) -> (String, NoteQueryParams) {
    let (condition, params) = note_filter_output_notes_condition(filter);
    let query = format!("{} WHERE {condition}", output_notes_base_query());

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
        NoteFilter::Processing | NoteFilter::Unverified => "1 = 0".to_string(),
        NoteFilter::ScriptRoots(script_roots) => {
            // Notes without known details have a NULL script root and never match.
            in_rarray_condition("note.script_root", blob_array(script_roots), &mut params)
        },
        NoteFilter::Unique(note_id) => {
            in_rarray_condition("note.note_id", blob_array([note_id.as_word()]), &mut params)
        },
        NoteFilter::List(note_ids) => in_rarray_condition(
            "note.note_id",
            blob_array(note_ids.iter().map(NoteId::as_word)),
            &mut params,
        ),
        NoteFilter::DetailsCommitments(commitments) => {
            in_rarray_condition("note.details_commitment", blob_array(commitments), &mut params)
        },
        NoteFilter::Nullifiers(nullifiers) => {
            in_rarray_condition("note.nullifier", blob_array(nullifiers), &mut params)
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

fn input_notes_base_query() -> String {
    format!(
        "SELECT {INPUT_NOTE_COLUMNS} from input_notes AS note \
         LEFT OUTER JOIN notes_scripts AS script ON note.script_root = script.script_root"
    )
}

pub(super) fn note_filter_to_query_input_notes(filter: &NoteFilter) -> (String, NoteQueryParams) {
    let base_query = input_notes_base_query();
    let (condition, params) = note_filter_input_notes_condition(filter);
    let query = if matches!(filter, NoteFilter::Consumed) {
        format!(
            "{base_query} WHERE {condition} \
             ORDER BY note.consumed_block_height ASC, \
                      note.consumed_tx_order IS NULL, note.consumed_tx_order ASC, \
                      note.details_commitment ASC"
        )
    } else {
        format!("{base_query} WHERE {condition}")
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
        "{} WHERE {condition} \
         ORDER BY note.consumed_block_height ASC, note.consumed_tx_order ASC, \
                  note.details_commitment ASC \
         LIMIT 1",
        input_notes_base_query()
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
            in_rarray_condition("note.note_id", blob_array([note_id.as_word()]), &mut params)
        },
        NoteFilter::List(note_ids) => in_rarray_condition(
            "note.note_id",
            blob_array(note_ids.iter().map(NoteId::as_word)),
            &mut params,
        ),
        NoteFilter::DetailsCommitments(commitments) => {
            in_rarray_condition("note.details_commitment", blob_array(commitments), &mut params)
        },
        NoteFilter::Nullifiers(nullifiers) => {
            in_rarray_condition("note.nullifier", blob_array(nullifiers), &mut params)
        },
        NoteFilter::ScriptRoots(script_roots) => {
            in_rarray_condition("note.script_root", blob_array(script_roots), &mut params)
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
