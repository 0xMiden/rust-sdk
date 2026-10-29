use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::transaction::PartialBlockchain;
use miden_protocol::{MAX_INPUT_NOTES_PER_TX, Word};
use thiserror::Error;

// CHAIN ANCHOR
// ================================================================================================

/// A self-contained, verifiable anchor for executing a transaction against a specific reference
/// block instead of the client's current sync height.
///
/// The anchor bundles the reference [`BlockHeader`] with a [`PartialBlockchain`] consistent with it
/// — exactly the chain data `TransactionInputs` requires: `chain_length()` equals the header's
/// block number and the peaks hash to the header's chain commitment. Both invariants are enforced
/// on construction, so an anchor received from an untrusted party only needs its
/// [`Self::block_commitment`] checked against an independently trusted value — e.g. the
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
/// The anchor's [`PartialBlockchain`] must track each block that the transaction authenticates.
/// [`crate::Client::chain_anchor_for_request`] captures the blocks declared by the request and the
/// creation blocks of its authenticated input notes.
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

        // Bound the work required to validate an anchor received from an untrusted source.
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
    /// independently trusted commitment (e.g. the block commitment bound into a signed transaction
    /// summary) before executing with the anchor.
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

    use miden_protocol::block::BlockHeader;
    use miden_protocol::crypto::merkle::mmr::{Mmr, PartialMmr};
    use miden_protocol::transaction::PartialBlockchain;

    use super::{ChainAnchor, ChainAnchorError};

    /// Returns a partial blockchain of length `chain_length` tracking the given block numbers,
    /// alongside a header whose block number and chain commitment are consistent with it.
    fn anchor_parts(chain_length: usize, tracked: &[usize]) -> (BlockHeader, PartialBlockchain) {
        let mut mmr = Mmr::default();
        let mut headers = Vec::with_capacity(chain_length);
        for block_num in 0..chain_length {
            let header = BlockHeader::mock(u32::try_from(block_num).unwrap(), None, None, &[]);
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
        let header = BlockHeader::mock(9, Some(chain.peaks().hash_peaks()), None, &[]);

        let err = ChainAnchor::new(header, chain).unwrap_err();

        assert!(matches!(err, ChainAnchorError::ChainLengthMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn new_rejects_peaks_that_do_not_hash_to_the_chain_commitment() {
        let (_, chain) = anchor_parts(8, &[3]);
        // Right block number, but a header committing to an unrelated chain commitment.
        let header = BlockHeader::mock(8, None, None, &[]);

        let err = ChainAnchor::new(header, chain).unwrap_err();

        assert!(matches!(err, ChainAnchorError::ChainCommitmentMismatch { .. }), "got {err:?}");
    }
}
