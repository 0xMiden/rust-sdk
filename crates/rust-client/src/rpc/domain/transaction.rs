use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteHeader, NoteId, NoteInclusionProof, Nullifier};
use miden_protocol::transaction::{
    InputNoteCommitment,
    InputNotes,
    TransactionHeader,
    TransactionId,
};

use super::note::CommittedNote;
use crate::rpc::{RpcConversionError, RpcError, generated as proto};

// INTO TRANSACTION ID
// ================================================================================================

impl TryFrom<proto::primitives::Digest> for TransactionId {
    type Error = RpcConversionError;

    fn try_from(value: proto::primitives::Digest) -> Result<Self, Self::Error> {
        let word: Word = value.try_into()?;
        Ok(Self::from_raw(word))
    }
}

impl TryFrom<proto::transaction::TransactionId> for TransactionId {
    type Error = RpcConversionError;

    fn try_from(value: proto::transaction::TransactionId) -> Result<Self, Self::Error> {
        value
            .id
            .ok_or(RpcConversionError::MissingFieldInProtobufRepresentation {
                entity: "TransactionId",
                field_name: "id",
            })?
            .try_into()
    }
}

impl From<TransactionId> for proto::transaction::TransactionId {
    fn from(value: TransactionId) -> Self {
        Self { id: Some(value.as_word().into()) }
    }
}

// TRANSACTION RECORD
// ================================================================================================

/// Contains information about a transaction that got included in the chain at a specific block
/// number.
#[derive(Debug, Clone)]
pub struct TransactionRecord {
    /// Block number in which the transaction was included.
    pub block_num: BlockNumber,
    /// A transaction header.
    pub transaction_header: TransactionHeader,
    /// Output notes with inclusion proofs, as returned by the node's `SyncTransactions`
    /// response. Does not include erased notes.
    pub output_notes: Vec<CommittedNote>,
    /// Output notes that were erased by same-batch note erasure.
    pub erased_output_notes: Vec<NoteHeader>,
}

impl TryFrom<proto::rpc::TransactionRecord> for TransactionRecord {
    type Error = RpcError;

    fn try_from(value: proto::rpc::TransactionRecord) -> Result<Self, Self::Error> {
        let block_num = value.block_num.into();
        let proto_header =
            value.header.ok_or(RpcConversionError::MissingFieldInProtobufRepresentation {
                entity: "TransactionRecord",
                field_name: "transaction_header",
            })?;

        let (transaction_header, output_notes, erased_output_notes) =
            convert_transaction_header(proto_header, value.output_note_proofs)?;

        Ok(Self {
            block_num,
            transaction_header,
            output_notes,
            erased_output_notes,
        })
    }
}

/// Converts a proto `TransactionHeader` and its associated output note inclusion proofs
/// into the domain `TransactionHeader`, committed output notes, and erased note IDs.
///
/// The proto `TransactionHeader.output_notes` contains `NoteHeader`s for ALL output notes
/// (including erased ones). Inclusion proofs for committed notes are provided separately in
/// `output_note_proofs`. Notes present in `output_notes` but without a corresponding proof
/// are erased (created and consumed within the same batch).
fn convert_transaction_header(
    value: proto::transaction::TransactionHeader,
    output_note_proofs: Vec<proto::note::NoteInclusionInBlockProof>,
) -> Result<(TransactionHeader, Vec<CommittedNote>, Vec<NoteHeader>), RpcError> {
    let account_id =
        value
            .account_id
            .ok_or(RpcConversionError::MissingFieldInProtobufRepresentation {
                entity: "TransactionHeader",
                field_name: "account_id",
            })?;

    let initial_state_commitment = value.initial_state_commitment.ok_or(
        RpcConversionError::MissingFieldInProtobufRepresentation {
            entity: "TransactionHeader",
            field_name: "initial_state_commitment",
        },
    )?;

    let final_state_commitment = value.final_state_commitment.ok_or(
        RpcConversionError::MissingFieldInProtobufRepresentation {
            entity: "TransactionHeader",
            field_name: "final_state_commitment",
        },
    )?;

    let note_commitments = value
        .input_notes
        .into_iter()
        .map(|d| {
            let word: Word = d
                .nullifier
                .ok_or(RpcError::ExpectedDataMissing("nullifier".into()))?
                .try_into()
                .map_err(|e: RpcConversionError| RpcError::InvalidResponse(e.to_string()))?;
            Ok(InputNoteCommitment::from(Nullifier::from_raw(word)))
        })
        .collect::<Result<Vec<_>, RpcError>>()?;
    let input_notes = InputNotes::new_unchecked(note_commitments);

    // Parse all output note headers from the transaction header.
    let output_note_headers: Vec<NoteHeader> = value
        .output_notes
        .into_iter()
        .map(|proto_header| {
            proto_header
                .try_into()
                .map_err(|e: RpcConversionError| RpcError::InvalidResponse(e.to_string()))
        })
        .collect::<Result<Vec<_>, RpcError>>()?;

    // Build a map of note_id to inclusion_proof from the separate proofs field.
    let mut proof_map: BTreeMap<NoteId, NoteInclusionProof> = BTreeMap::new();
    for mut proto_proof in output_note_proofs {
        let note_id: NoteId = proto_proof
            .note_id
            .take()
            .ok_or(RpcError::ExpectedDataMissing("output_note_proofs.note_id".into()))?
            .try_into()
            .map_err(|e: RpcConversionError| RpcError::InvalidResponse(e.to_string()))?;
        let inclusion_proof: NoteInclusionProof = proto_proof
            .try_into()
            .map_err(|e: RpcConversionError| RpcError::InvalidResponse(e.to_string()))?;
        proof_map.insert(note_id, inclusion_proof);
    }

    // Join: notes with a matching proof are committed; notes without are erased.
    let mut committed_output_notes = Vec::with_capacity(proof_map.len());
    let mut erased_output_notes =
        Vec::with_capacity(output_note_headers.len().saturating_sub(proof_map.len()));

    for header in &output_note_headers {
        let note_id = header.id();
        if let Some(proof) = proof_map.remove(&note_id) {
            committed_output_notes.push(CommittedNote::new(note_id, *header.metadata(), proof));
        } else {
            erased_output_notes.push(*header);
        }
    }

    let transaction_header = TransactionHeader::new(
        account_id.try_into()?,
        initial_state_commitment.try_into()?,
        final_state_commitment.try_into()?,
        input_notes,
        output_note_headers,
    );
    Ok((transaction_header, committed_output_notes, erased_output_notes))
}
