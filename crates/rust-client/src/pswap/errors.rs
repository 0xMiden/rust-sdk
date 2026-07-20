//! Errors specific to PSWAP chain tracking.

use alloc::boxed::Box;

use miden_protocol::Felt;
use miden_protocol::account::AccountId;
use miden_protocol::note::NoteId;

use super::lineage::PswapLineageState;
use crate::store::StoreError;

/// Failures raised by the PSWAP chain-tracking subsystem.
#[derive(Debug, thiserror::Error)]
pub enum PswapLineageError {
    /// No tracked PSWAP lineage for the given `order_id`.
    #[error("no PSWAP lineage tracked for order_id {0}")]
    NotFound(Felt),

    /// The lineage exists but is already in a terminal state.
    #[error("PSWAP lineage is not active (state = {0:?}); no further rounds expected")]
    NotActive(PswapLineageState),

    /// The lineage's creator is not a local account — reclaim requires
    /// the creator's signing authority.
    #[error(
        "PSWAP creator account {0} is not local; reclaim requires the creator's signing authority"
    )]
    CreatorNotLocal(AccountId),

    /// The current tip is missing from the store — the tracked lineage is
    /// out of sync with the stored notes.
    #[error("current tip note is missing from the local store; the tracked lineage is out of sync")]
    TipMissing,

    /// The depth-0 note referenced by `original_note_id` could not be fetched
    /// from `output_notes`, or was stored without the recipient needed to
    /// reconstruct it. The note is written before the lineage record, so this
    /// signals a broken invariant (e.g. the output note was pruned) rather than
    /// an expected race.
    #[error(
        "PSWAP original note {0} is unavailable in the output-note store or lacks recipient details"
    )]
    OriginalNoteUnavailable(NoteId),

    /// A stored lineage's `state` byte has no matching [`PswapLineageState`] variant.
    #[error("unknown PSWAP lineage state byte: {0}")]
    UnknownState(u8),

    /// A store call from the PSWAP layer failed.
    #[error("PSWAP store call failed: {0}")]
    Store(#[from] StoreError),
}

impl From<PswapLineageError> for crate::ClientError {
    fn from(err: PswapLineageError) -> Self {
        crate::ClientError::Observer(Box::new(err))
    }
}
