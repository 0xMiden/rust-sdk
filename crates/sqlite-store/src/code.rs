//! Persists the code registry and the code tree.
//!
//! The code registry holds the MAST forests in the `code_mast_forests` table, and the procedure
//! roots of each forest in the `code_procedures` table. The code tree holds the same procedure
//! roots in a lineage of the SMT forest tables.

use std::sync::Arc;
use std::vec::Vec;

use miden_client_core::errors::StoreError;
use miden_protocol::MastForest;
use miden_protocol::block::code_tree::{CodeTreeRoot, CodeWitness};
use miden_protocol::code::ProcedureRoot;
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_tx::{LoadedMastForest, MastForestStore};
use rusqlite::{Connection, OptionalExtension, params};

use crate::db_management::pool_manager::{Access, SqlitePoolManager};
use crate::forest::{ScopedCodeForest, SqliteForestBackend};
use crate::sql_error::SqlResultExt;
use crate::{SqliteStore, with_write_tx};

impl SqliteStore {
    // PUBLIC API
    // --------------------------------------------------------------------------------------------

    /// Adds the local procedures of `procedure` to the code registry and to the code tree.
    ///
    /// The registry and the code tree change in the same database transaction. A procedure that is
    /// already registered keeps the forest that it was first registered with.
    pub async fn add_procedure(&self, procedure: Arc<MastForest>) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::insert_procedure(conn, &procedure))
            .await
    }

    /// Returns the MAST forest that holds the procedure, or `None` if the procedure is not
    /// registered.
    pub async fn get_procedure(
        &self,
        procedure_root: ProcedureRoot,
    ) -> Result<Option<Arc<MastForest>>, StoreError> {
        self.interact_with_connection(move |conn| {
            Ok(SqliteStore::query_procedure(conn, procedure_root)?.map(Arc::new))
        })
        .await
    }

    /// Returns a witness that proves the presence or absence of the procedure in the code tree.
    pub async fn get_procedure_witness(
        &self,
        procedure_root: ProcedureRoot,
    ) -> Result<CodeWitness, StoreError> {
        self.interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            let forest = ScopedCodeForest::new(SqliteForestBackend::new(&tx))?;
            forest.get_procedure_witness(procedure_root)
        })
        .await
    }

    /// Returns the root of the code tree.
    pub async fn get_code_tree_root(&self) -> Result<CodeTreeRoot, StoreError> {
        self.interact_with_connection(|conn| {
            let tx = conn.transaction().into_store_error()?;
            let forest = ScopedCodeForest::new(SqliteForestBackend::new(&tx))?;
            Ok(forest.code_tree_root())
        })
        .await
    }

    // HELPERS
    // --------------------------------------------------------------------------------------------

    fn insert_procedure(conn: &mut Connection, procedure: &MastForest) -> Result<(), StoreError> {
        let procedure_roots: Vec<_> = procedure.local_procedure_digests().collect();
        if procedure_roots.is_empty() {
            return Ok(());
        }

        with_write_tx(conn, |tx| {
            let mut forest = ScopedCodeForest::new(SqliteForestBackend::new(tx))?;
            forest.add_procedure(procedure)?;

            let commitment = procedure.commitment();
            tx.execute(
                "INSERT OR IGNORE INTO code_mast_forests (commitment, mast) VALUES (?1, ?2)",
                params![commitment.as_bytes(), procedure.to_bytes()],
            )
            .into_store_error()?;

            let mut insert = tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO code_procedures (procedure_root, commitment) \
                     VALUES (?1, ?2)",
                )
                .into_store_error()?;
            for procedure_root in procedure_roots {
                insert
                    .execute(params![procedure_root.as_bytes(), commitment.as_bytes()])
                    .into_store_error()?;
            }
            Ok(())
        })
    }

    fn query_procedure(
        conn: &Connection,
        procedure_root: ProcedureRoot,
    ) -> Result<Option<MastForest>, StoreError> {
        const QUERY: &str = "SELECT f.mast FROM code_procedures p \
             JOIN code_mast_forests f ON f.commitment = p.commitment \
             WHERE p.procedure_root = ?1";

        let mast: Option<Vec<u8>> = conn
            .prepare_cached(QUERY)
            .into_store_error()?
            .query_row([procedure_root.as_word().as_bytes()], |row| row.get(0))
            .optional()
            .into_store_error()?;

        mast.map(|bytes| MastForest::read_from_bytes(&bytes).map_err(StoreError::from))
            .transpose()
    }
}

// MAST FOREST STORE
// ================================================================================================

/// The executor calls [`MastForestStore::get`] synchronously, so it cannot use the async connection
/// pool. Each call reads through a new read-only connection.
impl MastForestStore for SqliteStore {
    fn get(&self, procedure_hash: &miden_protocol::Word) -> Option<LoadedMastForest> {
        let procedure_root = ProcedureRoot::from_raw(*procedure_hash);
        let mast = SqlitePoolManager::new(self.database_filepath.clone(), Access::ReadOnly)
            .new_connection()
            .into_store_error()
            .and_then(|conn| SqliteStore::query_procedure(&conn, procedure_root));
        match mast {
            Ok(mast) => mast.map(|mast| LoadedMastForest::new(Arc::new(mast))),
            Err(err) => {
                tracing::warn!(%err, %procedure_root, "failed to read a procedure from the store");
                None
            },
        }
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::vec::Vec;

    use miden_protocol::block::code_tree::CodeTree;
    use miden_protocol::{ProtocolLib, Word};
    use miden_standards::StandardsLib;

    use super::*;
    use crate::tests::create_test_store;

    fn local_roots(procedure: &MastForest) -> BTreeSet<ProcedureRoot> {
        procedure.local_procedure_digests().map(ProcedureRoot::from_raw).collect()
    }

    fn absent_procedure_root() -> ProcedureRoot {
        ProcedureRoot::from_raw(Word::from([1u32, 2, 3, 4]))
    }

    async fn row_counts(store: &SqliteStore) -> (u64, u64) {
        store
            .interact_with_connection(|conn| {
                let count = |table: &str| -> Result<u64, StoreError> {
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                        .into_store_error()
                };
                Ok((count("code_mast_forests")?, count("code_procedures")?))
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn added_procedures_can_be_read_back() {
        let store = create_test_store().await;
        let protocol = ProtocolLib::default().mast_forest().clone();

        store.add_procedure(protocol.clone()).await.unwrap();

        for procedure_root in local_roots(&protocol) {
            let stored = store.get_procedure(procedure_root).await.unwrap().unwrap();
            assert_eq!(stored.commitment(), protocol.commitment());
        }
        assert!(store.get_procedure(absent_procedure_root()).await.unwrap().is_none());
    }

    /// The code tree must have the same root and witnesses as the protocol's code tree with the
    /// same procedures, since both are checked against the block header.
    #[tokio::test]
    async fn code_tree_matches_protocol_code_tree() {
        let store = create_test_store().await;
        let protocol = ProtocolLib::default().mast_forest().clone();
        let standards = StandardsLib::default().mast_forest().clone();

        store.add_procedure(protocol.clone()).await.unwrap();
        store.add_procedure(standards.clone()).await.unwrap();

        let roots: BTreeSet<_> =
            local_roots(&protocol).union(&local_roots(&standards)).copied().collect();
        let code_tree = CodeTree::with_entries(roots.iter().copied()).unwrap();
        assert_eq!(store.get_code_tree_root().await.unwrap(), code_tree.root());

        let present = *roots.first().unwrap();
        for procedure_root in [present, absent_procedure_root()] {
            let witness = store.get_procedure_witness(procedure_root).await.unwrap();
            assert_eq!(witness, code_tree.open(&procedure_root));
        }
    }

    #[tokio::test]
    async fn adding_the_same_procedure_twice_changes_nothing() {
        let store = create_test_store().await;
        let protocol = ProtocolLib::default().mast_forest().clone();

        store.add_procedure(protocol.clone()).await.unwrap();
        let root = store.get_code_tree_root().await.unwrap();
        let counts = row_counts(&store).await;

        store.add_procedure(protocol).await.unwrap();

        assert_eq!(store.get_code_tree_root().await.unwrap(), root);
        assert_eq!(row_counts(&store).await, counts);
    }

    /// The code tree and the registry change together: a failed registry insert also discards the
    /// code tree update.
    #[tokio::test]
    async fn failed_registry_insert_rolls_back_the_code_tree() {
        let store = create_test_store().await;
        let empty_root = store.get_code_tree_root().await.unwrap();
        let protocol = ProtocolLib::default().mast_forest().clone();

        let result = store
            .interact_with_connection(move |conn| {
                conn.execute_batch("DROP TABLE code_procedures").into_store_error()?;
                Ok(SqliteStore::insert_procedure(conn, &protocol))
            })
            .await
            .unwrap();

        assert!(result.is_err());
        assert_eq!(store.get_code_tree_root().await.unwrap(), empty_root);
        let forests: u64 = store
            .interact_with_connection(|conn| {
                conn.query_row("SELECT COUNT(*) FROM code_mast_forests", [], |row| row.get(0))
                    .into_store_error()
            })
            .await
            .unwrap();
        assert_eq!(forests, 0);
    }

    #[tokio::test]
    async fn mast_forest_store_serves_added_procedures() {
        let store = create_test_store().await;
        let protocol = ProtocolLib::default().mast_forest().clone();
        let procedure_root: Word = protocol.local_procedure_digests().next().unwrap();

        assert!(MastForestStore::get(&store, &procedure_root).is_none());

        store.add_procedure(protocol.clone()).await.unwrap();

        let loaded = MastForestStore::get(&store, &procedure_root).unwrap();
        assert_eq!(loaded.mast_forest().commitment(), protocol.commitment());
        let roots: Vec<Word> = loaded.mast_forest().local_procedure_digests().collect();
        assert!(roots.contains(&procedure_root));
    }
}

#[cfg(test)]
mod bench;
