//! Defines the persisted PSWAP lineage types shared by the client and the store implementations.

use miden_protocol::Felt;
use miden_protocol::account::AccountId;
use miden_protocol::asset::AssetAmount;
use miden_protocol::note::NoteId;
use miden_protocol::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

// PSWAP LINEAGE STATE
// ================================================================================================

/// Lifecycle state of a PSWAP order. Discriminants are part of the serialized encoding — do not
/// renumber.
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

    /// Errors on unknown discriminants, guards against forward-incompatible serialized encodings.
    pub fn try_from_u8(value: u8) -> Result<Self, DeserializationError> {
        match value {
            0 => Ok(Self::Active),
            1 => Ok(Self::FullyFilled),
            2 => Ok(Self::Reclaimed),
            other => Err(DeserializationError::InvalidValue(format!(
                "unknown PSWAP lineage state discriminant {other}"
            ))),
        }
    }
}

// PSWAP LINEAGE RECORD
// ================================================================================================

/// Persistent record of one PSWAP order's chain state. The order id and creator are mirrored here
/// so the common read paths — keying, filtering — stay lookup-free. Only the remaining *amounts*
/// are stored; the asset pair's faucets, the script/recipient, and the `note_type` all live on the
/// depth-0 note, fetched on demand from the output notes by `original_note_id` when reconstruction,
/// a depth-0 reclaim, or asset-pair-tag derivation needs them.
#[derive(Debug, Clone)]
pub struct PswapLineageRecord {
    /// Fetch handle for the depth-0 PSWAP note in `output_notes`. Stable for the order's lifetime;
    /// distinct from `current_tip_note_id`, which advances each round.
    pub original_note_id: NoteId,

    // Immutable order facts, mirrored from the depth-0 note so keying and filtering need no store
    // lookup.
    order_id: Felt,
    creator_account_id: AccountId,

    /// Current tip's note id. Equals `original_note_id` at depth 0; otherwise a remainder we didn't
    /// originate.
    pub current_tip_note_id: NoteId,
    /// 0 for the original tip; +1 per round. Matches `PswapNoteAttachment::depth()`.
    pub current_depth: u32,
    /// Remaining offered amount. The order's offered faucet is chain-invariant and recovered from
    /// the depth-0 note when needed (e.g. for tag derivation).
    pub remaining_offered: AssetAmount,
    /// Remaining requested amount (requested faucet recovered the same way).
    pub remaining_requested: AssetAmount,
    pub state: PswapLineageState,
}

impl PswapLineageRecord {
    /// Builds a record from all of its fields. A store uses this to rebuild a record that it
    /// persisted in its own format.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        original_note_id: NoteId,
        order_id: Felt,
        creator_account_id: AccountId,
        current_tip_note_id: NoteId,
        current_depth: u32,
        remaining_offered: AssetAmount,
        remaining_requested: AssetAmount,
        state: PswapLineageState,
    ) -> Self {
        Self {
            original_note_id,
            order_id,
            creator_account_id,
            current_tip_note_id,
            current_depth,
            remaining_offered,
            remaining_requested,
            state,
        }
    }

    /// Stable identifier (== the depth-0 note's `serial[1]`) shared by every note in the chain.
    pub fn order_id(&self) -> Felt {
        self.order_id
    }

    /// Account that created the order (recipient of every payback).
    pub fn creator_account_id(&self) -> AccountId {
        self.creator_account_id
    }
}

// PSWAP LINEAGE FILTER
// ================================================================================================

/// Selects the lineages that [`Store::get_pswap_lineages`](crate::store::Store::get_pswap_lineages)
/// returns.
#[derive(Debug, Clone)]
pub enum PswapLineageFilter {
    /// Every lineage.
    All,
    /// Lineages in the [`PswapLineageState::Active`] state.
    Active,
    /// Lineages created by the given account.
    ByCreator(AccountId),
}

impl PswapLineageFilter {
    /// Returns `true` if `record` matches the filter.
    pub fn matches(&self, record: &PswapLineageRecord) -> bool {
        match self {
            Self::All => true,
            Self::Active => record.state == PswapLineageState::Active,
            Self::ByCreator(creator) => record.creator_account_id() == *creator,
        }
    }
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
        let state = PswapLineageState::try_from_u8(u8::read_from(source)?)?;
        Ok(Self::from_parts(
            original_note_id,
            order_id,
            creator_account_id,
            current_tip_note_id,
            current_depth,
            remaining_offered,
            remaining_requested,
            state,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stable byte encoding of `PswapLineageState`. The values are persisted in the serialized
    /// lineage record; reordering would silently corrupt existing stores.
    #[test]
    fn state_byte_encoding_is_stable() {
        assert_eq!(PswapLineageState::Active.as_u8(), 0);
        assert_eq!(PswapLineageState::FullyFilled.as_u8(), 1);
        assert_eq!(PswapLineageState::Reclaimed.as_u8(), 2);
    }

    /// Round-trip every state via `try_from_u8`. Belt-and-suspenders against a future renumbering
    /// breaking the serialized format.
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

    /// Unknown discriminants must error — defends against a future store reading a
    /// forward-incompatible byte.
    #[test]
    fn state_try_from_u8_rejects_unknown() {
        assert!(PswapLineageState::try_from_u8(99).is_err());
    }
}
