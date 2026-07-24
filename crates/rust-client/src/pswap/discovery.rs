//! Post-sync correlator: joins tracked-note consumption events from
//! `NoteUpdateTracker::consumed_note_ids()` with the PSWAP-attachment
//! notes collected by [`super::observer::PswapChainObserver`], emitting
//! one `PswapLineageRoundUpdate` per round transition.
//!
//! See [`crate::pswap`] for the overall design.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::Felt;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::NoteId;
use miden_standards::note::PswapNote;
use tracing::error;

use super::errors::PswapLineageError;
use super::lineage::{
    ObservedPswapNote,
    PswapLineageRecord,
    PswapLineageRoundUpdate,
    PswapLineageState,
};
use super::store;
use crate::store::Store;
use crate::sync::StateSyncUpdate;

/// Returns one [`PswapLineageRoundUpdate`] per round advanced this sync.
///
/// Each active lineage is walked in memory across as many rounds as this sync
/// window reveals. Only the final tip's remainder is persisted to `input_notes`;
/// intermediate remainders are already spent on-chain.
pub(crate) async fn discover_pswap_rounds(
    store: Arc<dyn Store>,
    state_sync_update: &StateSyncUpdate,
    chain_note_updates: &[ObservedPswapNote],
) -> Result<Vec<PswapLineageRoundUpdate>, PswapLineageError> {
    let consumed_note_ids: BTreeSet<NoteId> =
        state_sync_update.note_updates().consumed_note_ids().collect();

    if consumed_note_ids.is_empty() && chain_note_updates.is_empty() {
        return Ok(Vec::new());
    }

    let candidate_orders =
        collect_candidate_orders(&store, &consumed_note_ids, chain_note_updates).await?;
    let active_lineages = load_active_lineages(&store, candidate_orders).await?;
    if active_lineages.is_empty() {
        return Ok(Vec::new());
    }

    let notes_by_order_depth = group_notes_by_order_depth(chain_note_updates);

    // Commit-block note roots for inserting reconstructed notes as `Committed`.
    let block_headers: BTreeMap<BlockNumber, BlockHeader> = state_sync_update
        .partial_blockchain_updates()
        .block_headers()
        .map(|(header, _)| (header.block_num(), header.clone()))
        .collect();

    let mut round_updates: Vec<PswapLineageRoundUpdate> = Vec::new();
    for lineage in active_lineages {
        let lineage_rounds = advance_lineage(
            &store,
            lineage,
            &consumed_note_ids,
            &notes_by_order_depth,
            &block_headers,
        )
        .await;
        round_updates.extend(lineage_rounds);
    }

    Ok(round_updates)
}

/// Candidate orders from a union of two signals, each resolving to an `order_id`
/// without scanning:
///   1. a consumed note id that is a tracked tip → via the tip index;
///   2. a chain note → carries its `order_id` on its attachment.
///
/// Both are needed: signal 2 catches a fill whose notes arrive before its tip
/// nullifier; signal 1 carries reclaim, which emits no chain notes.
async fn collect_candidate_orders(
    store: &Arc<dyn Store>,
    consumed_note_ids: &BTreeSet<NoteId>,
    chain_note_updates: &[ObservedPswapNote],
) -> Result<BTreeSet<Felt>, PswapLineageError> {
    let mut candidate_orders: BTreeSet<Felt> = BTreeSet::new();
    for note_id in consumed_note_ids {
        if let Some(order_id) = store::resolve_order_by_tip(store, *note_id).await? {
            candidate_orders.insert(order_id);
        }
    }
    for note in chain_note_updates {
        candidate_orders.insert(note.attachment.order_id());
    }
    Ok(candidate_orders)
}

/// Loads the `Active` lineage record for each candidate order, skipping orders
/// with no tracked record or already in a terminal state.
async fn load_active_lineages(
    store: &Arc<dyn Store>,
    candidate_orders: BTreeSet<Felt>,
) -> Result<Vec<PswapLineageRecord>, PswapLineageError> {
    let mut active_lineages = Vec::new();
    for order_id in candidate_orders {
        if let Some(record) = store::get_lineage(store, order_id).await?
            && record.state == PswapLineageState::Active
        {
            active_lineages.push(record);
        }
    }
    Ok(active_lineages)
}

/// Groups observed chain notes by `(order_id, depth)` for O(1) per-round lookup.
fn group_notes_by_order_depth(
    chain_note_updates: &[ObservedPswapNote],
) -> BTreeMap<(Felt, u32), Vec<&ObservedPswapNote>> {
    let mut notes_by_order_depth: BTreeMap<(Felt, u32), Vec<&ObservedPswapNote>> = BTreeMap::new();
    for note in chain_note_updates {
        notes_by_order_depth
            .entry((note.attachment.order_id(), note.attachment.depth()))
            .or_default()
            .push(note);
    }
    notes_by_order_depth
}

/// Walks one active lineage across every round this sync window reveals,
/// returning its round updates (final-tip remainder kept, intermediates dropped).
///
/// Advances round-by-round while live. A round fires when the tip's consumption
/// was observed (`tip_consumed`) OR depth+1 chain notes exist: by protocol
/// invariant a payback/remainder at depth N+1 can only come from consuming the
/// depth-N tip, so notes alone prove consumption. That's what follows a
/// same-block multi-fill on a private chain, whose intermediate remainder is
/// never tracked. The state guard ends the loop on terminal.
async fn advance_lineage(
    store: &Arc<dyn Store>,
    mut lineage: PswapLineageRecord,
    consumed_note_ids: &BTreeSet<NoteId>,
    notes_by_order_depth: &BTreeMap<(Felt, u32), Vec<&ObservedPswapNote>>,
    block_headers: &BTreeMap<BlockNumber, BlockHeader>,
) -> Vec<PswapLineageRoundUpdate> {
    let mut lineage_rounds: Vec<PswapLineageRoundUpdate> = Vec::new();
    // The depth-0 note is immutable across rounds and only fills (not reclaim)
    // need it to reconstruct outputs. Fetched lazily from `output_notes` on the
    // first fill and cached for the rest of this lineage's rounds.
    let mut original_pswap: Option<PswapNote> = None;

    while lineage.state == PswapLineageState::Active {
        let round_depth = lineage.current_depth + 1;
        let notes = notes_by_order_depth
            .get(&(lineage.order_id(), round_depth))
            .map_or(&[][..], Vec::as_slice);

        let tip_consumed = consumed_note_ids.contains(&lineage.current_tip_note_id);
        if !tip_consumed && notes.is_empty() {
            break;
        }

        // Fills (notes present) reconstruct payback/remainder from the original note;
        // fetch it once. A reclaim round (no notes) needs nothing from the note.
        if !notes.is_empty() && original_pswap.is_none() {
            match store::get_original_pswap(store, lineage.original_note_id).await {
                Ok(pswap) => original_pswap = Some(pswap),
                Err(err) => {
                    error!(
                        order_id = ?lineage.order_id(),
                        original_note_id = ?lineage.original_note_id,
                        error = ?err,
                        "discover_pswap_rounds: original note unavailable; skipping lineage",
                    );
                    break;
                },
            }
        }

        let update = match lineage.build_round_update(
            round_depth,
            notes,
            block_headers,
            original_pswap.as_ref(),
            tip_consumed,
        ) {
            Ok(Some(u)) => u,
            // Notes present but none reconstruct to a genuine payback/remainder for our live tip
            // (forged or unrelated) — not our round; stop advancing this lineage.
            Ok(None) => break,
            Err(err) => {
                error!(
                    order_id = ?lineage.order_id(),
                    round_depth,
                    error = ?err,
                    "discover_pswap_rounds: round build failed; skipping lineage",
                );
                break;
            },
        };

        lineage = lineage.advance(&update);
        lineage_rounds.push(update);
    }

    // Intermediate remainders are already spent on-chain; inserting them would leave stale
    // Unverified notes whose consumption falls outside the next sync's window. Keep only the
    // final (live) tip's remainder; drop the rest. Paybacks are all kept — each is a distinct
    // consumable note for the creator.
    if let Some((_, intermediate_rounds)) = lineage_rounds.split_last_mut() {
        for round in intermediate_rounds {
            round.remainder = None;
        }
    }
    lineage_rounds
}
