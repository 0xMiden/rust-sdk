#![allow(clippy::items_after_statements)]

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::vec::Vec;

use miden_client::Word;
use miden_client::block::BlockHeader;
use miden_client::crypto::{Forest, InOrderIndex, MmrPeaks};
use miden_client::note::BlockNumber;
use miden_client::store::{BlockRelevance, PartialBlockchainFilter, StoreError};
use miden_client::utils::{Deserializable, Serializable};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};

use super::SqliteStore;
use crate::sql_error::SqlResultExt;
use crate::{insert_sql, subst};

struct SerializedBlockHeaderData {
    block_num: u32,
    header: Vec<u8>,
    has_client_notes: bool,
}
struct SerializedBlockHeaderParts {
    _block_num: u64,
    header: Vec<u8>,
    has_client_notes: bool,
}

struct SerializedPartialBlockchainNodeData {
    id: i64,
    node: String,
}
struct SerializedPartialBlockchainNodeParts {
    id: u64,
    node: String,
}

impl SqliteStore {
    pub(crate) fn get_block_headers(
        conn: &mut Connection,
        block_numbers: &BTreeSet<BlockNumber>,
    ) -> Result<Vec<(BlockHeader, BlockRelevance)>, StoreError> {
        let block_number_list = block_numbers
            .iter()
            .map(|block_number| Value::Integer(i64::from(block_number.as_u32())))
            .collect::<Vec<Value>>();

        const QUERY: &str = "SELECT block_num, header, has_client_notes FROM block_headers WHERE block_num IN rarray(?)";

        conn.prepare(QUERY)
            .into_store_error()?
            .query_map(params![Rc::new(block_number_list)], parse_block_headers_columns)
            .into_store_error()?
            .map(|result| {
                let serialized_block_header_parts: SerializedBlockHeaderParts =
                    result.into_store_error()?;
                parse_block_header(&serialized_block_header_parts)
            })
            .collect()
    }

    pub(crate) fn get_tracked_block_headers(
        conn: &mut Connection,
    ) -> Result<Vec<BlockHeader>, StoreError> {
        const QUERY: &str = "SELECT block_num, header, has_client_notes FROM block_headers WHERE has_client_notes=true";
        conn.prepare(QUERY)
            .into_store_error()?
            .query_map(params![], parse_block_headers_columns)
            .into_store_error()?
            .map(|result| {
                let serialized_block_header_parts: SerializedBlockHeaderParts =
                    result.into_store_error()?;
                parse_block_header(&serialized_block_header_parts).map(|(block, _)| block)
            })
            .collect()
    }

    pub(crate) fn get_tracked_block_header_numbers(
        conn: &mut Connection,
    ) -> Result<BTreeSet<usize>, StoreError> {
        const QUERY: &str = "SELECT block_num FROM block_headers WHERE has_client_notes=true";
        conn.prepare(QUERY)
            .into_store_error()?
            .query_map(params![], |row| row.get::<_, u32>(0))
            .into_store_error()?
            .map(|result| {
                let block_num: u32 = result.into_store_error()?;
                Ok(block_num as usize)
            })
            .collect()
    }

    pub(crate) fn get_partial_blockchain_nodes(
        conn: &mut Connection,
        filter: &PartialBlockchainFilter,
    ) -> Result<BTreeMap<InOrderIndex, Word>, StoreError> {
        match filter {
            PartialBlockchainFilter::All => query_partial_blockchain_nodes(
                conn,
                "SELECT id, node FROM partial_blockchain_nodes",
                params![],
            ),

            PartialBlockchainFilter::List(ids) if ids.is_empty() => Ok(BTreeMap::new()),
            PartialBlockchainFilter::List(ids) => {
                let id_values = ids
                    .iter()
                    .map(|id| Value::Integer(i64::try_from(id.inner()).expect("id is a valid i64")))
                    .collect::<Vec<_>>();

                query_partial_blockchain_nodes(
                    conn,
                    "SELECT id, node FROM partial_blockchain_nodes WHERE id IN rarray(?)",
                    params_from_iter([Rc::new(id_values)]),
                )
            },

            PartialBlockchainFilter::Forest(forest) if forest.is_empty() => Ok(BTreeMap::new()),
            PartialBlockchainFilter::Forest(forest) => {
                let max_index = i64::try_from(forest.rightmost_in_order_index().inner())
                    .expect("id is a valid i64");

                query_partial_blockchain_nodes(
                    conn,
                    "SELECT id, node FROM partial_blockchain_nodes WHERE id <= ?",
                    params![max_index],
                )
            },
        }
    }

    pub(crate) fn get_current_blockchain_peaks(
        conn: &mut Connection,
    ) -> Result<MmrPeaks, StoreError> {
        const QUERY: &str =
            "SELECT block_num, partial_blockchain_peaks FROM blockchain_checkpoint LIMIT 1";

        let row: Option<(u32, Vec<u8>)> = conn
            .prepare(QUERY)
            .into_store_error()?
            .query_row(params![], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
            .into_store_error()?;

        match row {
            Some((block_num, peaks_bytes)) if !peaks_bytes.is_empty() => {
                parse_partial_blockchain_peaks(block_num, &peaks_bytes)
            },
            _ => Ok(MmrPeaks::new(Forest::empty(), vec![])?),
        }
    }

    pub(crate) fn insert_block_header(
        conn: &mut Connection,
        block_header: &BlockHeader,
        nodes: &[(InOrderIndex, Word)],
        has_client_notes: bool,
    ) -> Result<(), StoreError> {
        let tx = conn.transaction().into_store_error()?;

        Self::insert_block_header_tx(&tx, block_header, has_client_notes)?;
        Self::insert_partial_blockchain_nodes_tx(&tx, nodes)?;
        tx.commit().into_store_error()?;
        Ok(())
    }

    /// Inserts a list of MMR authentication nodes to the Partial Blockchain nodes table.
    pub(crate) fn insert_partial_blockchain_nodes_tx(
        tx: &Transaction<'_>,
        nodes: &[(InOrderIndex, Word)],
    ) -> Result<(), StoreError> {
        for (index, node) in nodes {
            insert_partial_blockchain_node(tx, *index, *node)?;
        }
        Ok(())
    }

    /// Inserts a block header using a [`rusqlite::Transaction`].
    ///
    /// If the block header exists and `has_client_notes` is `true` then the `has_client_notes`
    /// column is updated to `true` to signify that the block now contains a relevant note.
    pub(crate) fn insert_block_header_tx(
        tx: &Transaction<'_>,
        block_header: &BlockHeader,
        has_client_notes: bool,
    ) -> Result<(), StoreError> {
        let SerializedBlockHeaderData { block_num, header, has_client_notes } =
            serialize_block_header(block_header, has_client_notes);
        const QUERY: &str =
            insert_sql!(block_headers { block_num, header, has_client_notes } | IGNORE);
        tx.execute(QUERY, params![block_num, header, has_client_notes])
            .into_store_error()?;

        set_block_header_has_client_notes(tx, u64::from(block_num), has_client_notes)?;
        Ok(())
    }

    /// Prunes irrelevant block data from the store.
    ///
    /// This performs three operations in a single transaction:
    /// 1. Deletes MMR authentication nodes at the given `node_indices`.
    /// 2. Sets `has_client_notes = false` for `blocks_to_untrack`.
    /// 3. Deletes block headers with `has_client_notes = false` that are not the genesis or
    ///    sync-height block.
    pub fn prune_irrelevant_blocks(
        conn: &mut Connection,
        blocks_to_untrack: &[BlockNumber],
        node_indices_to_remove: &[InOrderIndex],
    ) -> Result<(), StoreError> {
        let tx = conn.transaction().into_store_error()?;

        // 1. Delete stale MMR authentication nodes.
        if !node_indices_to_remove.is_empty() {
            let id_values = node_indices_to_remove
                .iter()
                .map(|id| Value::Integer(i64::try_from(id.inner()).expect("id is a valid i64")))
                .collect::<Vec<_>>();

            tx.execute(
                "DELETE FROM partial_blockchain_nodes WHERE id IN rarray(?)",
                params![Rc::new(id_values)],
            )
            .into_store_error()?;
        }

        // 2. Mark untracked blocks as irrelevant.
        if !blocks_to_untrack.is_empty() {
            let block_values = blocks_to_untrack
                .iter()
                .map(|b| Value::Integer(i64::from(b.as_u32())))
                .collect::<Vec<_>>();

            tx.execute(
                "UPDATE block_headers SET has_client_notes = 0 WHERE block_num IN rarray(?)",
                params![Rc::new(block_values)],
            )
            .into_store_error()?;
        }

        // 3. Delete irrelevant block headers.
        let genesis: u32 = BlockNumber::GENESIS.as_u32();

        let sync_block: Option<u32> = tx
            .query_row("SELECT block_num FROM blockchain_checkpoint LIMIT 1", [], |r| r.get(0))
            .optional()
            .into_store_error()?;

        if let Some(sync_height) = sync_block {
            tx.execute(
                "DELETE FROM block_headers \
                 WHERE has_client_notes = 0 \
                 AND block_num > ?1 \
                 AND block_num < ?2",
                rusqlite::params![genesis, sync_height],
            )
            .into_store_error()?;
        }

        tx.commit().into_store_error()
    }
}

// HELPERS
// ================================================================================================

/// Inserts a node represented by its in-order index and the node value.
fn insert_partial_blockchain_node(
    tx: &Transaction<'_>,
    id: InOrderIndex,
    node: Word,
) -> Result<(), StoreError> {
    let SerializedPartialBlockchainNodeData { id, node } =
        serialize_partial_blockchain_node(id, node);
    const QUERY: &str = insert_sql!(partial_blockchain_nodes { id, node } | IGNORE);
    tx.execute(QUERY, params![id, node]).into_store_error()?;
    Ok(())
}

fn query_partial_blockchain_nodes<P: rusqlite::Params>(
    conn: &mut Connection,
    sql: &str,
    params: P,
) -> Result<BTreeMap<InOrderIndex, Word>, StoreError> {
    let mut stmt = conn.prepare_cached(sql).into_store_error()?;

    stmt.query_map(params, parse_partial_blockchain_nodes_columns)
        .into_store_error()?
        .map(|row_res| {
            let parts: SerializedPartialBlockchainNodeParts = row_res.into_store_error()?;
            parse_partial_blockchain_nodes(&parts)
        })
        .collect()
}

fn parse_partial_blockchain_peaks(forest: u32, peaks_nodes: &[u8]) -> Result<MmrPeaks, StoreError> {
    let mmr_peaks_nodes = Vec::<Word>::read_from_bytes(peaks_nodes)?;

    let forest_size = usize::try_from(forest).expect("u64 should fit in usize");
    let forest = Forest::new(forest_size).map_err(|err| {
        StoreError::DataDeserializationError(
            miden_protocol::utils::serde::DeserializationError::InvalidValue(format!(
                "invalid forest size {forest_size}: {err}"
            )),
        )
    })?;
    MmrPeaks::new(forest, mmr_peaks_nodes).map_err(StoreError::MmrError)
}

fn serialize_block_header(
    block_header: &BlockHeader,
    has_client_notes: bool,
) -> SerializedBlockHeaderData {
    let block_num = block_header.block_num();
    let header = block_header.to_bytes();

    SerializedBlockHeaderData {
        block_num: block_num.as_u32(),
        header,
        has_client_notes,
    }
}

fn parse_block_headers_columns(
    row: &rusqlite::Row<'_>,
) -> Result<SerializedBlockHeaderParts, rusqlite::Error> {
    let block_num: u32 = row.get(0)?;
    let header: Vec<u8> = row.get(1)?;
    let has_client_notes: bool = row.get(2)?;

    Ok(SerializedBlockHeaderParts {
        _block_num: u64::from(block_num),
        header,
        has_client_notes,
    })
}

fn parse_block_header(
    serialized_block_header_parts: &SerializedBlockHeaderParts,
) -> Result<(BlockHeader, BlockRelevance), StoreError> {
    Ok((
        BlockHeader::read_from_bytes(&serialized_block_header_parts.header)?,
        serialized_block_header_parts.has_client_notes.into(),
    ))
}

fn serialize_partial_blockchain_node(
    id: InOrderIndex,
    node: Word,
) -> SerializedPartialBlockchainNodeData {
    let id = i64::try_from(id.inner()).expect("id is a valid i64");
    let node = node.to_hex();
    SerializedPartialBlockchainNodeData { id, node }
}

fn parse_partial_blockchain_nodes_columns(
    row: &rusqlite::Row<'_>,
) -> Result<SerializedPartialBlockchainNodeParts, rusqlite::Error> {
    let id: u64 = row.get(0)?;
    let node = row.get(1)?;
    Ok(SerializedPartialBlockchainNodeParts { id, node })
}

fn parse_partial_blockchain_nodes(
    serialized_partial_blockchain_node_parts: &SerializedPartialBlockchainNodeParts,
) -> Result<(InOrderIndex, Word), StoreError> {
    let id = InOrderIndex::new(
        NonZeroUsize::new(
            usize::try_from(serialized_partial_blockchain_node_parts.id)
                .expect("id is u64, should not fail"),
        )
        .unwrap(),
    );
    let node: Word = Word::try_from(&serialized_partial_blockchain_node_parts.node)?;
    Ok((id, node))
}

pub(crate) fn set_block_header_has_client_notes(
    tx: &Transaction<'_>,
    block_num: u64,
    has_client_notes: bool,
) -> Result<(), StoreError> {
    // Only update to change has_client_notes to true if it was false previously
    const QUERY: &str = "\
        UPDATE block_headers
        SET has_client_notes=?
        WHERE block_num=? AND has_client_notes=FALSE;";
    tx.execute(QUERY, params![has_client_notes, block_num]).into_store_error()?;
    Ok(())
}

#[cfg(test)]
mod test {
    use std::collections::{BTreeMap, BTreeSet};
    use std::vec::Vec;

    use miden_client::Word;
    use miden_client::block::BlockHeader;
    use miden_client::crypto::{Forest, InOrderIndex, MmrPeaks};
    use miden_client::note::BlockNumber;
    use miden_client::store::{PartialBlockchainFilter, Store};
    use miden_client::utils::Serializable;
    use miden_protocol::crypto::merkle::mmr::Mmr;
    use miden_protocol::transaction::TransactionKernel;
    use rusqlite::params;

    use crate::SqliteStore;
    use crate::tests::create_test_store;

    async fn insert_dummy_block_headers(store: &mut SqliteStore) -> Vec<BlockHeader> {
        let block_headers: Vec<BlockHeader> = (0..5)
            .map(|block_num| {
                BlockHeader::mock(block_num, None, None, &[], TransactionKernel.to_commitment())
            })
            .collect();

        let block_headers_clone = block_headers.clone();
        store
            .interact_with_connection(move |conn| {
                let tx = conn.transaction().unwrap();
                (0..5).for_each(|block_num| {
                    SqliteStore::insert_block_header_tx(
                        &tx,
                        &block_headers_clone[block_num],
                        false,
                    )
                    .unwrap();
                });
                tx.commit().unwrap();
                Ok(())
            })
            .await
            .unwrap();

        block_headers
    }

    #[tokio::test]
    async fn insert_and_get_block_headers_by_number() {
        let mut store = create_test_store().await;
        let block_headers = insert_dummy_block_headers(&mut store).await;

        let block_header = Store::get_block_header_by_num(&store, 3.into()).await.unwrap().unwrap();
        assert_eq!(block_headers[3], block_header.0);
    }

    #[tokio::test]
    async fn insert_and_get_block_headers_by_list() {
        let mut store = create_test_store().await;
        let mock_block_headers = insert_dummy_block_headers(&mut store).await;

        let block_headers: Vec<BlockHeader> =
            Store::get_block_headers(&store, &[1.into(), 3.into()].into_iter().collect())
                .await
                .unwrap()
                .into_iter()
                .map(|(block_header, _has_notes)| block_header)
                .collect();
        assert_eq!(
            &[mock_block_headers[1].clone(), mock_block_headers[3].clone()],
            &block_headers[..]
        );
    }

    /// Tests that `insert_block_header` persists the tracked header and its MMR
    /// authentication nodes in the same call, so both are retrievable afterwards.
    #[tokio::test]
    async fn insert_block_header_stores_header_and_nodes() {
        let store = create_test_store().await;
        const TOTAL_BLOCKS: usize = 8;
        let tx_kernel = TransactionKernel.to_commitment();

        let headers: Vec<BlockHeader> = (0..TOTAL_BLOCKS)
            .map(|n| BlockHeader::mock(u32::try_from(n).unwrap(), None, None, &[], tx_kernel))
            .collect();
        let mut mmr = Mmr::default();
        for header in &headers {
            mmr.add(header.commitment()).expect("valid MMR append");
        }

        let tracked: BTreeSet<usize> = [5].into();
        let auth_nodes = collect_auth_nodes(&mmr, &headers, &tracked);
        let header = headers[5].clone();

        Store::insert_block_header(&store, &header, &auth_nodes, true).await.unwrap();

        // The header is stored and marked as tracked.
        let stored = Store::get_block_headers(&store, &[5.into()].into_iter().collect())
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].0, header);

        let tracked = Store::get_tracked_block_header_numbers(&store).await.unwrap();
        assert_eq!(tracked, [5].into());

        // Every authentication node was stored by the same call.
        let stored_nodes =
            Store::get_partial_blockchain_nodes(&store, PartialBlockchainFilter::All)
                .await
                .unwrap();
        let expected: BTreeMap<InOrderIndex, Word> = auth_nodes.iter().copied().collect();
        assert_eq!(stored_nodes, expected);
    }

    /// Tests that a failure inserting the MMR nodes rolls back the block header written in the
    /// same call, proving both land in a single transaction.
    #[tokio::test]
    async fn insert_block_header_rolls_back_header_when_nodes_fail() {
        let store = create_test_store().await;
        let header = BlockHeader::mock(5, None, None, &[], TransactionKernel.to_commitment());
        // One node so the node insert actually runs (an empty slice would be a no-op).
        let nodes = [(InOrderIndex::from_leaf_pos(5), header.commitment())];

        // Force the node insert (the second statement in the transaction) to fail.
        store
            .interact_with_connection(|conn| {
                conn.execute("DROP TABLE partial_blockchain_nodes", []).unwrap();
                Ok(())
            })
            .await
            .unwrap();

        let result = Store::insert_block_header(&store, &header, &nodes, true).await;
        assert!(result.is_err(), "node insert must fail against the dropped table");

        // The header must not survive: a non-atomic two-transaction insert would leave it behind.
        let stored = Store::get_block_headers(&store, &[5.into()].into_iter().collect())
            .await
            .unwrap();
        assert!(stored.is_empty(), "header must roll back when the node insert fails");
    }

    /// Tests that large stored MMRs are built consistently throughout multiple prunes
    #[tokio::test]
    async fn partial_mmr_reconstructs_after_multiple_prune() {
        // Setup (mock a large MMR to work with, with a partial tracked set)
        // ----------------------------------------------------------------------------------------

        let store = create_test_store().await;
        const TOTAL_BLOCKS: usize = 7300;

        let tx_kernel_commitment = TransactionKernel.to_commitment();
        let block_headers: Vec<BlockHeader> = (0..TOTAL_BLOCKS)
            .map(|block_num| {
                BlockHeader::mock(
                    u32::try_from(block_num).unwrap(),
                    None,
                    None,
                    &[],
                    tx_kernel_commitment,
                )
            })
            .collect();

        let mut mmr = Mmr::default();
        for header in &block_headers {
            mmr.add(header.commitment()).expect("valid MMR append");
        }

        let mut tracked_set: BTreeSet<usize> = (0..(TOTAL_BLOCKS - 1)).step_by(97).collect();
        tracked_set.insert(TOTAL_BLOCKS - 2);
        let tracked_blocks: Vec<usize> = tracked_set.iter().copied().collect();

        let mut tracked_nodes: BTreeMap<InOrderIndex, Word> = BTreeMap::new();
        for &block_num in &tracked_blocks {
            let header = &block_headers[block_num];
            tracked_nodes.insert(InOrderIndex::from_leaf_pos(block_num), header.commitment());

            let proof = mmr.open(block_num).expect("valid proof");
            let mut idx = InOrderIndex::from_leaf_pos(block_num);
            for node in proof.merkle_path().nodes() {
                tracked_nodes.insert(idx.sibling(), *node);
                idx = idx.parent();
            }
        }
        let tracked_nodes: Vec<(InOrderIndex, Word)> = tracked_nodes.into_iter().collect();

        let peaks_by_block: Vec<MmrPeaks> = (0..TOTAL_BLOCKS)
            .map(|block_num| {
                mmr.peaks_at(Forest::new(block_num).expect("valid forest"))
                    .expect("valid peaks")
            })
            .collect();

        // Save blocks and nodes
        store
            .interact_with_connection(move |conn| {
                let tx = conn.transaction().unwrap();
                for (block_num, block_header) in block_headers.iter().enumerate() {
                    let has_notes = tracked_set.contains(&block_num);
                    SqliteStore::insert_block_header_tx(&tx, block_header, has_notes).unwrap();
                }

                SqliteStore::insert_partial_blockchain_nodes_tx(&tx, &tracked_nodes).unwrap();
                tx.commit().unwrap();
                Ok(())
            })
            .await
            .unwrap();

        let prune_heights = [
            TOTAL_BLOCKS / 5,
            (TOTAL_BLOCKS * 2) / 5,
            (TOTAL_BLOCKS * 3) / 5,
            TOTAL_BLOCKS - 1,
        ];

        // Tests/assertions
        // ----------------------------------------------------------------------------------------

        let mut previous_remaining: Option<i64> = None;
        for height in prune_heights {
            let height_i64 = i64::try_from(height).expect("fits in i64");
            let peaks_bytes = peaks_by_block[height].peaks().to_vec().to_bytes();

            // Update sync height (and the matching MMR peaks) to simulate having synced further
            store
                .interact_with_connection(move |conn| {
                    conn.execute(
                        "UPDATE blockchain_checkpoint SET block_num = ?, partial_blockchain_peaks = ?",
                        params![height_i64, peaks_bytes],
                    )
                    .unwrap();
                    Ok(())
                })
                .await
                .unwrap();

            // Prune
            store.untrack_and_prune_irrelevant_blocks(&[], &[]).await.unwrap();

            // Assert blocks
            let remaining_headers: i64 = store
                .interact_with_connection(|conn| {
                    let count = conn
                        .query_row("SELECT COUNT(*) FROM block_headers", [], |row| row.get(0))
                        .unwrap();
                    Ok(count)
                })
                .await
                .unwrap();
            if let Some(previous) = previous_remaining {
                assert!(remaining_headers < previous);
            } else {
                assert!(remaining_headers < i64::try_from(TOTAL_BLOCKS).unwrap());
            }
            previous_remaining = Some(remaining_headers);
        }

        // Try build MMR
        let partial_mmr = Store::get_current_partial_mmr(&store).await.unwrap();
        assert_eq!(partial_mmr.peaks().hash_peaks(), mmr.peaks().hash_peaks());

        for block_num in tracked_blocks {
            let partial_proof = partial_mmr.open(block_num).expect("partial mmr query succeeds");
            assert!(partial_proof.is_some());
            assert_eq!(
                partial_proof.unwrap().merkle_path(),
                mmr.open(block_num).unwrap().merkle_path()
            );
        }
    }

    /// Collects authentication nodes for a set of tracked leaves in an MMR.
    fn collect_auth_nodes(
        mmr: &Mmr,
        block_headers: &[BlockHeader],
        tracked: &BTreeSet<usize>,
    ) -> Vec<(InOrderIndex, Word)> {
        let mut nodes: BTreeMap<InOrderIndex, Word> = BTreeMap::new();
        for &block_num in tracked {
            nodes.insert(
                InOrderIndex::from_leaf_pos(block_num),
                block_headers[block_num].commitment(),
            );
            let proof = mmr.open(block_num).expect("valid proof");
            let mut idx = InOrderIndex::from_leaf_pos(block_num);
            for node in proof.merkle_path().nodes() {
                nodes.insert(idx.sibling(), *node);
                idx = idx.parent();
            }
        }
        nodes.into_iter().collect()
    }

    /// Tests that `untrack_and_prune_irrelevant_blocks` removes redundant authentication nodes
    /// for untracked blocks while preserving nodes needed by blocks that remain tracked.
    #[tokio::test]
    async fn prune_irrelevant_blocks_removes_redundant_auth_nodes() {
        let store = create_test_store().await;
        const TOTAL_BLOCKS: usize = 16;
        let tx_kernel = TransactionKernel.to_commitment();

        let headers: Vec<BlockHeader> = (0..TOTAL_BLOCKS)
            .map(|n| BlockHeader::mock(u32::try_from(n).unwrap(), None, None, &[], tx_kernel))
            .collect();
        let mut mmr = Mmr::default();
        for h in &headers {
            mmr.add(h.commitment()).expect("valid MMR append");
        }

        // Track blocks 3 and 10; we will untrack 3 later.
        let tracked: BTreeSet<usize> = [3, 10].into();
        let auth_nodes = collect_auth_nodes(&mmr, &headers, &tracked);
        let tip_peaks_bytes = mmr
            .peaks_at(Forest::new(TOTAL_BLOCKS - 1).expect("valid forest"))
            .unwrap()
            .peaks()
            .to_vec()
            .to_bytes();

        // Persist everything.
        let headers_clone = headers.clone();
        store
            .interact_with_connection(move |conn| {
                let tx = conn.transaction().unwrap();
                for (i, header) in headers_clone.iter().enumerate().take(TOTAL_BLOCKS) {
                    SqliteStore::insert_block_header_tx(&tx, header, tracked.contains(&i)).unwrap();
                }
                SqliteStore::insert_partial_blockchain_nodes_tx(&tx, &auth_nodes).unwrap();
                tx.execute(
                    "UPDATE blockchain_checkpoint SET block_num = ?, partial_blockchain_peaks = ?",
                    params![i64::try_from(TOTAL_BLOCKS - 1).unwrap(), tip_peaks_bytes],
                )
                .unwrap();
                tx.commit().unwrap();
                Ok(())
            })
            .await
            .unwrap();

        // Untrack block 3 via the PartialMmr, then prune.
        let mut partial_mmr = Store::get_current_partial_mmr(&store).await.unwrap();
        let removed: Vec<InOrderIndex> =
            partial_mmr.untrack(3).into_iter().map(|(idx, _)| idx).collect();
        assert!(!removed.is_empty(), "untracking should remove at least one node");

        store
            .untrack_and_prune_irrelevant_blocks(&[BlockNumber::from(3u32)], &removed)
            .await
            .unwrap();

        // Block 3 header should be deleted, block 10 should still be provable.
        let rebuilt = Store::get_current_partial_mmr(&store).await.unwrap();
        assert_eq!(rebuilt.peaks().hash_peaks(), mmr.peaks().hash_peaks());

        let proof_10 = rebuilt.open(10).expect("open succeeds");
        assert!(proof_10.is_some(), "block 10 should still be provable");

        let proof_3 = rebuilt.open(3).expect("open succeeds");
        assert!(proof_3.is_none(), "block 3 should no longer be provable");
    }
}
