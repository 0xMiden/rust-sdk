//! Provides note importing methods.
//!
//! This module allows users to import notes into the client's store.
//! Depending on the variant of [`NoteFile`] provided, the client will either fetch note details
//! from the network or create a new note record from supplied data. If a note already exists in
//! the store, it is updated with the new information. Additionally, the appropriate note tag
//! is tracked based on the imported note's metadata.
//!
//! For more specific information on how the process is performed, refer to the docs for
//! [`Client::import_note()`].
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::block::BlockNumber;
use miden_protocol::note::{
    Note,
    NoteAttachments,
    NoteDetails,
    NoteDetailsCommitment,
    NoteId,
    NoteInclusionProof,
    NoteTag,
};
use miden_standards::note::NoteFile;
use miden_tx::auth::TransactionAuthenticator;

use crate::rpc::domain::note::{FetchedNote, ResolvedSyncNotesBlock, SyncedNote};
use crate::rpc::{NoteContentFetch, RpcError};
use crate::store::input_note_states::ExpectedNoteState;
use crate::store::{InputNoteRecord, InputNoteState, NoteFilter};
use crate::sync::NoteTagRecord;
use crate::{Client, ClientError};

/// Note importing methods.
impl<AUTH> Client<AUTH>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    // INPUT NOTE CREATION
    // --------------------------------------------------------------------------------------------

    /// Imports a batch of new input notes into the client's store. The information stored depends
    /// on the type of note files provided. If the notes existed previously, it will be updated
    /// with the new information. The tags specified by the `NoteFile`s will start being
    /// tracked. Returns the details commitments of notes that were successfully imported or
    /// updated. The details commitment is used (rather than the note ID) because notes imported
    /// without metadata — e.g. from [`NoteFile::ExpectedNote`] in an `Expected` state — have no
    /// note ID yet, whereas the details commitment is always available.
    ///
    /// - If the note files are [`NoteFile::NoteId`], the notes are fetched from the node and stored
    ///   in the client's store. If the note is private or doesn't exist, an error is returned.
    /// - If the note files are [`NoteFile::ExpectedNote`], new notes are created with the provided
    ///   details and tags.
    /// - If the note files are [`NoteFile::Committed`], the notes are stored with the provided
    ///   inclusion proof and metadata. The block header data is only fetched from the node if the
    ///   note is committed in the past relative to the client.
    ///
    /// # Errors
    ///
    /// - If an attempt is made to overwrite a note that is currently processing.
    ///
    /// Note: This operation is atomic. If any note file is invalid or any existing note is in the
    /// processing state, the entire operation fails and no notes are imported.
    // TODO: Validations need to be added to the import workflows. For example, when adding a block
    // header for a note we need to check the chain root validity, etc.
    pub async fn import_notes(
        &mut self,
        note_files: &[NoteFile],
    ) -> Result<Vec<NoteDetailsCommitment>, ClientError> {
        self.ensure_genesis_in_place().await?;

        // Deduplicate the incoming files, keeping note IDs and details commitments in separate
        // collections. `NoteFile::NoteId` entries are keyed by their note ID; detail-carrying
        // entries (`ExpectedNote`/`Committed`) are keyed by their details commitment, since
        // they may have no note ID of their own.
        let mut ids = BTreeSet::new();
        let mut files_by_commitment = BTreeMap::new();
        for note_file in note_files {
            match note_file {
                NoteFile::NoteId(id) => {
                    ids.insert(*id);
                },
                NoteFile::ExpectedNote { details, .. } => {
                    files_by_commitment.insert(details.commitment(), note_file.clone());
                },
                NoteFile::Committed { note, .. } => {
                    files_by_commitment.insert(note.details_commitment(), note_file.clone());
                },
            }
        }

        // Resolve previously stored versions: by id for `NoteFile::NoteId`, by details commitment
        // otherwise (which also matches metadata-less records, whose `note_id` is NULL).
        let previous_by_id: BTreeMap<NoteId, InputNoteRecord> = self
            .get_input_notes(NoteFilter::List(ids.iter().copied().collect()))
            .await?
            .into_iter()
            .filter_map(|note| note.id().map(|id| (id, note)))
            .collect();
        let previous_by_commitment: BTreeMap<NoteDetailsCommitment, InputNoteRecord> = self
            .get_input_notes(NoteFilter::DetailsCommitments(
                files_by_commitment.keys().copied().collect(),
            ))
            .await?
            .into_iter()
            .map(|note| (note.details_commitment(), note))
            .collect();

        // Pair each deduplicated file with its previously stored version (if any), bucketed by
        // variant. A note that is currently being processed can't be overwritten.
        let mut requests_by_id = BTreeMap::new();
        let mut requests_by_details = vec![];
        let mut requests_by_proof = vec![];

        for id in ids {
            let previous_note = previous_by_id.get(&id).cloned();
            ensure_not_processing(previous_note.as_ref())?;
            requests_by_id.insert(id, previous_note);
        }

        for (commitment, note_file) in files_by_commitment {
            let previous_note = previous_by_commitment.get(&commitment).cloned();
            ensure_not_processing(previous_note.as_ref())?;
            match note_file {
                NoteFile::ExpectedNote { details, sync_hint } => {
                    requests_by_details.push((
                        previous_note,
                        details,
                        sync_hint.after_block_num(),
                        sync_hint.tag(),
                    ));
                },
                NoteFile::Committed { note, proof } => {
                    requests_by_proof.push((previous_note, note, proof));
                },
                NoteFile::NoteId(_) => {
                    unreachable!("files_by_commitment only holds detail-carrying note files")
                },
            }
        }

        let mut imported_notes = vec![];
        if !requests_by_id.is_empty() {
            let notes_by_id = self.import_note_records_by_id(requests_by_id).await?;
            imported_notes.extend(notes_by_id);
        }

        if !requests_by_details.is_empty() {
            let notes_by_details = self.import_note_records_by_details(requests_by_details).await?;
            imported_notes.extend(notes_by_details);
        }

        if !requests_by_proof.is_empty() {
            let notes_by_proof = self.import_note_records_by_proof(requests_by_proof).await?;
            imported_notes.extend(notes_by_proof);
        }

        let mut imported_commitments = Vec::with_capacity(imported_notes.len());
        for note in imported_notes {
            let details_commitment = note.details_commitment();
            if let InputNoteState::Expected(ExpectedNoteState { tag: Some(tag), .. }) = note.state()
            {
                self.store
                    .add_note_tag(NoteTagRecord::with_note_source(*tag, details_commitment))
                    .await?;
            }
            self.store.upsert_input_notes(&[note]).await?;
            imported_commitments.push(details_commitment);
        }

        Ok(imported_commitments)
    }

    // HELPERS
    // ================================================================================================

    /// Builds note records from the note IDs. If a note with the same ID was already stored it
    /// is passed via `previous_note` so it can be updated. The note information is fetched from
    /// the node and stored in the client's store.
    ///
    /// Only records that changed as a result of the import are returned.
    ///
    /// # Errors:
    /// - If a note doesn't exist on the node.
    /// - If a note exists but is private.
    async fn import_note_records_by_id(
        &mut self,
        notes: BTreeMap<NoteId, Option<InputNoteRecord>>,
    ) -> Result<Vec<InputNoteRecord>, ClientError> {
        let note_ids = notes.keys().copied().collect::<Vec<_>>();

        let fetched_notes =
            self.rpc_api.get_notes_by_id(&note_ids).await.map_err(|err| match err {
                RpcError::NoteNotFound(note_id) => ClientError::NoteNotFoundOnChain(note_id),
                err => ClientError::RpcError(err),
            })?;

        if fetched_notes.is_empty() {
            return Err(ClientError::NoteImportError("No notes fetched from node".to_string()));
        }

        let mut note_records = Vec::new();
        let mut notes_to_request = vec![];
        for fetched_note in fetched_notes {
            let note_id = fetched_note.id();
            let inclusion_proof = fetched_note.inclusion_proof().clone();

            let previous_note =
                notes.get(&note_id).cloned().ok_or(ClientError::NoteImportError(format!(
                    "Failed to retrieve note with id {note_id} from node"
                )))?;
            if let Some(mut previous_note) = previous_note {
                if previous_note
                    .inclusion_proof_received(inclusion_proof, *fetched_note.metadata())?
                {
                    self.store.remove_note_tag((&previous_note).try_into()?).await?;

                    note_records.push(previous_note);
                }
            } else {
                let fetched_note = match fetched_note {
                    FetchedNote::Public(note, _) => note,
                    FetchedNote::Private(..) => {
                        return Err(ClientError::NoteImportError(
                            "Incomplete imported note is private".to_string(),
                        ));
                    },
                };

                let note_request = (previous_note, fetched_note, inclusion_proof);
                notes_to_request.push(note_request);
            }
        }

        if !notes_to_request.is_empty() {
            let note_records_by_proof = self.import_note_records_by_proof(notes_to_request).await?;
            note_records.extend(note_records_by_proof);
        }
        Ok(note_records)
    }

    /// Builds a note record list from notes and inclusion proofs. If a note with the same ID was
    /// already stored it is passed via `previous_note` so it can be updated. The note's
    /// nullifier is used to determine if the note has been consumed in the node and gives it
    /// the correct state.
    ///
    /// If the note isn't consumed and it was committed in the past relative to the client, then
    /// the MMR for the relevant block is fetched from the node and stored.
    ///
    /// Only records that changed as a result of the import are returned.
    pub(crate) async fn import_note_records_by_proof(
        &mut self,
        requested_notes: Vec<(Option<InputNoteRecord>, Note, NoteInclusionProof)>,
    ) -> Result<Vec<InputNoteRecord>, ClientError> {
        // TODO: iterating twice over requested notes
        let mut note_records = vec![];

        let mut nullifier_requests = BTreeSet::new();
        let mut lowest_block_height: BlockNumber = u32::MAX.into();
        for (previous_note, note, inclusion_proof) in &requested_notes {
            let nullifier = match previous_note {
                Some(previous_note) => previous_note.nullifier(),
                None => Some(note.nullifier()),
            };
            if let Some(nullifier) = nullifier {
                nullifier_requests.insert(nullifier);
            }
            if inclusion_proof.location().block_num() < lowest_block_height {
                lowest_block_height = inclusion_proof.location().block_num();
            }
        }

        let nullifier_commit_heights = self
            .rpc_api
            .get_nullifier_commit_heights(nullifier_requests, lowest_block_height)
            .await?;
        let mut partial_mmr = self.get_current_partial_mmr().await?;

        for (previous_note, note, inclusion_proof) in requested_notes {
            let metadata = *note.metadata();
            let attachments = note.attachments().clone();
            let mut note_record = previous_note.unwrap_or(InputNoteRecord::new(
                note.into(),
                attachments,
                self.store.get_current_timestamp(),
                ExpectedNoteState {
                    metadata: Some(metadata),
                    after_block_num: inclusion_proof.location().block_num(),
                    tag: Some(metadata.tag()),
                }
                .into(),
            ));

            if let Some(nullifier) = note_record.nullifier()
                && let Some(Some(block_height)) = nullifier_commit_heights.get(&nullifier)
            {
                if note_record.consumed_externally(nullifier, *block_height, None)? {
                    note_records.push(note_record);
                }
            } else {
                let block_height = inclusion_proof.location().block_num();
                let current_block_num = self.get_sync_height().await?;

                let tag = metadata.tag();
                let mut note_changed =
                    note_record.inclusion_proof_received(inclusion_proof, metadata)?;

                if block_height <= current_block_num {
                    // A note committed in the past needs its block header fetched and
                    // authenticated to verify the inclusion proof.
                    let block_header = self
                        .get_and_store_authenticated_block(block_height, &mut partial_mmr)
                        .await?;
                    note_changed |= note_record.block_header_received(&block_header)?;
                } else {
                    // If the note is in the future we import it as unverified. We add the note tag
                    // so that the note is verified naturally in the next sync.
                    self.store
                        .add_note_tag(NoteTagRecord::with_note_source(
                            tag,
                            note_record.details_commitment(),
                        ))
                        .await?;
                }

                if note_changed {
                    note_records.push(note_record);
                }
            }
        }
        self.cache_partial_mmr(partial_mmr).await?;

        Ok(note_records)
    }

    /// Builds a note record list from note details. If a note with the same ID was already stored
    /// it is passed via `previous_note` so it can be updated.
    ///
    /// Only records that need to be stored are returned: notes the node has not reported as
    /// committed keep (or get) their expected record, while committed notes are returned only if
    /// the new information changed them.
    async fn import_note_records_by_details(
        &mut self,
        requested_notes: Vec<NoteImportRequest>,
    ) -> Result<Vec<InputNoteRecord>, ClientError> {
        let note_updates = self.fetch_transport_notes_onchain_state(requested_notes).await?;

        let mut partial_mmr = self.get_current_partial_mmr().await?;
        self.insert_note_blocks(note_updates.note_blocks, &mut partial_mmr).await?;
        // Cache MMR so pruning can reuse in-memory MMR.
        self.cache_partial_mmr(partial_mmr).await?;

        for tag in note_updates.tags_to_remove {
            self.store.remove_note_tag(tag).await?;
        }

        Ok(note_updates.notes_to_write)
    }

    // TRANSPORT-DELIVERED NOTE IMPORT
    // --------------------------------------------------------------------------------------------

    /// Fetches the on-chain state of transport-delivered notes, returning the records to write
    /// and the blocks that committed them.
    ///
    /// A note with a stored version is passed via `previous_note` so it can be updated. Notes the
    /// node has not reported as committed keep (or get) their expected record; the rest become
    /// `Committed`, since the response carries the block header that verifies their inclusion,
    /// and are returned only if the new information changed them.
    pub(crate) async fn fetch_transport_notes_onchain_state(
        &self,
        requested_notes: Vec<NoteImportRequest>,
    ) -> Result<TransportNoteUpdates, ClientError> {
        let mut lowest_request_block: BlockNumber = u32::MAX.into();
        let mut sync_tags = BTreeSet::new();
        let mut requested_commitments = Vec::with_capacity(requested_notes.len());
        for (_, details, after_block_num, tag) in &requested_notes {
            sync_tags.insert(*tag);
            requested_commitments.push(details.commitment());
            lowest_request_block = lowest_request_block.min(*after_block_num);
        }
        let blocks = self.sync_expected_notes(lowest_request_block, &sync_tags).await?;

        let mut note_updates = TransportNoteUpdates::default();

        // An expected note has no metadata and thus no `NoteId`, so each returned note is matched
        // to its request by rebuilding the id from the committed metadata. Only the blocks holding
        // a match are kept: the rest hold notes under the same tag that answer no request.
        let mut committed_notes_data = BTreeMap::new();
        for block in blocks {
            let mut block_matched = false;
            for (note_id, sync_note) in &block.notes {
                let metadata = sync_note.committed.metadata();
                let Some(commitment) = requested_commitments
                    .iter()
                    .find(|commitment| NoteId::new(**commitment, metadata) == *note_id)
                else {
                    continue;
                };

                committed_notes_data
                    .insert(*commitment, (sync_note.clone(), block.block_header.clone()));
                block_matched = true;
            }

            if block_matched {
                note_updates.note_blocks.push(block);
            }
        }

        for (previous_note, details, after_block_num, tag) in requested_notes {
            let mut note_record = previous_note.unwrap_or_else(|| {
                InputNoteRecord::new(
                    details,
                    NoteAttachments::empty(),
                    self.store.get_current_timestamp(),
                    ExpectedNoteState {
                        metadata: None,
                        after_block_num,
                        tag: Some(tag),
                    }
                    .into(),
                )
            });

            // Notes the node has not reported as committed keep their expected record untouched.
            let Some((
                SyncedNote {
                    committed: committed_note, attachments, ..
                },
                block_header,
            )) = committed_notes_data.remove(&note_record.details_commitment())
            else {
                note_updates.notes_to_write.push(note_record);
                continue;
            };

            // A note that carries no attachments has nothing to apply to the record.
            let attachments = (!attachments.is_empty()).then_some(attachments);

            let metadata = *committed_note.metadata();
            let mut note_changed = note_record
                .inclusion_proof_received(committed_note.inclusion_proof().clone(), metadata)?;

            if let Some(attachments) = attachments {
                note_changed |= note_record.attachments_received(attachments);
            }

            // `block_header_received` transitions the record's state, so it must always run.
            note_changed |= note_record.block_header_received(&block_header)?;

            // Once committed, the note no longer needs its expected-note tag.
            if note_changed {
                note_updates.tags_to_remove.push(NoteTagRecord::with_note_source(
                    metadata.tag(),
                    note_record.details_commitment(),
                ));
            }

            if note_changed {
                note_updates.notes_to_write.push(note_record);
            }
        }

        Ok(note_updates)
    }

    /// Fetches the nullifier commit heights of the notes in `note_updates`, marking those already
    /// spent as consumed.
    ///
    /// Must run after [`Client::fetch_transport_notes_onchain_state`]: only a note the node
    /// reported as committed has the metadata a nullifier is derived from, so before that there
    /// is nothing to ask about.
    pub(crate) async fn fetch_note_nullifiers(
        &self,
        note_updates: &mut TransportNoteUpdates,
    ) -> Result<(), ClientError> {
        // A record carries a nullifier only once the node has reported it committed, which is
        // what supplies the metadata the nullifier is derived from.
        let mut nullifiers = BTreeSet::new();
        let mut lowest_commitment_block: BlockNumber = u32::MAX.into();
        for note_record in &note_updates.notes_to_write {
            let (Some(nullifier), Some(inclusion_proof)) =
                (note_record.nullifier(), note_record.inclusion_proof())
            else {
                continue;
            };
            nullifiers.insert(nullifier);
            lowest_commitment_block =
                lowest_commitment_block.min(inclusion_proof.location().block_num());
        }

        if nullifiers.is_empty() {
            return Ok(());
        }

        let spent_heights = self
            .rpc_api
            .get_nullifier_commit_heights(nullifiers, lowest_commitment_block)
            .await?;

        for note_record in &mut note_updates.notes_to_write {
            let Some(nullifier) = note_record.nullifier() else {
                continue;
            };
            if let Some(Some(spent_at)) = spent_heights.get(&nullifier) {
                note_record.consumed_externally(nullifier, *spent_at, None)?;
            }
        }

        Ok(())
    }

    /// Applies the changes from `note_updates` to the store, returning the written records.
    ///
    /// The block headers go in first, so a record is never persisted as committed before the
    /// header proving its inclusion is tracked and stored.
    pub(crate) async fn apply_note_transport_updates(
        &mut self,
        note_updates: TransportNoteUpdates,
    ) -> Result<Vec<InputNoteRecord>, ClientError> {
        let mut partial_mmr = self.get_current_partial_mmr().await?;
        self.insert_note_blocks(note_updates.note_blocks, &mut partial_mmr).await?;
        // Cache MMR so pruning can reuse in-memory MMR.
        self.cache_partial_mmr(partial_mmr).await?;

        for tag in note_updates.tags_to_remove {
            self.store.remove_note_tag(tag).await?;
        }

        for note in &note_updates.notes_to_write {
            // A record still expected needs its tag tracked so a later sync finds it. A committed
            // one is no longer in that state, so it is skipped here.
            if let InputNoteState::Expected(ExpectedNoteState { tag: Some(tag), .. }) = note.state()
            {
                self.store
                    .add_note_tag(NoteTagRecord::with_note_source(*tag, note.details_commitment()))
                    .await?;
            }
        }
        self.store.upsert_input_notes(&note_updates.notes_to_write).await?;

        Ok(note_updates.notes_to_write)
    }

    /// Fetches every block between `request_block_num` and the client's sync height that holds a
    /// note under one of `sync_tags`.
    ///
    /// Each block carries its header and the MMR path proving its inclusion at the sync height,
    /// which is the forest the client's partial MMR is at, so a note found here needs no further
    /// block lookup. Deciding which of the returned notes answer a request is the caller's.
    async fn sync_expected_notes(
        &self,
        request_block_num: BlockNumber,
        sync_tags: &BTreeSet<NoteTag>,
    ) -> Result<Vec<ResolvedSyncNotesBlock>, ClientError> {
        let current_block_num = self.get_sync_height().await?;

        // Notes expected only after a block we have not reached can't be committed within our
        // synced view yet: skip the lookup and let them stay expected until a future sync.
        if request_block_num > current_block_num {
            return Ok(Vec::new());
        }

        let mut blocks = self
            .rpc_api
            .sync_notes_with_content(
                request_block_num,
                current_block_num,
                sync_tags,
                NoteContentFetch::AttachmentsOnly,
            )
            .await
            .map_err(ClientError::RpcError)?;

        blocks.retain_mut(|block| {
            if block.block_header.block_num() > current_block_num {
                return false;
            }

            // A note carries its own commit height in its inclusion proof, which is a separate
            // field from the block header checked above. Authenticating the note later looks that
            // height up in the partial MMR, so a height beyond our synced view has to be dropped
            // here rather than trusted.
            block
                .notes
                .retain(|_, sync_note| sync_note.committed.block_num() <= current_block_num);

            !block.notes.is_empty()
        });

        Ok(blocks)
    }
}

// EXPECTED NOTE IMPORT
// ================================================================================================

/// A note to import: the stored record it updates when there is one, its details, the block from
/// which to look for its commitment, and the tag to track it under.
pub(crate) type NoteImportRequest = (Option<InputNoteRecord>, NoteDetails, BlockNumber, NoteTag);

/// Notes fetched from the Note Transport Layer, along with note tags to remove, and the blocks
/// that must be stored before their corresponding committed notes.
#[derive(Default)]
pub(crate) struct TransportNoteUpdates {
    /// The note records to write to the storage.
    notes_to_write: Vec<InputNoteRecord>,
    /// Blocks holding a committed note, as the node returned them. They must be tracked and
    /// stored before the note records that need them.
    note_blocks: Vec<ResolvedSyncNotesBlock>,
    /// Note-source tags to remove, one per committed note.
    tags_to_remove: Vec<NoteTagRecord>,
}

impl TransportNoteUpdates {
    /// Appends another batch to this one.
    ///
    /// Order is preserved, which is what makes a note returned by more than one transport page
    /// resolve to the version fetched last.
    pub(crate) fn merge(&mut self, other: Self) {
        self.notes_to_write.extend(other.notes_to_write);
        self.note_blocks.extend(other.note_blocks);
        self.tags_to_remove.extend(other.tags_to_remove);
    }
}

// HELPERS
// ================================================================================================

/// Returns an error if the already-stored note is currently being processed by a local
/// transaction, since an in-flight note can't be overwritten by an import.
pub(crate) fn ensure_not_processing(
    previous_note: Option<&InputNoteRecord>,
) -> Result<(), ClientError> {
    if let Some(note) = previous_note
        && note.is_processing()
    {
        return Err(ClientError::NoteImportError(format!(
            "Can't overwrite note with details commitment {} as it's currently being processed",
            note.details_commitment().to_hex(),
        )));
    }
    Ok(())
}
