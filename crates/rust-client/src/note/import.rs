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

use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::MmrProof;
use miden_protocol::note::{
    Note,
    NoteAttachments,
    NoteDetails,
    NoteDetailsCommitment,
    NoteId,
    NoteInclusionProof,
    NoteMetadata,
    NoteTag,
    Nullifier,
};
use miden_standards::note::NoteFile;
use miden_tx::auth::TransactionAuthenticator;

use crate::rpc::domain::note::{FetchedNote, ResolvedNoteContent, SyncedNote};
use crate::rpc::{NoteContentFetch, RpcError};
use crate::store::input_note_states::ExpectedNoteState;
use crate::store::{InputNoteRecord, InputNoteState, NoteFilter};
use crate::sync::NoteTagRecord;
use crate::{Client, ClientError};

/// An expected note to import: its details, the block after which it is expected to be committed,
/// the tag it should be tracked under, and its metadata when the caller already knows it (see
/// [`Client::import_notes_with_metadata`]).
pub(crate) type ExpectedNoteRequest = (NoteDetails, BlockNumber, NoteTag, Option<NoteMetadata>);

/// One expected note the import is about to write, together with what the chain said about it.
///
/// The stored record is deliberately not captured here. It is read when the plan is applied, so
/// that a write landing between the planning and the applying — the chain half of a
/// [`Client::sync_state`] does exactly that — is built on rather than overwritten.
struct PlannedNote {
    request: ExpectedNoteRequest,
    /// What the commitment scan found for this note, if it found it at all.
    committed: Option<SyncedNote>,
}

/// Everything an expected-note import needs from the network, fetched before anything is written.
///
/// Splitting the fetching from the writing is what lets [`Client::sync_state`] overlap the note
/// import with the chain fetch: the planning is read-only and can run inside the join, and
/// applying the plan touches only the store.
pub(crate) struct ExpectedNoteImportPlan {
    notes: Vec<PlannedNote>,
    /// Header and MMR proof for every block a planned note was committed in.
    blocks: BTreeMap<BlockNumber, (BlockHeader, MmrProof)>,
    /// Commit height of every nullifier the plan asked about and found spent.
    spent: BTreeMap<Nullifier, BlockNumber>,
}

impl ExpectedNoteImportPlan {
    /// A plan with nothing to import.
    pub(crate) fn empty() -> Self {
        Self {
            notes: Vec::new(),
            blocks: BTreeMap::new(),
            spent: BTreeMap::new(),
        }
    }
}

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
        let note_files: Vec<_> = note_files.iter().map(|file| (file.clone(), None)).collect();
        self.import_notes_with_metadata(&note_files).await
    }

    /// Imports note files whose metadata is already known from a trusted-enough source, such as
    /// the note header the transport delivers alongside the details.
    ///
    /// [`NoteFile::ExpectedNote`] carries no metadata, because a note file is normally expected to
    /// recover it from the chain. That recovery is bounded by the client's sync height, so a note
    /// committed above it stays metadata-less — and without metadata there is no nullifier to
    /// check, leaving an already-consumed note looking unspent. Supplying the metadata here skips
    /// the recovery and lets [`Client::mark_externally_consumed`] cover the note in the same call.
    ///
    /// The metadata is ignored for variants that carry their own.
    pub(crate) async fn import_notes_with_metadata(
        &mut self,
        note_files: &[(NoteFile, Option<NoteMetadata>)],
    ) -> Result<Vec<NoteDetailsCommitment>, ClientError> {
        self.ensure_genesis_in_place().await?;

        // Deduplicate the incoming files, keeping note IDs and details commitments in separate
        // collections. `NoteFile::NoteId` entries are keyed by their note ID; detail-carrying
        // entries (`ExpectedNote`/`Committed`) are keyed by their details commitment, since
        // they may have no note ID of their own.
        let mut ids = BTreeSet::new();
        let mut expected_by_commitment = BTreeMap::new();
        let mut committed_by_commitment = BTreeMap::new();
        for (note_file, metadata) in note_files {
            match note_file {
                NoteFile::NoteId(id) => {
                    ids.insert(*id);
                },
                NoteFile::ExpectedNote { details, sync_hint } => {
                    expected_by_commitment.insert(
                        details.commitment(),
                        (details.clone(), sync_hint.after_block_num(), sync_hint.tag(), *metadata),
                    );
                },
                NoteFile::Committed { note, proof } => {
                    committed_by_commitment
                        .insert(note.details_commitment(), (note.clone(), proof.clone()));
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
                committed_by_commitment.keys().copied().collect(),
            ))
            .await?
            .into_iter()
            .map(|note| (note.details_commitment(), note))
            .collect();

        // Pair each deduplicated file with its previously stored version (if any). A note that is
        // currently being processed can't be overwritten.
        let mut requests_by_id = BTreeMap::new();
        let mut requests_by_proof = vec![];

        for id in ids {
            let previous_note = previous_by_id.get(&id).cloned();
            ensure_not_processing(previous_note.as_ref())?;
            requests_by_id.insert(id, previous_note);
        }

        for (commitment, (note, proof)) in committed_by_commitment {
            let previous_note = previous_by_commitment.get(&commitment).cloned();
            ensure_not_processing(previous_note.as_ref())?;
            requests_by_proof.push((previous_note, note, proof));
        }

        let mut imported_notes = vec![];
        if !requests_by_id.is_empty() {
            let notes_by_id = self.import_note_records_by_id(requests_by_id).await?;
            imported_notes.extend(notes_by_id);
        }

        if !requests_by_proof.is_empty() {
            let notes_by_proof = self.import_note_records_by_proof(requests_by_proof).await?;
            imported_notes.extend(notes_by_proof);
        }

        let mut imported_commitments = self.store_imported_notes(imported_notes).await?;

        // Expected notes go through the plan/apply pair, so that this path and the sync path share
        // one implementation. Scanning up to the sync height keeps a standalone import from
        // reporting a note as committed in a block the caller has not synced to yet.
        if !expected_by_commitment.is_empty() {
            let requests: Vec<ExpectedNoteRequest> = expected_by_commitment.into_values().collect();
            let scan_to = self.get_sync_height().await?;
            let plan = self.plan_expected_note_import(&requests, scan_to).await?;
            imported_commitments.extend(self.apply_expected_note_import(plan).await?);
        }

        Ok(imported_commitments)
    }

    /// Registers the expected-note tag of every record that still needs one, upserts the records,
    /// and returns their details commitments.
    async fn store_imported_notes(
        &mut self,
        notes: Vec<InputNoteRecord>,
    ) -> Result<Vec<NoteDetailsCommitment>, ClientError> {
        let mut imported_commitments = Vec::with_capacity(notes.len());
        for note in notes {
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

        let current_block_num = self.get_sync_height().await?;
        // Search all the way to the chain tip, not just to the sync height. Every note here
        // arrives with an inclusion proof, so the caller is asking what state it is really in;
        // answering "not spent" for a spend the client simply has not synced past yet would hand
        // back a note that looks consumable and is not.
        let (chain_tip, _) = self.rpc_api.get_block_header_by_number(None, false).await?;
        let nullifier_commit_heights = self
            .rpc_api
            .get_nullifier_commit_heights(
                nullifier_requests,
                lowest_block_height,
                chain_tip.block_num(),
            )
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

    /// Plans the import of expected notes: asks the chain which of them are committed at or below
    /// `scan_to`, and fetches everything applying the plan will need — the authentication data for
    /// each commitment block, and the nullifier state of every note that turned out to be
    /// committed.
    ///
    /// Touches the network only, never the store, so this can run concurrently with other
    /// fetching (see [`Client::sync_state`]) and nothing it produces can go stale against a write
    /// that lands before the plan is applied.
    ///
    /// `scan_to` bounds both the commitment scan and the nullifier search, and the caller picks
    /// it: it is the highest block the store will have synced to by the time the plan is applied,
    /// which is both how far an answer can be acted on and how far the partial MMR will be able to
    /// authenticate.
    pub(crate) async fn plan_expected_note_import(
        &self,
        requests: &[ExpectedNoteRequest],
        scan_to: BlockNumber,
    ) -> Result<ExpectedNoteImportPlan, ClientError> {
        let mut lowest_request_block: BlockNumber = u32::MAX.into();
        let mut scan_requests = Vec::with_capacity(requests.len());
        for (details, after_block_num, tag, _) in requests {
            scan_requests.push((details.commitment(), *tag));
            lowest_request_block = lowest_request_block.min(*after_block_num);
        }
        let mut committed_notes =
            self.sync_expected_notes(lowest_request_block, scan_to, scan_requests).await?;

        let mut notes = Vec::with_capacity(requests.len());
        // Authentication data is per block, not per note: several notes can share one.
        let mut commitment_blocks = BTreeSet::new();
        let mut nullifier_requests = BTreeSet::new();
        let mut lowest_commitment_block: BlockNumber = u32::MAX.into();

        for request in requests {
            let (details, ..) = request;
            let committed = committed_notes.remove(&details.commitment());
            if let Some(synced) = &committed {
                let block_num = synced.committed.block_num();
                commitment_blocks.insert(block_num);
                nullifier_requests.insert(Nullifier::from_details_and_metadata(
                    details,
                    synced.committed.metadata(),
                ));
                lowest_commitment_block = lowest_commitment_block.min(block_num);
            }

            notes.push(PlannedNote { request: request.clone(), committed });
        }

        let mut blocks = BTreeMap::new();
        for block_num in commitment_blocks {
            blocks.insert(block_num, self.rpc_api.get_block_header_with_proof(block_num).await?);
        }

        // Only committed notes are asked about. A note that is not committed cannot have been
        // spent, and a spend cannot precede its commitment, so the earliest commitment among them
        // is the tightest lower bound the batch can share — and the bound is shared, so taking it
        // from anything looser (a sender's block hint, or the import's lookback floor) would drag
        // the search back towards genesis for every note in the batch.
        let spent = if nullifier_requests.is_empty() {
            BTreeMap::new()
        } else {
            self.rpc_api
                .get_nullifier_commit_heights(nullifier_requests, lowest_commitment_block, scan_to)
                .await?
                .into_iter()
                .filter_map(|(nullifier, height)| height.map(|height| (nullifier, height)))
                .collect()
        };

        Ok(ExpectedNoteImportPlan { notes, blocks, spent })
    }

    /// Writes a planned expected-note import: applies the state transitions the plan's fetched
    /// data implies, authenticates each commitment block against the partial MMR, clears the
    /// expected-note tag of the notes that settled, and upserts the records.
    ///
    /// Returns the details commitments of the notes that were stored. Notes the chain has not
    /// reported as committed keep their expected record; committed ones are stored only when the
    /// new information changed them.
    ///
    /// This writes to the store and must not run concurrently with anything else that does. It
    /// reaches the network only when a prefetched MMR proof predates the forest the store is now
    /// at, in which case that one block's proof is fetched again.
    pub(crate) async fn apply_expected_note_import(
        &mut self,
        plan: ExpectedNoteImportPlan,
    ) -> Result<Vec<NoteDetailsCommitment>, ClientError> {
        let ExpectedNoteImportPlan { notes, blocks, spent } = plan;
        if notes.is_empty() {
            return Ok(Vec::new());
        }

        // Read what is stored now rather than what was stored when the plan was made: anything
        // written in between has to be built on, not replaced. Matching by details commitment also
        // matches metadata-less records, whose `note_id` is NULL.
        let previous: BTreeMap<NoteDetailsCommitment, InputNoteRecord> = self
            .get_input_notes(NoteFilter::DetailsCommitments(
                notes.iter().map(|planned| planned.request.0.commitment()).collect(),
            ))
            .await?
            .into_iter()
            .map(|note| (note.details_commitment(), note))
            .collect();

        let mut note_records = vec![];
        let mut partial_mmr = self.get_current_partial_mmr().await?;
        let sync_height = self.get_sync_height().await?;

        for PlannedNote { request, committed } in notes {
            let (details, after_block_num, tag, metadata) = request;

            let previous_note = previous.get(&details.commitment()).cloned();
            // A note a local transaction is currently consuming can't be overwritten.
            ensure_not_processing(previous_note.as_ref())?;

            let mut record = previous_note.unwrap_or_else(|| {
                InputNoteRecord::new(
                    details,
                    NoteAttachments::empty(),
                    self.store.get_current_timestamp(),
                    ExpectedNoteState {
                        metadata,
                        after_block_num,
                        tag: Some(tag),
                    }
                    .into(),
                )
            });
            // Notes the chain has not reported as committed keep their expected record untouched.
            let Some(SyncedNote { committed: committed_note, content }) = committed else {
                note_records.push(record);
                continue;
            };

            // The plan's scan bound is read concurrently with the chain sync's own tip lookup, so
            // it can land a block or two above where the store ended up. The partial MMR cannot
            // authenticate a block it does not reach yet, so such a note stays expected and is
            // settled by the next sync rather than verified against a forest that predates it.
            if committed_note.block_num() > sync_height {
                note_records.push(record);
                continue;
            }

            let attachments = content
                .map(ResolvedNoteContent::into_attachments)
                .filter(|attachments| !attachments.is_empty());

            let block_num = committed_note.block_num();
            let block_header = self
                .store_authenticated_block(block_num, blocks.get(&block_num), &mut partial_mmr)
                .await?;

            let metadata = *committed_note.metadata();
            let mut note_changed = record
                .inclusion_proof_received(committed_note.inclusion_proof().clone(), metadata)?;

            if let Some(attachments) = attachments {
                note_changed |= record.attachments_received(attachments);
            }

            // `block_header_received` transitions the record's state, so it must always run.
            note_changed |= record.block_header_received(&block_header)?;

            // Once committed, the note no longer needs its expected-note tag.
            if note_changed {
                self.store
                    .remove_note_tag(NoteTagRecord::with_note_source(
                        metadata.tag(),
                        record.details_commitment(),
                    ))
                    .await?;
            }

            // A note the chain has already spent is recorded as consumed rather than left looking
            // consumable. Only the plan can tell: the forward nullifier sync searches above the
            // client's checkpoint, and a commitment found by the backward scan can sit below it.
            if let Some(nullifier) = record.nullifier()
                && let Some(block_height) = spent.get(&nullifier)
            {
                note_changed |= record.consumed_externally(nullifier, *block_height, None)?;
            }

            if note_changed {
                note_records.push(record);
            }
        }
        self.cache_partial_mmr(partial_mmr).await?;

        self.store_imported_notes(note_records).await
    }

    /// Checks whether the expected notes (identified by their details commitments and tags) have
    /// been committed on chain between `request_block_num` and `scan_to`, returning the matching
    /// synced notes keyed by details commitment.
    ///
    /// Expected notes have no metadata and thus no `NoteId`, so each committed note is matched by
    /// reconstructing the id from the committed metadata: `NoteId::new(details_commitment,
    /// metadata)`.
    async fn sync_expected_notes(
        &self,
        request_block_num: BlockNumber,
        scan_to: BlockNumber,
        // Expected notes' details commitments with their tags.
        expected_notes: Vec<(NoteDetailsCommitment, NoteTag)>,
    ) -> Result<BTreeMap<NoteDetailsCommitment, SyncedNote>, ClientError> {
        let sync_tags: BTreeSet<NoteTag> = expected_notes.iter().map(|(_, tag)| *tag).collect();

        let mut matched_notes = BTreeMap::new();

        // Notes expected only after a block the scan does not reach can't be committed within its
        // range: skip the lookup and let them stay expected until a future sync.
        if request_block_num > scan_to {
            return Ok(matched_notes);
        }

        let blocks = self
            .rpc_api
            .sync_notes_with_content(
                request_block_num,
                scan_to,
                &sync_tags,
                NoteContentFetch::AttachmentsOnly,
            )
            .await
            .map_err(ClientError::RpcError)?;

        for block in blocks {
            if block.block_header.block_num() > scan_to {
                break;
            }

            for sync_note in block.notes.into_values() {
                let committed = &sync_note.committed;
                let Some((commitment, _)) = expected_notes.iter().find(|(commitment, _)| {
                    NoteId::new(*commitment, committed.metadata()) == *committed.note_id()
                }) else {
                    continue;
                };

                matched_notes.insert(*commitment, sync_note);
            }
        }

        Ok(matched_notes)
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
