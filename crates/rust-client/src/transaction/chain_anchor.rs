use alloc::string::ToString;

use miden_protocol::Word;
use miden_protocol::block::{BlockHeader, BlockNumber};
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
        "block {block_num} is not tracked by the anchor's partial blockchain; capture the anchor with the blocks of all authenticated input notes"
    )]
    BlockNotTracked { block_num: BlockNumber },
    #[error("transaction reference block {requested} does not match the anchor block {anchor}")]
    ReferenceBlockMismatch {
        requested: BlockNumber,
        anchor: BlockNumber,
    },
}
