use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec::Vec;

use miden_objects::DecodeMessageExt;
use miden_protocol::block::BlockHeader;
use miden_protocol::crypto::SequentialCommit;
use miden_protocol::crypto::merkle::MerklePath;
use miden_protocol::note::{
    NoteAttachment,
    NoteAttachmentHeader,
    NoteAttachmentScheme,
    NoteAttachments,
    NoteId,
    NoteInclusionProof,
    NoteMetadata,
    NoteType,
    PartialNoteMetadata,
};
use miden_protocol::{Felt, Word};

use super::MissingFieldHelper;
use crate::rpc::domain::note::{CommittedNote, FetchedNote, SyncNotesBlock};
use crate::rpc::{RpcConversionError, RpcError, generated as proto};

/// Reads a note ID off the wire. A free function because both types are foreign, so there can be no
/// `TryFrom` impl.
pub(super) fn note_id_from_proto(value: proto::note::NoteId) -> Result<NoteId, RpcConversionError> {
    Ok(value.decode_and_verify()?)
}

/// Reads a note inclusion proof and the ID of the note it proves.
pub(super) fn note_inclusion_proof_from_proto(
    value: proto::note::NoteInclusionProof,
) -> Result<(NoteId, NoteInclusionProof), RpcConversionError> {
    Ok(value.decode_and_verify()?)
}

/// Aggregates individual attachment commitments into the note's attachments commitment.
///
/// The element layout mirrors [`NoteAttachments`]' own sequential commitment, so this yields the
/// same value as the full attachments would, which is what lets commitment-only records work.
struct AttachmentCommitments<'a>(&'a [Word]);

impl SequentialCommit for AttachmentCommitments<'_> {
    type Commitment = Word;

    fn to_elements(&self) -> Vec<Felt> {
        let mut elements = Vec::with_capacity(self.0.len() * miden_protocol::WORD_SIZE);
        for commitment in self.0 {
            elements.extend_from_slice(commitment.as_elements());
        }
        elements
    }
}

/// A single attachment as a `SyncNotes` record reports it: verbatim when the node sent its content,
/// commitment-only otherwise.
#[derive(Debug)]
enum ReportedAttachment {
    /// The attachment arrived verbatim, so its content is known.
    Full(NoteAttachment),
    /// The attachment arrived as its commitment only.
    Commitment(Word),
}

impl ReportedAttachment {
    /// Returns the attachment's commitment.
    fn to_commitment(&self) -> Word {
        match self {
            Self::Full(attachment) => attachment.to_commitment(),
            Self::Commitment(commitment) => *commitment,
        }
    }
}

/// The attachments of a note as a `SyncNotes` record reports them. Both variants determine the
/// note's attachments commitment exactly, but only one carries the content.
#[derive(Debug)]
enum ReportedAttachments {
    /// Every attachment arrived verbatim, so the content is known and needs no fetching.
    Full(NoteAttachments),
    /// At least one attachment arrived as a commitment only. Holds one commitment per attachment,
    /// enough to rebuild the note's metadata but not its content.
    Commitments(Vec<Word>),
}

impl ReportedAttachments {
    /// Aggregates the per-attachment reports of one record. The content is reconstructible only as
    /// a whole set, so a single commitment-only attachment demotes the entire report to
    /// commitments.
    fn from_reports(reports: Vec<ReportedAttachment>) -> Result<Self, RpcConversionError> {
        let commitments: Vec<Word> =
            reports.iter().map(ReportedAttachment::to_commitment).collect();
        let contents: Option<Vec<NoteAttachment>> = reports
            .into_iter()
            .map(|report| match report {
                ReportedAttachment::Full(attachment) => Some(attachment),
                ReportedAttachment::Commitment(_) => None,
            })
            .collect();

        match contents {
            Some(contents) => {
                let attachments = NoteAttachments::new(contents).map_err(|err| {
                    RpcConversionError::InvalidField(format!("attachments: {err}"))
                })?;
                Ok(Self::Full(attachments))
            },
            None => Ok(Self::Commitments(commitments)),
        }
    }

    /// Returns the note's attachments commitment.
    fn to_commitment(&self) -> Word {
        match self {
            Self::Full(attachments) => attachments.to_commitment(),
            Self::Commitments(commitments) => AttachmentCommitments(commitments).to_commitment(),
        }
    }

    /// Consumes the report and returns the content, when the record carried all of it.
    fn into_content(self) -> Option<NoteAttachments> {
        match self {
            Self::Full(attachments) => Some(attachments),
            Self::Commitments(_) => None,
        }
    }
}

/// The note metadata reconstructed from a `SyncNotes` record, together with what that record
/// reported about the note's attachments.
#[derive(Debug)]
struct SyncNoteMetadata {
    /// The note's full metadata, equal to what the on-chain note commits to.
    metadata: NoteMetadata,
    /// What the record reported about the note's attachments.
    attachments: ReportedAttachments,
}

impl TryFrom<proto::rpc::NoteSyncMetadata> for SyncNoteMetadata {
    type Error = RpcConversionError;

    fn try_from(value: proto::rpc::NoteSyncMetadata) -> Result<Self, Self::Error> {
        // The sync record spreads the canonical metadata fields over its own message, so they are
        // gathered back into that message and verified as a whole.
        let partial_metadata: PartialNoteMetadata = proto::note::PartialNoteMetadata {
            version: value.version,
            sender: value.sender,
            note_type: value.note_type,
            tag: value.tag,
        }
        .decode_and_verify()?;

        if value.attachments.len() > NoteAttachments::MAX_COUNT {
            return Err(RpcConversionError::InvalidField(format!(
                "attachments length {} exceeds NoteAttachments::MAX_COUNT",
                value.attachments.len(),
            )));
        }

        let mut attachment_headers = [NoteAttachmentHeader::absent(); NoteAttachments::MAX_COUNT];
        let mut reports = Vec::with_capacity(value.attachments.len());

        for (slot, attachment) in value.attachments.into_iter().enumerate() {
            let raw_scheme = u16::try_from(attachment.scheme).map_err(|_| {
                RpcConversionError::InvalidField(format!(
                    "attachments[{slot}].scheme={} does not fit in u16",
                    attachment.scheme,
                ))
            })?;
            let scheme = NoteAttachmentScheme::new(raw_scheme).map_err(|err| {
                RpcConversionError::InvalidField(format!("attachments[{slot}].scheme: {err}"))
            })?;
            attachment_headers[slot] = NoteAttachmentHeader::new(scheme);

            let payload = attachment.payload.ok_or_else(|| {
                proto::rpc::NoteSyncAttachment::missing_field(stringify!(payload))
            })?;
            // An attachment that fits in a single word is sent verbatim, so it can be rebuilt in
            // full. A larger one is sent as a commitment to keep the sync response bounded. The
            // node may send a commitment even for a single-word one, so the choice is read off the
            // payload variant and never inferred from a word count.
            let report = match payload {
                proto::rpc::note_sync_attachment::Payload::Value(value) => {
                    ReportedAttachment::Full(NoteAttachment::with_word(
                        scheme,
                        Word::try_from(value)?,
                    ))
                },
                proto::rpc::note_sync_attachment::Payload::Commitment(commitment) => {
                    ReportedAttachment::Commitment(Word::try_from(commitment)?)
                },
            };
            reports.push(report);
        }

        let attachments = ReportedAttachments::from_reports(reports)?;

        Ok(SyncNoteMetadata {
            metadata: NoteMetadata::from_parts(
                partial_metadata,
                attachment_headers,
                attachments.to_commitment(),
            ),
            attachments,
        })
    }
}

// SYNC NOTES BLOCK
// ================================================================================================

impl TryFrom<proto::rpc::sync_notes_response::NoteSyncBlock> for SyncNotesBlock {
    type Error = RpcError;

    fn try_from(
        block: proto::rpc::sync_notes_response::NoteSyncBlock,
    ) -> Result<Self, Self::Error> {
        let block_header: BlockHeader = block
            .block_header
            .ok_or(proto::rpc::SyncNotesResponse::missing_field(stringify!(blocks.block_header)))?
            .decode_and_build_unchecked()?;

        let mmr_path: MerklePath = block
            .mmr_path
            .ok_or(proto::rpc::SyncNotesResponse::missing_field(stringify!(blocks.mmr_path)))?
            .decode_and_verify()?;

        let notes: BTreeMap<NoteId, CommittedNote> = block
            .notes
            .into_iter()
            .map(|n| {
                let note = CommittedNote::try_from(n)?;
                Ok((*note.note_id(), note))
            })
            .collect::<Result<_, RpcConversionError>>()?;

        Ok(SyncNotesBlock { block_header, mmr_path, notes })
    }
}

// COMMITTED NOTE
// ================================================================================================

impl TryFrom<proto::rpc::NoteSyncRecord> for CommittedNote {
    type Error = RpcConversionError;

    fn try_from(note: proto::rpc::NoteSyncRecord) -> Result<Self, Self::Error> {
        let proto_metadata = note
            .metadata
            .ok_or(proto::rpc::SyncNotesResponse::missing_field(stringify!(notes.metadata)))?;
        let SyncNoteMetadata { metadata, attachments } = proto_metadata.try_into()?;

        let proto_inclusion_proof = note.inclusion_proof.ok_or(
            proto::rpc::SyncNotesResponse::missing_field(stringify!(notes.inclusion_proof)),
        )?;

        let (note_id, inclusion_proof) = note_inclusion_proof_from_proto(proto_inclusion_proof)?;

        let committed = CommittedNote::new(note_id, metadata, inclusion_proof);

        match attachments.into_content() {
            Some(attachments) => committed.with_attachments(attachments),
            None => Ok(committed),
        }
    }
}

// FETCHED NOTE
// ================================================================================================

impl TryFrom<proto::rpc::CommittedNote> for FetchedNote {
    type Error = RpcConversionError;

    fn try_from(value: proto::rpc::CommittedNote) -> Result<Self, Self::Error> {
        let proto_inclusion_proof = value
            .inclusion_proof
            .ok_or_else(|| proto::rpc::CommittedNote::missing_field(stringify!(inclusion_proof)))?;
        let (note_id, inclusion_proof) = note_inclusion_proof_from_proto(proto_inclusion_proof)?;

        let note = value
            .note
            .ok_or_else(|| proto::rpc::CommittedNote::missing_field(stringify!(note)))?;

        let partial_metadata: PartialNoteMetadata = note
            .metadata
            .ok_or_else(|| proto::rpc::CommittedNote::missing_field(stringify!(note.metadata)))?
            .decode_and_verify()?;

        // The note type decides which variant the response describes. The details are checked
        // against it, since a note is not usable when the two disagree.
        match partial_metadata.note_type() {
            NoteType::Public => {
                if note.note_details.is_none() {
                    return Err(RpcConversionError::InvalidField(format!(
                        "no note details were returned for public note {note_id}"
                    )));
                }

                Ok(FetchedNote::Public(note.decode_and_verify()?, inclusion_proof))
            },
            NoteType::Private => {
                if note.note_details.is_some() {
                    return Err(RpcConversionError::InvalidField(format!(
                        "note details were returned for private note {note_id}"
                    )));
                }

                let attachments: NoteAttachments = note
                    .note_attachments
                    .ok_or_else(|| {
                        proto::rpc::CommittedNote::missing_field(stringify!(note.note_attachments))
                    })?
                    .decode_and_verify()?;
                let metadata = NoteMetadata::new(partial_metadata, &attachments);

                Ok(FetchedNote::Private(note_id, metadata, attachments, inclusion_proof))
            },
        }
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::account::{AccountId, AccountIdVersion, AccountType, AssetCallbackFlag};
    use miden_protocol::block::BlockNumber;
    use miden_protocol::crypto::merkle::SparseMerklePath;
    use miden_protocol::note::{NoteAssets, NoteDetails, NoteRecipient, NoteStorage, NoteTag};
    use miden_standards::code_builder::CodeBuilder;

    use super::*;
    use crate::rpc::domain::note::SyncedNote;

    fn sender() -> AccountId {
        AccountId::dummy(
            [1; 15],
            AccountIdVersion::Version1,
            AccountType::Public,
            AssetCallbackFlag::Disabled,
        )
    }

    fn single_word_attachment(scheme: u16, word: u32) -> NoteAttachment {
        NoteAttachment::with_word(
            NoteAttachmentScheme::new(scheme).unwrap(),
            Word::from([word, word, word, word]),
        )
    }

    fn multi_word_attachment(scheme: u16) -> NoteAttachment {
        NoteAttachment::with_words(
            NoteAttachmentScheme::new(scheme).unwrap(),
            vec![Word::from([5u32, 6, 7, 8]), Word::from([9u32, 10, 11, 12])],
        )
        .unwrap()
    }

    /// Builds the sync record a node sends for `attachments`, then decodes it the way
    /// [`CommittedNote`] does.
    fn decode_sync_metadata(attachments: &NoteAttachments) -> SyncNoteMetadata {
        sync_metadata(sync_attachments(attachments)).try_into().unwrap()
    }

    fn bare_committed_note(metadata: NoteMetadata) -> CommittedNote {
        let path = SparseMerklePath::from_parts(0, Vec::new()).unwrap();
        let inclusion_proof =
            NoteInclusionProof::new(BlockNumber::GENESIS, 0, path).expect("index 0 is in range");

        CommittedNote::new(NoteId::from_raw(Word::empty()), metadata, inclusion_proof)
    }

    fn committed_note(decoded: SyncNoteMetadata) -> CommittedNote {
        let committed = bare_committed_note(decoded.metadata);

        match decoded.attachments.into_content() {
            Some(attachments) => committed.with_attachments(attachments).unwrap(),
            None => committed,
        }
    }

    /// Encodes attachments the way the node does in a sync response: single-word attachments carry
    /// their value, larger ones only their commitment.
    fn sync_attachments(attachments: &NoteAttachments) -> Vec<proto::rpc::NoteSyncAttachment> {
        attachments
            .iter()
            .map(|attachment| {
                let payload = if attachment.num_words() == 1 {
                    proto::rpc::note_sync_attachment::Payload::Value(
                        attachment.content().as_words()[0].into(),
                    )
                } else {
                    proto::rpc::note_sync_attachment::Payload::Commitment(
                        attachment.to_commitment().into(),
                    )
                };

                proto::rpc::NoteSyncAttachment {
                    scheme: u32::from(attachment.attachment_scheme().as_u16()),
                    payload: Some(payload),
                }
            })
            .collect()
    }

    fn sync_metadata(
        attachments: Vec<proto::rpc::NoteSyncAttachment>,
    ) -> proto::rpc::NoteSyncMetadata {
        proto::rpc::NoteSyncMetadata {
            sender: Some(sender().into()),
            note_type: proto::note::NoteType::from(NoteType::Private) as i32,
            tag: 7,
            attachments,
            version: proto::note::NoteVersion::V1 as i32,
        }
    }

    #[test]
    fn sync_metadata_reconstructs_metadata_with_mixed_attachments() {
        let attachments =
            NoteAttachments::new(vec![single_word_attachment(42, 1), multi_word_attachment(100)])
                .unwrap();

        let expected = NoteMetadata::new(
            PartialNoteMetadata::new(sender(), NoteType::Private).with_tag(NoteTag::new(7)),
            &attachments,
        );

        assert_eq!(decode_sync_metadata(&attachments).metadata, expected);
    }

    #[test]
    fn sync_metadata_reconstructs_metadata_without_attachments() {
        let attachments = NoteAttachments::empty();
        let expected = NoteMetadata::new(
            PartialNoteMetadata::new(sender(), NoteType::Private).with_tag(NoteTag::new(7)),
            &attachments,
        );

        let decoded: SyncNoteMetadata = sync_metadata(Vec::new()).try_into().unwrap();

        assert_eq!(decoded.metadata, expected);
    }

    /// A record whose every attachment fits in a single word describes the note's attachments in
    /// full, so no `GetNotesById` request is needed to obtain them.
    #[test]
    fn sync_metadata_reports_attachments_sent_verbatim() {
        let attachments = NoteAttachments::new(vec![
            single_word_attachment(42, 1),
            single_word_attachment(64, 2),
        ])
        .unwrap();

        let decoded = decode_sync_metadata(&attachments);

        let committed = committed_note(decoded);
        assert_eq!(committed.attachments(), Some(&attachments));
        assert!(!committed.needs_attachment_fetch());
    }

    /// A full set leaves no trailing slot absent, so it is the only case where the positional
    /// header fill writes every slot.
    #[test]
    fn sync_metadata_reconstructs_a_full_attachment_set() {
        let attachments = NoteAttachments::new(
            (0..NoteAttachments::MAX_COUNT)
                .map(|i| {
                    let scheme = u16::try_from(i).unwrap() + 42;
                    single_word_attachment(scheme, u32::try_from(i).unwrap() + 1)
                })
                .collect(),
        )
        .unwrap();

        let expected = NoteMetadata::new(
            PartialNoteMetadata::new(sender(), NoteType::Private).with_tag(NoteTag::new(7)),
            &attachments,
        );
        let decoded = decode_sync_metadata(&attachments);

        assert_eq!(decoded.metadata, expected);
        let committed = committed_note(decoded);
        assert_eq!(committed.attachments(), Some(&attachments));
        assert!(!committed.needs_attachment_fetch());
    }

    /// A note with no attachments has nothing to fetch, and its (empty) attachments are known.
    #[test]
    fn sync_metadata_reports_empty_attachments() {
        let decoded: SyncNoteMetadata = sync_metadata(Vec::new()).try_into().unwrap();

        let committed = committed_note(decoded);
        assert_eq!(committed.attachments(), Some(&NoteAttachments::empty()));
        assert!(!committed.needs_attachment_fetch());
    }

    /// One attachment sent as a commitment withholds the whole set, since the attachments can only
    /// be rebuilt as a whole.
    #[test]
    fn sync_metadata_withholds_partially_reported_attachments() {
        let attachments =
            NoteAttachments::new(vec![single_word_attachment(42, 1), multi_word_attachment(100)])
                .unwrap();

        let decoded = decode_sync_metadata(&attachments);

        assert!(matches!(decoded.attachments, ReportedAttachments::Commitments(_)));
        assert!(committed_note(decoded).needs_attachment_fetch());
    }

    /// The node may send a commitment even for a single-word attachment, so availability follows
    /// the payload variant alone, never a word count.
    #[test]
    fn sync_metadata_withholds_single_word_attachment_sent_as_commitment() {
        let attachment = single_word_attachment(42, 1);
        let proto_attachments = vec![proto::rpc::NoteSyncAttachment {
            scheme: u32::from(attachment.attachment_scheme().as_u16()),
            payload: Some(proto::rpc::note_sync_attachment::Payload::Commitment(
                attachment.to_commitment().into(),
            )),
        }];
        let attachments = NoteAttachments::new(vec![attachment]).unwrap();

        let decoded: SyncNoteMetadata = sync_metadata(proto_attachments).try_into().unwrap();

        // The metadata is still reconstructed exactly, only the content is missing.
        assert_eq!(
            decoded.metadata,
            NoteMetadata::new(
                PartialNoteMetadata::new(sender(), NoteType::Private).with_tag(NoteTag::new(7)),
                &attachments,
            )
        );
        assert!(matches!(decoded.attachments, ReportedAttachments::Commitments(_)));
        assert!(committed_note(decoded).needs_attachment_fetch());
    }

    #[test]
    fn sync_metadata_rejects_too_many_attachments() {
        let attachment = proto::rpc::NoteSyncAttachment {
            scheme: 42,
            payload: Some(proto::rpc::note_sync_attachment::Payload::Value(Word::empty().into())),
        };
        let attachments = vec![attachment; NoteAttachments::MAX_COUNT + 1];

        let err = SyncNoteMetadata::try_from(sync_metadata(attachments)).unwrap_err();

        assert!(matches!(err, RpcConversionError::InvalidField(_)), "got {err:?}");
    }

    #[test]
    fn sync_metadata_rejects_reserved_absent_scheme() {
        let attachments = vec![proto::rpc::NoteSyncAttachment {
            scheme: 0,
            payload: Some(proto::rpc::note_sync_attachment::Payload::Value(Word::empty().into())),
        }];

        let err = SyncNoteMetadata::try_from(sync_metadata(attachments)).unwrap_err();

        assert!(matches!(err, RpcConversionError::InvalidField(_)), "got {err:?}");
    }

    #[test]
    fn sync_metadata_rejects_missing_attachment_payload() {
        let attachments = vec![proto::rpc::NoteSyncAttachment { scheme: 42, payload: None }];

        let err = SyncNoteMetadata::try_from(sync_metadata(attachments)).unwrap_err();

        assert!(
            matches!(err, RpcConversionError::MissingFieldInProtobufRepresentation { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn sync_metadata_rejects_an_unusable_note_version() {
        for version in [proto::note::NoteVersion::Unspecified as i32, 999] {
            let mut wire = sync_metadata(Vec::new());
            wire.version = version;

            let err = SyncNoteMetadata::try_from(wire).unwrap_err();

            assert!(matches!(err, RpcConversionError::CanonicalConversion(_)), "got {err:?}");
        }
    }

    #[test]
    fn synced_note_rejects_a_body_for_a_private_note() {
        let decoded: SyncNoteMetadata = sync_metadata(Vec::new()).try_into().unwrap();
        let committed = committed_note(decoded);

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
        let attachments = NoteAttachments::new(vec![multi_word_attachment(100)]).unwrap();
        let committed = committed_note(decode_sync_metadata(&attachments));

        let err = SyncedNote::new(committed, None, NoteAttachments::empty()).unwrap_err();

        assert!(matches!(err, RpcError::InvalidResponse(_)), "got {err:?}");
    }
}
