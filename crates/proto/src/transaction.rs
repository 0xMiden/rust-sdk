use std::collections::BTreeMap;
use std::string::ToString;
use std::vec::Vec;

use miden_client::Word;
use miden_client::rpc::domain::account::AccountStorageRequirements;
use miden_client::transaction::{
    AdviceMap,
    DiscardCause,
    ForeignAccount,
    NoteArgs,
    TransactionDetails,
    TransactionRequest,
    TransactionRequestBuilder,
    TransactionScriptTemplate,
    TransactionStatus,
};
use miden_objects::DecodeMessageExt;
use miden_protocol::account::{StorageMapKey, StorageSlotName};
use miden_protocol::crypto::merkle::store::MerkleStore;
use miden_protocol::note::{Note, NoteDetails, NoteId, NoteRecipient, NoteTag, PartialNote};
use miden_protocol::transaction::{InputNote, TransactionScript};

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, required};

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

/// The request is built again with [`TransactionRequestBuilder`], so decoding applies the same
/// checks as building a request.
impl ProtobufValue for TransactionRequest {
    type Message = proto::TransactionRequest;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            block_numbers: self
                .block_numbers()
                .iter()
                .map(|block_num| (*block_num).into())
                .collect(),
            input_notes: self.input_notes().iter().map(Into::into).collect(),
            input_notes_args: self
                .input_notes_args()
                .iter()
                .map(|(note_id, note_args)| proto::NoteArgsEntry {
                    note_id: Some(note_id.into()),
                    note_args: note_args.map(Into::into),
                })
                .collect(),
            explicit_input_notes: self.explicit_input_notes().values().map(Into::into).collect(),
            script_template: self.script_template().as_ref().map(Into::into),
            expected_output_recipients: self.expected_output_recipients().map(Into::into).collect(),
            expected_future_notes: self
                .expected_future_notes()
                .map(|(details, tag)| proto::ExpectedFutureNote {
                    details: Some(details.into()),
                    tag: (*tag).into(),
                })
                .collect(),
            advice_map: Some(self.advice_map().into()),
            merkle_store: Some(self.merkle_store().into()),
            foreign_accounts: self.foreign_accounts().values().map(Into::into).collect(),
            expiration_delta: self.expiration_delta().map(u32::from),
            ignore_invalid_input_notes: self.ignore_invalid_input_notes(),
            script_arg: self.script_arg().map(Into::into),
            auth_arg: self.auth_arg().map(Into::into),
            fee_conversion_salt: self.fee_conversion_salt().map(Into::into),
            expected_ntx_scripts: self.expected_ntx_scripts().iter().map(Into::into).collect(),
        }
    }

    fn from_proto(request: Self::Message) -> Result<Self, ProtoDecodeError> {
        const MESSAGE: &str = "transaction request";

        // DECODING
        // ----------------------------------------------------------------------------------------

        let block_numbers = request
            .block_numbers
            .into_iter()
            .map(DecodeMessageExt::decode_and_verify)
            .collect::<Result<Vec<_>, _>>()?;
        let input_notes = decode_input_notes(
            request.input_notes,
            request.input_notes_args,
            request.explicit_input_notes,
        )?;
        let expected_output_recipients = request
            .expected_output_recipients
            .into_iter()
            .map(DecodeMessageExt::decode_and_verify)
            .collect::<Result<Vec<NoteRecipient>, _>>()?;
        let (custom_script, own_output_notes) =
            decode_script_template(request.script_template, &expected_output_recipients)?;
        let expected_future_notes = decode_expected_future_notes(request.expected_future_notes)?;
        let advice_map: AdviceMap =
            required(request.advice_map, MESSAGE, "advice map")?.decode_and_verify()?;
        let merkle_store: MerkleStore =
            required(request.merkle_store, MESSAGE, "merkle store")?.decode_and_verify()?;
        let foreign_accounts = request
            .foreign_accounts
            .into_iter()
            .map(ForeignAccount::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let expiration_delta = request
            .expiration_delta
            .map(|expiration_delta| {
                u16::try_from(expiration_delta).map_err(|_| {
                    ProtoDecodeError::InvalidValue(format!(
                        "expiration delta {expiration_delta} does not fit in 16 bits"
                    ))
                })
            })
            .transpose()?;
        let script_arg = request.script_arg.map(Word::try_from).transpose()?;
        let auth_arg = request.auth_arg.map(Word::try_from).transpose()?;
        let fee_conversion_salt = request.fee_conversion_salt.map(Word::try_from).transpose()?;
        // The builder keeps only one of the two values, because each one replaces the other.
        if auth_arg.is_some() && fee_conversion_salt.is_some() {
            return Err(ProtoDecodeError::InvalidValue(
                "a request cannot have both an auth argument and a fee conversion salt".to_string(),
            ));
        }
        let expected_ntx_scripts = request
            .expected_ntx_scripts
            .into_iter()
            .map(DecodeMessageExt::decode_and_verify)
            .collect::<Result<Vec<_>, _>>()?;

        // BUILDING
        // ----------------------------------------------------------------------------------------

        let mut builder = TransactionRequestBuilder::new().block_numbers(block_numbers);

        // The builder appends the input notes in call order, so each note is added with its own
        // call to keep the order of the request.
        for input_note in input_notes {
            builder = match input_note {
                // If the note is in the `explicit_input_notes` map, use the `explicit_input_note`
                // setter
                DecodedInputNote::Explicit(input_note, note_args) => {
                    builder.explicit_input_notes([(input_note, note_args)])
                },
                DecodedInputNote::Plain(note, note_args) => {
                    builder.input_notes([(note, note_args)])
                },
            };
        }
        if let Some(script) = custom_script {
            builder = builder.custom_script(script);
        }
        builder = builder.own_output_notes(own_output_notes);
        // This call replaces the recipients that `own_output_notes` added, so it receives all of
        // them.
        builder = builder
            .expected_output_recipients(expected_output_recipients)
            .expected_future_notes(expected_future_notes)
            .extend_merkle_store(merkle_store.inner_nodes())
            .foreign_accounts(foreign_accounts)
            .expected_ntx_scripts(expected_ntx_scripts);
        if let Some(expiration_delta) = expiration_delta {
            builder = builder.expiration_delta(expiration_delta);
        }
        if request.ignore_invalid_input_notes {
            builder = builder.ignore_invalid_input_notes();
        }
        if let Some(script_arg) = script_arg {
            builder = builder.script_arg(script_arg);
        }
        if let Some(auth_arg) = auth_arg {
            builder = builder.auth_arg(auth_arg);
        }
        if let Some(salt) = fee_conversion_salt {
            builder = builder.fee_conversion_salt(salt);
        }

        let mut request =
            builder.build().map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;
        *request.advice_map_mut() = advice_map;
        Ok(request)
    }
}

/// An input note of a decoded request, with its arguments.
enum DecodedInputNote {
    /// A note with a pinned consumption mode.
    Explicit(InputNote, Option<NoteArgs>),
    /// A note whose consumption mode the executing client selects.
    Plain(Note, Option<NoteArgs>),
}

/// Decodes the input notes of a request with their arguments, in the order of the request.
fn decode_input_notes(
    input_notes: Vec<miden_objects::proto::note::Note>,
    input_notes_args: Vec<proto::NoteArgsEntry>,
    explicit_input_notes: Vec<miden_objects::proto::transaction::InputNote>,
) -> Result<Vec<DecodedInputNote>, ProtoDecodeError> {
    let input_notes = input_notes
        .into_iter()
        .map(DecodeMessageExt::decode_and_verify)
        .collect::<Result<Vec<Note>, _>>()?;
    if input_notes_args.len() != input_notes.len() {
        return Err(ProtoDecodeError::InvalidValue(
            "the number of input note arguments is not the number of input notes".to_string(),
        ));
    }
    let mut explicit_input_notes = explicit_input_notes
        .into_iter()
        .map(|input_note| {
            let input_note: InputNote = input_note.decode_and_verify()?;
            Ok((input_note.id(), input_note))
        })
        .collect::<Result<BTreeMap<_, _>, ProtoDecodeError>>()?;

    let mut decoded = Vec::with_capacity(input_notes.len());
    for (note, entry) in input_notes.into_iter().zip(input_notes_args) {
        let note_id: NoteId =
            required(entry.note_id, "note arguments", "note id")?.decode_and_verify()?;
        if note_id != note.id() {
            return Err(ProtoDecodeError::InvalidValue(format!(
                "the arguments of input note {} are for note {note_id}",
                note.id()
            )));
        }
        let note_args = entry.note_args.map(Word::try_from).transpose()?;

        decoded.push(match explicit_input_notes.remove(&note_id) {
            Some(input_note) if *input_note.note() == note => {
                DecodedInputNote::Explicit(input_note, note_args)
            },
            Some(_) => {
                return Err(ProtoDecodeError::InvalidValue(format!(
                    "explicit input note {note_id} does not match the input note"
                )));
            },
            None => DecodedInputNote::Plain(note, note_args),
        });
    }
    if let Some(note_id) = explicit_input_notes.keys().next() {
        return Err(ProtoDecodeError::InvalidValue(format!(
            "explicit input note {note_id} is not an input note"
        )));
    }

    Ok(decoded)
}

/// Decodes the transaction script template of a request into the custom script and the own output
/// notes that the builder takes. At most one of the two is set, and neither is set when the request
/// has no template.
///
/// The own output notes are built from their partial notes and from `expected_output_recipients`.
fn decode_script_template(
    template: Option<proto::TransactionScriptTemplate>,
    expected_output_recipients: &[NoteRecipient],
) -> Result<(Option<TransactionScript>, Vec<Note>), ProtoDecodeError> {
    use proto::transaction_script_template::Template;

    let Some(template) = template else {
        return Ok((None, Vec::new()));
    };

    match required(template.template, "transaction script template", "variant")? {
        Template::CustomScript(script) => Ok((Some(script.decode_and_verify()?), Vec::new())),
        Template::SendNotes(send_notes) => {
            let notes = send_notes
                .notes
                .into_iter()
                .map(|note| own_output_note(note, expected_output_recipients))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((None, notes))
        },
    }
}

/// Decodes the notes that later transactions are expected to create, with their tags.
fn decode_expected_future_notes(
    notes: Vec<proto::ExpectedFutureNote>,
) -> Result<Vec<(NoteDetails, NoteTag)>, ProtoDecodeError> {
    notes
        .into_iter()
        .map(|note| {
            let details =
                required(note.details, "expected future note", "details")?.decode_and_verify()?;
            Ok((details, NoteTag::from(note.tag)))
        })
        .collect()
}

/// Builds an own output note from its partial note and the recipient that the partial note commits
/// to.
fn own_output_note(
    note: miden_objects::proto::note::PartialNote,
    recipients: &[NoteRecipient],
) -> Result<Note, ProtoDecodeError> {
    let partial: PartialNote = note.decode_and_verify()?;
    let recipient = recipients
        .iter()
        .find(|recipient| recipient.digest() == partial.recipient_digest())
        .ok_or_else(|| {
            ProtoDecodeError::InvalidValue(format!(
                "own output note has no expected recipient with digest {}",
                partial.recipient_digest()
            ))
        })?;

    Ok(Note::with_attachments(
        partial.assets().clone(),
        *partial.partial_metadata(),
        recipient.clone(),
        partial.attachments().clone(),
    ))
}

impl From<&TransactionScriptTemplate> for proto::TransactionScriptTemplate {
    fn from(template: &TransactionScriptTemplate) -> Self {
        use proto::transaction_script_template::{SendNotes, Template};

        let template = match template {
            TransactionScriptTemplate::CustomScript(script) => {
                Template::CustomScript(script.into())
            },
            TransactionScriptTemplate::SendNotes(notes) => Template::SendNotes(SendNotes {
                notes: notes.iter().map(Into::into).collect(),
            }),
        };

        Self { template: Some(template) }
    }
}

impl From<&ForeignAccount> for proto::ForeignAccount {
    fn from(account: &ForeignAccount) -> Self {
        use proto::foreign_account::{Account, Public};

        let account = match account {
            ForeignAccount::Public(account_id, storage_requirements) => Account::Public(Public {
                account_id: Some(account_id.into()),
                storage_requirements: Some(storage_requirements.into()),
            }),
            ForeignAccount::Private(partial_account) => Account::Private(partial_account.into()),
        };

        Self { account: Some(account) }
    }
}

/// The account is built with [`ForeignAccount::public`] or [`ForeignAccount::private`], so a public
/// account in the private variant is rejected, and the reverse.
impl TryFrom<proto::ForeignAccount> for ForeignAccount {
    type Error = ProtoDecodeError;

    fn try_from(account: proto::ForeignAccount) -> Result<Self, Self::Error> {
        use proto::foreign_account::Account;

        const MESSAGE: &str = "foreign account";

        let account = match required(account.account, MESSAGE, "variant")? {
            Account::Public(public) => ForeignAccount::public(
                required(public.account_id, MESSAGE, "account id")?.decode_and_verify()?,
                required(public.storage_requirements, MESSAGE, "storage requirements")?
                    .try_into()?,
            ),
            Account::Private(partial_account) => {
                ForeignAccount::private(partial_account.decode_and_verify()?)
            },
        };

        account.map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
    }
}

impl From<&AccountStorageRequirements> for proto::AccountStorageRequirements {
    fn from(requirements: &AccountStorageRequirements) -> Self {
        Self {
            slots: requirements
                .inner()
                .iter()
                .map(|(slot_name, keys)| proto::account_storage_requirements::SlotKeys {
                    slot_name: slot_name.as_str().to_string(),
                    keys: keys.iter().map(|key| Word::from(*key).into()).collect(),
                })
                .collect(),
        }
    }
}

impl TryFrom<proto::AccountStorageRequirements> for AccountStorageRequirements {
    type Error = ProtoDecodeError;

    fn try_from(requirements: proto::AccountStorageRequirements) -> Result<Self, Self::Error> {
        let slots = requirements
            .slots
            .into_iter()
            .map(|slot| {
                let slot_name = StorageSlotName::new(slot.slot_name)
                    .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;
                let keys = slot
                    .keys
                    .into_iter()
                    .map(|key| Word::try_from(key).map(StorageMapKey::new))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((slot_name, keys))
            })
            .collect::<Result<Vec<_>, ProtoDecodeError>>()?;

        Ok(AccountStorageRequirements::new(
            slots.iter().map(|(slot_name, keys)| (slot_name.clone(), keys)),
        ))
    }
}
