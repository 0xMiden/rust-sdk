//! Persistent record and per-round transition types for one PSWAP order.
//!
//! See module-level docs on [`crate::pswap`].

use alloc::collections::BTreeMap;
use alloc::string::ToString;

use miden_protocol::account::AccountId;
use miden_protocol::asset::AssetAmount;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{Note, NoteId, NoteInclusionProof, NoteTag};
use miden_protocol::{Felt, Word};
use miden_standards::note::{PswapNote, PswapNoteAttachment};

use super::errors::PswapLineageError;
use crate::utils::{ByteReader, ByteWriter, Deserializable, DeserializationError, Serializable};

// PSWAP LINEAGE STATE
// ================================================================================================

/// Lifecycle state of a PSWAP order. Discriminants are part of the
/// serialized encoding — do not renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PswapLineageState {
    /// Still fillable / reclaimable.
    Active = 0,
    /// Fully filled. Terminal.
    FullyFilled = 1,
    /// Reclaimed by the creator. Terminal.
    Reclaimed = 2,
}

impl PswapLineageState {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Errors on unknown discriminants — guards against forward-
    /// incompatible serialized encodings.
    pub fn try_from_u8(value: u8) -> Result<Self, PswapLineageError> {
        match value {
            0 => Ok(Self::Active),
            1 => Ok(Self::FullyFilled),
            2 => Ok(Self::Reclaimed),
            other => Err(PswapLineageError::UnknownState(other)),
        }
    }
}

// PSWAP LINEAGE RECORD
// ================================================================================================

/// Persistent record of one PSWAP order's chain state. The order id and creator
/// are mirrored here so the common read paths — keying, filtering — stay
/// lookup-free. Only the remaining *amounts* are stored; the asset pair's faucets,
/// the script/recipient, and the `note_type` all live on the depth-0 note, fetched
/// on demand from `output_notes` by `original_note_id` (see
/// `store::get_original_pswap`) when reconstruction, a depth-0 reclaim, or
/// asset-pair-tag derivation needs them.
#[derive(Debug, Clone)]
pub struct PswapLineageRecord {
    /// Fetch handle for the depth-0 PSWAP note in `output_notes`. Stable for the
    /// order's lifetime; distinct from `current_tip_note_id`, which advances
    /// each round.
    pub original_note_id: NoteId,

    // Immutable order facts, mirrored from the depth-0 note so keying and
    // filtering need no store lookup.
    order_id: Felt,
    creator_account_id: AccountId,

    /// Current tip's note id. Equals `original_note_id` at depth 0; otherwise a
    /// remainder we didn't originate.
    pub current_tip_note_id: NoteId,
    /// 0 for the original tip; +1 per round. Matches `PswapNoteAttachment::depth()`.
    pub current_depth: u32,
    /// Remaining offered amount. The order's offered faucet is chain-invariant and
    /// recovered from the depth-0 note when needed (e.g. for tag derivation).
    pub remaining_offered: AssetAmount,
    /// Remaining requested amount (requested faucet recovered the same way).
    pub remaining_requested: AssetAmount,
    pub state: PswapLineageState,
}

impl PswapLineageRecord {
    /// Builds the depth-0 record for a PSWAP the wallet has just emitted. Mirrors
    /// the order id and creator off the note and seeds the mutable tip state: the
    /// tip is the original note, depth is 0, and `remaining_*` equal the initial
    /// offered/requested amounts.
    pub fn new_depth_zero(original_note_id: NoteId, pswap: &PswapNote) -> Self {
        Self {
            original_note_id,
            order_id: pswap.order_id(),
            creator_account_id: pswap.storage().creator_account_id(),
            current_tip_note_id: original_note_id,
            current_depth: 0,
            remaining_offered: pswap.offered_asset().amount(),
            remaining_requested: pswap.storage().requested_asset().amount(),
            state: PswapLineageState::Active,
        }
    }

    /// Stable identifier (== the depth-0 note's `serial[1]`) shared by every
    /// note in the chain.
    pub fn order_id(&self) -> Felt {
        self.order_id
    }

    /// Account that created the order (recipient of every payback).
    pub fn creator_account_id(&self) -> AccountId {
        self.creator_account_id
    }
}

// PSWAP LINEAGE ROUND UPDATE
// ================================================================================================

/// One round's transition. Fill = payback + remainder (≤1 each); reclaim
/// = no outputs. Applied atomically by `apply_round`.
#[derive(Debug, Clone)]
pub(crate) struct PswapLineageRoundUpdate {
    pub order_id: Felt,
    pub round_depth: u32,
    // Post-round state — all fields below describe the lineage AFTER this round.
    pub remaining_offered: AssetAmount,
    pub remaining_requested: AssetAmount,
    pub state: PswapLineageState,
    /// New tip; `None` for terminal rounds.
    pub tip_note_id: Option<NoteId>,
    /// Commit block's note root, used by `apply_round` to insert payback/remainder as
    /// `Committed`. `None` on reclaim rounds (no note to insert) and in store-tier fixtures.
    pub at_block_note_root: Option<Word>,
    /// Reconstructed payback and its inclusion proof. `None` only on
    /// reclaim. The note and proof are always observed together in the
    /// same sync window, so they live or die as a pair.
    pub payback: Option<(Note, NoteInclusionProof)>,
    /// Reconstructed remainder and its inclusion proof. `None` on terminal
    /// rounds (full fill / reclaim). Paired for the same reason as `payback`.
    pub remainder: Option<(Note, NoteInclusionProof)>,
}

// OBSERVED CHAIN NOTE
// ================================================================================================

/// Observed PSWAP-attachment note. The typed attachment carries
/// `order_id`, `depth`, and amount (fill on payback, payout on remainder)
/// — role distinguished by [`Self::tag`].
#[derive(Debug, Clone)]
pub(crate) struct ObservedPswapNote {
    pub note_id: NoteId,
    pub attachment: PswapNoteAttachment,
    pub sender: AccountId,
    /// Payback uses the P2ID-style tag; remainder uses the asset-pair tag.
    pub tag: NoteTag,
    pub block_num: BlockNumber,
    pub inclusion_proof: NoteInclusionProof,
}

// PER-ROUND CLASSIFICATION AND ADVANCE
// ================================================================================================

impl PswapLineageRecord {
    /// Builds this round's [`PswapLineageRoundUpdate`] from the chain notes observed at
    /// `round_depth`.
    ///
    /// The `(order_id, depth)` bucket is keyed off attachment fields the sender controls, so the
    /// raw note set is untrusted. We **validate then classify**: each candidate is
    /// reconstructed from our stored depth-0 note and kept only if its id matches the observed
    /// note (a forger can't match without actually emitting a genuine payback/remainder of our
    /// order). Classification runs on the surviving genuine notes, never on the raw count.
    ///
    /// Returns `Ok(None)` when the bucket holds no genuine note and the tip wasn't consumed — the
    /// notes are forged/unrelated and this isn't our round, so the caller stops advancing.
    pub(crate) fn build_round_update(
        &self,
        round_depth: u32,
        notes: &[&ObservedPswapNote],
        block_headers: &BTreeMap<BlockNumber, BlockHeader>,
        original_pswap: Option<&PswapNote>,
        tip_consumed: bool,
    ) -> Result<Option<PswapLineageRoundUpdate>, PswapLineageError> {
        // No outputs at all: the only way the round fired is a consumed tip → reclaim.
        if notes.is_empty() {
            return Ok(Some(self.build_reclaim_round(round_depth)));
        }

        // Fetched by the caller before any fill round; absence is a broken invariant.
        let pswap = original_pswap
            .ok_or(PswapLineageError::OriginalNoteUnavailable(self.original_note_id))?;
        let payback_tag = pswap.storage().payback_note_tag();

        // The genuine payback anchors the round; forged candidates reconstruct to a different id
        // and fall away. More than one genuine payback at a depth is impossible (one tip →
        // one fill), so the first match is the one.
        let Some((observed_payback, payback_note)) = notes
            .iter()
            .copied()
            .filter(|note| note.tag == payback_tag)
            .find_map(|note| validate_payback(pswap, note).map(|recon| (note, recon)))
        else {
            // No valid fill: a consumed tip with no genuine payback is a reclaim; otherwise the
            // notes are forged against a still-live tip and this isn't our round.
            return Ok(tip_consumed.then(|| self.build_reclaim_round(round_depth)));
        };

        // Requested amount filled this round, read straight off the validated payback's attachment.
        let fill_amount = observed_payback.attachment.amount();

        // A genuine remainder (if any) is validated against the post-round balances derived from
        // the payback's fill. Present → partial fill; absent → full fill.
        let remainder =
            notes.iter().copied().filter(|note| note.tag != payback_tag).find_map(|note| {
                self.validate_remainder(pswap, note, fill_amount).map(|recon| (note, recon))
            });

        Ok(Some(match remainder {
            Some((observed_remainder, remainder_note)) => self.build_partial_fill_round(
                round_depth,
                observed_payback,
                payback_note,
                observed_remainder,
                remainder_note,
                fill_amount,
                block_headers,
            ),
            None => self.build_full_fill_round(
                round_depth,
                observed_payback,
                payback_note,
                block_headers,
            ),
        }))
    }

    /// The genuine remainder for `observed`, if it reconstructs (against the post-round balances
    /// derived from `fill_amount` and the candidate's own payout) to a matching id; `None`
    /// otherwise.
    fn validate_remainder(
        &self,
        pswap: &PswapNote,
        observed: &ObservedPswapNote,
        fill_amount: AssetAmount,
    ) -> Option<Note> {
        let payout_amount = observed.attachment.amount();
        let (remaining_offered, remaining_requested) =
            self.remaining_after_fill(fill_amount, payout_amount);
        let remainder_note = pswap
            .remainder_note(
                observed.sender,
                &observed.attachment,
                remaining_offered,
                remaining_requested,
            )
            .ok()?;
        (remainder_note.id() == observed.note_id).then_some(remainder_note)
    }

    /// Saturating post-round balances after filling `fill_amount` (requested) for `payout_amount`
    /// (offered). Clamps to zero on over-fill.
    fn remaining_after_fill(
        &self,
        fill_amount: AssetAmount,
        payout_amount: AssetAmount,
    ) -> (AssetAmount, AssetAmount) {
        (
            saturating_sub(self.remaining_offered, payout_amount),
            saturating_sub(self.remaining_requested, fill_amount),
        )
    }

    /// Reclaim — cancel branch emits no outputs; only the creator can hit it.
    fn build_reclaim_round(&self, round_depth: u32) -> PswapLineageRoundUpdate {
        PswapLineageRoundUpdate {
            order_id: self.order_id(),
            round_depth,
            remaining_offered: AssetAmount::ZERO,
            remaining_requested: AssetAmount::ZERO,
            state: PswapLineageState::Reclaimed,
            tip_note_id: None,
            at_block_note_root: None,
            payback: None,
            remainder: None,
        }
    }

    /// Full fill — only payback emitted; both `remaining_*` → 0. Takes the already-validated
    /// `payback_note` and its observed note (for the inclusion proof and commit block).
    fn build_full_fill_round(
        &self,
        round_depth: u32,
        observed_payback: &ObservedPswapNote,
        payback_note: Note,
        block_headers: &BTreeMap<BlockNumber, BlockHeader>,
    ) -> PswapLineageRoundUpdate {
        PswapLineageRoundUpdate {
            order_id: self.order_id(),
            round_depth,
            remaining_offered: AssetAmount::ZERO,
            remaining_requested: AssetAmount::ZERO,
            state: PswapLineageState::FullyFilled,
            tip_note_id: None,
            at_block_note_root: block_headers
                .get(&observed_payback.block_num)
                .map(BlockHeader::note_root),
            payback: Some((payback_note, observed_payback.inclusion_proof.clone())),
            remainder: None,
        }
    }

    /// Partial fill — payback + remainder. Takes the already-validated notes; balances come from
    /// the payback's `fill_amount` and the remainder's payout.
    #[allow(clippy::too_many_arguments)]
    fn build_partial_fill_round(
        &self,
        round_depth: u32,
        observed_payback: &ObservedPswapNote,
        payback_note: Note,
        observed_remainder: &ObservedPswapNote,
        remainder_note: Note,
        fill_amount: AssetAmount,
        block_headers: &BTreeMap<BlockNumber, BlockHeader>,
    ) -> PswapLineageRoundUpdate {
        let payout_amount = observed_remainder.attachment.amount();
        let (remaining_offered, remaining_requested) =
            self.remaining_after_fill(fill_amount, payout_amount);

        PswapLineageRoundUpdate {
            order_id: self.order_id(),
            round_depth,
            remaining_offered,
            remaining_requested,
            state: PswapLineageState::Active,
            tip_note_id: Some(observed_remainder.note_id),
            at_block_note_root: block_headers
                .get(&observed_payback.block_num)
                .map(BlockHeader::note_root),
            payback: Some((payback_note, observed_payback.inclusion_proof.clone())),
            remainder: Some((remainder_note, observed_remainder.inclusion_proof.clone())),
        }
    }

    /// Returns the post-round version of the record. Drives the same-block multi-fill loop in
    /// `discovery`, and is reused by `store::apply_round` to compute the persisted advance.
    pub(crate) fn advance(mut self, update: &PswapLineageRoundUpdate) -> PswapLineageRecord {
        self.current_depth = update.round_depth;
        self.remaining_offered = update.remaining_offered;
        self.remaining_requested = update.remaining_requested;
        self.state = update.state;
        if let Some(note_id) = update.tip_note_id {
            self.current_tip_note_id = note_id;
        }
        self
    }
}

/// The genuine payback for `observed`, if it reconstructs to a matching id; `None` otherwise
/// (forged, unrelated, or unreconstructable — all skipped, never trusted).
fn validate_payback(pswap: &PswapNote, observed: &ObservedPswapNote) -> Option<Note> {
    let payback_note = pswap.payback_note(observed.sender, &observed.attachment).ok()?;
    (payback_note.id() == observed.note_id).then_some(payback_note)
}

/// `total - used`, clamped to zero — an over-fill can't drive a balance negative.
fn saturating_sub(total: AssetAmount, used: AssetAmount) -> AssetAmount {
    AssetAmount::new(total.as_u64().saturating_sub(used.as_u64()))
        .expect("a value <= an existing AssetAmount is itself a valid AssetAmount")
}

// PSWAP LINEAGE FILTER
// ================================================================================================

/// Client-side filter for `crate::pswap::store::list_lineages`. Applied in
/// Rust after a prefix-scan of the `settings` KV — not a store-trait concept.
#[derive(Debug, Clone)]
pub(crate) enum PswapLineageFilter {
    All,
    Active,
    ByCreator(AccountId),
}

// SERDE HELPERS
// ================================================================================================

/// Builds a [`PswapLineageRecord`] from its decoded fields. Lives here (not
/// in a store backend) so alternative backends can reuse it. The only validation
/// is decoding the `state_byte` into a known [`PswapLineageState`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_record_from_fields(
    original_note_id: NoteId,
    order_id: Felt,
    creator_account_id: AccountId,
    current_tip_note_id: NoteId,
    current_depth: u32,
    remaining_offered: AssetAmount,
    remaining_requested: AssetAmount,
    state_byte: u8,
) -> Result<PswapLineageRecord, PswapLineageError> {
    Ok(PswapLineageRecord {
        original_note_id,
        order_id,
        creator_account_id,
        current_tip_note_id,
        current_depth,
        remaining_offered,
        remaining_requested,
        state: PswapLineageState::try_from_u8(state_byte)?,
    })
}

// VALUE CODEC
// ================================================================================================

/// Encodes the record's fields in declaration order: the `original_note_id` fetch handle, the
/// mirrored order id and creator, then the mutable tip state. Only the remaining *amounts* are
/// written — the faucets and full note live on the depth-0 note, recovered via `original_note_id`
/// when needed.
impl Serializable for PswapLineageRecord {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.original_note_id.write_into(target);
        self.order_id.write_into(target);
        self.creator_account_id.write_into(target);
        self.current_tip_note_id.write_into(target);
        self.current_depth.write_into(target);
        self.remaining_offered.write_into(target);
        self.remaining_requested.write_into(target);
        self.state.as_u8().write_into(target);
    }
}

impl Deserializable for PswapLineageRecord {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let original_note_id = NoteId::read_from(source)?;
        let order_id = Felt::read_from(source)?;
        let creator_account_id = AccountId::read_from(source)?;
        let current_tip_note_id = NoteId::read_from(source)?;
        let current_depth = u32::read_from(source)?;
        let remaining_offered = AssetAmount::read_from(source)?;
        let remaining_requested = AssetAmount::read_from(source)?;
        let state_byte = u8::read_from(source)?;
        build_record_from_fields(
            original_note_id,
            order_id,
            creator_account_id,
            current_tip_note_id,
            current_depth,
            remaining_offered,
            remaining_requested,
            state_byte,
        )
        .map_err(|err| DeserializationError::InvalidValue(err.to_string()))
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
    //! Synthetic-PSWAP factory shared across lineage / discovery / store tests.

    use miden_protocol::Word;
    use miden_protocol::account::AccountId;
    use miden_protocol::asset::FungibleAsset;
    use miden_protocol::note::NoteType;
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE_2,
    };
    use miden_standards::note::{PswapNote, PswapNoteStorage};

    /// Returns `(sender, creator, offered_faucet, requested_faucet)` —
    /// four distinct testing `AccountId`s chosen to satisfy PSWAP's
    /// faucet-distinctness invariant.
    pub fn fixed_account_ids() -> (AccountId, AccountId, AccountId, AccountId) {
        (
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap(),
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE_2).unwrap(),
            AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET).unwrap(),
            AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1).unwrap(),
        )
    }

    /// Builds a fully-formed [`PswapNote`] for use in tests. Defaults:
    /// public note type, 100-unit offered, 50-unit requested, serial
    /// number `[1, 2, 3, 4]`. Override via the params.
    pub fn build_test_pswap(
        sender: AccountId,
        creator: AccountId,
        offered_faucet: AccountId,
        offered_amount: u64,
        requested_faucet: AccountId,
        requested_amount: u64,
    ) -> PswapNote {
        let offered = FungibleAsset::new(offered_faucet, offered_amount).unwrap();
        let requested = FungibleAsset::new(requested_faucet, requested_amount).unwrap();
        let storage = PswapNoteStorage::builder()
            .requested_asset(requested)
            .creator_account_id(creator)
            .build();
        PswapNote::builder()
            .sender(sender)
            .storage(storage)
            .serial_number(Word::from([
                miden_protocol::Felt::new(1).unwrap(),
                miden_protocol::Felt::new(2).unwrap(),
                miden_protocol::Felt::new(3).unwrap(),
                miden_protocol::Felt::new(4).unwrap(),
            ]))
            .note_type(NoteType::Public)
            .offered_asset(offered)
            .build()
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use miden_protocol::asset::AssetAmount;
    use miden_protocol::crypto::merkle::SparseMerklePath;
    use miden_standards::note::PswapNote;

    use super::test_helpers::{build_test_pswap, fixed_account_ids};
    use super::*;

    /// Builds a record from a test `PswapNote`, mirroring the immutable scalars
    /// the observer would extract. Keeps the codec/accessor tests focused on the
    /// fields they exercise instead of the new wide constructor signature.
    fn record_from_test_pswap(
        pswap: &PswapNote,
        current_tip_note_id: NoteId,
        current_depth: u32,
        remaining_offered: u64,
        remaining_requested: u64,
        state_byte: u8,
    ) -> Result<PswapLineageRecord, PswapLineageError> {
        let original_note_id = miden_protocol::note::Note::from(pswap.clone()).id();
        build_record_from_fields(
            original_note_id,
            pswap.order_id(),
            pswap.storage().creator_account_id(),
            current_tip_note_id,
            current_depth,
            AssetAmount::new(remaining_offered).unwrap(),
            AssetAmount::new(remaining_requested).unwrap(),
            state_byte,
        )
    }

    /// Stable byte encoding of `PswapLineageState`. The values are
    /// persisted in the serialized lineage record; reordering
    /// would silently corrupt existing stores.
    #[test]
    fn state_byte_encoding_is_stable() {
        assert_eq!(PswapLineageState::Active.as_u8(), 0);
        assert_eq!(PswapLineageState::FullyFilled.as_u8(), 1);
        assert_eq!(PswapLineageState::Reclaimed.as_u8(), 2);
    }

    /// Round-trip every state via `try_from_u8`. Belt-and-suspenders
    /// against a future renumbering breaking the serialized format.
    #[test]
    fn state_try_from_u8_round_trips_known_variants() {
        for state in [
            PswapLineageState::Active,
            PswapLineageState::FullyFilled,
            PswapLineageState::Reclaimed,
        ] {
            assert_eq!(PswapLineageState::try_from_u8(state.as_u8()).unwrap(), state);
        }
    }

    /// Unknown discriminants must error — defends against a future
    /// store reading a forward-incompatible byte.
    #[test]
    fn state_try_from_u8_rejects_unknown() {
        match PswapLineageState::try_from_u8(99) {
            Err(PswapLineageError::UnknownState(99)) => {},
            other => panic!("expected UnknownState(99), got {other:?}"),
        }
    }

    /// Happy path for `build_record_from_fields` at depth 0.
    #[test]
    fn build_record_from_fields_accepts_valid_depth_zero_record() {
        let (sender, creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let pswap = build_test_pswap(sender, creator, offered_faucet, 100, requested_faucet, 50);
        let initial_note_id = miden_protocol::note::Note::from(pswap.clone()).id();

        let record = record_from_test_pswap(
            &pswap,
            initial_note_id,
            0,
            100,
            50,
            PswapLineageState::Active.as_u8(),
        )
        .unwrap();

        assert_eq!(record.current_depth, 0);
        assert_eq!(record.remaining_offered, AssetAmount::new(100).unwrap());
        assert_eq!(record.remaining_requested, AssetAmount::new(50).unwrap());
        assert_eq!(record.state, PswapLineageState::Active);
    }

    /// Happy path at `current_depth > 0`.
    #[test]
    fn build_record_from_fields_accepts_valid_advanced_record() {
        let (sender, creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let pswap = build_test_pswap(sender, creator, offered_faucet, 100, requested_faucet, 50);
        let note = miden_protocol::note::Note::from(pswap.clone());
        let record =
            record_from_test_pswap(&pswap, note.id(), 3, 70, 35, PswapLineageState::Active.as_u8())
                .unwrap();

        assert_eq!(record.current_depth, 3);
        assert_eq!(record.remaining_offered, AssetAmount::new(70).unwrap());
    }

    /// Unknown state discriminant in a stored record bubbles up as `UnknownState`.
    #[test]
    fn build_record_from_fields_rejects_unknown_state() {
        let (sender, creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let pswap = build_test_pswap(sender, creator, offered_faucet, 100, requested_faucet, 50);
        let note = miden_protocol::note::Note::from(pswap.clone());
        match record_from_test_pswap(&pswap, note.id(), 0, 100, 50, 42) {
            Err(PswapLineageError::UnknownState(42)) => {},
            other => panic!("expected UnknownState(42), got {other:?}"),
        }
    }

    /// The mirrored scalars back the accessors with the same values the depth-0 note would yield,
    /// so `order_id()` and `creator_account_id()` stay correct without re-fetching the note.
    #[test]
    fn accessors_mirror_depth_zero_note() {
        let (sender, creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let pswap = build_test_pswap(sender, creator, offered_faucet, 100, requested_faucet, 50);

        let expected_order_id = pswap.order_id();

        let note = miden_protocol::note::Note::from(pswap.clone());
        let record = record_from_test_pswap(
            &pswap,
            note.id(),
            0,
            100,
            50,
            PswapLineageState::Active.as_u8(),
        )
        .unwrap();

        assert_eq!(record.original_note_id, note.id());
        assert_eq!(record.order_id(), expected_order_id);
        assert_eq!(record.creator_account_id(), creator);
    }

    /// `Serializable`/`Deserializable` round-trip preserves every field. Exercised at an advanced
    /// depth with reduced amounts to catch an offered/requested mix-up.
    #[test]
    fn value_codec_round_trips() {
        let (sender, creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let pswap = build_test_pswap(sender, creator, offered_faucet, 100, requested_faucet, 50);
        let note = miden_protocol::note::Note::from(pswap.clone());
        let record =
            record_from_test_pswap(&pswap, note.id(), 3, 70, 35, PswapLineageState::Active.as_u8())
                .unwrap();

        let bytes = record.to_bytes();
        let decoded = PswapLineageRecord::read_from_bytes(&bytes).unwrap();

        assert_eq!(decoded.original_note_id, record.original_note_id);
        assert_eq!(decoded.creator_account_id(), record.creator_account_id());
        assert_eq!(decoded.order_id(), record.order_id());
        assert_eq!(decoded.current_tip_note_id, record.current_tip_note_id);
        assert_eq!(decoded.current_depth, record.current_depth);
        assert_eq!(decoded.remaining_offered, record.remaining_offered);
        assert_eq!(decoded.remaining_requested, record.remaining_requested);
        assert_eq!(decoded.remaining_offered, AssetAmount::new(70).unwrap());
        assert_eq!(decoded.remaining_requested, AssetAmount::new(35).unwrap());
        assert_eq!(decoded.state, record.state);
    }

    // PER-ROUND CLASSIFICATION TESTS
    // --------------------------------------------------------------------------------------------

    /// Minimum-valid inclusion proof — correlator never inspects the path.
    fn dummy_inclusion_proof(block: u32) -> NoteInclusionProof {
        let path =
            SparseMerklePath::from_parts(0, Vec::new()).expect("empty SparseMerklePath is valid");
        NoteInclusionProof::new(BlockNumber::from(block), 0, path)
            .expect("zero index is well below the per-block notes ceiling")
    }

    /// Empty header map — these tests don't assert on inserted-note state.
    fn no_block_headers() -> BTreeMap<BlockNumber, BlockHeader> {
        BTreeMap::new()
    }

    /// Active lineage at depth 0 built from a fresh test PSWAP.
    fn initial_record(pswap: &PswapNote, offered: u64, requested: u64) -> PswapLineageRecord {
        let original_note_id = Note::from(pswap.clone()).id();
        let mut record = PswapLineageRecord::new_depth_zero(original_note_id, pswap);
        // Override the seeded remaining_* so callers can exercise reduced balances.
        record.remaining_offered =
            AssetAmount::new(offered).expect("test value fits in AssetAmount");
        record.remaining_requested =
            AssetAmount::new(requested).expect("test value fits in AssetAmount");
        record
    }

    /// `ObservedPswapNote` mirroring `note` (id + tag) so the
    /// correlator's tag-based payback/remainder split works.
    fn chain_update_from(
        note: &Note,
        attachment: PswapNoteAttachment,
        sender: AccountId,
        block: u32,
    ) -> ObservedPswapNote {
        ObservedPswapNote {
            note_id: note.id(),
            attachment,
            sender,
            tag: note.metadata().tag(),
            block_num: BlockNumber::from(block),
            inclusion_proof: dummy_inclusion_proof(block),
        }
    }

    /// A forged candidate: claims `tag` + `attachment` for our order but carries a `note_id` that
    /// won't match any reconstruction (here, the depth-0 note's id).
    fn forged_note(
        forged_id: NoteId,
        attachment: PswapNoteAttachment,
        tag: NoteTag,
        sender: AccountId,
        block: u32,
    ) -> ObservedPswapNote {
        ObservedPswapNote {
            note_id: forged_id,
            attachment,
            sender,
            tag,
            block_num: BlockNumber::from(block),
            inclusion_proof: dummy_inclusion_proof(block),
        }
    }

    /// Unwraps a successful, present round update.
    fn expect_round(
        result: Result<Option<PswapLineageRoundUpdate>, PswapLineageError>,
    ) -> PswapLineageRoundUpdate {
        result
            .expect("build_round_update should not error")
            .expect("expected a round update")
    }

    /// 2-candidate partial fill → `Active`, both `remaining_*` reduced.
    #[test]
    fn build_round_update_partial_fill_advances_active() {
        let (_sender, _creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let consumer = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        )
        .unwrap();
        let creator = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE_2,
        )
        .unwrap();

        let pswap = build_test_pswap(consumer, creator, offered_faucet, 100, requested_faucet, 50);
        let record = initial_record(&pswap, 100, 50);

        let fill_amount = 20;
        let payout_amount = 40;
        let new_off = 100 - payout_amount;
        let new_req = 50 - fill_amount;

        let payback_att =
            PswapNoteAttachment::new(AssetAmount::new(fill_amount).unwrap(), pswap.order_id(), 1);
        let remainder_att =
            PswapNoteAttachment::new(AssetAmount::new(payout_amount).unwrap(), pswap.order_id(), 1);
        let payback = pswap.payback_note(consumer, &payback_att).unwrap();
        let remainder = pswap
            .remainder_note(
                consumer,
                &remainder_att,
                AssetAmount::new(new_off).unwrap(),
                AssetAmount::new(new_req).unwrap(),
            )
            .unwrap();

        let cand_payback = chain_update_from(&payback, payback_att, consumer, 7);
        let cand_remainder = chain_update_from(&remainder, remainder_att, consumer, 7);

        let update = expect_round(record.build_round_update(
            1,
            &[&cand_payback, &cand_remainder],
            &no_block_headers(),
            Some(&pswap),
            true,
        ));

        assert_eq!(update.round_depth, 1);
        assert_eq!(update.remaining_offered, AssetAmount::new(new_off).unwrap());
        assert_eq!(update.remaining_requested, AssetAmount::new(new_req).unwrap());
        assert_eq!(update.state, PswapLineageState::Active);
        assert_eq!(update.tip_note_id, Some(remainder.id()));
        // Each side carries its note paired with its inclusion proof.
        assert!(update.payback.is_some());
        assert!(update.remainder.is_some());
    }

    /// Note order within a round must not change classification: passing
    /// `[remainder, payback]` (the reverse of the natural ordering) yields the
    /// same result as `[payback, remainder]`. Covers the tag-split else-branch.
    #[test]
    fn build_round_update_partial_fill_classifies_regardless_of_note_order() {
        let (_sender, _creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let consumer = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        )
        .unwrap();
        let creator = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE_2,
        )
        .unwrap();

        let pswap = build_test_pswap(consumer, creator, offered_faucet, 100, requested_faucet, 50);
        let record = initial_record(&pswap, 100, 50);

        let fill_amount = 20;
        let payout_amount = 40;
        let new_off = 100 - payout_amount;
        let new_req = 50 - fill_amount;

        let payback_att =
            PswapNoteAttachment::new(AssetAmount::new(fill_amount).unwrap(), pswap.order_id(), 1);
        let remainder_att =
            PswapNoteAttachment::new(AssetAmount::new(payout_amount).unwrap(), pswap.order_id(), 1);
        let payback = pswap.payback_note(consumer, &payback_att).unwrap();
        let remainder = pswap
            .remainder_note(
                consumer,
                &remainder_att,
                AssetAmount::new(new_off).unwrap(),
                AssetAmount::new(new_req).unwrap(),
            )
            .unwrap();

        let cand_payback = chain_update_from(&payback, payback_att, consumer, 7);
        let cand_remainder = chain_update_from(&remainder, remainder_att, consumer, 7);

        // Reverse the input order — remainder first.
        let update = expect_round(record.build_round_update(
            1,
            &[&cand_remainder, &cand_payback],
            &no_block_headers(),
            Some(&pswap),
            true,
        ));

        assert_eq!(update.tip_note_id, Some(remainder.id()));
        assert_eq!(update.state, PswapLineageState::Active);
    }

    /// A candidate that can't be reconstructed (here, a `depth == 0` attachment that trips
    /// `payback_note`'s "depth must be >= 1" guard) is *filtered*, not fatal: with the tip still
    /// live and no genuine note, the round yields `Ok(None)` and the lineage stays at its tip. This
    /// non-fatal skip is what stops a single malformed forged note from stalling the chain.
    #[test]
    fn build_round_update_filters_unreconstructable_candidate() {
        let (_sender, _creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let consumer = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        )
        .unwrap();
        let creator = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE_2,
        )
        .unwrap();

        let pswap = build_test_pswap(consumer, creator, offered_faucet, 100, requested_faucet, 50);
        let record = initial_record(&pswap, 100, 50);

        // Carries the payback tag (so it reaches reconstruction) but a depth-0 attachment that
        // makes `payback_note` fail — `validate_payback` returns `None` and the candidate
        // is skipped.
        let good_att = PswapNoteAttachment::new(AssetAmount::new(20).unwrap(), pswap.order_id(), 1);
        let payback = pswap.payback_note(consumer, &good_att).unwrap();
        let bad_att = PswapNoteAttachment::new(AssetAmount::new(20).unwrap(), pswap.order_id(), 0);
        let cand = forged_note(payback.id(), bad_att, payback.metadata().tag(), consumer, 5);

        // Tip still live → no genuine note → no round.
        let result =
            record.build_round_update(1, &[&cand], &no_block_headers(), Some(&pswap), false);
        assert!(
            matches!(result, Ok(None)),
            "unreconstructable candidate must be filtered, not fatal"
        );
    }

    /// 1-candidate full fill → `FullyFilled`, no remainder, both zeros.
    #[test]
    fn build_round_update_full_fill_marks_fully_filled() {
        let (_sender, _creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let consumer = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        )
        .unwrap();
        let creator = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE_2,
        )
        .unwrap();

        // Smaller initial sizes so the single fill exhausts both sides.
        let pswap = build_test_pswap(consumer, creator, offered_faucet, 30, requested_faucet, 50);
        let record = initial_record(&pswap, 30, 50);

        let fill_amount = 50; // exhausts requested side
        let payback_att =
            PswapNoteAttachment::new(AssetAmount::new(fill_amount).unwrap(), pswap.order_id(), 1);
        let payback = pswap.payback_note(consumer, &payback_att).unwrap();
        let cand = chain_update_from(&payback, payback_att, consumer, 9);

        let update = expect_round(record.build_round_update(
            1,
            &[&cand],
            &no_block_headers(),
            Some(&pswap),
            true,
        ));

        assert_eq!(update.state, PswapLineageState::FullyFilled);
        assert_eq!(update.remaining_offered, AssetAmount::ZERO);
        assert_eq!(update.remaining_requested, AssetAmount::ZERO);
        assert_eq!(update.tip_note_id, None);
        assert!(update.remainder.is_none());
    }

    /// 0-candidate consumption → `Reclaimed`, both `remaining_*` zeroed.
    /// Regression guard.
    #[test]
    fn build_round_update_zero_outputs_marks_reclaimed_with_remaining_zero() {
        let (_sender, _creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let consumer = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        )
        .unwrap();
        let creator = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE_2,
        )
        .unwrap();

        let pswap = build_test_pswap(consumer, creator, offered_faucet, 80, requested_faucet, 40);
        let record = initial_record(&pswap, 80, 40);

        let update =
            expect_round(record.build_round_update(1, &[], &no_block_headers(), None, true));

        assert_eq!(update.state, PswapLineageState::Reclaimed);
        assert_eq!(update.remaining_offered, AssetAmount::ZERO);
        // Regression: reclaim used to leak the pre-reclaim
        // `remaining_requested` into the terminal record.
        assert_eq!(update.remaining_requested, AssetAmount::ZERO);
        assert!(update.payback.is_none());
    }

    /// Same-block multi-fill: round 2 must build against round 1's
    /// in-memory-advanced lineage, not the original.
    #[test]
    fn advance_chains_correctly_for_multi_fill() {
        let (_sender, _creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let consumer = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        )
        .unwrap();
        let creator = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE_2,
        )
        .unwrap();

        let pswap = build_test_pswap(consumer, creator, offered_faucet, 100, requested_faucet, 50);
        let record0 = initial_record(&pswap, 100, 50);

        // ── Round 1: partial fill, 20 requested for 40 offered.
        let fill1 = 20;
        let payout1 = 40;
        let new_off1 = 100 - payout1;
        let new_req1 = 50 - fill1;
        let payback_att1 =
            PswapNoteAttachment::new(AssetAmount::new(fill1).unwrap(), pswap.order_id(), 1);
        let remainder_att1 =
            PswapNoteAttachment::new(AssetAmount::new(payout1).unwrap(), pswap.order_id(), 1);
        let payback1 = pswap.payback_note(consumer, &payback_att1).unwrap();
        let remainder1 = pswap
            .remainder_note(
                consumer,
                &remainder_att1,
                AssetAmount::new(new_off1).unwrap(),
                AssetAmount::new(new_req1).unwrap(),
            )
            .unwrap();
        let payback_cand = chain_update_from(&payback1, payback_att1, consumer, 11);
        let remainder_cand = chain_update_from(&remainder1, remainder_att1, consumer, 11);

        let update1 = expect_round(record0.build_round_update(
            1,
            &[&payback_cand, &remainder_cand],
            &no_block_headers(),
            Some(&pswap),
            true,
        ));

        // Mirrors `discover_pswap_rounds`'s inner loop.
        let record1 = record0.advance(&update1);
        assert_eq!(record1.current_depth, 1);
        assert_eq!(record1.remaining_offered, AssetAmount::new(new_off1).unwrap());
        assert_eq!(record1.remaining_requested, AssetAmount::new(new_req1).unwrap());
        assert_eq!(record1.current_tip_note_id, remainder1.id());
        assert_eq!(record1.state, PswapLineageState::Active);

        // ── Round 2: full fill of the remainder, exhausts requested side.
        let fill2 = new_req1; // = 30
        let payback_att2 =
            PswapNoteAttachment::new(AssetAmount::new(fill2).unwrap(), pswap.order_id(), 2);
        let payback2 = pswap.payback_note(consumer, &payback_att2).unwrap();
        let cand_p2 = chain_update_from(&payback2, payback_att2, consumer, 11);

        let update2 = expect_round(record1.build_round_update(
            2,
            &[&cand_p2],
            &no_block_headers(),
            Some(&pswap),
            true,
        ));

        assert_eq!(update2.round_depth, 2);
        assert_eq!(update2.state, PswapLineageState::FullyFilled);
        assert_eq!(update2.remaining_offered, AssetAmount::ZERO);
        assert_eq!(update2.remaining_requested, AssetAmount::ZERO);

        let record2 = record1.advance(&update2);
        assert_eq!(record2.state, PswapLineageState::FullyFilled);
        let emitted = [update1, update2];
        assert_eq!(emitted.len(), 2);
    }

    /// A payback-tagged note whose id doesn't match the reconstruction is a forgery and is
    /// filtered. With the tip still live there's no genuine payback → no round; with the tip
    /// consumed the forged-only bucket reads as a reclaim.
    #[test]
    fn build_round_update_filters_forged_payback() {
        let (_sender, _creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let consumer = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        )
        .unwrap();
        let creator = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE_2,
        )
        .unwrap();

        let pswap = build_test_pswap(consumer, creator, offered_faucet, 100, requested_faucet, 50);
        let record = initial_record(&pswap, 100, 50);

        let att = PswapNoteAttachment::new(AssetAmount::new(20).unwrap(), pswap.order_id(), 1);
        let genuine_payback = pswap.payback_note(consumer, &att).unwrap();
        // Same payback tag + attachment, but the depth-0 note's id — won't match the
        // reconstruction.
        let forged = forged_note(
            Note::from(pswap.clone()).id(),
            att,
            genuine_payback.metadata().tag(),
            consumer,
            7,
        );

        // Tip still live → forged-only bucket → not our round.
        assert!(matches!(
            record.build_round_update(1, &[&forged], &no_block_headers(), Some(&pswap), false,),
            Ok(None)
        ));

        // Tip consumed → forged-only bucket → it was a reclaim.
        let reclaim = expect_round(record.build_round_update(
            1,
            &[&forged],
            &no_block_headers(),
            Some(&pswap),
            true,
        ));
        assert_eq!(reclaim.state, PswapLineageState::Reclaimed);
    }

    /// A genuine payback plus a forged remainder-tagged note: the forgery is filtered, so the round
    /// is classified as a full fill (no remainder), never used as the tip.
    #[test]
    fn build_round_update_forged_remainder_yields_full_fill() {
        let (_sender, _creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let consumer = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        )
        .unwrap();
        let creator = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE_2,
        )
        .unwrap();

        let pswap = build_test_pswap(consumer, creator, offered_faucet, 100, requested_faucet, 50);
        let record = initial_record(&pswap, 100, 50);

        let payback_att =
            PswapNoteAttachment::new(AssetAmount::new(20).unwrap(), pswap.order_id(), 1);
        let payback = pswap.payback_note(consumer, &payback_att).unwrap();
        let cand_payback = chain_update_from(&payback, payback_att, consumer, 7);

        // Remainder tag + plausible attachment, but a non-matching id → forged.
        let remainder_att =
            PswapNoteAttachment::new(AssetAmount::new(40).unwrap(), pswap.order_id(), 1);
        let genuine_remainder = pswap
            .remainder_note(
                consumer,
                &remainder_att,
                AssetAmount::new(60).unwrap(),
                AssetAmount::new(30).unwrap(),
            )
            .unwrap();
        let forged_remainder = forged_note(
            Note::from(pswap.clone()).id(),
            remainder_att,
            genuine_remainder.metadata().tag(),
            consumer,
            7,
        );

        let update = expect_round(record.build_round_update(
            1,
            &[&cand_payback, &forged_remainder],
            &no_block_headers(),
            Some(&pswap),
            true,
        ));
        assert_eq!(
            update.state,
            PswapLineageState::FullyFilled,
            "forged remainder filtered → full fill"
        );
        assert!(update.remainder.is_none());
    }

    /// A padded bucket — genuine payback + genuine remainder + an extra forged note — still
    /// classifies as a partial fill; the forgery is dropped, never tripping an ambiguity error.
    #[test]
    fn build_round_update_bucket_padding_stays_partial() {
        let (_sender, _creator, offered_faucet, requested_faucet) = fixed_account_ids();
        let consumer = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        )
        .unwrap();
        let creator = AccountId::try_from(
            miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE_2,
        )
        .unwrap();

        let pswap = build_test_pswap(consumer, creator, offered_faucet, 100, requested_faucet, 50);
        let record = initial_record(&pswap, 100, 50);

        let payback_att =
            PswapNoteAttachment::new(AssetAmount::new(20).unwrap(), pswap.order_id(), 1);
        let remainder_att =
            PswapNoteAttachment::new(AssetAmount::new(40).unwrap(), pswap.order_id(), 1);
        let payback = pswap.payback_note(consumer, &payback_att).unwrap();
        let remainder = pswap
            .remainder_note(
                consumer,
                &remainder_att,
                AssetAmount::new(60).unwrap(),
                AssetAmount::new(30).unwrap(),
            )
            .unwrap();
        let cand_payback = chain_update_from(&payback, payback_att, consumer, 7);
        let cand_remainder = chain_update_from(&remainder, remainder_att, consumer, 7);
        // Forged extra, payback-tagged, placed first to prove it's skipped before the genuine one.
        let forged = forged_note(
            Note::from(pswap.clone()).id(),
            payback_att,
            payback.metadata().tag(),
            consumer,
            7,
        );

        let update = expect_round(record.build_round_update(
            1,
            &[&forged, &cand_payback, &cand_remainder],
            &no_block_headers(),
            Some(&pswap),
            true,
        ));
        assert_eq!(
            update.state,
            PswapLineageState::Active,
            "forgery dropped → still a partial fill"
        );
        assert_eq!(update.tip_note_id, Some(remainder.id()));
    }
}
