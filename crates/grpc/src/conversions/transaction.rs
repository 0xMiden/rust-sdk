use std::collections::BTreeMap;

use miden_client_core::errors::{RpcConversionError, RpcError};
use miden_client_core::rpc::domain::note::CommittedNote;
use miden_client_core::rpc::domain::transaction::{TransactionLogsInfo, TransactionRecord};
use miden_objects::DecodeMessageExt;
use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::note::{NoteHeader, NoteId, NoteInclusionProof};
use miden_protocol::transaction::{TransactionHeader, TransactionId, TransactionLogDetails};

use super::nullifier::nullifier_from_proto;
use crate::generated as proto;

// TRANSACTION RECORD
// ================================================================================================

impl TryFrom<proto::node::v1::TransactionRecord> for TransactionRecord {
    type Error = RpcError;

    fn try_from(value: proto::node::v1::TransactionRecord) -> Result<Self, Self::Error> {
        let block_num = value.block_num.into();
        let proto_header =
            value.header.ok_or(RpcConversionError::MissingFieldInProtobufRepresentation {
                entity: "TransactionRecord",
                field_name: "transaction_header",
            })?;

        let (transaction_header, output_notes, erased_output_notes) =
            convert_transaction_header(proto_header, value.output_note_proofs)?;

        let consumed_note_refs = value
            .consumed_note_refs
            .into_iter()
            .map(|r| {
                let nullifier =
                    nullifier_from_proto(r.nullifier.ok_or(RpcError::ExpectedDataMissing(
                        "consumed_note_ref.nullifier".into(),
                    ))?)?;
                let note_id: NoteId = r
                    .note_id
                    .ok_or(RpcError::ExpectedDataMissing("consumed_note_ref.note_id".into()))?
                    .decode_and_verify()?;
                Ok((nullifier, note_id))
            })
            .collect::<Result<Vec<_>, RpcError>>()?;

        Ok(Self::new(
            block_num,
            transaction_header,
            output_notes,
            erased_output_notes,
            consumed_note_refs,
        ))
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
        let (note_id, inclusion_proof): (NoteId, NoteInclusionProof) =
            proto_proof.decode_and_verify()?;
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

// TRANSACTION LOGS INFO
// ================================================================================================

impl TryFrom<proto::node::v1::GetTransactionLogsResponse> for TransactionLogsInfo {
    type Error = RpcError;

    fn try_from(value: proto::node::v1::GetTransactionLogsResponse) -> Result<Self, Self::Error> {
        let missing = |field: &str| {
            RpcError::ExpectedDataMissing(format!("GetTransactionLogsResponse.{field}"))
        };

        let transaction_id: TransactionId = value
            .transaction_id
            .ok_or_else(|| missing("transaction_id"))?
            .decode_and_verify()?;
        let account_id: AccountId = value
            .native_account_id
            .ok_or_else(|| missing("native_account_id"))?
            .decode_and_verify()?;
        let logs_commitment: Word =
            value.logs_commitment.ok_or_else(|| missing("logs_commitment"))?.try_into()?;
        let log_details: TransactionLogDetails =
            value.log_details.ok_or_else(|| missing("log_details"))?.decode_and_verify()?;

        // The response carries the commitment next to the logs. Both must describe the same logs.
        if log_details.commitment() != logs_commitment {
            return Err(RpcError::InvalidResponse(format!(
                "logs of transaction {transaction_id} do not match the commitment {logs_commitment}"
            )));
        }

        Ok(Self {
            transaction_id,
            account_id,
            block_num: value.block_num.into(),
            transaction_index: value.transaction_index,
            log_details,
        })
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client_core::errors::RpcError;
    use miden_client_core::rpc::domain::transaction::TransactionLogsInfo;
    use miden_protocol::Word;
    use miden_protocol::account::AccountId;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE;
    use miden_protocol::transaction::{
        LogTopic,
        TransactionId,
        TransactionLog,
        TransactionLogDetails,
        TransactionLogs,
    };

    use crate::generated as proto;

    fn account_id() -> AccountId {
        AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap()
    }

    fn transaction_id() -> TransactionId {
        TransactionId::from_raw(Word::from([5u32; 4]))
    }

    /// Returns two logs. The second log has no content.
    fn logs() -> TransactionLogs {
        let topic = LogTopic::from_name("miden::testing::topic");
        TransactionLogs::new(vec![
            TransactionLog::new(account_id(), topic, vec![Word::from([1u32, 2, 3, 4])]).unwrap(),
            TransactionLog::new(account_id(), topic, vec![]).unwrap(),
        ])
        .unwrap()
    }

    fn response(
        log_details: &TransactionLogDetails,
    ) -> proto::node::v1::GetTransactionLogsResponse {
        proto::node::v1::GetTransactionLogsResponse {
            transaction_id: Some(transaction_id().into()),
            native_account_id: Some(account_id().into()),
            block_num: 7,
            transaction_index: 2,
            logs_commitment: Some(log_details.commitment().into()),
            log_details: Some(log_details.into()),
        }
    }

    #[test]
    fn public_logs_response_converts() {
        let log_details = TransactionLogDetails::Public(logs());

        let info = TransactionLogsInfo::try_from(response(&log_details)).unwrap();

        assert_eq!(
            info,
            TransactionLogsInfo {
                transaction_id: transaction_id(),
                account_id: account_id(),
                block_num: BlockNumber::from(7u32),
                transaction_index: 2,
                log_details,
            }
        );
        assert_eq!(info.logs(), Some(&logs()));
        assert_eq!(info.logs_commitment(), logs().commitment());
    }

    #[test]
    fn private_logs_response_converts() {
        let log_details = TransactionLogDetails::Private(logs().commitment());

        let info = TransactionLogsInfo::try_from(response(&log_details)).unwrap();

        assert_eq!(info.log_details, log_details);
        assert_eq!(info.logs(), None);
        assert_eq!(info.logs_commitment(), logs().commitment());
    }

    #[test]
    fn response_with_a_commitment_that_does_not_match_the_logs_is_rejected() {
        for log_details in [
            TransactionLogDetails::Public(logs()),
            TransactionLogDetails::Private(logs().commitment()),
        ] {
            let mut response = response(&log_details);
            response.logs_commitment = Some(Word::from([9u32; 4]).into());

            let err = TransactionLogsInfo::try_from(response).unwrap_err();
            assert!(matches!(err, RpcError::InvalidResponse(_)), "{err:?}");
        }
    }

    #[test]
    fn response_with_a_missing_field_is_rejected() {
        let complete = response(&TransactionLogDetails::Public(logs()));

        let without_fields = [
            proto::node::v1::GetTransactionLogsResponse {
                transaction_id: None,
                ..complete.clone()
            },
            proto::node::v1::GetTransactionLogsResponse {
                native_account_id: None,
                ..complete.clone()
            },
            proto::node::v1::GetTransactionLogsResponse {
                logs_commitment: None,
                ..complete.clone()
            },
            proto::node::v1::GetTransactionLogsResponse { log_details: None, ..complete },
        ];

        for response in without_fields {
            let err = TransactionLogsInfo::try_from(response).unwrap_err();
            assert!(matches!(err, RpcError::ExpectedDataMissing(_)), "{err:?}");
        }
    }
}
