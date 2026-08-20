use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::MerklePath;
use miden_protocol::crypto::merkle::mmr::MmrError;
use miden_protocol::transaction::PartialBlockchain;
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
/// anchor tracking the blocks of a request's authenticated input notes. An executing client
/// serves blocks the anchor misses from its own store when it tracks them, and an anchor can be
/// widened after capture with [`Self::track_block`].
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

    /// Tracks an additional block in the anchor's partial blockchain, so that transactions
    /// consuming authenticated notes created in that block can execute against the anchor.
    ///
    /// `path` must be the block's MMR authentication path at the anchor's forest. It is verified
    /// against the anchor's peaks, so [`Self::block_commitment`] is unaffected. Tracking an
    /// already-tracked block is a no-op.
    ///
    /// # Errors
    ///
    /// - The block is not older than the anchor block.
    /// - The path does not verify against the anchor's peaks.
    pub fn track_block(
        &mut self,
        header: BlockHeader,
        path: &MerklePath,
    ) -> Result<(), ChainAnchorError> {
        let block_num = header.block_num();
        if block_num >= self.header.block_num() {
            return Err(ChainAnchorError::BlockPastAnchor {
                block_num,
                anchor: self.header.block_num(),
            });
        }

        if self.chain.contains_block(block_num) {
            return Ok(());
        }

        let mut mmr = self.chain.mmr().clone();
        mmr.track(block_num.as_usize(), header.commitment(), path)
            .map_err(|source| ChainAnchorError::UntrackablePath { block_num, source })?;

        let mut blocks: Vec<BlockHeader> = self.chain.block_headers().cloned().collect();
        blocks.push(header);

        // `track` verified the path against the peaks, so the cheap constructor suffices.
        self.chain = PartialBlockchain::new_unchecked(mmr, blocks)
            .expect("tracked block extends an already-valid partial blockchain");

        Ok(())
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
        let chain = PartialBlockchain::read_from(source)?;

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
        "block {block_num} is not tracked by the anchor's partial blockchain and could not be served from the local store; capture the anchor with the blocks of all authenticated input notes"
    )]
    BlockNotTracked { block_num: BlockNumber },
    #[error(
        "block {block_num} is not older than the anchor block {anchor}, so it cannot be tracked"
    )]
    BlockPastAnchor {
        block_num: BlockNumber,
        anchor: BlockNumber,
    },
    #[error("authentication path for block {block_num} does not verify against the anchor's peaks")]
    UntrackablePath { block_num: BlockNumber, source: MmrError },
    #[error("transaction reference block {requested} does not match the anchor block {anchor}")]
    ReferenceBlockMismatch {
        requested: BlockNumber,
        anchor: BlockNumber,
    },
}
