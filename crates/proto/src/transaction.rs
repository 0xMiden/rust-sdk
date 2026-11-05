use std::string::ToString;
use std::vec::Vec;

use miden_client_core::rpc::domain::account::{
    AccountFetch,
    AccountStorageRequirements,
    StorageMapFetch,
    VaultFetch,
};
use miden_client_core::store::{
    InputNoteRecord,
    InputNoteUpdate,
    NoteUpdateType,
    NoteUpdates,
    OutputNoteRecord,
    OutputNoteUpdate,
};
use miden_client_core::sync::{NoteTagRecord, NoteTagSource};
use miden_client_core::transaction::{
    DiscardCause,
    ForeignAccount,
    InputNoteAuthentication,
    NoteArgs,
    RequestInputNote,
    RequestScript,
    TransactionDetails,
    TransactionRequest,
    TransactionRequestBuilder,
    TransactionStatus,
    TransactionStoreUpdate,
};
use miden_objects::DecodeMessageExt;
use miden_protocol::Word;
use miden_protocol::account::{AccountCodeUpgrade, PartialAccount, StorageMapKey, StorageSlotName};
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::crypto::merkle::store::MerkleStore;
use miden_protocol::note::{Note, NoteDetails, NoteDetailsCommitment, NoteRecipient, NoteTag};
use miden_protocol::transaction::{AccountInputs, ExecutedTransaction, TransactionScript};
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::vm::AdviceMap;

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
        let request = proto::TransactionRequestV1 {
            block_numbers: self
                .block_numbers()
                .iter()
                .map(|block_num| (*block_num).into())
                .collect(),
            input_notes: self.input_notes().iter().map(Into::into).collect(),
            transaction_script: self.transaction_script().as_ref().map(Into::into),
            expected_output_recipients: self.expected_output_recipients().map(Into::into).collect(),
            expected_future_notes: self
                .expected_future_notes()
                .map(|(details, tag)| expected_future_note(details, *tag))
                .collect(),
            advice_map: Some(self.advice_map().into()),
            merkle_store: Some(self.merkle_store().into()),
            foreign_accounts: self.foreign_accounts().values().map(Into::into).collect(),
            ignore_invalid_input_notes: self.ignore_invalid_input_notes(),
            auth_arg: self.auth_arg().map(Into::into),
            fee_conversion_salt: self.fee_conversion_salt().map(Into::into),
            expected_ntx_scripts: self.expected_ntx_scripts().iter().map(Into::into).collect(),
            account_code_upgrade: self.account_code_upgrade().map(Into::into),
        };

        Self::Message {
            version: Some(proto::transaction_request::Version::V1(request)),
        }
    }

    fn from_proto(request: Self::Message) -> Result<Self, ProtoDecodeError> {
        use proto::transaction_request::Version;

        match required(request.version, "transaction request", "version")? {
            Version::V1(request) => decode_transaction_request_v1(request),
        }
    }
}

/// Decodes version 1 of the transaction request format. The request is built again with
/// [`TransactionRequestBuilder`], so decoding applies the same checks as building a request.
fn decode_transaction_request_v1(
    request: proto::TransactionRequestV1,
) -> Result<TransactionRequest, ProtoDecodeError> {
    const MESSAGE: &str = "transaction request";

    // DECODING
    // ----------------------------------------------------------------------------------------

    let block_numbers = request
        .block_numbers
        .into_iter()
        .map(DecodeMessageExt::decode_and_verify)
        .collect::<Result<Vec<_>, _>>()?;
    let input_notes = request
        .input_notes
        .into_iter()
        .map(decode_input_note)
        .collect::<Result<Vec<_>, _>>()?;
    let expected_output_recipients = request
        .expected_output_recipients
        .into_iter()
        .map(DecodeMessageExt::decode_and_verify)
        .collect::<Result<Vec<NoteRecipient>, _>>()?;
    let script = decode_script(request.transaction_script)?;
    let expected_future_notes = request
        .expected_future_notes
        .into_iter()
        .map(expected_future_note_from_proto)
        .collect::<Result<Vec<(NoteDetails, NoteTag)>, ProtoDecodeError>>()?;
    let advice_map: AdviceMap =
        required(request.advice_map, MESSAGE, "advice map")?.decode_and_verify()?;
    let merkle_store: MerkleStore =
        required(request.merkle_store, MESSAGE, "merkle store")?.decode_and_verify()?;
    let foreign_accounts = request
        .foreign_accounts
        .into_iter()
        .map(ForeignAccount::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    let auth_arg = request.auth_arg.map(Word::try_from).transpose()?;
    let fee_conversion_salt = request.fee_conversion_salt.map(Word::try_from).transpose()?;
    let expected_ntx_scripts = request
        .expected_ntx_scripts
        .into_iter()
        .map(DecodeMessageExt::decode_and_verify)
        .collect::<Result<Vec<_>, _>>()?;
    let account_code_upgrade: Option<AccountCodeUpgrade> = request
        .account_code_upgrade
        .map(DecodeMessageExt::decode_and_verify)
        .transpose()?;

    // BUILDING
    // ----------------------------------------------------------------------------------------

    let mut builder = TransactionRequestBuilder::new().block_numbers(block_numbers);

    // The builder appends the input notes in call order, so each note is added with its own call to
    // keep the order of the request.
    for (authentication, args) in input_notes {
        builder = match authentication {
            InputNoteAuthentication::Inferred(note) => builder.input_notes([(note, args)]),
            InputNoteAuthentication::Explicit(input_note) => {
                builder.explicit_input_notes([(input_note, args)])
            },
        };
    }
    if let Some(custom_script) = script.custom_script {
        builder = builder.custom_script(custom_script);
    }
    if let Some(script_arg) = script.script_arg {
        builder = builder.script_arg(script_arg);
    }
    if let Some(expiration_delta) = script.expiration_delta {
        builder = builder.expiration_delta(expiration_delta);
    }
    builder = builder
        .own_output_notes(script.own_output_notes)
        .expected_output_recipients(expected_output_recipients)
        .expected_future_notes(expected_future_notes)
        .extend_advice_map(advice_map)
        .extend_merkle_store(merkle_store.inner_nodes())
        .foreign_accounts(foreign_accounts)
        .expected_ntx_scripts(expected_ntx_scripts);
    if request.ignore_invalid_input_notes {
        builder = builder.ignore_invalid_input_notes();
    }
    if let Some(salt) = fee_conversion_salt {
        builder = builder.fee_conversion_salt(salt);
    }
    if let Some(account_code_upgrade) = account_code_upgrade {
        builder = builder.account_code_upgrade(account_code_upgrade.into_code());
    }

    let mut transaction_request =
        builder.build().map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;
    if let Some(auth_arg) = auth_arg {
        set_auth_arg(&mut transaction_request, auth_arg)?;
    }

    Ok(transaction_request)
}

/// Sets `auth_arg` on `transaction_request`.
///
/// A request always stores the preimage of its auth arg in its advice map.
///
/// # Errors
/// - If the advice map of the request has no preimage for `auth_arg`.
fn set_auth_arg(
    transaction_request: &mut TransactionRequest,
    auth_arg: Word,
) -> Result<(), ProtoDecodeError> {
    let preimage = transaction_request.advice_map().get(&auth_arg).ok_or_else(|| {
        ProtoDecodeError::InvalidValue(format!(
            "advice map has no preimage for auth arg {auth_arg}"
        ))
    })?;
    let preimage = preimage.to_vec();
    transaction_request.commit_auth_args(auth_arg, preimage);
    Ok(())
}

/// Decodes an input note of a request into its authentication and its arguments.
fn decode_input_note(
    input_note: proto::RequestInputNote,
) -> Result<(InputNoteAuthentication, Option<NoteArgs>), ProtoDecodeError> {
    use proto::request_input_note::Authentication;

    let authentication =
        match required(input_note.authentication, "request input note", "authentication")? {
            Authentication::Inferred(note) => {
                InputNoteAuthentication::Inferred(note.decode_and_verify()?)
            },
            Authentication::Explicit(input_note) => {
                InputNoteAuthentication::Explicit(input_note.decode_and_verify()?)
            },
        };
    let args = input_note.args.map(Word::try_from).transpose()?;

    Ok((authentication, args))
}

impl From<&RequestInputNote> for proto::RequestInputNote {
    fn from(input_note: &RequestInputNote) -> Self {
        use proto::request_input_note::Authentication;

        let authentication = match input_note.authentication() {
            InputNoteAuthentication::Inferred(note) => Authentication::Inferred(note.into()),
            InputNoteAuthentication::Explicit(input_note) => {
                Authentication::Explicit(input_note.into())
            },
        };

        Self {
            authentication: Some(authentication),
            args: input_note.args().map(Into::into),
        }
    }
}

/// The values of a request script that the builder takes.
#[derive(Default)]
struct DecodedScript {
    custom_script: Option<TransactionScript>,
    script_arg: Option<Word>,
    own_output_notes: Vec<Note>,
    expiration_delta: Option<u16>,
}

/// Decodes the script of a request into the values that the builder takes. Every value is empty
/// when the request has no script.
fn decode_script(script: Option<proto::RequestScript>) -> Result<DecodedScript, ProtoDecodeError> {
    use proto::request_script::Script;

    let Some(script) = script else {
        return Ok(DecodedScript::default());
    };

    match required(script.script, "request script", "variant")? {
        Script::Generated(generated) => Ok(DecodedScript {
            own_output_notes: generated
                .own_output_notes
                .into_iter()
                .map(DecodeMessageExt::decode_and_verify)
                .collect::<Result<Vec<_>, _>>()?,
            expiration_delta: generated
                .expiration_delta
                .map(decode_expiration_delta)
                .transpose()?,
            ..Default::default()
        }),
        Script::Custom(custom) => Ok(DecodedScript {
            custom_script: Some(
                required(custom.script, "custom script", "script")?.decode_and_verify()?,
            ),
            script_arg: custom.arg.map(Word::try_from).transpose()?,
            ..Default::default()
        }),
    }
}

/// Decodes an expiration delta, which the builder takes as a 16-bit value.
fn decode_expiration_delta(expiration_delta: u32) -> Result<u16, ProtoDecodeError> {
    u16::try_from(expiration_delta).map_err(|_| {
        ProtoDecodeError::InvalidValue(format!(
            "expiration delta {expiration_delta} does not fit in 16 bits"
        ))
    })
}

impl From<&RequestScript> for proto::RequestScript {
    fn from(script: &RequestScript) -> Self {
        use proto::request_script::{Custom, Generated, Script};

        let script = match script {
            RequestScript::Generated { own_output_notes, expiration_delta } => {
                Script::Generated(Generated {
                    own_output_notes: own_output_notes.iter().map(Into::into).collect(),
                    expiration_delta: expiration_delta.map(|delta| u32::from(delta.get())),
                })
            },
            RequestScript::Custom { script, arg } => Script::Custom(Custom {
                script: Some(script.into()),
                arg: arg.map(Into::into),
            }),
        };

        Self { script: Some(script) }
    }
}

impl From<&ForeignAccount> for proto::ForeignAccount {
    fn from(account: &ForeignAccount) -> Self {
        use proto::foreign_account::{Account, Prefetched, Public};

        let account = match account {
            ForeignAccount::Public(account_id, fetch) => Account::Public(Public {
                account_id: Some(account_id.into()),
                fetch: Some(fetch.into()),
            }),
            ForeignAccount::Private(partial_account) => Account::Private(partial_account.into()),
            ForeignAccount::Prefetched(inputs) => Account::Prefetched(Prefetched {
                account: Some(inputs.account().into()),
                witness: Some(inputs.witness().into()),
            }),
        };

        Self { account: Some(account) }
    }
}

impl TryFrom<proto::ForeignAccount> for ForeignAccount {
    type Error = ProtoDecodeError;

    fn try_from(account: proto::ForeignAccount) -> Result<Self, Self::Error> {
        use proto::foreign_account::Account;

        const MESSAGE: &str = "foreign account";

        let account = match required(account.account, MESSAGE, "variant")? {
            Account::Public(public) => ForeignAccount::public(
                required(public.account_id, MESSAGE, "account id")?.decode_and_verify()?,
                required(public.fetch, MESSAGE, "fetch")?.try_into()?,
            ),
            Account::Private(partial_account) => {
                ForeignAccount::private(partial_account.decode_and_verify()?)
            },
            Account::Prefetched(prefetched) => {
                let partial_account: PartialAccount =
                    required(prefetched.account, MESSAGE, "account")?.decode_and_verify()?;
                let witness: AccountWitness =
                    required(prefetched.witness, MESSAGE, "witness")?.decode_and_verify()?;
                if witness.id() != partial_account.id() {
                    return Err(ProtoDecodeError::InvalidValue(format!(
                        "witness of account {} is for account {}",
                        partial_account.id(),
                        witness.id()
                    )));
                }
                Ok(AccountInputs::new(partial_account, witness).into())
            },
        };

        account.map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
    }
}

fn expected_future_note(details: &NoteDetails, tag: NoteTag) -> proto::ExpectedFutureNote {
    proto::ExpectedFutureNote {
        details: Some(details.into()),
        tag: tag.into(),
    }
}

fn expected_future_note_from_proto(
    note: proto::ExpectedFutureNote,
) -> Result<(NoteDetails, NoteTag), ProtoDecodeError> {
    let details = required(note.details, "expected future note", "details")?.decode_and_verify()?;
    Ok((details, NoteTag::from(note.tag)))
}

impl From<&AccountFetch> for proto::AccountFetch {
    fn from(fetch: &AccountFetch) -> Self {
        use proto::account_fetch::{AllAssets, AllStorageMaps, AssetIds, Storage, Vault};

        let storage = match fetch.storage() {
            StorageMapFetch::Skip => None,
            StorageMapFetch::All => Some(Storage::AllStorageMaps(AllStorageMaps {})),
            StorageMapFetch::Slots(requirements) => Some(Storage::StorageMaps(requirements.into())),
        };
        let vault = match fetch.vault() {
            VaultFetch::Skip => None,
            VaultFetch::Always => Some(Vault::AllAssets(AllAssets {})),
            VaultFetch::IfChangedFrom(root) => Some(Vault::IfChangedFrom((*root).into())),
            VaultFetch::Assets(asset_ids) => Some(Vault::AssetIds(AssetIds {
                asset_ids: asset_ids.iter().map(Into::into).collect(),
            })),
        };

        Self { storage, vault }
    }
}

impl TryFrom<proto::AccountFetch> for AccountFetch {
    type Error = ProtoDecodeError;

    fn try_from(fetch: proto::AccountFetch) -> Result<Self, Self::Error> {
        use proto::account_fetch::{Storage, Vault};

        let storage = match fetch.storage {
            None => StorageMapFetch::Skip,
            Some(Storage::AllStorageMaps(_)) => StorageMapFetch::All,
            Some(Storage::StorageMaps(requirements)) => {
                StorageMapFetch::Slots(requirements.try_into()?)
            },
        };
        let vault = match fetch.vault {
            None => VaultFetch::Skip,
            Some(Vault::AllAssets(_)) => VaultFetch::Always,
            Some(Vault::IfChangedFrom(root)) => VaultFetch::IfChangedFrom(root.try_into()?),
            Some(Vault::AssetIds(asset_ids)) => VaultFetch::Assets(
                asset_ids
                    .asset_ids
                    .into_iter()
                    .map(DecodeMessageExt::decode_and_verify)
                    .collect::<Result<_, _>>()?,
            ),
        };

        Ok(AccountFetch::new().with_storage(storage).with_vault(vault))
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

// TRANSACTION STORE UPDATE
// ================================================================================================

impl ProtobufValue for TransactionStoreUpdate {
    type Message = proto::TransactionStoreUpdate;

    fn to_proto(&self) -> Self::Message {
        let note_updates = self.note_updates();
        Self::Message {
            executed_transaction: self.executed_transaction().to_bytes(),
            submission_height: Some(self.submission_height().into()),
            future_notes: self
                .future_notes()
                .iter()
                .map(|(details, tag)| expected_future_note(details, *tag))
                .collect(),
            input_notes: note_updates
                .input_notes()
                .iter()
                .map(|update| proto::InputNoteUpdate {
                    note: Some(update.inner().to_proto()),
                    update_type: proto::NoteUpdateType::from(*update.update_type()).into(),
                })
                .collect(),
            output_notes: note_updates
                .output_notes()
                .iter()
                .map(|update| proto::OutputNoteUpdate {
                    note: Some(update.inner().to_proto()),
                    update_type: proto::NoteUpdateType::from(*update.update_type()).into(),
                })
                .collect(),
            new_tags: self.new_tags().iter().map(ProtobufValue::to_proto).collect(),
        }
    }

    fn from_proto(update: Self::Message) -> Result<Self, ProtoDecodeError> {
        const MESSAGE: &str = "transaction store update";

        let executed_transaction =
            ExecutedTransaction::read_from_bytes(&update.executed_transaction)
                .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;
        let submission_height = required(update.submission_height, MESSAGE, "submission height")?
            .decode_and_verify()?;
        let future_notes = update
            .future_notes
            .into_iter()
            .map(expected_future_note_from_proto)
            .collect::<Result<_, _>>()?;
        let input_notes = update
            .input_notes
            .into_iter()
            .map(|update| {
                Ok(InputNoteUpdate::new(
                    InputNoteRecord::from_proto(required(
                        update.note,
                        "input note update",
                        "note",
                    )?)?,
                    proto::NoteUpdateType::try_from(update.update_type)?.try_into()?,
                ))
            })
            .collect::<Result<_, ProtoDecodeError>>()?;
        let output_notes = update
            .output_notes
            .into_iter()
            .map(|update| {
                Ok(OutputNoteUpdate::new(
                    OutputNoteRecord::from_proto(required(
                        update.note,
                        "output note update",
                        "note",
                    )?)?,
                    proto::NoteUpdateType::try_from(update.update_type)?.try_into()?,
                ))
            })
            .collect::<Result<_, ProtoDecodeError>>()?;
        let new_tags = update
            .new_tags
            .into_iter()
            .map(NoteTagRecord::from_proto)
            .collect::<Result<_, _>>()?;

        Ok(TransactionStoreUpdate::new(
            executed_transaction,
            submission_height,
            NoteUpdates::new(input_notes, output_notes),
            future_notes,
            new_tags,
        ))
    }
}

impl From<NoteUpdateType> for proto::NoteUpdateType {
    fn from(update_type: NoteUpdateType) -> Self {
        match update_type {
            NoteUpdateType::Insert => Self::Insert,
            NoteUpdateType::Update => Self::Update,
            NoteUpdateType::InsertCommitted => Self::InsertCommitted,
        }
    }
}

/// The unspecified value is rejected, because the type decides how the store writes the note.
impl TryFrom<proto::NoteUpdateType> for NoteUpdateType {
    type Error = ProtoDecodeError;

    fn try_from(update_type: proto::NoteUpdateType) -> Result<Self, Self::Error> {
        match update_type {
            proto::NoteUpdateType::Insert => Ok(Self::Insert),
            proto::NoteUpdateType::Update => Ok(Self::Update),
            proto::NoteUpdateType::InsertCommitted => Ok(Self::InsertCommitted),
            proto::NoteUpdateType::Unspecified => {
                Err(ProtoDecodeError::InvalidValue("note update type is unspecified".to_string()))
            },
        }
    }
}

impl ProtobufValue for NoteTagRecord {
    type Message = proto::NoteTagRecord;

    fn to_proto(&self) -> Self::Message {
        use proto::note_tag_record::{Source, User};

        let source = match self.source {
            NoteTagSource::Account(account_id) => Source::Account(account_id.into()),
            NoteTagSource::Note(commitment) => Source::Note(commitment.as_word().into()),
            NoteTagSource::User => Source::User(User {}),
            NoteTagSource::Subscription(key) => Source::Subscription(key.into()),
        };

        Self::Message {
            tag: self.tag.into(),
            source: Some(source),
        }
    }

    fn from_proto(record: Self::Message) -> Result<Self, ProtoDecodeError> {
        use proto::note_tag_record::Source;

        let source = match required(record.source, "note tag record", "source")? {
            Source::Account(account_id) => NoteTagSource::Account(account_id.decode_and_verify()?),
            Source::Note(commitment) => {
                NoteTagSource::Note(NoteDetailsCommitment::from_raw(Word::try_from(commitment)?))
            },
            Source::User(_) => NoteTagSource::User,
            Source::Subscription(key) => NoteTagSource::Subscription(Word::try_from(key)?),
        };

        Ok(NoteTagRecord { tag: NoteTag::from(record.tag), source })
    }
}

#[cfg(test)]
mod tests {
    use std::vec;
    use std::vec::Vec;

    use miden_client_core::store::{InputNoteRecord, OutputNoteRecord};
    use miden_client_core::transaction::{
        ForeignAccount,
        TransactionRequest,
        TransactionRequestBuilder,
    };
    use miden_protocol::account::auth::{AuthScheme, PublicKeyCommitment};
    use miden_protocol::account::{
        Account,
        AccountBuilder,
        AccountCode,
        AccountId,
        AccountType,
        StorageMapKey,
        StorageSlotName,
    };
    use miden_protocol::asset::FungibleAsset;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::block::account_tree::AccountTree;
    use miden_protocol::crypto::merkle::MerkleTree;
    use miden_protocol::crypto::rand::{FeltRng, RandomCoin};
    use miden_protocol::note::{Note, NoteScript, NoteTag, NoteType};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        ACCOUNT_ID_SENDER,
    };
    use miden_protocol::transaction::{AccountInputs, InputNote, TransactionScript};
    use miden_protocol::{EMPTY_WORD, Felt, Word};
    use miden_standards::account::auth::{Approver, AuthSingleSig, MultisigAuthArgs};
    use miden_standards::note::P2idNote;
    use miden_standards::testing::account_component::MockAccountComponent;
    use miden_testing::{Auth, MockChainBuilder, MockTransactionInput};

    use super::{
        AccountFetch,
        AccountStorageRequirements,
        InputNoteUpdate,
        NoteTagRecord,
        NoteTagSource,
        NoteUpdateType,
        NoteUpdates,
        OutputNoteUpdate,
        StorageMapFetch,
        TransactionStoreUpdate,
        VaultFetch,
        proto,
    };
    use crate::{decode, encode};

    /// A store update with an executed transaction, every note update type, an output note and
    /// every tag source.
    #[tokio::test]
    async fn transaction_store_update_round_trips() {
        let sender_id = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        let asset = FungibleAsset::new(faucet_id, 100).unwrap().into();

        let mut builder = MockChainBuilder::new();
        let account = builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
        let note = builder
            .add_p2id_note(sender_id, account.id(), &[asset], NoteType::Public)
            .unwrap();
        let mut chain = builder.build().unwrap();
        chain.prove_next_block().unwrap();
        let executed_transaction = Box::pin(
            chain
                .build_transaction(MockTransactionInput::Account(account.clone()))
                .unauthenticated_input_note(note.clone())
                .build()
                .unwrap()
                .execute(),
        )
        .await
        .unwrap();

        let input_record = InputNoteRecord::from(note.clone());
        let output_record = OutputNoteRecord::from_full_note(note.clone(), BlockNumber::from(9u32));
        let note_updates = NoteUpdates::new(
            vec![
                InputNoteUpdate::new(input_record.clone(), NoteUpdateType::Insert),
                InputNoteUpdate::new(input_record.clone(), NoteUpdateType::Update),
                InputNoteUpdate::new(input_record, NoteUpdateType::InsertCommitted),
            ],
            vec![OutputNoteUpdate::new(output_record, NoteUpdateType::Insert)],
        );
        let tag = NoteTag::with_account_target(account.id());
        let new_tags = vec![
            NoteTagRecord::with_note_source(tag, note.details_commitment()),
            NoteTagRecord::with_account_source(tag, account.id()),
            NoteTagRecord { tag, source: NoteTagSource::User },
            NoteTagRecord {
                tag,
                source: NoteTagSource::Subscription(Word::from([3u32; 4])),
            },
        ];

        let update = TransactionStoreUpdate::new(
            executed_transaction,
            BlockNumber::from(42u32),
            note_updates,
            vec![(note.into(), tag)],
            new_tags,
        );

        assert_eq!(decode::<TransactionStoreUpdate>(&encode(&update)).unwrap(), update);
    }

    #[test]
    fn note_update_type_unspecified_is_rejected() {
        assert!(NoteUpdateType::try_from(proto::NoteUpdateType::Unspecified).is_err());
    }

    #[test]
    fn account_fetch_round_trips() {
        let slot = StorageSlotName::new("miden::testing::map").unwrap();
        let key = StorageMapKey::new(Word::from([1u32; 4]));
        let storage = AccountStorageRequirements::new([(slot, &[key])]);
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        let asset_id = FungibleAsset::new(faucet_id, 100).unwrap().id();

        for storage in
            [StorageMapFetch::Skip, StorageMapFetch::All, StorageMapFetch::Slots(storage)]
        {
            let fetch = AccountFetch::new().with_storage(storage);
            for fetch in [
                fetch.clone().with_vault(VaultFetch::Skip),
                fetch.clone().with_vault(VaultFetch::Always),
                fetch.clone().with_vault(VaultFetch::IfChangedFrom(Word::from([2u32; 4]))),
                fetch.with_vault(VaultFetch::Assets([asset_id].into())),
            ] {
                let decoded = AccountFetch::try_from(proto::AccountFetch::from(&fetch)).unwrap();
                assert_eq!(decoded, fetch);
            }
        }
    }

    /// A request with a send notes template. Plain and explicit input notes are mixed, so the
    /// decoder must keep their order.
    #[test]
    fn transaction_request_with_send_notes_round_trips() {
        let sender_id = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();
        let target_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        let mut rng = RandomCoin::new(Word::default());
        let mut notes = notes(8, &mut rng);

        let private_account = mock_private_account([0; 32]);
        let prefetched_account = mock_private_account([1; 32]);
        let account_tree = AccountTree::with_entries([(
            prefetched_account.id(),
            prefetched_account.to_commitment(),
        )])
        .unwrap();
        let prefetched_inputs = AccountInputs::new(
            (&prefetched_account).into(),
            account_tree.open(prefetched_account.id()),
        );
        let merkle_tree =
            MerkleTree::new([rng.draw_word(), rng.draw_word(), rng.draw_word(), rng.draw_word()])
                .unwrap();

        let request = TransactionRequestBuilder::new()
            .block_numbers([BlockNumber::from(1u32), BlockNumber::from(3u32)])
            .input_notes([(notes.pop().unwrap(), None)])
            .explicit_input_notes([(
                InputNote::unauthenticated(notes.pop().unwrap()),
                Some(rng.draw_word()),
            )])
            .input_notes([(notes.pop().unwrap(), Some(rng.draw_word()))])
            .expected_output_recipients([notes.pop().unwrap().recipient().clone()])
            .expected_future_notes(vec![(
                notes.pop().unwrap().into(),
                NoteTag::with_account_target(sender_id),
            )])
            .extend_advice_map([(rng.draw_word(), vec![Felt::from(1u32), Felt::from(2u32)])])
            .extend_merkle_store(merkle_tree.inner_nodes())
            .foreign_accounts([
                ForeignAccount::public(
                    target_id,
                    AccountFetch::new()
                        .with_storage(StorageMapFetch::Slots(AccountStorageRequirements::new([(
                            StorageSlotName::new("demo::storage_slot").unwrap(),
                            &[StorageMapKey::new(Word::default())],
                        )])))
                        .with_vault(VaultFetch::Assets(
                            [FungibleAsset::new(faucet_id, 100).unwrap().id()].into(),
                        )),
                )
                .unwrap(),
                ForeignAccount::private(&private_account).unwrap(),
                ForeignAccount::Prefetched(prefetched_inputs),
            ])
            .own_output_notes([notes.pop().unwrap(), notes.pop().unwrap()])
            .expiration_delta(10)
            .auth_args(&MultisigAuthArgs::new(BlockNumber::from(7u32), rng.draw_word()))
            .expected_ntx_scripts(vec![notes.pop().unwrap().recipient().script().clone()])
            .account_code_upgrade(AccountCode::mock())
            .build()
            .unwrap();

        assert_eq!(decode::<TransactionRequest>(&encode(&request)).unwrap(), request);
    }

    /// A request with a custom script and the fields that a send notes request cannot have.
    #[test]
    fn transaction_request_with_custom_script_round_trips() {
        let mut rng = RandomCoin::new(Word::default());
        let note_script = NoteScript::mock();
        let script =
            TransactionScript::from_parts(note_script.mast(), note_script.entrypoint()).unwrap();

        let request = TransactionRequestBuilder::new()
            .input_notes(notes(2, &mut rng).into_iter().map(|note| (note, None)))
            .custom_script(script)
            .script_arg(rng.draw_word())
            .ignore_invalid_input_notes()
            .fee_conversion_salt(rng.draw_word())
            .build()
            .unwrap();

        assert_eq!(decode::<TransactionRequest>(&encode(&request)).unwrap(), request);
    }

    /// A request without a script template and without optional values.
    #[test]
    fn empty_transaction_request_round_trips() {
        let request = TransactionRequestBuilder::new().build().unwrap();

        assert_eq!(decode::<TransactionRequest>(&encode(&request)).unwrap(), request);
    }

    /// Returns an existing private account built from `seed`.
    fn mock_private_account(seed: [u8; 32]) -> Account {
        AccountBuilder::new(seed)
            .with_component(MockAccountComponent::with_empty_slots())
            .with_component(AuthSingleSig::new(Approver::new(
                PublicKeyCommitment::from(EMPTY_WORD),
                AuthScheme::Falcon512Poseidon2,
            )))
            .account_type(AccountType::Private)
            .build_existing()
            .unwrap()
    }

    /// Returns `count` P2ID notes with different serial numbers and amounts.
    fn notes(count: u64, rng: &mut RandomCoin) -> Vec<Note> {
        let sender_id = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();
        let target_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();

        (0..count)
            .map(|i| {
                P2idNote::builder()
                    .sender(sender_id)
                    .target(target_id)
                    .assets(vec![FungibleAsset::new(faucet_id, 100 + i).unwrap()])
                    .note_type(NoteType::Private)
                    .generate_serial_number(rng)
                    .build()
                    .unwrap()
                    .into()
            })
            .collect()
    }
}
