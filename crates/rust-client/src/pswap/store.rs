//! Client-side persistence for PSWAP lineages over the existing `settings`
//! KV store, using two key families:
//!
//! ```text
//! pswap/order/{order_id_hex}    →  serialized PswapLineageRecord  (PRIMARY; stable, never re-keyed)
//! pswap/tip/{tip_note_id_hex}   →  order_id (Felt, 8 bytes)       (SECONDARY INDEX; re-keyed each round)
//! ```
//!
//! The record lives under the stable `order_id`. The tip changes each round,
//! so it keys a tiny index value (the `order_id`) that lets discovery resolve
//! a consumed tip back to its order without a full scan.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::{format, vec};

use miden_protocol::note::{Note, NoteDetails, NoteId, NoteInclusionProof};
use miden_protocol::{Felt, Word};
use miden_standards::note::PswapNote;

use super::errors::PswapLineageError;
use super::lineage::{
    PswapLineageFilter,
    PswapLineageRecord,
    PswapLineageRoundUpdate,
    PswapLineageState,
};
use crate::store::input_note_states::{CommittedNoteState, UnverifiedNoteState};
use crate::store::{InputNoteRecord, NoteFilter, SettingMutation, Store, StoreError};
use crate::sync::{NoteTagRecord, NoteTagSource};
use crate::utils::{Deserializable, Serializable, bytes_to_hex_string};

// KEY SCHEME
// ================================================================================================

const ORDER_PREFIX: &str = "pswap/order/";
const TIP_PREFIX: &str = "pswap/tip/";

/// Stable primary key for an order's lineage record. Hex of the `order_id`
/// canonical bytes — only uniqueness + stability matter; we never parse it
/// back (the record carries its own `order_id`).
fn order_key(order_id: Felt) -> String {
    format!(
        "{ORDER_PREFIX}{}",
        bytes_to_hex_string(order_id.as_canonical_u64().to_le_bytes())
    )
}

/// Secondary-index key for the current tip. Hex convention matches the note
/// id encoding used elsewhere in the store layer.
fn tip_key(tip: NoteId) -> String {
    format!("{TIP_PREFIX}{}", tip.as_word())
}

// READ / WRITE HELPERS
// ================================================================================================

/// Persists a lineage record and its tip index in one atomic batch, so the
/// record and its tip index can never diverge. Used at creation and as the
/// building block for [`apply_round`].
pub(crate) async fn put_lineage(
    store: &Arc<dyn Store>,
    record: &PswapLineageRecord,
) -> Result<(), StoreError> {
    store
        .apply_settings_mutations(vec![
            SettingMutation::Set {
                key: order_key(record.order_id()),
                value: record.to_bytes(),
            },
            SettingMutation::Set {
                key: tip_key(record.current_tip_note_id),
                value: record.order_id().to_bytes(),
            },
        ])
        .await
}

/// Point-get a lineage by its stable `order_id`.
pub(crate) async fn get_lineage(
    store: &Arc<dyn Store>,
    order_id: Felt,
) -> Result<Option<PswapLineageRecord>, StoreError> {
    let Some(bytes) = store.get_setting(order_key(order_id)).await? else {
        return Ok(None);
    };
    let record = PswapLineageRecord::read_from_bytes(&bytes)
        .map_err(StoreError::DataDeserializationError)?;
    Ok(Some(record))
}

/// Resolves a (possibly consumed) tip note id back to its `order_id` via the
/// tip index. `None` when the note id is not a tracked tip.
pub(crate) async fn resolve_order_by_tip(
    store: &Arc<dyn Store>,
    tip: NoteId,
) -> Result<Option<Felt>, StoreError> {
    let Some(bytes) = store.get_setting(tip_key(tip)).await? else {
        return Ok(None);
    };
    let order_id = Felt::read_from_bytes(&bytes).map_err(StoreError::DataDeserializationError)?;
    Ok(Some(order_id))
}

/// Fetches and reconstructs the immutable depth-0 PSWAP note from `output_notes`
/// by its stable id. The lineage record stores only `original_note_id` and the
/// cheap order facts; the full note (script + recipient) is recovered here when
/// reconstruction (payback/remainder rebuild) or a depth-0 reclaim needs it.
///
/// The output note is persisted before the lineage record that points at it, so
/// a miss — or a record lacking the recipient — means a broken invariant (e.g.
/// the note was pruned), surfaced as [`PswapLineageError::OriginalNoteUnavailable`]
/// rather than silently dropping work.
pub(crate) async fn get_original_pswap(
    store: &Arc<dyn Store>,
    original_note_id: NoteId,
) -> Result<PswapNote, PswapLineageError> {
    let record = store
        .get_output_notes(NoteFilter::List(vec![original_note_id]))
        .await?
        .into_iter()
        .next()
        .ok_or(PswapLineageError::OriginalNoteUnavailable(original_note_id))?;
    // `TryFrom<OutputNoteRecord> for Note` errors when the record carries no
    // recipient (a `*Partial` state); our depth-0 notes are always `Full`.
    let note: Note = record
        .try_into()
        .map_err(|_| PswapLineageError::OriginalNoteUnavailable(original_note_id))?;
    PswapNote::try_from(&note)
        .map_err(|_| PswapLineageError::OriginalNoteUnavailable(original_note_id))
}

/// Prefix-scans the `pswap/order/` family and applies the (client-side)
/// filter. `pswap/tip/` and non-PSWAP settings keys are excluded by the
/// full-prefix check. Rare path (a client's own open orders).
pub(crate) async fn list_lineages(
    store: &Arc<dyn Store>,
    filter: PswapLineageFilter,
) -> Result<Vec<PswapLineageRecord>, StoreError> {
    let mut out = Vec::new();
    for key in store.list_setting_keys().await? {
        if !key.starts_with(ORDER_PREFIX) {
            continue;
        }
        let Some(bytes) = store.get_setting(key).await? else {
            continue;
        };
        let record = PswapLineageRecord::read_from_bytes(&bytes)
            .map_err(StoreError::DataDeserializationError)?;
        let keep = match &filter {
            PswapLineageFilter::All => true,
            PswapLineageFilter::Active => record.state == PswapLineageState::Active,
            PswapLineageFilter::ByCreator(creator) => record.creator_account_id() == *creator,
        };
        if keep {
            out.push(record);
        }
    }
    Ok(out)
}

// ROUND APPLICATION
// ================================================================================================

/// Applies one round transition: persists any reconstructed payback/remainder
/// into `input_notes`, advances the lineage record, re-keys the tip index,
/// and drops the asset-pair tag on terminal states.
///
/// The lineage-record advance and tip re-key are committed as one atomic
/// settings batch, so the record and its tip index never diverge. The
/// `input_notes` and tag writes target other tables and keep the note-first
/// ordering: a crash before the settings batch leaves the lineage at the old
/// tip, and the next sync re-derives the round idempotently (`upsert_input_notes`
/// is keyed on `note_id`; settings are last-writer-wins).
pub(crate) async fn apply_round(
    store: &Arc<dyn Store>,
    update: &PswapLineageRoundUpdate,
) -> Result<(), StoreError> {
    // Load the current record and enforce the monotonic-depth invariant before
    // any write. The store is the last line of defense against correlator
    // off-by-ones / duplicate deliveries.
    let record = get_lineage(store, update.order_id).await?.ok_or_else(|| {
        StoreError::DatabaseError(format!(
            "apply_round: no lineage for order_id {}",
            update.order_id
        ))
    })?;
    if update.round_depth != record.current_depth + 1 {
        return Err(StoreError::DatabaseError(format!(
            "apply_round: round_depth {} for order_id {} does not advance by 1 \
             (current_depth {}); refusing to corrupt the reconstruction chain",
            update.round_depth, update.order_id, record.current_depth,
        )));
    }

    // 1. Notes first (see the note-first rationale above).
    let at_block_note_root = update.at_block_note_root;
    if let Some((payback_note, inclusion_proof)) = &update.payback {
        upsert_round_note(store, payback_note, inclusion_proof, at_block_note_root).await?;
    }
    if let Some((remainder_note, inclusion_proof)) = &update.remainder {
        upsert_round_note(store, remainder_note, inclusion_proof, at_block_note_root).await?;
    }

    // 2. Advance the lineage record and re-key the tip index in one atomic batch, so the order
    //    record and its tip index can never diverge. On terminal rounds `tip_note_id` is `None`, so
    //    the tip stays frozen at the last live tip while `current_depth` advances to the
    //    terminating round, and the live tip index is dropped (a terminal lineage has no tip to
    //    resolve).
    let old_tip = record.current_tip_note_id;
    // Reuse the record's own advance logic (the same `advance` the in-memory walk in `discovery`
    // uses) so the persisted transition can never drift from it.
    let new_record = record.advance(update);
    let mut mutations = vec![
        SettingMutation::Set {
            key: order_key(update.order_id),
            value: new_record.to_bytes(),
        },
        SettingMutation::Remove { key: tip_key(old_tip) },
    ];
    if update.state == PswapLineageState::Active
        && let Some(new_tip) = update.tip_note_id
    {
        mutations.push(SettingMutation::Set {
            key: tip_key(new_tip),
            value: update.order_id.to_bytes(),
        });
    }
    store.apply_settings_mutations(mutations).await?;

    // 4. Terminal states no longer need the asset-pair subscription. The tag is re-derived from the
    //    depth-0 note (the record stores only amounts, not the faucets the tag needs) — one fetch,
    //    fired once per lineage lifetime. The subscription is keyed by the stable
    //    `original_note_id` (the same key used at creation).
    if matches!(update.state, PswapLineageState::FullyFilled | PswapLineageState::Reclaimed) {
        let pswap =
            get_original_pswap(store, new_record.original_note_id).await.map_err(|err| {
                StoreError::DatabaseError(format!(
                    "apply_round: cannot recover the depth-0 note to remove the asset-pair tag for \
                 order_id {}: {err}",
                    update.order_id
                ))
            })?;
        store
            .remove_note_tag(NoteTagRecord {
                tag: PswapNote::create_tag(
                    pswap.note_type(),
                    pswap.offered_asset(),
                    pswap.storage().requested_asset(),
                ),
                source: NoteTagSource::Subscription(new_record.original_note_id.as_word()),
            })
            .await?;
    }

    Ok(())
}

/// Inserts a reconstructed payback or remainder into `input_notes`. Skips if an
/// entry already exists so we never downgrade an already-tracked note (e.g. a
/// public payback the screener already inserted as `Committed` this same sync).
/// With `at_block_note_root` the note lands as `Committed`, otherwise `Unverified`.
async fn upsert_round_note(
    store: &Arc<dyn Store>,
    note: &Note,
    inclusion_proof: &NoteInclusionProof,
    at_block_note_root: Option<Word>,
) -> Result<(), StoreError> {
    let note_id = note.id();
    if !store.get_input_notes(NoteFilter::List(vec![note_id])).await?.is_empty() {
        return Ok(());
    }

    let metadata = *note.metadata();
    let details = NoteDetails::from(note.clone());
    let attachments = note.attachments().clone();

    let state = match at_block_note_root {
        Some(note_root) => CommittedNoteState {
            inclusion_proof: inclusion_proof.clone(),
            metadata,
            block_note_root: note_root,
        }
        .into(),
        None => UnverifiedNoteState {
            metadata,
            inclusion_proof: inclusion_proof.clone(),
        }
        .into(),
    };

    store
        .upsert_input_notes(&[InputNoteRecord::new(details, attachments, None, state)])
        .await
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::Word;

    use super::*;

    /// Builds a deterministic `Felt` from a `u64` for key-encoding tests.
    fn felt(value: u64) -> Felt {
        Felt::new(value).unwrap()
    }

    /// Builds a deterministic `NoteId` from a `u64` for key-encoding tests.
    fn note_id(value: u64) -> NoteId {
        let f = felt(value);
        NoteId::from_raw(Word::from([f, f, f, f]))
    }

    #[test]
    fn order_key_carries_order_prefix() {
        assert!(order_key(felt(1)).starts_with(ORDER_PREFIX));
    }

    #[test]
    fn tip_key_carries_tip_prefix() {
        assert!(tip_key(note_id(1)).starts_with(TIP_PREFIX));
    }

    /// `list_lineages` skips `pswap/tip/` rows by prefix — but only while
    /// neither family is a prefix of the other. Pin it so a future prefix tweak
    /// that would leak tip rows into the order scan fails here, not silently.
    #[test]
    fn key_families_are_prefix_isolated() {
        assert!(!TIP_PREFIX.starts_with(ORDER_PREFIX));
        assert!(!ORDER_PREFIX.starts_with(TIP_PREFIX));
        assert!(!tip_key(note_id(1)).starts_with(ORDER_PREFIX));
        assert!(!order_key(felt(1)).starts_with(TIP_PREFIX));
    }

    /// Both key families must map each id to one stable, unique key — a
    /// non-deterministic or colliding encoding would corrupt lookups. Pin
    /// determinism + injectivity.
    #[test]
    fn keys_are_deterministic_and_injective() {
        // Bind each construction separately so `clippy::eq_op` doesn't flag the
        // determinism checks (identical call expressions as assert operands).
        let order_a = order_key(felt(7));
        let order_b = order_key(felt(7));
        assert_eq!(order_a, order_b);
        assert_ne!(order_key(felt(1)), order_key(felt(2)));

        let tip_a = tip_key(note_id(7));
        let tip_b = tip_key(note_id(7));
        assert_eq!(tip_a, tip_b);
        assert_ne!(tip_key(note_id(1)), tip_key(note_id(2)));
    }
}
