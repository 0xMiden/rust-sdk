use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::ToString;

use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::PartialMmr;
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
/// on construction (including deserialization), so an anchor received from an untrusted party only
/// needs its [`Self::block_commitment`] checked against an independently trusted value — e.g. the
/// `BLOCK_COMMITMENT` word bound into a signed [`TransactionSummary`] — to be safe to execute
/// against.
///
/// Since protocol 0.16 the signed transaction summary binds the reference block commitment, so a
/// summary produced at one block cannot be reproduced by re-executing at another. Flows that
/// collect signatures over a summary and execute later (e.g. multisig) capture an anchor at the
/// block the summary was built at ([`crate::Client::chain_anchor_for_request`]), ship it with the
/// signed data, and replay the transaction with [`crate::Client::execute_transaction_at`] so the
/// summary — and with it the signature advice keys — reproduces exactly.
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
}

impl ChainAnchor {
    /// Returns a new anchor after validating that `chain` is consistent with `header`.
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

        Ok(Self { header, chain })
    }

    /// Returns the number of the anchored reference block.
    pub fn block_num(&self) -> BlockNumber {
        self.header.block_num()
    }

    /// Returns the commitment of the anchored reference block.
    ///
    /// Callers holding an anchor from an untrusted source should compare this against an
    /// independently trusted commitment (e.g. the block commitment bound into a signed
    /// transaction summary) before executing with the anchor.
    pub fn block_commitment(&self) -> Word {
        self.header.commitment()
    }

    /// Returns the anchored reference block header.
    pub fn header(&self) -> &BlockHeader {
        &self.header
    }

    /// Returns the partial blockchain at the anchored reference block.
    pub fn partial_blockchain(&self) -> &PartialBlockchain {
        &self.chain
    }

    /// Consumes the anchor and returns its parts.
    pub fn into_parts(self) -> (BlockHeader, PartialBlockchain) {
        (self.header, self.chain)
    }
}

impl Serializable for ChainAnchor {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.header.write_into(target);
        self.chain.write_into(target);
    }
}

impl Deserializable for ChainAnchor {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let header = BlockHeader::read_from(source)?;

        // Read the partial blockchain's parts rather than calling `PartialBlockchain::read_from`:
        // that path `expect`s on `PartialMmr::open`, which fails when a tracked leaf's ancestor
        // sibling is absent — a remotely triggerable panic, since anchor bytes come from another
        // party. Opening every tracked block here turns it into a rejected deserialization.
        let mmr = PartialMmr::read_from(source)?;
        let blocks = BTreeMap::<BlockNumber, BlockHeader>::read_from(source)?;

        // `Self::new` enforces this too, but only after opening and proving every block; rejecting
        // early bounds the work an oversized anchor can buy.
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
            // The constructor re-derives each position from the header, so the key must agree
            // with it — otherwise a crafted anchor could aim the `open` below at a harmless
            // position while the constructor opens the dangerous one.
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

        Self::new(header, chain).map_err(|err| DeserializationError::InvalidValue(err.to_string()))
    }
}

// CHAIN ANCHOR ERROR
// ================================================================================================

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
        "block {block_num} is not tracked by the anchor's partial blockchain; capture the anchor with the blocks of all authenticated input notes"
    )]
    BlockNotTracked { block_num: BlockNumber },
    #[error("the anchor tracks {count} blocks, more than the {max} a transaction can reference")]
    TooManyTrackedBlocks { count: usize, max: usize },
    #[error("transaction reference block {requested} does not match the anchor block {anchor}")]
    ReferenceBlockMismatch {
        requested: BlockNumber,
        anchor: BlockNumber,
    },
    #[error(
        "the anchored transaction expires at block {expiration}, which the chain has already reached (sync height {sync_height}); it would be rejected by the network, so re-capture the anchor closer to the tip or raise the request's expiration delta"
    )]
    AnchoredTransactionExpired {
        expiration: BlockNumber,
        sync_height: BlockNumber,
    },
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use miden_protocol::Word;
    use miden_protocol::block::BlockHeader;
    use miden_protocol::crypto::merkle::mmr::{Mmr, PartialMmr};
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

    #[test]
    fn new_accepts_a_consistent_header_and_chain() {
        let (header, chain) = anchor_parts(8, &[3]);
        let block_num = header.block_num();

        let anchor = ChainAnchor::new(header, chain).unwrap();

        assert_eq!(anchor.block_num(), block_num);
    }

    #[test]
    fn new_rejects_a_chain_length_that_does_not_match_the_header() {
        let (_, chain) = anchor_parts(8, &[3]);
        // Commit to the right chain, so the block number is the only defect.
        let header =
            BlockHeader::mock(9, Some(chain.peaks().hash_peaks()), None, &[], Word::empty());

        let err = ChainAnchor::new(header, chain).unwrap_err();

        assert!(matches!(err, ChainAnchorError::ChainLengthMismatch { .. }), "got {err:?}");
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
    fn serialization_round_trips() {
        let (header, chain) = anchor_parts(8, &[3]);
        let anchor = ChainAnchor::new(header, chain).unwrap();

        let deserialized = ChainAnchor::read_from_bytes(&anchor.to_bytes()).unwrap();
        assert_eq!(anchor, deserialized);
    }

    #[test]
    fn deserialization_rejects_truncated_and_garbage_input() {
        let (header, chain) = anchor_parts(8, &[3]);
        let bytes = ChainAnchor::new(header, chain).unwrap().to_bytes();

        assert!(ChainAnchor::read_from_bytes(&bytes[..bytes.len() - 1]).is_err());
        assert!(ChainAnchor::read_from_bytes(&[0xaa; 64]).is_err());
    }

    /// A tracked leaf whose ancestor siblings are absent makes `PartialBlockchain::new` panic on
    /// the `expect` around `PartialMmr::open`; deserialization must reject it instead.
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
            buf
        };

        assert!(ChainAnchor::read_from_bytes(&bytes).is_err());
    }

    /// A block-map key that disagrees with its header would let a crafted anchor aim the
    /// pre-flight `open` at a harmless position, so deserialization must reject it.
    #[test]
    fn deserialization_rejects_a_block_key_that_disagrees_with_its_header() {
        use alloc::collections::BTreeMap;

        use miden_protocol::block::BlockNumber;

        // A fully valid chain, so the disagreeing key is the payload's only defect.
        let (header, chain) = anchor_parts(8, &[3]);
        let tracked = chain.get_block(BlockNumber::from(3u32)).unwrap().clone();

        let mut blocks = BTreeMap::new();
        blocks.insert(BlockNumber::from(0u32), tracked);

        let bytes = {
            let mut buf = Vec::new();
            header.write_into(&mut buf);
            chain.mmr().write_into(&mut buf);
            blocks.write_into(&mut buf);
            buf
        };

        let err = ChainAnchor::read_from_bytes(&bytes).unwrap_err();
        assert!(
            matches!(&err, DeserializationError::InvalidValue(msg) if msg.contains("does not match the block number")),
            "got {err:?}"
        );
    }
}
