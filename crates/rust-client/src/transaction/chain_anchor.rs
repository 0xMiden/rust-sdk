use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::PartialMmr;
use miden_protocol::note::NoteId;
use miden_protocol::transaction::PartialBlockchain;
use miden_protocol::{MAX_INPUT_NOTES_PER_TX, Word};
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};
use thiserror::Error;

// CHAIN ANCHOR
// ================================================================================================

/// A self-contained, verifiable anchor for executing a transaction against a specific reference
/// block instead of the client's current sync height.
///
/// The anchor bundles the reference [`BlockHeader`] with a [`PartialBlockchain`] consistent with
/// it — exactly the chain data `TransactionInputs` requires: `chain_length()` equals the header's
/// block number and the peaks hash to the header's chain commitment. Both invariants are enforced
/// on construction (including deserialization), so the *chain data* in an anchor received from an
/// untrusted party only needs its [`Self::block_commitment`] checked against an independently
/// trusted value — e.g. the reference-block commitment bound into a signed [`TransactionSummary`]
/// — to be safe to execute against. [`Self::verify_block_commitment`] performs that check.
///
/// That check covers the header and the chain, and nothing else. The recorded classification (see
/// [`Self::with_authenticated_notes`]) is not committed to by the block commitment, so a party
/// relaying the anchor can strip it — turning the classification check into a no-op —
/// or add to it, turning a correct replay into an error. Neither forges a valid transaction, since
/// a divergent summary fails signature verification regardless; the classification is a diagnostic
/// that fails early and clearly, so callers who rely on it must receive the anchor over a channel
/// that authenticates it.
///
/// The anchor never has to be trusted for the block headers it carries: [`PartialBlockchain::new`]
/// proves every tracked header's commitment against the MMR, and [`Deserializable`] routes through
/// it, so headers are authenticated by the peaks the block commitment already covers. Building the
/// anchor over a [`PartialBlockchain::new_unchecked`] chain forfeits that and is only safe when the
/// chain came from a trusted source.
///
/// Since protocol 0.16 the signed transaction summary binds the reference block commitment, so a
/// summary produced at one block cannot be reproduced by re-executing at another. Flows that
/// collect signatures over a summary and execute later (e.g. multisig) capture an anchor at the
/// block the summary was built at ([`crate::Client::chain_anchor_for_request`]), ship it with the
/// signed data, and replay the transaction with [`crate::Client::execute_transaction_at`].
///
/// The anchor pins the chain-dependent half of that reproduction. The rest — the same request, the
/// native account state the transaction executes against, and the same
/// authenticated/unauthenticated classification of the input notes — still comes from the replaying
/// client, so an anchor makes the summary reproducible rather than guaranteeing it on its own. The
/// classification is the one of those the anchor can at least check, if it was recorded: see
/// [`Self::with_authenticated_notes`].
///
/// When the transaction consumes authenticated notes, the anchor's [`PartialBlockchain`] must
/// track each note's creation block; [`crate::Client::chain_anchor_for_request`] captures an
/// anchor tracking the blocks of a request's authenticated input notes.
///
/// [`TransactionSummary`]: miden_protocol::transaction::TransactionSummary
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainAnchor {
    header: BlockHeader,
    chain: PartialBlockchain,
    authenticated_notes: Option<BTreeSet<NoteId>>,
}

impl ChainAnchor {
    /// Returns a new anchor after validating that `chain` is consistent with `header`.
    ///
    /// The anchor records no input note classification; see [`Self::with_authenticated_notes`].
    ///
    /// # Errors
    ///
    /// - The partial blockchain's length does not match the header's block number.
    /// - The partial blockchain's peaks do not hash to the header's chain commitment.
    /// - The partial blockchain tracks more blocks than a transaction can reference.
    pub fn new(header: BlockHeader, chain: PartialBlockchain) -> Result<Self, ChainAnchorError> {
        if chain.chain_length() != header.block_num() {
            return Err(ChainAnchorError::ChainLengthMismatch {
                chain_length: chain.chain_length(),
                block_num: header.block_num(),
            });
        }

        if chain.peaks().hash_peaks() != header.chain_commitment() {
            return Err(ChainAnchorError::ChainCommitmentMismatch {
                block_num: header.block_num(),
            });
        }

        // A transaction references at most one creation block per input note, so a chain tracking
        // more blocks than that is not an anchor any honest peer would produce.
        if chain.num_tracked_blocks() > MAX_INPUT_NOTES_PER_TX {
            return Err(ChainAnchorError::TooManyTrackedBlocks {
                count: chain.num_tracked_blocks(),
                max: MAX_INPUT_NOTES_PER_TX,
            });
        }

        Ok(Self { header, chain, authenticated_notes: None })
    }

    /// Records which of the request's input notes were authenticated when the anchor was captured.
    ///
    /// The input notes commitment covers each note's nullifier paired with its id, and that id is
    /// present only for a note consumed unauthenticated — an authenticated one contributes an
    /// empty word in its place. Classification therefore changes the commitment, and with it the
    /// [`TransactionSummary`], so a replaying client that classifies a note differently from the
    /// capturing client produces a summary the collected signatures silently fail to apply to.
    /// Classification is read from each client's own store, which the reference block does not
    /// pin, so the anchor has to carry it.
    ///
    /// Recording it lets [`crate::Client::execute_transaction_at`] reject the mismatch up front
    /// with [`ChainAnchorError::NoteAuthenticationMismatch`]. An anchor without this (see
    /// [`Self::new`]) executes as before, and a divergence surfaces later as an unexplained
    /// signature failure.
    ///
    /// [`TransactionSummary`]: miden_protocol::transaction::TransactionSummary
    /// # Errors
    ///
    /// Returns [`ChainAnchorError::TooManyAuthenticatedNotes`] if more notes are recorded than a
    /// transaction can consume. Such a set could never match what a replaying client classifies,
    /// and enforcing the bound here rather than only on the wire keeps an anchor that was built
    /// in-process from failing its own deserialization.
    pub fn with_authenticated_notes(
        mut self,
        authenticated_notes: impl IntoIterator<Item = NoteId>,
    ) -> Result<Self, ChainAnchorError> {
        let authenticated_notes: BTreeSet<NoteId> = authenticated_notes.into_iter().collect();

        if authenticated_notes.len() > MAX_INPUT_NOTES_PER_TX {
            return Err(ChainAnchorError::TooManyAuthenticatedNotes {
                count: authenticated_notes.len(),
                max: MAX_INPUT_NOTES_PER_TX,
            });
        }

        self.authenticated_notes = Some(authenticated_notes);
        Ok(self)
    }

    /// Returns the number of the anchored reference block.
    pub fn block_num(&self) -> BlockNumber {
        self.header.block_num()
    }

    /// Returns the commitment of the anchored reference block.
    ///
    /// Callers holding an anchor from an untrusted source should compare this against an
    /// independently trusted commitment (e.g. the block commitment bound into a signed
    /// transaction summary) before executing with the anchor. [`Self::verify_block_commitment`]
    /// does that comparison.
    pub fn block_commitment(&self) -> Word {
        self.header.commitment()
    }

    /// Checks the anchored reference block against an independently trusted commitment.
    ///
    /// Nothing within an anchor proves it refers to the block the signers agreed on, so this is
    /// the check that ties it to one. Pass the reference-block commitment bound into the signed
    /// [`TransactionSummary`].
    ///
    /// It is sufficient only in combination with the construction-time invariants: for an anchor
    /// that reached this client through [`Deserializable`] every tracked header is proven against
    /// the MMR, so a matching commitment covers the whole chain. An anchor assembled in-process
    /// over a [`PartialBlockchain::new_unchecked`] chain carries no such proof, and this check
    /// does not add one.
    ///
    /// # Errors
    ///
    /// Returns [`ChainAnchorError::BlockCommitmentMismatch`] if the commitments differ.
    ///
    /// [`TransactionSummary`]: miden_protocol::transaction::TransactionSummary
    pub fn verify_block_commitment(&self, expected: Word) -> Result<(), ChainAnchorError> {
        let actual = self.block_commitment();
        if actual != expected {
            return Err(ChainAnchorError::BlockCommitmentMismatch { expected, actual });
        }

        Ok(())
    }

    /// Returns the anchored reference block header.
    pub fn header(&self) -> &BlockHeader {
        &self.header
    }

    /// Returns the partial blockchain at the anchored reference block.
    pub fn partial_blockchain(&self) -> &PartialBlockchain {
        &self.chain
    }

    /// Returns the ids of the input notes that were authenticated when this anchor was captured,
    /// or `None` if the anchor does not record the classification.
    pub fn authenticated_notes(&self) -> Option<&BTreeSet<NoteId>> {
        self.authenticated_notes.as_ref()
    }

    /// Checks that `locally_authenticated` matches the classification recorded when the anchor was
    /// captured, so that a divergent transaction summary is caught before proving rather than
    /// surfacing as a signature that will not apply.
    ///
    /// Returns `Ok(())` when the anchor records no classification.
    ///
    /// # Errors
    ///
    /// Returns [`ChainAnchorError::NoteAuthenticationMismatch`] for the first note whose
    /// classification differs, in either direction.
    pub(crate) fn verify_authenticated_notes(
        &self,
        locally_authenticated: &BTreeSet<NoteId>,
    ) -> Result<(), ChainAnchorError> {
        let Some(anchored) = self.authenticated_notes.as_ref() else {
            return Ok(());
        };

        if let Some(&note_id) = anchored.difference(locally_authenticated).next() {
            return Err(ChainAnchorError::NoteAuthenticationMismatch {
                note_id,
                authenticated_at_capture: true,
            });
        }

        if let Some(&note_id) = locally_authenticated.difference(anchored).next() {
            return Err(ChainAnchorError::NoteAuthenticationMismatch {
                note_id,
                authenticated_at_capture: false,
            });
        }

        Ok(())
    }

    /// Consumes the anchor and returns its parts.
    ///
    /// The recorded classification is returned alongside the chain data so that rebuilding an
    /// anchor from the parts cannot silently drop it.
    pub fn into_parts(self) -> (BlockHeader, PartialBlockchain, Option<BTreeSet<NoteId>>) {
        (self.header, self.chain, self.authenticated_notes)
    }
}

impl Serializable for ChainAnchor {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.header.write_into(target);
        self.chain.write_into(target);
        self.authenticated_notes.write_into(target);
    }
}

impl Deserializable for ChainAnchor {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let header = BlockHeader::read_from(source)?;

        // Read the partial blockchain's parts rather than calling `PartialBlockchain::read_from`.
        // That constructor hands the parts to `PartialBlockchain::new`, which `expect`s on
        // `PartialMmr::open`; `open` returns `Err` when a tracked leaf's ancestor sibling is
        // absent, and `PartialMmr::from_parts` does not check for that — it only checks that each
        // tracked leaf is in bounds and has a value of its own. Anchor bytes come from another
        // party, so reaching that `expect` is a remotely triggerable panic. Opening every tracked
        // block here turns it into a rejected deserialization.
        let mmr = PartialMmr::read_from(source)?;
        let blocks = BTreeMap::<BlockNumber, BlockHeader>::read_from(source)?;

        // `Self::new` enforces this too, but only after every block below has been opened and then
        // proved again by `PartialBlockchain::new`. Rejecting here keeps the work an oversized
        // anchor can buy proportional to the bytes it costs to send.
        if blocks.len() > MAX_INPUT_NOTES_PER_TX {
            return Err(DeserializationError::InvalidValue(
                ChainAnchorError::TooManyTrackedBlocks {
                    count: blocks.len(),
                    max: MAX_INPUT_NOTES_PER_TX,
                }
                .to_string(),
            ));
        }

        for (block_num, header) in &blocks {
            // `PartialBlockchain::new_unchecked` discards these keys and re-derives each position
            // from the header itself, so opening the key would let a crafted anchor aim this check
            // at a harmless position while the constructor opens the dangerous one. Requiring the
            // two to agree closes that and makes the encoding canonical.
            if block_num != &header.block_num() {
                return Err(DeserializationError::InvalidValue(format!(
                    "block map key {block_num} does not match the block number {} of the header it maps to",
                    header.block_num()
                )));
            }

            mmr.open(header.block_num().as_usize())
                .map_err(|err| DeserializationError::InvalidValue(err.to_string()))?;
        }
        let chain = PartialBlockchain::new(mmr, blocks.into_values())
            .map_err(|err| DeserializationError::InvalidValue(err.to_string()))?;

        let authenticated_notes = Option::<BTreeSet<NoteId>>::read_from(source)?;

        let anchor = Self::new(header, chain)
            .map_err(|err| DeserializationError::InvalidValue(err.to_string()))?;

        match authenticated_notes {
            Some(notes) => anchor
                .with_authenticated_notes(notes)
                .map_err(|err| DeserializationError::InvalidValue(err.to_string())),
            None => Ok(anchor),
        }
    }
}

// CHAIN ANCHOR ERROR
// ================================================================================================

/// An error raised while constructing a [`ChainAnchor`], while checking one against the state of
/// the client that is about to execute with it, while the data store serves chain data during
/// execution, or — for expiry — once execution has already produced a transaction.
#[derive(Debug, Error)]
pub enum ChainAnchorError {
    #[error(
        "partial blockchain length {chain_length} does not match the anchor block number {block_num}"
    )]
    ChainLengthMismatch {
        chain_length: BlockNumber,
        block_num: BlockNumber,
    },
    #[error(
        "partial blockchain peaks do not hash to the chain commitment of anchor block {block_num}"
    )]
    ChainCommitmentMismatch { block_num: BlockNumber },
    #[error(
        "anchor block commitment is {actual} but {expected} was expected; the anchor refers to a different block than the one that was signed"
    )]
    BlockCommitmentMismatch { expected: Word, actual: Word },
    #[error(
        "block {block_num} is not tracked by the anchor's partial blockchain; capture the anchor with the blocks of all authenticated input notes"
    )]
    BlockNotTracked { block_num: BlockNumber },
    #[error("the anchor tracks {count} blocks, more than the {max} a transaction can reference")]
    TooManyTrackedBlocks { count: usize, max: usize },
    #[error(
        "the anchor records {count} authenticated notes, more than the {max} a transaction can consume"
    )]
    TooManyAuthenticatedNotes { count: usize, max: usize },
    #[error("transaction reference block {requested} does not match the anchor block {anchor}")]
    ReferenceBlockMismatch {
        requested: BlockNumber,
        anchor: BlockNumber,
    },
    #[error(
        "input note {note_id} was {} when the anchor was captured but is {} in this client's store, so the transaction summary would not reproduce",
        if *.authenticated_at_capture { "authenticated" } else { "unauthenticated" },
        if *.authenticated_at_capture { "not authenticated" } else { "authenticated" }
    )]
    NoteAuthenticationMismatch {
        note_id: NoteId,
        authenticated_at_capture: bool,
    },
    #[error(
        "the anchored transaction expires at block {expiration}, which the chain has already reached (sync height {sync_height}); it would be rejected by the network, so re-capture the anchor closer to the tip or raise the request's expiration delta"
    )]
    AnchoredTransactionExpired {
        expiration: BlockNumber,
        sync_height: BlockNumber,
    },
    #[error(
        "{} of the request's input notes were dropped as unconsumable ({dropped:?}), which changes the input notes commitment and therefore the transaction summary; `ignore_invalid_input_notes` decides consumability from the native account state, which an anchor cannot pin, so drop the unconsumable notes from the request instead of relying on the flag",
        dropped.len()
    )]
    InputNotesDropped { dropped: Vec<NoteId> },
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeSet;
    use alloc::vec::Vec;

    use miden_protocol::Word;
    use miden_protocol::block::BlockHeader;
    use miden_protocol::crypto::merkle::mmr::{Mmr, PartialMmr};
    use miden_protocol::note::NoteId;
    use miden_protocol::transaction::PartialBlockchain;
    use miden_tx::utils::serde::{Deserializable, DeserializationError, Serializable};

    use super::{ChainAnchor, ChainAnchorError};

    /// Returns a partial blockchain of length `chain_length` tracking the given block numbers,
    /// alongside a header whose block number and chain commitment are consistent with it.
    fn anchor_parts(chain_length: usize, tracked: &[usize]) -> (BlockHeader, PartialBlockchain) {
        let mut mmr = Mmr::default();
        let mut headers = Vec::with_capacity(chain_length);
        for block_num in 0..chain_length {
            let header = BlockHeader::mock(
                u32::try_from(block_num).unwrap(),
                None,
                None,
                &[],
                Word::empty(),
            );
            mmr.add(header.commitment()).unwrap();
            headers.push(header);
        }

        let peaks = mmr.peaks();
        let mut partial_mmr = PartialMmr::from_peaks(peaks.clone());
        let mut tracked_headers = Vec::new();
        for &pos in tracked {
            partial_mmr
                .track(pos, mmr.get(pos).unwrap(), mmr.open(pos).unwrap().merkle_path())
                .unwrap();
            tracked_headers.push(headers[pos].clone());
        }

        let chain = PartialBlockchain::new(partial_mmr, tracked_headers).unwrap();
        let header = BlockHeader::mock(
            u32::try_from(chain_length).unwrap(),
            Some(peaks.hash_peaks()),
            None,
            &[],
            Word::empty(),
        );

        (header, chain)
    }

    fn test_note_id(n: u32) -> NoteId {
        NoteId::from_raw(Word::from([n, 0, 0, 0]))
    }

    #[test]
    fn new_accepts_a_consistent_header_and_chain() {
        let (header, chain) = anchor_parts(8, &[3]);
        let block_num = header.block_num();

        let anchor = ChainAnchor::new(header, chain).unwrap();

        assert_eq!(anchor.block_num(), block_num);
        assert!(anchor.authenticated_notes().is_none());
    }

    #[test]
    fn new_rejects_a_chain_length_that_does_not_match_the_header() {
        let (_, chain) = anchor_parts(8, &[3]);
        // Commit to the right chain, so the block number is the only thing wrong. A `None`
        // commitment here would be filled with a random word, and the test would then pass on the
        // commitment check too — including if the two checks were ever reordered.
        let header =
            BlockHeader::mock(9, Some(chain.peaks().hash_peaks()), None, &[], Word::empty());

        let err = ChainAnchor::new(header, chain).unwrap_err();

        assert!(matches!(err, ChainAnchorError::ChainLengthMismatch { .. }), "got {err:?}");
    }

    /// The bound exists so that an anchor built in process cannot fail its own deserialization.
    /// Both halves matter: the builder has to reject the set, and the wire path has to route
    /// through the builder rather than accepting what the builder would not have produced.
    #[test]
    fn a_classification_larger_than_a_transaction_can_consume_is_rejected_by_both_paths() {
        let (header, chain) = anchor_parts(8, &[3]);
        let too_many: Vec<NoteId> = (0..=u32::try_from(super::MAX_INPUT_NOTES_PER_TX).unwrap())
            .map(test_note_id)
            .collect();

        let anchor = ChainAnchor::new(header, chain).unwrap();
        let err = anchor.clone().with_authenticated_notes(too_many.clone()).unwrap_err();
        assert!(matches!(err, ChainAnchorError::TooManyAuthenticatedNotes { .. }), "got {err:?}");

        // The builder cannot produce such an anchor, so the payload has to be assembled by hand.
        let mut bytes = anchor.to_bytes();
        bytes.truncate(bytes.len() - Option::<BTreeSet<NoteId>>::None.to_bytes().len());
        Some(too_many.into_iter().collect::<BTreeSet<NoteId>>()).write_into(&mut bytes);

        let err = ChainAnchor::read_from_bytes(&bytes).unwrap_err();
        assert!(
            matches!(&err, DeserializationError::InvalidValue(msg) if msg.contains("authenticated notes")),
            "got {err:?}"
        );
    }

    #[test]
    fn new_rejects_peaks_that_do_not_hash_to_the_chain_commitment() {
        let (_, chain) = anchor_parts(8, &[3]);
        // Right block number, but a header committing to an unrelated chain commitment.
        let header = BlockHeader::mock(8, None, None, &[], Word::empty());

        let err = ChainAnchor::new(header, chain).unwrap_err();

        assert!(matches!(err, ChainAnchorError::ChainCommitmentMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn verify_block_commitment_accepts_the_anchored_block_and_rejects_any_other() {
        let (header, chain) = anchor_parts(8, &[3]);
        let commitment = header.commitment();
        let anchor = ChainAnchor::new(header, chain).unwrap();

        anchor.verify_block_commitment(commitment).unwrap();

        let err = anchor.verify_block_commitment(Word::empty()).unwrap_err();
        assert!(matches!(err, ChainAnchorError::BlockCommitmentMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn verify_authenticated_notes_is_a_noop_when_the_anchor_records_no_classification() {
        let (header, chain) = anchor_parts(8, &[3]);
        let anchor = ChainAnchor::new(header, chain).unwrap();

        anchor
            .verify_authenticated_notes(&[test_note_id(1)].into_iter().collect())
            .unwrap();
    }

    #[test]
    fn verify_authenticated_notes_catches_a_mismatch_in_both_directions() {
        let (header, chain) = anchor_parts(8, &[3]);
        let anchor = ChainAnchor::new(header, chain)
            .unwrap()
            .with_authenticated_notes([test_note_id(1), test_note_id(2)])
            .unwrap();

        anchor
            .verify_authenticated_notes(&[test_note_id(1), test_note_id(2)].into_iter().collect())
            .unwrap();

        // Authenticated at capture, unauthenticated locally.
        let err = anchor
            .verify_authenticated_notes(&[test_note_id(1)].into_iter().collect())
            .unwrap_err();
        assert!(
            matches!(
                err,
                ChainAnchorError::NoteAuthenticationMismatch {
                    note_id,
                    authenticated_at_capture: true
                } if note_id == test_note_id(2)
            ),
            "got {err:?}"
        );

        // Unauthenticated at capture, authenticated locally.
        let err = anchor
            .verify_authenticated_notes(
                &[test_note_id(1), test_note_id(2), test_note_id(3)].into_iter().collect(),
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                ChainAnchorError::NoteAuthenticationMismatch {
                    note_id,
                    authenticated_at_capture: false
                } if note_id == test_note_id(3)
            ),
            "got {err:?}"
        );
    }

    /// Recording "no note was authenticated" must stay distinguishable from recording nothing at
    /// all: the first rejects a client that authenticated one, the second waves it through.
    #[test]
    fn an_empty_recorded_classification_is_not_the_same_as_an_unrecorded_one() {
        let (header, chain) = anchor_parts(8, &[3]);
        let anchor = ChainAnchor::new(header, chain).unwrap().with_authenticated_notes([]).unwrap();

        assert_eq!(anchor.authenticated_notes(), Some(&BTreeSet::new()));

        anchor.verify_authenticated_notes(&BTreeSet::new()).unwrap();

        let err = anchor
            .verify_authenticated_notes(&[test_note_id(1)].into_iter().collect())
            .unwrap_err();
        assert!(
            matches!(
                err,
                ChainAnchorError::NoteAuthenticationMismatch {
                    authenticated_at_capture: false,
                    ..
                }
            ),
            "got {err:?}"
        );

        let deserialized = ChainAnchor::read_from_bytes(&anchor.to_bytes()).unwrap();
        assert_eq!(deserialized.authenticated_notes(), Some(&BTreeSet::new()));
    }

    #[test]
    fn serialization_round_trips_with_and_without_a_recorded_classification() {
        let (header, chain) = anchor_parts(8, &[3]);
        let anchor = ChainAnchor::new(header, chain).unwrap();

        let deserialized = ChainAnchor::read_from_bytes(&anchor.to_bytes()).unwrap();
        assert_eq!(anchor, deserialized);
        assert!(deserialized.authenticated_notes().is_none());

        let anchor = anchor.with_authenticated_notes([test_note_id(7)]).unwrap();
        let deserialized = ChainAnchor::read_from_bytes(&anchor.to_bytes()).unwrap();
        assert_eq!(anchor, deserialized);
        assert_eq!(deserialized.authenticated_notes().unwrap().len(), 1);
    }

    #[test]
    fn deserialization_rejects_truncated_and_garbage_input() {
        let (header, chain) = anchor_parts(8, &[3]);
        let bytes = ChainAnchor::new(header, chain).unwrap().to_bytes();

        assert!(ChainAnchor::read_from_bytes(&bytes[..bytes.len() - 1]).is_err());
        assert!(ChainAnchor::read_from_bytes(&[0xaa; 64]).is_err());
    }

    /// Reproduces a crafted-anchor panic: a `PartialMmr` whose tracked leaf has a value but whose
    /// ancestor siblings are absent passes `PartialMmr::from_parts` and
    /// `PartialBlockchain::new_unchecked`, then makes `PartialBlockchain::new` panic on the
    /// `expect` around `PartialMmr::open`.
    #[test]
    fn deserialization_rejects_a_tracked_leaf_with_a_missing_sibling() {
        use alloc::collections::{BTreeMap, BTreeSet};

        use miden_protocol::crypto::merkle::mmr::InOrderIndex;

        let mut mmr = Mmr::default();
        let mut headers = Vec::new();
        for block_num in 0..4u32 {
            let header = BlockHeader::mock(block_num, None, None, &[], Word::empty());
            mmr.add(header.commitment()).unwrap();
            headers.push(header);
        }
        let peaks = mmr.peaks();

        // Only the tracked leaf itself, none of its authentication path.
        let mut nodes = BTreeMap::new();
        nodes.insert(InOrderIndex::from_leaf_pos(3), headers[3].commitment());

        let partial_mmr =
            PartialMmr::from_parts(peaks.clone(), nodes, BTreeSet::from([3])).unwrap();

        let bytes = {
            let mut buf = Vec::new();
            let header = BlockHeader::mock(4, Some(peaks.hash_peaks()), None, &[], Word::empty());
            header.write_into(&mut buf);
            PartialBlockchain::new_unchecked(partial_mmr, [headers[3].clone()])
                .unwrap()
                .write_into(&mut buf);
            None::<BTreeSet<NoteId>>.write_into(&mut buf);
            buf
        };

        assert!(ChainAnchor::read_from_bytes(&bytes).is_err());
    }

    /// Requiring the key to agree with its header is what makes the pre-flight check sound: the
    /// constructor re-derives the position it opens from the header, so a crafted anchor could
    /// otherwise aim the pre-flight at a harmless position. The chain here is entirely valid, so
    /// the disagreeing key is the only defect and the test fails if the check is removed.
    #[test]
    fn deserialization_rejects_a_block_key_that_disagrees_with_its_header() {
        use alloc::collections::BTreeMap;

        use miden_protocol::block::BlockNumber;

        // A fully valid chain, so the only thing wrong with the payload below is the key. Reusing
        // the missing-sibling MMR from the test above would make this pass for that reason
        // instead, and the key check could then be deleted without the test noticing.
        let (header, chain) = anchor_parts(8, &[3]);
        let tracked = chain.get_block(BlockNumber::from(3u32)).unwrap().clone();

        let mut blocks = BTreeMap::new();
        blocks.insert(BlockNumber::from(0u32), tracked);

        let bytes = {
            let mut buf = Vec::new();
            header.write_into(&mut buf);
            chain.mmr().write_into(&mut buf);
            blocks.write_into(&mut buf);
            None::<BTreeSet<NoteId>>.write_into(&mut buf);
            buf
        };

        let err = ChainAnchor::read_from_bytes(&bytes).unwrap_err();
        assert!(
            matches!(&err, DeserializationError::InvalidValue(msg) if msg.contains("does not match the block number")),
            "got {err:?}"
        );
    }
}
