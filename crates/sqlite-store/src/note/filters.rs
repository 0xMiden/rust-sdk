// NOTE FILTER QUERIES
// ================================================================================================

use std::rc::Rc;

use miden_client::account::AccountId;
use miden_client::note::{BlockNumber, NoteId};
use miden_client::store::{InputNoteState, NoteFilter, OutputNoteState};
use rusqlite::types::Value;

use super::{INPUT_NOTE_COLUMNS, OUTPUT_NOTE_COLUMNS};
use crate::blob_array;

type NoteQueryParams = Vec<Rc<Vec<Value>>>;

/// Builds a `column IN rarray(?)` condition, pushing the bound value list onto `params`.
///
/// The list is bound as a single table-valued parameter so the SQL text stays constant no matter
/// how many values the filter carries.
fn in_rarray_condition(
    column: &str,
    values: Rc<Vec<Value>>,
    params: &mut NoteQueryParams,
) -> String {
    params.push(values);
    format!("({column} IN rarray(?))")
}

// NOTE FILTER (OUTPUT NOTES)
// ================================================================================================

/// Returns the output notes query for a specific `NoteFilter`
pub(super) fn note_filter_to_query_output_notes(filter: &NoteFilter) -> (String, NoteQueryParams) {
    let (condition, params) = note_filter_output_notes_condition(filter);
    let query = format!("SELECT {OUTPUT_NOTE_COLUMNS} from output_notes AS note WHERE {condition}");

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
                      note.note_id ASC"
        )
    } else {
        format!("{base_query} WHERE {condition}")
    };

    (query, params)
}

/// Returns a query that fetches a single input note at the given offset from the filtered set,
/// restricted to a consumer account and optionally to a block range.
pub(super) fn note_filter_to_query_input_note_by_offset(
    filter: &NoteFilter,
    consumer: AccountId,
    block_start: Option<BlockNumber>,
    block_end: Option<BlockNumber>,
    offset: u32,
) -> (String, NoteQueryParams) {
    use core::fmt::Write;
    let (mut condition, mut params) = note_filter_input_notes_condition(filter);

    let consumer_condition =
        in_rarray_condition("note.consumer_account_id", blob_array([&consumer]), &mut params);
    let _ = write!(condition, " AND {consumer_condition}");
    condition.push_str(" AND note.consumed_tx_order IS NOT NULL");

    if let Some(start) = block_start {
        let _ = write!(condition, " AND note.consumed_block_height >= {}", start.as_u32());
    }
    if let Some(end) = block_end {
        let _ = write!(condition, " AND note.consumed_block_height <= {}", end.as_u32());
    }

    let query = format!(
        "{} WHERE {condition} \
         ORDER BY note.consumed_block_height ASC, note.consumed_tx_order ASC, note.note_id ASC \
         LIMIT 1 OFFSET {offset}",
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
            format!(
                "(state_discriminant in ({}, {}, {}, {}, {}))",
                InputNoteState::STATE_EXPECTED,
                InputNoteState::STATE_PROCESSING_AUTHENTICATED,
                InputNoteState::STATE_PROCESSING_UNAUTHENTICATED,
                InputNoteState::STATE_UNVERIFIED,
                InputNoteState::STATE_COMMITTED
            )
        },
    };

    (condition, params)
}
