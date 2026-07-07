// NOTE FILTER (OUTPUT NOTES)
// ================================================================================================

use std::rc::Rc;

use miden_client::note::BlockNumber;
use miden_client::store::{InputNoteState, NoteFilter, OutputNoteState};
use rusqlite::types::Value;

type NoteQueryParams = Vec<Rc<Vec<Value>>>;

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
        NoteFilter::Processing | NoteFilter::Unverified => "1 = 0".to_string(),
        NoteFilter::Unique(note_id) => {
            let note_ids_list = vec![Value::Text(note_id.as_word().to_string())];
            params.push(Rc::new(note_ids_list));
            "note.note_id IN rarray(?)".to_string()
        },
        NoteFilter::List(note_ids) => {
            let note_ids_list = note_ids
                .iter()
                .map(|note_id| Value::Text(note_id.as_word().to_string()))
                .collect::<Vec<Value>>();

            params.push(Rc::new(note_ids_list));
            "note.note_id IN rarray(?)".to_string()
        },
        NoteFilter::DetailsCommitments(commitments) => {
            let commitments_list = commitments
                .iter()
                .map(|commitment| Value::Text(commitment.to_hex()))
                .collect::<Vec<Value>>();

            params.push(Rc::new(commitments_list));
            "note.details_commitment IN rarray(?)".to_string()
        },
        NoteFilter::Nullifiers(nullifiers) => {
            let nullifiers_list = nullifiers
                .iter()
                .map(|nullifier| Value::Text(nullifier.to_string()))
                .collect::<Vec<Value>>();

            params.push(Rc::new(nullifiers_list));
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
                      note.note_id ASC"
        )
    } else {
        format!("{INPUT_NOTES_BASE_QUERY} WHERE {condition}")
    };

    (query, params)
}

/// Returns a query that fetches a single input note at the given offset from the filtered set,
/// restricted to a consumer account and optionally to a block range.
pub(super) fn note_filter_to_query_input_note_by_offset(
    filter: &NoteFilter,
    consumer: &str,
    block_start: Option<BlockNumber>,
    block_end: Option<BlockNumber>,
    offset: u32,
) -> (String, NoteQueryParams) {
    use core::fmt::Write;
    let (mut condition, mut params) = note_filter_input_notes_condition(filter);

    params.push(Rc::new(vec![Value::Text(consumer.to_string())]));
    condition.push_str(" AND note.consumer_account_id IN rarray(?)");
    condition.push_str(" AND note.consumed_tx_order IS NOT NULL");

    if let Some(start) = block_start {
        let _ = write!(condition, " AND note.consumed_block_height >= {}", start.as_u32());
    }
    if let Some(end) = block_end {
        let _ = write!(condition, " AND note.consumed_block_height <= {}", end.as_u32());
    }

    let query = format!(
        "{INPUT_NOTES_BASE_QUERY} WHERE {condition} \
         ORDER BY note.consumed_block_height ASC, note.consumed_tx_order ASC, note.note_id ASC \
         LIMIT 1 OFFSET {offset}"
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
            let note_ids_list = vec![Value::Text(note_id.as_word().to_string())];
            params.push(Rc::new(note_ids_list));
            "(note.note_id IN rarray(?))".to_string()
        },
        NoteFilter::List(note_ids) => {
            let note_ids_list = note_ids
                .iter()
                .map(|note_id| Value::Text(note_id.as_word().to_string()))
                .collect::<Vec<Value>>();

            params.push(Rc::new(note_ids_list));
            "(note.note_id IN rarray(?))".to_string()
        },
        NoteFilter::DetailsCommitments(commitments) => {
            let commitments_list = commitments
                .iter()
                .map(|commitment| Value::Text(commitment.to_hex()))
                .collect::<Vec<Value>>();

            params.push(Rc::new(commitments_list));
            "(note.details_commitment IN rarray(?))".to_string()
        },
        NoteFilter::Nullifiers(nullifiers) => {
            let nullifiers_list = nullifiers
                .iter()
                .map(|nullifier| Value::Text(nullifier.to_string()))
                .collect::<Vec<Value>>();

            params.push(Rc::new(nullifiers_list));
            "(note.nullifier IN rarray(?))".to_string()
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
