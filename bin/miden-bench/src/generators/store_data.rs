//! Synthetic store contents for the SQL store scaling benchmark.
//!
//! Every record is built through public client APIs, so the seeded database is the same shape a
//! real client would produce, and the measurements below it stay honest about what the store has
//! to do.

use miden_client::account::component::BasicWallet;
use miden_client::account::{Account, AccountBuilder, AccountId, AccountType};
use miden_client::auth::{Approver, AuthSchemeId, AuthSingleSig, PublicKeyCommitment};
use miden_client::block::BlockHeader;
use miden_client::note::{
    BlockNumber,
    NoteAssets,
    NoteAttachments,
    NoteDetails,
    NoteMetadata,
    NoteRecipient,
    NoteScript,
    NoteStorage,
    NoteTag,
    NoteType,
    PartialNoteMetadata,
    StandardNote,
};
use miden_client::store::InputNoteRecord;
use miden_client::store::input_note_states::{
    ConsumedUnauthenticatedLocalNoteState,
    ExpectedNoteState,
    NoteSubmissionData,
};
use miden_client::testing::account_id::ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE;
use miden_client::transaction::{TransactionId, TransactionKernel};
use miden_client::utils::Serializable;
use miden_client::{EMPTY_WORD, Felt, Word, ZERO};

/// Notes consumed by a single transaction of the benchmarked account. Consumed notes are spread
/// over blocks and transaction orders so that walking them exercises the whole cursor key.
const NOTES_PER_TX: usize = 2;

/// Transactions per block, so a note count spreads over several blocks instead of piling into one.
const TXS_PER_BLOCK: usize = 4;

/// Share of the seeded notes that are consumed. The rest stay unspent, which is what the unspent
/// filters and the nullifier listing read.
const CONSUMED_SHARE: usize = 3;
const SHARE_DIVISOR: usize = 4;

/// Serial number offsets keeping the generated note families disjoint, so no two seeded notes
/// collapse onto the same details commitment.
const CONSUMED_SERIAL_BASE: u64 = 1_000_000;
const UNSPENT_SERIAL_BASE: u64 = 2_000_000;
const INSERT_SERIAL_BASE: u64 = 3_000_000;

// SEED
// ================================================================================================

/// The store contents a note-count measurement runs against.
pub struct NoteSeed {
    /// Notes consumed by the benchmarked account.
    pub consumed: Vec<InputNoteRecord>,
    /// Notes that have not been consumed.
    pub unspent: Vec<InputNoteRecord>,
    /// One header per block the consumed notes were consumed in, paired with the
    /// `has_client_notes` flag it is stored under.
    pub block_headers: Vec<(BlockHeader, bool)>,
}

/// Returns the account whose consumed notes the benchmark walks.
pub fn consumer_account_id() -> AccountId {
    AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE)
        .expect("the testing account id is valid")
}

/// Builds `count` input notes for `consumer`, alongside the block headers covering their
/// consumption.
pub fn note_seed(consumer: AccountId, count: usize) -> NoteSeed {
    let consumed_count = count * CONSUMED_SHARE / SHARE_DIVISOR;
    let consumed_notes = consumed_input_notes(consumer, consumed_count, CONSUMED_SERIAL_BASE);
    let unspent_notes = unspent_input_notes(consumer, count - consumed_count, UNSPENT_SERIAL_BASE);

    // Half the headers are stored as holding client notes, so the partial index that serves the
    // tracked-header query covers a part of the table rather than all of it.
    let block_headers = (0..block_count(consumed_count))
        .map(|block| (mock_block_header(block), block % 2 == 0))
        .collect();

    NoteSeed {
        consumed: consumed_notes,
        unspent: unspent_notes,
        block_headers,
    }
}

/// Builds the batch of consumed notes an insert measurement adds on iteration `iteration`. Each
/// iteration gets its own serial numbers, so every batch is an insert and never a replace.
pub fn insert_batch(consumer: AccountId, size: usize, iteration: usize) -> Vec<InputNoteRecord> {
    let iteration = u64::try_from(iteration).expect("iteration count fits in u64");
    let size_step = u64::try_from(size).expect("batch size fits in u64");
    let base = INSERT_SERIAL_BASE + (iteration + 1) * size_step;

    consumed_input_notes(consumer, size, base)
}

/// Returns the key the per-account consumption order sorts by. Sorting the generated notes by it
/// mirrors the order the store returns them in.
pub fn consumption_key(note: &InputNoteRecord) -> (u32, u32, Vec<u8>) {
    (
        note.state().consumed_block_height().expect("note is consumed").as_u32(),
        note.state().consumed_tx_order().expect("note has a consumption order"),
        note.details_commitment().to_bytes(),
    )
}

// NOTES
// ================================================================================================

/// Builds `count` notes consumed by `consumer`, spread over blocks and transaction orders.
fn consumed_input_notes(
    consumer: AccountId,
    count: usize,
    serial_base: u64,
) -> Vec<InputNoteRecord> {
    let scripts = note_scripts();

    (0..count)
        .map(|index| {
            let tx = index / NOTES_PER_TX;
            let block = u32::try_from(tx / TXS_PER_BLOCK).expect("block index fits in u32");
            let tx_order = u32::try_from(tx % TXS_PER_BLOCK).expect("tx order fits in u32");
            let details = note_details(serial_base, index, &scripts);

            let state = ConsumedUnauthenticatedLocalNoteState {
                metadata: note_metadata(consumer, index),
                nullifier_block_height: BlockNumber::from(block),
                submission_data: NoteSubmissionData {
                    submitted_at: Some(0),
                    consumer_account: consumer,
                    consumer_transaction: TransactionId::from_raw(Word::default()),
                },
                consumed_tx_order: Some(tx_order),
            };

            InputNoteRecord::new(details, NoteAttachments::empty(), Some(0), state.into())
        })
        .collect()
}

/// Builds `count` notes that carry metadata, and therefore a nullifier, but were never consumed.
fn unspent_input_notes(sender: AccountId, count: usize, serial_base: u64) -> Vec<InputNoteRecord> {
    let scripts = note_scripts();

    (0..count)
        .map(|index| {
            let state = ExpectedNoteState {
                metadata: Some(note_metadata(sender, index)),
                after_block_num: BlockNumber::from(0u32),
                tag: None,
            };

            InputNoteRecord::new(
                note_details(serial_base, index, &scripts),
                NoteAttachments::empty(),
                Some(0),
                state.into(),
            )
        })
        .collect()
}

/// Returns the note scripts the generated notes are split across, so that filtering by script root
/// selects a part of the table.
pub fn note_scripts() -> Vec<NoteScript> {
    vec![StandardNote::SWAP.script(), StandardNote::P2ID.script()]
}

/// Returns the number of blocks `count` consumed notes span.
fn block_count(count: usize) -> u32 {
    let notes_per_block = NOTES_PER_TX * TXS_PER_BLOCK;
    u32::try_from(count.div_ceil(notes_per_block)).expect("block count fits in u32")
}

fn note_details(serial_base: u64, index: usize, scripts: &[NoteScript]) -> NoteDetails {
    let serial = serial_base + u64::try_from(index).expect("note index fits in u64");
    let serial_number: Word = [Felt::new_unchecked(serial), ZERO, ZERO, ZERO].into();
    let script = scripts[index % scripts.len()].clone();
    let recipient = NoteRecipient::new(
        serial_number,
        script,
        NoteStorage::new(vec![]).expect("empty note storage is valid"),
    );

    NoteDetails::new(NoteAssets::new(vec![]).expect("an empty asset list is valid"), recipient)
}

fn note_metadata(sender: AccountId, index: usize) -> NoteMetadata {
    let tag = NoteTag::from(u32::try_from(index).expect("note index fits in u32"));
    let partial = PartialNoteMetadata::new(sender, NoteType::Public).with_tag(tag);

    NoteMetadata::new(partial, &NoteAttachments::empty())
}

// BLOCK HEADERS
// ================================================================================================

fn mock_block_header(block_num: u32) -> BlockHeader {
    BlockHeader::mock(block_num, None, None, &[], TransactionKernel.to_commitment())
}

// ACCOUNTS
// ================================================================================================

/// Builds `count` distinct wallet accounts. They all share one account code, which is what a store
/// full of wallets looks like and what makes the code reference lookups worth indexing.
pub fn wallet_accounts(count: usize) -> anyhow::Result<Vec<Account>> {
    (0..count)
        .map(|index| {
            let mut init_seed = [0u8; 32];
            let index = u64::try_from(index).expect("account index fits in u64");
            init_seed[0..8].copy_from_slice(&index.to_le_bytes());

            let auth = AuthSingleSig::new(Approver::new(
                PublicKeyCommitment::from(EMPTY_WORD),
                AuthSchemeId::Falcon512Poseidon2,
            ));

            AccountBuilder::new(init_seed)
                .account_type(AccountType::Private)
                .with_component(auth)
                .with_component(BasicWallet)
                .build_existing()
                .map_err(anyhow::Error::from)
        })
        .collect()
}
