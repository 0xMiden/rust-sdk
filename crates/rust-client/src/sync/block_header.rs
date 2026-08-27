use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::hash::rpo::Rpo256;
use miden_protocol::crypto::merkle::MerklePath;
use miden_protocol::crypto::merkle::mmr::{Forest, InOrderIndex, PartialMmr};
use miden_protocol::{Felt, Word};
use tracing::warn;

use crate::rpc::NodeRpcClient;
use crate::store::{BlockRelevance, StoreError};
#[cfg(feature = "testing")]
use crate::test_utils::mock::MockRpcApi;
use crate::{CachedPartialMmr, Client, ClientError};

/// Network information management methods.
impl<AUTH> Client<AUTH> {
    /// Retrieves a block header by its block number from the store.
    ///
    /// Returns `None` if the block header is not found in the store.
    pub async fn get_block_header_by_num(
        &self,
        block_num: BlockNumber,
    ) -> Result<Option<(BlockHeader, BlockRelevance)>, ClientError> {
        self.store.get_block_header_by_num(block_num).await.map_err(Into::into)
    }

    /// Ensures that the genesis block is available. If the genesis commitment is already
    /// cached in the RPC client, returns early. Otherwise, fetches the genesis block from
    /// the node, stores it, and sets the commitment in the RPC client.
    pub async fn ensure_genesis_in_place(&mut self) -> Result<(), ClientError> {
        if self.rpc_api.has_genesis_commitment().is_some() {
            return Ok(());
        }

        let (genesis, _) = self
            .rpc_api
            .get_block_header_by_number(Some(BlockNumber::GENESIS), false)
            .await?;

        // Genesis is untracked since there are no client notes associated with it, so we fetch no
        // MMR proof and pass no nodes.
        self.store.insert_block_header(&genesis, &[], false).await?;
        self.rpc_api.set_genesis_commitment(genesis.commitment()).await?;
        Ok(())
    }

    /// Seeds local state for offline account creation and debugging without a real node.
    ///
    /// Applies default RPC limits, then either aligns the RPC genesis with an existing stored
    /// genesis, or replaces the RPC client with [`MockRpcApi`] and runs
    /// [`Self::ensure_genesis_in_place`] so genesis comes from the mock chain.
    #[cfg(feature = "testing")]
    pub async fn prepare_offline_bootstrap(&mut self) -> Result<(), ClientError> {
        let limits = self.store.get_rpc_limits().await?.unwrap_or_default();
        self.store.set_rpc_limits(limits).await?;
        self.rpc_api.set_rpc_limits(limits).await;

        if let Some((genesis, _)) = self.store.get_block_header_by_num(BlockNumber::GENESIS).await?
        {
            self.rpc_api.set_genesis_commitment(genesis.commitment()).await?;
            return Ok(());
        }

        *self.test_rpc_api() = Arc::new(MockRpcApi::default());
        self.ensure_genesis_in_place().await?;
        Ok(())
    }

    /// Returns the cached [`PartialMmr`] if in-memory caching is enabled and its fingerprint
    /// matches the current store state, otherwise rebuilds from the store.
    pub async fn get_current_partial_mmr(&self) -> Result<PartialMmr, ClientError> {
        if self.cache_partial_mmr_in_memory
            && let Some(ref cached) = self.partial_mmr
            && cached.store_peaks_hash == self.current_store_peaks_hash().await?
            && cached.tracked_blocks_hash == self.current_tracked_blocks_hash().await?
        {
            return Ok(cached.mmr.clone());
        }
        self.store.get_current_partial_mmr().await.map_err(Into::into)
    }

    /// Stores the MMR in the cache if in-memory caching is enabled, capturing the current store
    /// fingerprint. Must run after any store mutation that may have changed the sync-height peaks
    /// or the tracked block set.
    pub(crate) async fn cache_partial_mmr(&mut self, mmr: PartialMmr) -> Result<(), ClientError> {
        if !self.cache_partial_mmr_in_memory {
            return Ok(());
        }

        let store_peaks_hash = self.current_store_peaks_hash().await?;
        let tracked_blocks_hash = self.current_tracked_blocks_hash().await?;
        self.partial_mmr = Some(CachedPartialMmr {
            store_peaks_hash,
            tracked_blocks_hash,
            mmr,
        });
        Ok(())
    }

    /// Hashes the store's peaks at the current sync height. Used as the cache freshness
    /// fingerprint.
    async fn current_store_peaks_hash(&self) -> Result<Word, ClientError> {
        Ok(self.store.get_current_blockchain_peaks().await?.hash_peaks())
    }

    /// Hashes the store's tracked block numbers (sorted). Used as the cache freshness
    /// fingerprint to detect tracked-set drift without rebuilding the MMR.
    async fn current_tracked_blocks_hash(&self) -> Result<Word, ClientError> {
        // BTreeSet iterates in sorted order, so the hash is deterministic.
        let tracked = self.store.get_tracked_block_header_numbers().await?;
        let elements: Vec<Felt> = tracked
            .iter()
            .map(|&n| Felt::from(u32::try_from(n).expect("block number fits in u32")))
            .collect();
        Ok(Rpo256::hash_elements(&elements))
    }

    // HELPERS
    // --------------------------------------------------------------------------------------------

    /// Retrieves and stores a [`BlockHeader`] by number, and stores its authentication data as
    /// well.
    ///
    /// If the store already contains MMR data for the requested block number, the request isn't
    /// done and the stored block header is returned.
    pub(crate) async fn get_and_store_authenticated_block(
        &self,
        block_num: BlockNumber,
        current_partial_mmr: &mut PartialMmr,
    ) -> Result<BlockHeader, ClientError> {
        if current_partial_mmr.is_tracked(block_num.as_usize()) {
            warn!("Current partial MMR already contains the requested data");
            let (block_header, _) = self
                .store
                .get_block_header_by_num(block_num)
                .await?
                .expect("Block header should be tracked");
            return Ok(block_header);
        }

        // Fetch the block header and MMR proof from the node
        let (block_header, path_nodes) =
            fetch_block_header(self.rpc_api.clone(), block_num, current_partial_mmr).await?;
        let tracked_nodes = authenticated_block_nodes(&block_header, path_nodes);

        // Insert header and MMR nodes atomically
        self.store.insert_block_header(&block_header, &tracked_nodes, true).await?;

        Ok(block_header)
    }
}

// UTILS
// --------------------------------------------------------------------------------------------

/// Returns a merkle path nodes for a specific block adjusted for a defined forest size.
/// This function trims the merkle path to include only the nodes that are relevant for
/// the MMR forest.
///
/// # Parameters
/// - `merkle_path`: Original merkle path.
/// - `block_num`: The block number for which the path is computed.
/// - `forest`: The target size of the forest.
pub(crate) fn adjust_merkle_path_for_forest(
    merkle_path: &MerklePath,
    block_num: BlockNumber,
    forest: Forest,
) -> Vec<(InOrderIndex, Word)> {
    let expected_path_len = forest
        .leaf_to_corresponding_tree(block_num.as_usize())
        .expect("forest includes block number") as usize;

    let mut idx = InOrderIndex::from_leaf_pos(block_num.as_usize());
    let mut path_nodes = Vec::with_capacity(expected_path_len);

    for node in merkle_path.nodes().iter().take(expected_path_len) {
        path_nodes.push((idx.sibling(), *node));
        idx = idx.parent();
    }

    path_nodes
}

/// Adjusts a Merkle path for the given forest, then calls [`PartialMmr::track`] to verify
/// and register the block. Returns the forest-adjusted authentication path nodes for the
/// tracked block.
pub(crate) fn track_block_in_mmr(
    partial_mmr: &mut PartialMmr,
    block_num: BlockNumber,
    block_commitment: Word,
    mmr_path: &MerklePath,
) -> Result<Vec<(InOrderIndex, Word)>, ClientError> {
    let path_nodes = adjust_merkle_path_for_forest(mmr_path, block_num, partial_mmr.forest());
    let adjusted_path = MerklePath::new(path_nodes.iter().map(|(_, n)| *n).collect());

    partial_mmr
        .track(block_num.as_usize(), block_commitment, &adjusted_path)
        .map_err(StoreError::MmrError)?;

    Ok(path_nodes)
}

fn authenticated_block_nodes(
    block_header: &BlockHeader,
    mut path_nodes: Vec<(InOrderIndex, Word)>,
) -> Vec<(InOrderIndex, Word)> {
    let mut nodes = Vec::with_capacity(path_nodes.len() + 1);
    nodes.push((
        InOrderIndex::from_leaf_pos(block_header.block_num().as_usize()),
        block_header.commitment(),
    ));
    nodes.append(&mut path_nodes);
    nodes
}

pub(crate) async fn fetch_block_header(
    rpc_api: Arc<dyn NodeRpcClient>,
    block_num: BlockNumber,
    current_partial_mmr: &mut PartialMmr,
) -> Result<(BlockHeader, Vec<(InOrderIndex, Word)>), ClientError> {
    let (block_header, mmr_proof) = rpc_api.get_block_header_with_proof(block_num).await?;

    let path_nodes = track_block_in_mmr(
        current_partial_mmr,
        block_num,
        block_header.commitment(),
        mmr_proof.merkle_path(),
    )?;

    Ok((block_header, path_nodes))
}

#[cfg(test)]
mod tests {
    use miden_protocol::block::{BlockHeader, BlockNumber};
    use miden_protocol::crypto::merkle::MerklePath;
    use miden_protocol::crypto::merkle::mmr::{Forest, InOrderIndex, Mmr, PartialMmr};
    use miden_protocol::transaction::TransactionKernel;
    use miden_protocol::{Felt, Word};

    use super::{adjust_merkle_path_for_forest, authenticated_block_nodes};

    fn word(n: u64) -> Word {
        Word::new([
            Felt::new(n).expect("test value should fit into the base field"),
            Felt::new(0).expect("zero is a valid field element"),
            Felt::new(0).expect("zero is a valid field element"),
            Felt::new(0).expect("zero is a valid field element"),
        ])
    }

    #[test]
    fn adjust_merkle_path_truncates_to_forest_bounds() {
        let forest = Forest::new(5).expect("valid forest");
        // Forest 5 <=> block 4 is rightmost leaf
        let block_num = BlockNumber::from(4u32);
        let path = MerklePath::new(vec![word(1), word(2), word(3)]);

        let adjusted = adjust_merkle_path_for_forest(&path, block_num, forest);
        // Block 4 conforms a single leaf tree so it should be empty
        assert!(adjusted.is_empty());
    }

    #[test]
    fn adjust_merkle_path_keeps_proof_valid_for_smaller_forest() {
        // Build a proof in a larger forest and ensure truncation does not keep siblings from a
        // different tree in the smaller forest, which would invalidate the proof.
        let mut mmr = Mmr::new();
        for value in 0u64..8 {
            mmr.add(word(value)).expect("test MMR append should succeed");
        }

        let large_forest = Forest::new(8).expect("valid forest");
        let small_forest = Forest::new(5).expect("valid forest");
        let leaf_pos = 4usize;
        let block_num = BlockNumber::from(u32::try_from(leaf_pos).unwrap());

        let proof = mmr.open_at(leaf_pos, large_forest).expect("valid proof");
        let adjusted_nodes =
            adjust_merkle_path_for_forest(proof.merkle_path(), block_num, small_forest);
        let adjusted_path = MerklePath::new(adjusted_nodes.iter().map(|(_, n)| *n).collect());

        let peaks = mmr.peaks_at(small_forest).unwrap();
        let mut partial = PartialMmr::from_peaks(peaks);
        let leaf = mmr.get(leaf_pos).expect("leaf exists");

        partial
            .track(leaf_pos, leaf, &adjusted_path)
            .expect("adjusted path should verify against smaller forest peaks");
    }

    #[test]
    fn adjust_merkle_path_correct_indices() {
        // Forest 6 has trees of size 2 and 4
        let forest = Forest::new(6).expect("valid forest");
        // Block 1 is on tree with size 4 (merkle path should have 2 nodes)
        let block_num = BlockNumber::from(1u32);
        let nodes = vec![word(10), word(11), word(12), word(13)];
        let path = MerklePath::new(nodes.clone());

        let adjusted = adjust_merkle_path_for_forest(&path, block_num, forest);

        assert_eq!(adjusted.len(), 2);
        assert_eq!(adjusted[0].1, nodes[0]);
        assert_eq!(adjusted[1].1, nodes[1]);

        let mut idx = InOrderIndex::from_leaf_pos(1);
        let expected0 = idx.sibling();
        idx = idx.parent();
        let expected1 = idx.sibling();

        assert_eq!(adjusted[0].0, expected0);
        assert_eq!(adjusted[1].0, expected1);
    }

    #[test]
    fn adjust_path_limit_correct_when_siblings_in_bounds() {
        // Ensure the expected depth limit matters even when the next sibling
        // is "in-bounds" (but not part of the leaf's subtree for that forest)
        let large_leaves = 8usize;
        let small_leaves = 7usize;
        let leaf_pos = 2usize;
        let mut mmr = Mmr::new();
        for value in 0u64..large_leaves as u64 {
            mmr.add(word(value)).expect("test MMR append should succeed");
        }

        let small_forest = Forest::new(small_leaves).expect("valid forest");
        let proof = mmr
            .open_at(leaf_pos, Forest::new(large_leaves).expect("valid forest"))
            .expect("valid proof");
        let expected_depth =
            small_forest.leaf_to_corresponding_tree(leaf_pos).expect("leaf is in forest") as usize;

        // Confirm the next sibling after the expected depth is still in bounds, which would
        // create an overlong path without the depth cap.
        let mut idx = InOrderIndex::from_leaf_pos(leaf_pos);
        for _ in 0..expected_depth {
            idx = idx.parent();
        }
        let next_sibling = idx.sibling();
        let rightmost = InOrderIndex::from_leaf_pos(small_leaves - 1);
        assert!(next_sibling <= rightmost);
        assert!(proof.merkle_path().depth() as usize > expected_depth);

        let adjusted = adjust_merkle_path_for_forest(
            proof.merkle_path(),
            BlockNumber::from(u32::try_from(leaf_pos).unwrap()),
            small_forest,
        );
        assert_eq!(adjusted.len(), expected_depth);
    }

    #[test]
    fn authenticated_block_nodes_include_leaf_commitment() {
        let block_header = BlockHeader::mock(4, None, None, &[], TransactionKernel.to_commitment());
        let path_nodes = vec![
            (InOrderIndex::from_leaf_pos(4).sibling(), word(10)),
            (InOrderIndex::from_leaf_pos(4).parent().sibling(), word(11)),
        ];

        let nodes = authenticated_block_nodes(&block_header, path_nodes.clone());

        assert_eq!(nodes[0], (InOrderIndex::from_leaf_pos(4), block_header.commitment()));
        assert_eq!(&nodes[1..], path_nodes.as_slice());
    }
}
