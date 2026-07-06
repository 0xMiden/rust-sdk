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
    NoteFile,
    NoteId,
    NoteInclusionProof,
    NoteMetadata,
    NoteTag,
};
use miden_tx::auth::TransactionAuthenticator;

use crate::rpc::RpcError;
use crate::rpc::domain::note::FetchedNote;
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
    /// without metadata — e.g. from [`NoteFile::NoteDetails`] in an `Expected` state — have no
    /// note ID yet, whereas the details commitment is always available.
    ///
    /// - If the note files are [`NoteFile::NoteId`], the notes are fetched from the node and stored
    ///   in the client's store. If the note is private or doesn't exist, an error is returned.
    /// - If the note files are [`NoteFile::NoteDetails`], new notes are created with the provided
    ///   details and tags.
    /// - If the note files are [`NoteFile::NoteWithProof`], the notes are stored with the provided
    ///   inclusion proof and metadata. The block header data is only fetched from the node if the
    ///   note is committed in the past relative to the client.
    ///
    /// # Errors
    ///
    /// - If an attempt is made to overwrite a note that is currently processing.
    ///
    /// Note: This operation is atomic. If any note file is invalid or any existing note is in the
    /// processing state, the entire operation fails and no notes are imported.
    pub async fn import_notes(
        &mut self,
        note_files: &[NoteFile],
    ) -> Result<Vec<NoteDetailsCommitment>, ClientError> {
        // Deduplicate the incoming files, keeping note IDs and details commitments in separate
        // collections. `NoteFile::NoteId` entries are keyed by their note ID; detail-carrying
        // entries (`NoteDetails`/`NoteWithProof`) are keyed by their details commitment, since
        // they may have no note ID of their own.
        let mut ids = BTreeSet::new();
        let mut files_by_commitment = BTreeMap::new();
        for note_file in note_files {
            match note_file {
                NoteFile::NoteId(id) => {
                    ids.insert(*id);
                },
                NoteFile::NoteDetails { details, .. } => {
                    files_by_commitment.insert(details.commitment(), note_file.clone());
                },
                NoteFile::NoteWithProof(note, _) => {
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
                NoteFile::NoteDetails { details, after_block_num, tag } => {
                    requests_by_details.push((previous_note, details, after_block_num, tag));
                },
                NoteFile::NoteWithProof(note, inclusion_proof) => {
                    requests_by_proof.push((previous_note, note, inclusion_proof));
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
        for note in imported_notes.into_iter().flatten() {
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
    /// The returned records are positional rather than keyed by [`NoteId`]; a `None` entry marks a
    /// note that needs no update.
    ///
    /// # Errors:
    /// - If a note doesn't exist on the node.
    /// - If a note exists but is private.
    async fn import_note_records_by_id(
        &mut self,
        notes: BTreeMap<NoteId, Option<InputNoteRecord>>,
    ) -> Result<Vec<Option<InputNoteRecord>>, ClientError> {
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

                    note_records.push(Some(previous_note));
                } else {
                    note_records.push(None);
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
    pub(crate) async fn import_note_records_by_proof(
        &mut self,
        requested_notes: Vec<(Option<InputNoteRecord>, Note, NoteInclusionProof)>,
    ) -> Result<Vec<Option<InputNoteRecord>>, ClientError> {
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
                    note_records.push(Some(note_record));
                }

                note_records.push(None);
            } else {
                let block_height = inclusion_proof.location().block_num();
                let current_block_num = self.get_sync_height().await?;

                let tag = metadata.tag();
                let mut note_changed =
                    note_record.inclusion_proof_received(inclusion_proof, metadata)?;

                if block_height <= current_block_num {
                    // If the note is committed in the past we need to manually fetch the block
                    // header and MMR proof to verify the inclusion proof.
                    //
                    // Building the MMR outside the loop would fail with BlockHeaderNotFound(0)
                    // because store will be fresh, which can't happen here.
                    let mut partial_mmr = self.get_current_partial_mmr().await?;
                    let block_header = self
                        .get_and_store_authenticated_block(block_height, &mut partial_mmr)
                        .await?;
                    self.cache_partial_mmr(partial_mmr).await?;

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
                    note_records.push(Some(note_record));
                } else {
                    note_records.push(None);
                }
            }
        }

        Ok(note_records)
    }

    /// Builds a note record list from note details. If a note with the same ID was already stored
    /// it is passed via `previous_note` so it can be updated.
    async fn import_note_records_by_details(
        &mut self,
        requested_notes: Vec<(Option<InputNoteRecord>, NoteDetails, BlockNumber, Option<NoteTag>)>,
    ) -> Result<Vec<Option<InputNoteRecord>>, ClientError> {
        let mut lowest_request_block: BlockNumber = u32::MAX.into();
        let mut note_requests = vec![];
        for (_, details, after_block_num, tag) in &requested_notes {
            if let Some(tag) = tag {
                note_requests.push((details.commitment(), tag));
                if after_block_num < &lowest_request_block {
                    lowest_request_block = *after_block_num;
                }
            }
        }
        let mut committed_notes_data =
            self.check_expected_notes(lowest_request_block, note_requests).await?;

        let mut note_records = vec![];
        for (previous_note, details, after_block_num, tag) in requested_notes {
            let note_record = previous_note.unwrap_or({
                InputNoteRecord::new(
                    details,
                    NoteAttachments::empty(),
                    self.store.get_current_timestamp(),
                    ExpectedNoteState { metadata: None, after_block_num, tag }.into(),
                )
            });

            match committed_notes_data.remove(&note_record.details_commitment()) {
                Some((metadata, inclusion_proof)) => {
                    // Building the MMR outside the loop would fail with BlockHeaderNotFound(0)
                    // because store will be fresh, which can't happen here.
                    let mut partial_mmr = self.get_current_partial_mmr().await?;
                    let block_header = self
                        .get_and_store_authenticated_block(
                            inclusion_proof.location().block_num(),
                            &mut partial_mmr,
                        )
                        .await?;

                    self.cache_partial_mmr(partial_mmr).await?;

                    let tag = metadata.tag();
                    let mut note_record = note_record;
                    let note_changed =
                        note_record.inclusion_proof_received(inclusion_proof, metadata)?;

                    if note_record.block_header_received(&block_header)? | note_changed {
                        self.store
                            .remove_note_tag(NoteTagRecord::with_note_source(
                                tag,
                                note_record.details_commitment(),
                            ))
                            .await?;

                        note_records.push(Some(note_record));
                    } else {
                        note_records.push(None);
                    }
                },
                None => {
                    note_records.push(Some(note_record));
                },
            }
        }

        Ok(note_records)
    }

    /// Checks if notes with the given details commitments and tags are present in the chain between
    /// `request_block_num` and the current block, returning their metadata and inclusion proof
    /// keyed by details commitment.
    ///
    /// Expected notes have no metadata and thus no `NoteId`, so each committed note is matched by
    /// reconstructing the id from the committed metadata: `NoteId::new(details_commitment,
    /// metadata)`.
    async fn check_expected_notes(
        &mut self,
        request_block_num: BlockNumber,
        // Expected notes' details commitments with their tags
        expected_notes: Vec<(NoteDetailsCommitment, &NoteTag)>,
    ) -> Result<BTreeMap<NoteDetailsCommitment, (NoteMetadata, NoteInclusionProof)>, ClientError>
    {
        let tracked_tags: BTreeSet<NoteTag> = expected_notes.iter().map(|(_, tag)| **tag).collect();
        let mut retrieved_proofs = BTreeMap::new();
        let current_block_num = self.get_sync_height().await?;

        if request_block_num > current_block_num {
            return Ok(retrieved_proofs);
        }

        let blocks = self
            .rpc_api
            .sync_notes(request_block_num, current_block_num, &tracked_tags)
            .await
            .map_err(ClientError::RpcError)?;

        for block in &blocks {
            if block.block_header.block_num() > current_block_num {
                break;
            }

            for sync_note in block.notes.values() {
                let Some((commitment, _)) = expected_notes.iter().find(|(commitment, _)| {
                    NoteId::new(*commitment, sync_note.metadata()) == *sync_note.note_id()
                }) else {
                    continue;
                };

                retrieved_proofs.insert(
                    *commitment,
                    (*sync_note.metadata(), sync_note.inclusion_proof().clone()),
                );
            }
        }

        Ok(retrieved_proofs)
    }
}

// HELPERS
// ================================================================================================

/// Returns an error if the already-stored note is currently being processed by a local
/// transaction, since an in-flight note can't be overwritten by an import.
fn ensure_not_processing(previous_note: Option<&InputNoteRecord>) -> Result<(), ClientError> {
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
