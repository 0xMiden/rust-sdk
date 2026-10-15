use std::string::{String, ToString};
use std::vec::Vec;

use miden_client::Word;
use miden_client::account::{StorageMapKey, StorageSlotName};
use miden_client::block::BlockHeader;
use miden_client::note::{NoteDetails, NoteTag, NoteUpdateTracker};
use miden_client::rpc::domain::account::AccountStorageRequirements;
use miden_client::sync::NoteTagRecord;
use miden_client::transaction::{
    ChainAnchor,
    ChainAnchorError,
    DiscardCause,
    ForeignAccount,
    TransactionDetails,
    TransactionRequest,
    TransactionRequestParts,
    TransactionResult,
    TransactionScriptTemplate,
    TransactionStatus,
    TransactionStoreUpdate,
};
use miden_client::utils::Serializable;
use miden_objects::DecodeMessageExt;
use miden_protocol::MAX_INPUT_NOTES_PER_TX;
use miden_protocol::crypto::merkle::mmr::PartialMmr;
use miden_protocol::note::{NoteId, NoteScript};
use miden_protocol::transaction::{PartialBlockchain, TransactionScript};

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, read_upstream, required};

impl ProtobufValue for TransactionDetails {
    type Message = proto::TransactionDetails;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            account_id: Some(self.account_id.into()),
            init_account_state: Some(self.init_account_state.into()),
            final_account_state: Some(self.final_account_state.into()),
            input_note_nullifiers: self
                .input_note_nullifiers
                .iter()
                .map(|nullifier| (*nullifier).into())
                .collect(),
            output_notes: Some((&self.output_notes).into()),
            block_num: Some(self.block_num.into()),
            submission_height: Some(self.submission_height.into()),
            expiration_block_num: Some(self.expiration_block_num.into()),
            creation_timestamp: self.creation_timestamp,
        }
    }

    fn from_proto(details: Self::Message) -> Result<Self, ProtoDecodeError> {
        const MESSAGE: &str = "transaction details";

        let input_note_nullifiers = details
            .input_note_nullifiers
            .into_iter()
            .map(Word::try_from)
            .collect::<Result<Vec<Word>, _>>()?;

        Ok(Self {
            account_id: required(details.account_id, MESSAGE, "account id")?.decode_and_verify()?,
            init_account_state: required(
                details.init_account_state,
                MESSAGE,
                "initial account state",
            )?
            .try_into()?,
            final_account_state: required(
                details.final_account_state,
                MESSAGE,
                "final account state",
            )?
            .try_into()?,
            input_note_nullifiers,
            output_notes: required(details.output_notes, MESSAGE, "output notes")?
                .decode_and_verify()?,
            block_num: required(details.block_num, MESSAGE, "block number")?.decode_and_verify()?,
            submission_height: required(details.submission_height, MESSAGE, "submission height")?
                .decode_and_verify()?,
            expiration_block_num: required(
                details.expiration_block_num,
                MESSAGE,
                "expiration block number",
            )?
            .decode_and_verify()?,
            creation_timestamp: details.creation_timestamp,
        })
    }
}

impl ProtobufValue for TransactionStatus {
    type Message = proto::TransactionStatus;

    fn to_proto(&self) -> Self::Message {
        use proto::transaction_status::{Committed, Discarded, Pending, Status};

        let status = match self {
            TransactionStatus::Pending => Status::Pending(Pending {}),
            TransactionStatus::Committed { block_number, commit_timestamp } => {
                Status::Committed(Committed {
                    block_number: Some((*block_number).into()),
                    commit_timestamp: *commit_timestamp,
                })
            },
            TransactionStatus::Discarded(cause) => Status::Discarded(Discarded {
                cause: proto::DiscardCause::from(*cause).into(),
            }),
        };

        Self::Message { status: Some(status) }
    }

    fn from_proto(status: Self::Message) -> Result<Self, ProtoDecodeError> {
        use proto::transaction_status::Status;

        const MESSAGE: &str = "transaction status";

        match required(status.status, MESSAGE, "variant")? {
            Status::Pending(_) => Ok(TransactionStatus::Pending),
            Status::Committed(committed) => Ok(TransactionStatus::Committed {
                block_number: required(committed.block_number, MESSAGE, "block number")?
                    .decode_and_verify()?,
                commit_timestamp: committed.commit_timestamp,
            }),
            Status::Discarded(discarded) => Ok(TransactionStatus::Discarded(
                proto::DiscardCause::try_from(discarded.cause)?.try_into()?,
            )),
        }
    }
}

impl From<DiscardCause> for proto::DiscardCause {
    fn from(cause: DiscardCause) -> Self {
        match cause {
            DiscardCause::Expired => Self::Expired,
            DiscardCause::InputConsumed => Self::InputConsumed,
            DiscardCause::DiscardedInitialState => Self::DiscardedInitialState,
            DiscardCause::Stale => Self::Stale,
            DiscardCause::Superseded => Self::Superseded,
        }
    }
}

/// The unspecified value is rejected, because the cause decides how the client reports a
/// transaction that can never commit.
impl TryFrom<proto::DiscardCause> for DiscardCause {
    type Error = ProtoDecodeError;

    fn try_from(cause: proto::DiscardCause) -> Result<Self, Self::Error> {
        match cause {
            proto::DiscardCause::Expired => Ok(Self::Expired),
            proto::DiscardCause::InputConsumed => Ok(Self::InputConsumed),
            proto::DiscardCause::DiscardedInitialState => Ok(Self::DiscardedInitialState),
            proto::DiscardCause::Stale => Ok(Self::Stale),
            proto::DiscardCause::Superseded => Ok(Self::Superseded),
            proto::DiscardCause::Unspecified => {
                Err(ProtoDecodeError::InvalidValue("discard cause is unspecified".to_string()))
            },
        }
    }
}

// TRANSACTION RESULT
// ================================================================================================

fn encode_future_note((details, tag): &(NoteDetails, NoteTag)) -> proto::FutureNote {
    proto::FutureNote {
        details: details.to_bytes(),
        tag: tag.as_u32(),
    }
}

fn decode_future_note(
    note: &proto::FutureNote,
) -> Result<(NoteDetails, NoteTag), ProtoDecodeError> {
    Ok((read_upstream(&note.details)?, NoteTag::from(note.tag)))
}

fn decode_future_notes(
    notes: &[proto::FutureNote],
) -> Result<Vec<(NoteDetails, NoteTag)>, ProtoDecodeError> {
    notes.iter().map(decode_future_note).collect()
}

impl ProtobufValue for TransactionResult {
    type Message = proto::TransactionResult;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            executed_transaction: self.executed_transaction().to_bytes(),
            future_notes: self.future_notes().iter().map(encode_future_note).collect(),
        }
    }

    fn from_proto(result: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(TransactionResult::new(
            read_upstream(&result.executed_transaction)?,
            decode_future_notes(&result.future_notes)?,
        ))
    }
}

impl ProtobufValue for TransactionStoreUpdate {
    type Message = proto::TransactionStoreUpdate;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            executed_transaction: self.executed_transaction().to_bytes(),
            submission_height: Some(self.submission_height().into()),
            future_notes: self.future_notes().iter().map(encode_future_note).collect(),
            note_updates: Some(self.note_updates().to_proto()),
            new_tags: self.new_tags().iter().map(ProtobufValue::to_proto).collect(),
        }
    }

    fn from_proto(update: Self::Message) -> Result<Self, ProtoDecodeError> {
        const MESSAGE: &str = "transaction store update";

        Ok(TransactionStoreUpdate::new(
            read_upstream(&update.executed_transaction)?,
            required(update.submission_height, MESSAGE, "submission height")?
                .decode_and_verify()?,
            NoteUpdateTracker::from_proto(required(update.note_updates, MESSAGE, "note updates")?)?,
            decode_future_notes(&update.future_notes)?,
            update
                .new_tags
                .into_iter()
                .map(NoteTagRecord::from_proto)
                .collect::<Result<Vec<_>, _>>()?,
        ))
    }
}

// TRANSACTION REQUEST
// ================================================================================================

impl ProtobufValue for AccountStorageRequirements {
    type Message = proto::AccountStorageRequirements;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            slots: self
                .inner()
                .iter()
                .map(|(name, keys)| proto::account_storage_requirements::Slot {
                    name: name.as_str().to_string(),
                    keys: keys.iter().map(|key| Word::from(*key).into()).collect(),
                })
                .collect(),
        }
    }

    fn from_proto(requirements: Self::Message) -> Result<Self, ProtoDecodeError> {
        let slots = requirements
            .slots
            .into_iter()
            .map(|slot| {
                let name = StorageSlotName::new(slot.name)
                    .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;
                let keys = slot
                    .keys
                    .into_iter()
                    .map(|key| Ok(StorageMapKey::new(key.try_into()?)))
                    .collect::<Result<Vec<_>, ProtoDecodeError>>()?;
                Ok((name, keys))
            })
            .collect::<Result<Vec<_>, ProtoDecodeError>>()?;
        Ok(AccountStorageRequirements::new(
            slots.iter().map(|(name, keys)| (name.clone(), keys)),
        ))
    }
}

impl ProtobufValue for ForeignAccount {
    type Message = proto::ForeignAccount;

    fn to_proto(&self) -> Self::Message {
        use proto::foreign_account::{Account, Public};

        let account = match self {
            ForeignAccount::Public(account_id, storage_requirements) => Account::Public(Public {
                account_id: Some((*account_id).into()),
                storage_requirements: Some(storage_requirements.to_proto()),
            }),
            ForeignAccount::Private(partial_account) => {
                Account::Private(partial_account.to_bytes())
            },
            ForeignAccount::Prefetched(inputs) => Account::Prefetched(inputs.to_bytes()),
        };
        Self::Message { account: Some(account) }
    }

    fn from_proto(account: Self::Message) -> Result<Self, ProtoDecodeError> {
        use proto::foreign_account::Account;

        const MESSAGE: &str = "foreign account";

        Ok(match required(account.account, MESSAGE, "account")? {
            Account::Public(public) => ForeignAccount::Public(
                required(public.account_id, MESSAGE, "account id")?.decode_and_verify()?,
                AccountStorageRequirements::from_proto(required(
                    public.storage_requirements,
                    MESSAGE,
                    "storage requirements",
                )?)?,
            ),
            Account::Private(partial_account) => {
                ForeignAccount::Private(read_upstream(&partial_account)?)
            },
            Account::Prefetched(inputs) => ForeignAccount::Prefetched(read_upstream(&inputs)?),
        })
    }
}

impl ProtobufValue for TransactionRequest {
    type Message = proto::TransactionRequest;

    fn to_proto(&self) -> Self::Message {
        use proto::transaction_request::{NoteArgs, ScriptTemplate, SendNotes};

        let script_template = self.script_template().as_ref().map(|template| match template {
            TransactionScriptTemplate::CustomScript(script) => {
                ScriptTemplate::CustomScript(script.to_proto())
            },
            TransactionScriptTemplate::SendNotes(notes) => ScriptTemplate::SendNotes(SendNotes {
                notes: notes.iter().map(Serializable::to_bytes).collect(),
            }),
        });

        Self::Message {
            block_numbers: self.block_numbers().iter().map(|block| (*block).into()).collect(),
            input_notes: self.input_notes().iter().map(Serializable::to_bytes).collect(),
            input_notes_args: self
                .input_notes_args()
                .iter()
                .map(|(note_id, args)| NoteArgs {
                    note_id: Some(note_id.as_word().into()),
                    args: args.map(Into::into),
                })
                .collect(),
            explicit_input_notes: self.explicit_input_notes().map(Serializable::to_bytes).collect(),
            script_template,
            expected_output_recipients: self
                .expected_output_recipients()
                .map(Serializable::to_bytes)
                .collect(),
            expected_future_notes: self.expected_future_notes().map(encode_future_note).collect(),
            advice_map: self.advice_map().to_bytes(),
            merkle_store: self.merkle_store().to_bytes(),
            foreign_accounts: self
                .foreign_accounts()
                .values()
                .map(ProtobufValue::to_proto)
                .collect(),
            expiration_delta: self.expiration_delta().map(u32::from),
            ignore_invalid_input_notes: self.ignore_invalid_input_notes(),
            script_arg: self.script_arg().map(Into::into),
            auth_arg: self.auth_arg().map(Into::into),
            fee_conversion_salt: self.fee_conversion_salt().map(Into::into),
            expected_ntx_scripts: self
                .expected_ntx_scripts()
                .iter()
                .map(ProtobufValue::to_proto)
                .collect(),
            account_code_upgrade: self.account_code_upgrade().map(Serializable::to_bytes),
        }
    }

    fn from_proto(request: Self::Message) -> Result<Self, ProtoDecodeError> {
        use proto::transaction_request::ScriptTemplate;

        let script_template = match request.script_template {
            None => None,
            Some(ScriptTemplate::CustomScript(script)) => Some(
                TransactionScriptTemplate::CustomScript(TransactionScript::from_proto(script)?),
            ),
            Some(ScriptTemplate::SendNotes(send_notes)) => {
                Some(TransactionScriptTemplate::SendNotes(
                    send_notes
                        .notes
                        .iter()
                        .map(|note| read_upstream(note))
                        .collect::<Result<Vec<_>, _>>()?,
                ))
            },
        };

        let input_notes_args = request
            .input_notes_args
            .into_iter()
            .map(|entry| {
                let note_id: Word = required(entry.note_id, "note args", "note id")?.try_into()?;
                let args = entry.args.map(Word::try_from).transpose()?;
                Ok((NoteId::from_raw(note_id), args))
            })
            .collect::<Result<Vec<_>, ProtoDecodeError>>()?;

        let expiration_delta = request
            .expiration_delta
            .map(|delta| {
                u16::try_from(delta).map_err(|_| {
                    ProtoDecodeError::InvalidValue(String::from(
                        "transaction request expiration delta does not fit in 16 bits",
                    ))
                })
            })
            .transpose()?;

        let parts = TransactionRequestParts {
            block_numbers: request
                .block_numbers
                .into_iter()
                .map(DecodeMessageExt::decode_and_verify)
                .collect::<Result<_, _>>()?,
            input_notes: request
                .input_notes
                .iter()
                .map(|note| read_upstream(note))
                .collect::<Result<_, _>>()?,
            input_notes_args,
            explicit_input_notes: request
                .explicit_input_notes
                .iter()
                .map(|note| read_upstream(note))
                .collect::<Result<_, _>>()?,
            script_template,
            expected_output_recipients: request
                .expected_output_recipients
                .iter()
                .map(|recipient| read_upstream(recipient))
                .collect::<Result<_, _>>()?,
            expected_future_notes: decode_future_notes(&request.expected_future_notes)?,
            advice_map: read_upstream(&request.advice_map)?,
            merkle_store: read_upstream(&request.merkle_store)?,
            foreign_accounts: request
                .foreign_accounts
                .into_iter()
                .map(ForeignAccount::from_proto)
                .collect::<Result<_, _>>()?,
            expiration_delta,
            ignore_invalid_input_notes: request.ignore_invalid_input_notes,
            script_arg: request.script_arg.map(Word::try_from).transpose()?,
            auth_arg: request.auth_arg.map(Word::try_from).transpose()?,
            fee_conversion_salt: request.fee_conversion_salt.map(Word::try_from).transpose()?,
            expected_ntx_scripts: request
                .expected_ntx_scripts
                .into_iter()
                .map(NoteScript::from_proto)
                .collect::<Result<_, _>>()?,
            account_code_upgrade: request
                .account_code_upgrade
                .map(|upgrade| read_upstream(&upgrade))
                .transpose()?,
        };

        TransactionRequest::from_parts(parts)
            .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
    }
}

// CHAIN ANCHOR
// ================================================================================================

impl ProtobufValue for ChainAnchor {
    type Message = proto::ChainAnchor;

    fn to_proto(&self) -> Self::Message {
        let chain = self.partial_blockchain();
        Self::Message {
            header: Some(self.header().to_proto()),
            partial_mmr: chain.mmr().to_bytes(),
            blocks: chain.block_headers().map(ProtobufValue::to_proto).collect(),
        }
    }

    /// Checks the anchor before it builds the partial blockchain. An anchor can come from another
    /// party, and [`PartialBlockchain::new`] panics on a tracked block whose sibling is absent.
    fn from_proto(anchor: Self::Message) -> Result<Self, ProtoDecodeError> {
        const MESSAGE: &str = "chain anchor";

        let header = BlockHeader::from_proto(required(anchor.header, MESSAGE, "header")?)?;
        let mmr: PartialMmr = read_upstream(&anchor.partial_mmr)?;

        // `ChainAnchor::new` checks the count too, but only after it opens every block. An early
        // check bounds the work that an oversized anchor causes.
        if anchor.blocks.len() > MAX_INPUT_NOTES_PER_TX {
            return Err(ProtoDecodeError::InvalidValue(
                ChainAnchorError::TooManyTrackedBlocks {
                    count: anchor.blocks.len(),
                    max: MAX_INPUT_NOTES_PER_TX,
                }
                .to_string(),
            ));
        }

        let blocks = anchor
            .blocks
            .into_iter()
            .map(BlockHeader::from_proto)
            .collect::<Result<Vec<_>, _>>()?;
        for block in &blocks {
            mmr.open(block.block_num().as_usize())
                .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;
        }
        let chain = PartialBlockchain::new(mmr, blocks)
            .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;

        ChainAnchor::new(header, chain)
            .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client::account::{AccountBuilder, AccountType, StorageMapKey, StorageSlotName};
    use miden_client::rpc::domain::account::AccountStorageRequirements;
    use miden_client::transaction::{
        ForeignAccount,
        TransactionRequest,
        TransactionRequestBuilder,
    };
    use miden_protocol::account::AccountId;
    use miden_protocol::account::auth::{AuthScheme, PublicKeyCommitment};
    use miden_protocol::asset::FungibleAsset;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::block::account_tree::AccountTree;
    use miden_protocol::note::{Note, NoteTag, NoteType};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        ACCOUNT_ID_SENDER,
    };
    use miden_protocol::transaction::{AccountInputs, InputNote};
    use miden_protocol::{EMPTY_WORD, Felt, Word};
    use miden_standards::account::auth::{Approver, AuthSingleSig};
    use miden_standards::note::P2idNote;
    use miden_standards::testing::account_component::MockAccountComponent;

    use crate::{decode, encode};

    /// The conversion is written field by field, so a request that sets every field catches a field
    /// that is dropped or read into the wrong place.
    #[test]
    fn transaction_request_round_trips() {
        let sender_id = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();
        let target_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();

        let mut notes: Vec<Note> = (0u32..7)
            .map(|i| {
                P2idNote::builder()
                    .sender(sender_id)
                    .target(target_id)
                    .assets(vec![FungibleAsset::new(faucet_id, 100 + u64::from(i)).unwrap()])
                    .note_type(NoteType::Private)
                    .serial_number(Word::from([i, 1, 2, 3]))
                    .build()
                    .unwrap()
                    .into()
            })
            .collect();

        let account = |seed: u8| {
            AccountBuilder::new([seed; 32])
                .with_component(MockAccountComponent::with_empty_slots())
                .with_component(AuthSingleSig::new(Approver::new(
                    PublicKeyCommitment::from(EMPTY_WORD),
                    AuthScheme::Falcon512Poseidon2,
                )))
                .account_type(AccountType::Private)
                .build_existing()
                .unwrap()
        };
        let private_account = account(1);
        let prefetched_account = account(2);
        let tree = AccountTree::with_entries([(
            prefetched_account.id(),
            prefetched_account.to_commitment(),
        )])
        .unwrap();
        let prefetched_inputs =
            AccountInputs::new((&prefetched_account).into(), tree.open(prefetched_account.id()));

        let request = TransactionRequestBuilder::new()
            .block_numbers([BlockNumber::from(1u32), BlockNumber::from(3u32)])
            .input_notes(vec![(notes.pop().unwrap(), Some(Word::from([5u32, 6, 7, 8])))])
            .explicit_input_notes(vec![(InputNote::unauthenticated(notes.pop().unwrap()), None)])
            .expected_output_recipients(vec![notes.pop().unwrap().recipient().clone()])
            .expected_future_notes(vec![(
                notes.pop().unwrap().into(),
                NoteTag::with_account_target(sender_id),
            )])
            .extend_advice_map([(Word::from([9u32, 9, 9, 9]), vec![Felt::from(1u32)])])
            .foreign_accounts([
                ForeignAccount::public(
                    target_id,
                    AccountStorageRequirements::new([(
                        StorageSlotName::new("demo::storage_slot").unwrap(),
                        &[StorageMapKey::new(Word::default())],
                    )]),
                )
                .unwrap(),
                ForeignAccount::private(&private_account).unwrap(),
                ForeignAccount::Prefetched(prefetched_inputs),
            ])
            .own_output_notes(vec![notes.pop().unwrap(), notes.pop().unwrap()])
            .script_arg(Word::from([1u32, 0, 0, 0]))
            .auth_arg(Word::from([2u32, 0, 0, 0]))
            .expected_ntx_scripts(vec![notes[0].recipient().script().clone()])
            .build()
            .unwrap();

        assert_eq!(decode::<TransactionRequest>(&encode(&request)).unwrap(), request);
    }
}
