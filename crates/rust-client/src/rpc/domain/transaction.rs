use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use miden_objects::DecodeMessageExt;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteHeader, NoteId, NoteInclusionProof, Nullifier};
use miden_protocol::transaction::{InputNoteCommitment, TransactionHeader};

use super::note::{CommittedNote, note_id_from_proto, note_inclusion_proof_from_proto};
use super::nullifier::nullifier_from_proto;
use crate::rpc::{RpcConversionError, RpcError, generated as proto};

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
    /// Output notes with inclusion proofs, as returned by the node's `SyncTransactions` response.
    /// Does not include erased notes.
    pub output_notes: Vec<CommittedNote>,
    /// Output notes that were erased by same-batch note erasure.
    pub erased_output_notes: Vec<NoteHeader>,
    /// Maps each consumed input note's nullifier to its note id, for public notes the node could
    /// resolve. Lets a client recover, by id, a consumed note it never tracked. Empty for
    /// private/unresolvable inputs.
    // TODO: perhaps we might want to rename this field (see
    // https://github.com/0xMiden/node/pull/2304#discussion_r3511308376)
    pub(crate) consumed_note_refs: Vec<(Nullifier, NoteId)>,
}

impl TransactionRecord {
    /// Returns the `(nullifier, note_id)` references of the public input notes this transaction
    /// consumed, letting a client fetch by id consumed notes it never tracked.
    ///
    /// Only yields references whose nullifier appears in the transaction header's input notes: a
    /// reference the node can't tie to an actually-consumed input is dropped, so a misbehaving node
    /// can't attribute an unrelated note to this transaction's account.
    pub fn trusted_consumed_note_refs(&self) -> impl Iterator<Item = (Nullifier, NoteId)> + '_ {
        let consumed_nullifiers: BTreeSet<Nullifier> = self
            .transaction_header
            .input_notes()
            .iter()
            .map(InputNoteCommitment::nullifier)
            .collect();
        self.consumed_note_refs
            .iter()
            .copied()
            .filter(move |(nullifier, _)| consumed_nullifiers.contains(nullifier))
    }
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

        let consumed_note_refs =
            value
                .consumed_note_refs
                .into_iter()
                .map(|r| {
                    let nullifier = nullifier_from_proto(r.nullifier.ok_or(
                        RpcError::ExpectedDataMissing("consumed_note_ref.nullifier".into()),
                    )?)?;
                    let note_id = note_id_from_proto(r.note_id.ok_or(
                        RpcError::ExpectedDataMissing("consumed_note_ref.note_id".into()),
                    )?)?;
                    Ok((nullifier, note_id))
                })
                .collect::<Result<Vec<_>, RpcError>>()?;

        Ok(Self {
            block_num,
            transaction_header,
            output_notes,
            erased_output_notes,
            consumed_note_refs,
        })
    }
}

/// Converts a proto `TransactionHeader` and its associated output note inclusion proofs into the
/// domain `TransactionHeader`, committed output notes, and erased note IDs.
///
/// The proto `TransactionHeader.output_notes` contains `NoteHeader`s for ALL output notes
/// (including erased ones). Inclusion proofs for committed notes are provided separately in
/// `output_note_proofs`. Notes present in `output_notes` but without a corresponding proof are
/// erased (created and consumed within the same batch).
fn convert_transaction_header(
    value: proto::transaction::TransactionHeader,
    output_note_proofs: Vec<proto::note::NoteInclusionProof>,
) -> Result<(TransactionHeader, Vec<CommittedNote>, Vec<NoteHeader>), RpcError> {
    let transaction_header: TransactionHeader = value.decode_and_build_unchecked()?;

    // Build a map of note_id to inclusion_proof from the separate proofs field.
    let mut proof_map: BTreeMap<NoteId, NoteInclusionProof> = BTreeMap::new();
    for proto_proof in output_note_proofs {
        let (note_id, inclusion_proof) = note_inclusion_proof_from_proto(proto_proof)?;
        proof_map.insert(note_id, inclusion_proof);
    }

    // Join: notes with a matching proof are committed; notes without are erased.
    let output_note_headers = transaction_header.output_notes();
    let mut committed_output_notes = Vec::with_capacity(proof_map.len());
    let mut erased_output_notes =
        Vec::with_capacity(output_note_headers.len().saturating_sub(proof_map.len()));

    for header in output_note_headers {
        let note_id = header.id();
        if let Some(proof) = proof_map.remove(&note_id) {
            committed_output_notes.push(CommittedNote::new(note_id, *header.metadata(), proof));
        } else {
            erased_output_notes.push(*header);
        }
    }

    Ok((transaction_header, committed_output_notes, erased_output_notes))
}
