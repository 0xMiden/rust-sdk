//! Syncs notes and resolves their content, on top of [`NodeRpcClient`].

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::vec::Vec;

use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::MerklePath;
use miden_protocol::note::{
    NoteAttachments,
    NoteDetails,
    NoteId,
    NoteInclusionProof,
    NoteMetadata,
    NoteTag,
    NoteType,
};

use crate::rpc::domain::note::{CommittedNote, FetchedNote};
use crate::rpc::{NodeRpcClient, RpcError};

// NOTE CONTENT FETCH
// ================================================================================================

/// Selects which note content [`sync_notes_with_content`] resolves via `GetNotesById` after syncing
/// note inclusions.
///
/// This enables the possibility of optimizing the call by not requesting more data than needed. For
/// example, when a public note's details are already known (but not the attachments),
/// `AttachmentsOnly` can be used. One example of this is when importing notes through
/// `NoteDetails`.
///
/// Neither policy requests attachment content the sync response already carried in full, so a note
/// whose attachments all fit in a single word is never fetched for its attachments alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoteContentFetch {
    /// Fetch the full body of every public note in the range, plus any attachment content still
    /// missing.
    PublicDetailsAndAttachments,
    /// Fetch only the attachment content still missing.
    AttachmentsOnly,
}

// SYNCED NOTE
// ================================================================================================

/// A block's worth of notes resolved by [`sync_notes_with_content`].
///
/// Unlike [`SyncNotesBlock`](crate::rpc::domain::note::SyncNotesBlock) (the raw `SyncNotes`
/// response), each note here also carries its attachments and, for a fetched public note, its
/// details, so no re-joining by note ID is needed.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedSyncNotesBlock {
    /// Block header containing the matching notes.
    pub block_header: BlockHeader,
    /// MMR path for verifying the block's inclusion in the MMR at `block_to`.
    pub mmr_path: MerklePath,
    /// Notes matching the requested tags in this block, keyed by note ID.
    pub notes: BTreeMap<NoteId, SyncedNote>,
}

/// Everything resolved about a single note during a notes sync: its identity, metadata, and
/// inclusion proof (always present, from `SyncNotes`), its attachments, and the public note body
/// when it was fetched via `GetNotesById`.
#[derive(Debug, Clone)]
pub struct SyncedNote {
    /// Note ID of the synced note, as reported by `SyncNotes`.
    pub note_id: NoteId,
    /// The note's full metadata, as reported by `SyncNotes`.
    pub metadata: NoteMetadata,
    /// Inclusion proof for the note in the block, as reported by `SyncNotes`.
    pub inclusion_proof: NoteInclusionProof,
    /// The public note's body, fetched via `GetNotesById`. `None` for a private note, and for a
    /// public note whose body was not requested or not returned.
    pub details: Option<NoteDetails>,
    /// The note's attachments, either carried in full by the sync record or fetched via
    /// `GetNotesById`. Empty for a note whose metadata advertises none.
    pub attachments: NoteAttachments,
}

impl SyncedNote {
    /// Pairs a sync record with the content resolved for it, checking that the content is
    /// consistent with the record:
    ///
    /// - Only a public note can have a body. The converse is not checked, since a public note
    ///   legitimately has no body whenever its body was not requested.
    /// - The attachments must hash to the metadata's attachments commitment. This also catches a
    ///   note advertising attachments whose content never arrived, which would be unconsumable.
    ///
    /// Both sides of that check come from the node, so it is a consistency check between its
    /// responses. The note is authenticated by a consumer recomputing its id and inclusion proof.
    ///
    /// A rejection concerns a single note, not the response as a whole: the notes sync skips the
    /// offending note with a warning instead of failing the sync, since content availability can be
    /// influenced by the note's creator.
    pub fn new(
        committed: CommittedNote,
        details: Option<NoteDetails>,
        attachments: NoteAttachments,
    ) -> Result<Self, RpcError> {
        if details.is_some() && committed.note_type() != NoteType::Public {
            return Err(RpcError::InvalidResponse(format!(
                "a note body was returned for private note {}",
                committed.note_id()
            )));
        }

        if attachments.to_commitment() != committed.metadata().attachments_commitment() {
            return Err(RpcError::InvalidResponse(format!(
                "the attachments resolved for note {} do not match the note's attachments \
                 commitment",
                committed.note_id()
            )));
        }

        let (note_id, metadata, inclusion_proof, _) = committed.into_parts();

        Ok(Self {
            note_id,
            metadata,
            inclusion_proof,
            details,
            attachments,
        })
    }

    /// Returns the number of the block in which the note was committed.
    pub fn block_num(&self) -> BlockNumber {
        self.inclusion_proof.location().block_num()
    }

    /// Consumes the synced note and returns its sync record together with the attachment content
    /// resolved for it. The note body, which the record does not hold, is dropped.
    ///
    /// The returned record reports its attachments as resolved, so
    /// [`CommittedNote::needs_attachment_fetch`] is always `false` for it. A note without
    /// attachments carries an empty set.
    pub fn into_committed_note(self) -> CommittedNote {
        CommittedNote::new(self.note_id, self.metadata, self.inclusion_proof)
            .with_attachments(self.attachments)
            .expect("the attachments were checked against the metadata in `SyncedNote::new`")
    }
}

// FETCH HELPERS
// ================================================================================================

/// Calls [`NodeRpcClient::sync_notes`] for the requested range, then makes a single
/// [`NodeRpcClient::get_notes_by_id`] call to resolve the note content the sync response did not
/// already carry, according to `fetch`, folding it into each note.
///
/// A note whose attachments the sync response already carried needs no request: only notes
/// reporting `needs_attachment_fetch` have theirs fetched.
///
/// With [`NoteContentFetch::PublicDetailsAndAttachments`], all public notes in the range are
/// additionally fetched so the request does not reveal the client's interest set. Narrowing it
/// reveals nothing either, since the omissions follow the node's own sync records.
///
/// Returns one [`ResolvedSyncNotesBlock`] per matching block, each note carrying its inclusion data
/// alongside its content.
///
/// A note whose resolved content contradicts its sync record is dropped with a warning rather than
/// failing the call, since anyone can commit a note whose content they never publish.
pub(crate) async fn sync_notes_with_content(
    rpc_api: &dyn NodeRpcClient,
    block_from: BlockNumber,
    block_to: BlockNumber,
    note_tags: &BTreeSet<NoteTag>,
    fetch: NoteContentFetch,
) -> Result<Vec<ResolvedSyncNotesBlock>, RpcError> {
    let blocks = rpc_api.sync_notes(block_from, block_to, note_tags).await?;
    let note_ids: Vec<NoteId> = blocks
        .iter()
        .flat_map(|block| block.notes.values())
        .filter(|note| match fetch {
            NoteContentFetch::PublicDetailsAndAttachments => {
                note.note_type() == NoteType::Public || note.needs_attachment_fetch()
            },
            NoteContentFetch::AttachmentsOnly => note.needs_attachment_fetch(),
        })
        .map(|note| *note.note_id())
        .collect();

    let mut fetched_content: BTreeMap<NoteId, (Option<NoteDetails>, Option<NoteAttachments>)> =
        BTreeMap::new();
    if !note_ids.is_empty() {
        for fetched_note in rpc_api.get_notes_by_id(&note_ids).await? {
            let (note_id, details, attachments) = match fetched_note {
                FetchedNote::Public(note, _) => {
                    let note_id = note.id();
                    let (assets, _, recipient, attachments) = note.into_parts();
                    (note_id, Some(NoteDetails::new(assets, recipient)), attachments)
                },
                FetchedNote::Private(note_id, _, attachments, _) => (note_id, None, attachments),
            };

            // An empty set carries nothing, so it is recorded as absent rather than as content:
            // keeping it would shadow the attachments the note's own sync record may already have
            // carried, for a public note as much as for a private one.
            let attachments = (!attachments.is_empty()).then_some(attachments);
            fetched_content.insert(note_id, (details, attachments));
        }
    }

    // Fold the resolved content into each note, keeping the per-block grouping so the inclusion
    // data (header + MMR path) is carried once per block. `SyncedNote::new` rejects content that is
    // inconsistent with its sync record (mismatched or missing attachment content); such notes are
    // dropped rather than failing the sync, since a tracked record is never stored incomplete this
    // way (it stays expected and can be retried by re-importing), while a hard error would wedge
    // every sync scanning this block range.
    let mut synced_blocks = Vec::with_capacity(blocks.len());
    for block in blocks {
        let mut notes = BTreeMap::new();
        for (note_id, committed) in block.notes {
            // Fetched attachments win when the response actually carried some: a public note's
            // attachments are bound to the requested id, which `VerifyingRpcClient` checks. The
            // sync record is the fallback, and a note reporting neither has none.
            let (details, fetched_attachments) =
                fetched_content.remove(&note_id).unwrap_or_default();
            let attachments = fetched_attachments
                .or_else(|| committed.attachments().cloned())
                .unwrap_or_else(NoteAttachments::empty);

            match SyncedNote::new(committed, details, attachments) {
                Ok(synced_note) => {
                    notes.insert(note_id, synced_note);
                },
                Err(err) => {
                    tracing::warn!(%note_id, %err, "skipping synced note with unusable content");
                },
            }
        }
        synced_blocks.push(ResolvedSyncNotesBlock {
            block_header: block.block_header,
            mmr_path: block.mmr_path,
            notes,
        });
    }

    Ok(synced_blocks)
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::Word;
    use miden_protocol::account::AccountId;
    use miden_protocol::crypto::merkle::SparseMerklePath;
    use miden_protocol::note::{
        NoteAssets,
        NoteAttachment,
        NoteAttachmentScheme,
        NoteRecipient,
        NoteStorage,
        PartialNoteMetadata,
    };
    use miden_protocol::testing::account_id::ACCOUNT_ID_SENDER;
    use miden_standards::code_builder::CodeBuilder;

    use super::*;

    fn committed_note(attachments: &NoteAttachments) -> CommittedNote {
        let sender: AccountId = ACCOUNT_ID_SENDER.try_into().unwrap();
        let metadata =
            NoteMetadata::new(PartialNoteMetadata::new(sender, NoteType::Private), attachments);
        let path = SparseMerklePath::from_parts(0, Vec::new()).unwrap();
        let inclusion_proof =
            NoteInclusionProof::new(BlockNumber::GENESIS, 0, path).expect("index 0 is in range");

        CommittedNote::new(NoteId::from_raw(Word::empty()), metadata, inclusion_proof)
    }

    #[test]
    fn synced_note_rejects_a_body_for_a_private_note() {
        let committed = committed_note(&NoteAttachments::empty());

        let note_script = CodeBuilder::new()
            .compile_note_script("@note_script\npub proc main\n    nop\nend")
            .unwrap();
        let recipient =
            NoteRecipient::new(Word::empty(), note_script, NoteStorage::new(vec![]).unwrap());
        let details = NoteDetails::new(NoteAssets::new(vec![]).unwrap(), recipient);

        let err = SyncedNote::new(committed, Some(details), NoteAttachments::empty()).unwrap_err();

        assert!(matches!(err, RpcError::InvalidResponse(_)), "got {err:?}");
    }

    /// A note advertising attachments whose content never arrived is rejected: empty attachments
    /// hash to a different commitment than the note's.
    #[test]
    fn synced_note_rejects_unresolved_attachments() {
        let attachment = NoteAttachment::with_words(
            NoteAttachmentScheme::new(100).unwrap(),
            vec![Word::from([5u32, 6, 7, 8]), Word::from([9u32, 10, 11, 12])],
        )
        .unwrap();
        let committed = committed_note(&NoteAttachments::new(vec![attachment]).unwrap());

        let err = SyncedNote::new(committed, None, NoteAttachments::empty()).unwrap_err();

        assert!(matches!(err, RpcError::InvalidResponse(_)), "got {err:?}");
    }
}
