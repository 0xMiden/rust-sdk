//! Per-note observer that collects every PSWAP-attachment note seen
//! during sync. Lineage-scope filtering happens later, in `discovery`.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use async_trait::async_trait;
use miden_protocol::asset::AssetAmount;
use miden_protocol::note::NoteAttachments;
use miden_standards::note::{PswapNote, PswapNoteAttachment};
use tracing::warn;

use crate::ClientError;
use crate::pswap::discovery::discover_pswap_rounds;
use crate::pswap::lineage::ObservedPswapNote;
use crate::rpc::domain::note::CommittedNote;
use crate::store::Store;
use crate::sync::NoteObserver;
use crate::utils::RwLock;

// PSWAP CHAIN OBSERVER
// ================================================================================================

/// Per-sync collector of PSWAP-attachment notes seen this sync.
///
/// - `observe()` runs per-note during sync: reads the PSWAP attachment word straight off the note's
///   resolved attachments (carried inline on the sync window) and records a `ObservedPswapNote`. No
///   RPC round trip, no DB write.
/// - `apply()` runs once post-sync: drains the collector, runs the correlator, applies round
///   updates.
pub struct PswapChainObserver {
    store: Arc<dyn Store>,
    /// `observe()` writes, `apply()` drains; never concurrent. The observer is
    /// shared via the outer `Arc<dyn NoteObserver>` and only ever touched
    /// through `&self`, so the `RwLock` alone provides the needed interior
    /// mutability — no inner `Arc`.
    chain_note_updates: RwLock<Vec<ObservedPswapNote>>,
}

impl PswapChainObserver {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self {
            store,
            chain_note_updates: RwLock::new(Vec::new()),
        }
    }
}

#[async_trait(?Send)]
impl NoteObserver for PswapChainObserver {
    fn name(&self) -> &'static str {
        "PswapChainObserver"
    }

    async fn observe(
        &self,
        committed_note: &CommittedNote,
        attachments: Option<&NoteAttachments>,
    ) -> Result<bool, ClientError> {
        // Notes without a PSWAP attachment are the common case; `extract_pswap_attachment`
        // fast-rejects them. Foreign-order filtering happens later in `discovery`.
        let Some(attachments) = attachments else {
            return Ok(false);
        };
        let Some(attachment) = extract_pswap_attachment(attachments) else {
            return Ok(false);
        };

        let inclusion_proof = committed_note.inclusion_proof().clone();
        self.chain_note_updates.write().push(ObservedPswapNote {
            note_id: *committed_note.note_id(),
            attachment,
            sender: committed_note.sender(),
            tag: committed_note.metadata().tag(),
            block_num: inclusion_proof.location().block_num(),
            inclusion_proof,
        });
        Ok(true)
    }

    /// Drains the collector, runs the correlator, applies round updates.
    /// Per-round failures are logged, not propagated.
    async fn apply(&self, sync_update: &crate::sync::StateSyncUpdate) -> Result<(), ClientError> {
        let chain_note_updates = core::mem::take(&mut *self.chain_note_updates.write());

        // Nothing observed AND nothing consumed — correlator has no work.
        if chain_note_updates.is_empty()
            && sync_update.note_updates.consumed_note_ids().next().is_none()
        {
            return Ok(());
        }

        let round_updates =
            discover_pswap_rounds(self.store.clone(), sync_update, &chain_note_updates).await?;

        for round_update in round_updates {
            if let Err(err) = crate::pswap::store::apply_round(&self.store, &round_update).await {
                warn!(
                    order_id = round_update.order_id.as_canonical_u64(),
                    round_depth = round_update.round_depth,
                    error = ?err,
                    "apply_round failed; lineage left at previous tip",
                );
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HELPERS
// ---------------------------------------------------------------------------

/// Pulls the typed [`PswapNoteAttachment`] off a note's attachment word
/// `[amount, order_id, depth, 0]`. Returns `None` for notes without a
/// PSWAP-scheme attachment or with malformed content.
fn extract_pswap_attachment(attachments: &NoteAttachments) -> Option<PswapNoteAttachment> {
    let pswap_attach = attachments.find(PswapNote::PSWAP_ATTACHMENT_SCHEME)?;
    let word = pswap_attach.content().as_words().first()?;

    let amount = AssetAmount::new(word[0].as_canonical_u64()).ok()?;
    let order_id = word[1];
    let depth = u32::try_from(word[2].as_canonical_u64()).ok()?;
    Some(PswapNoteAttachment::new(amount, order_id, depth))
}

// ---------------------------------------------------------------------------
// TESTS
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Reject-branch coverage for `extract_pswap_attachment` — the per-note
    //! fast-path that turns a raw attachment word into a typed
    //! [`PswapNoteAttachment`] (or rejects it).
    use alloc::vec::Vec;

    use miden_protocol::note::{NoteAttachment, NoteAttachmentScheme, NoteAttachments};
    use miden_protocol::{Felt, Word};
    use miden_standards::note::PswapNote;

    use super::*;

    /// A PSWAP attachment word `[amount, order_id, depth, 0]`.
    fn pswap_word(amount: u64, order_id: u64, depth: u64) -> Word {
        Word::from([
            Felt::new(amount).unwrap(),
            Felt::new(order_id).unwrap(),
            Felt::new(depth).unwrap(),
            Felt::new(0).unwrap(),
        ])
    }

    /// Wraps `word` in a single PSWAP-scheme attachment.
    fn pswap_attachments(word: Word) -> NoteAttachments {
        NoteAttachments::from(NoteAttachment::with_word(PswapNote::PSWAP_ATTACHMENT_SCHEME, word))
    }

    /// Well-formed word round-trips into the typed attachment.
    #[test]
    fn extract_pswap_attachment_reads_wellformed_word() {
        let parsed = extract_pswap_attachment(&pswap_attachments(pswap_word(25, 0xabcd, 3)))
            .expect("valid PSWAP word must parse");
        assert_eq!(u64::from(parsed.amount()), 25);
        assert_eq!(parsed.order_id().as_canonical_u64(), 0xabcd);
        assert_eq!(parsed.depth(), 3);
    }

    /// No PSWAP-scheme attachment present → `None`. Covers both the empty
    /// set and the "has attachments, but none is ours" case (the common
    /// path for unrelated notes during sync).
    #[test]
    fn extract_pswap_attachment_rejects_missing_scheme() {
        let empty = NoteAttachments::new(Vec::new()).unwrap();
        assert!(extract_pswap_attachment(&empty).is_none());

        // Scheme 1 ≠ the PSWAP scheme (3).
        let other = NoteAttachments::from(NoteAttachment::with_word(
            NoteAttachmentScheme::new(1).unwrap(),
            pswap_word(1, 2, 3),
        ));
        assert!(extract_pswap_attachment(&other).is_none());
    }

    /// `amount` above `AssetAmount::MAX` is rejected, not panicked on.
    #[test]
    fn extract_pswap_attachment_rejects_oversized_amount() {
        let word = pswap_word(AssetAmount::MAX.as_u64() + 1, 7, 1);
        assert!(extract_pswap_attachment(&pswap_attachments(word)).is_none());
    }

    /// `depth` above `u32::MAX` is rejected, not panicked on. The amount
    /// field is valid so the parser reaches the depth check.
    #[test]
    fn extract_pswap_attachment_rejects_oversized_depth() {
        let word = pswap_word(10, 7, u64::from(u32::MAX) + 1);
        assert!(extract_pswap_attachment(&pswap_attachments(word)).is_none());
    }
}
