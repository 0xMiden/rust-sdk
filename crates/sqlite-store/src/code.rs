//! Persists the code registry and the code tree.
//!
//! The code registry holds the MAST forests in the `code_mast_forests` table, the procedure roots
//! of each forest in the `code_procedures` table, and the debug info of a forest, if it is known,
//! in the `code_debug_info` table.

use std::collections::BTreeSet;
use std::string::ToString;
use std::sync::Arc;
use std::vec::Vec;

use miden_client_core::errors::StoreError;
use miden_processor::LoadedMastForest;
use miden_protocol::block::code_tree::{CodeTreeRoot, CodeWitness};
use miden_protocol::code::ProcedureRoot;
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::vm::{Package, PackageDebugInfo};
use miden_protocol::{MastForest, Word};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::forest::{ScopedCodeForest, SqliteForestBackend};
use crate::sql_error::SqlResultExt;
use crate::{SqliteStore, blob_array, with_write_tx};

impl SqliteStore {
    /// Adds the debug info of the MAST forest with the given commitment to the code registry.
    ///
    /// The debug info refers to the nodes of one forest, so it must belong to the forest that the
    /// registry stores with this commitment. Debug info that the registry already has for the
    /// forest is replaced.
    ///
    /// Returns an error if the registry does not have a forest with this commitment.
    pub async fn upsert_debug_info(
        &self,
        mast_forest_commitment: Word,
        debug_info: PackageDebugInfo,
    ) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::write_debug_info(conn, mast_forest_commitment, &debug_info)
        })
        .await
    }

    // HELPERS
    // --------------------------------------------------------------------------------------------

    /// Returns a witness that proves the presence or absence of the procedure in the code tree.
    pub(crate) fn query_procedure_witness(
        conn: &mut Connection,
        procedure_root: ProcedureRoot,
    ) -> Result<CodeWitness, StoreError> {
        let tx = conn.transaction().into_store_error()?;
        let forest = ScopedCodeForest::new(SqliteForestBackend::new(&tx))?;
        forest.get_procedure_witness(procedure_root)
    }

    /// Returns the root of the code tree.
    pub(crate) fn query_code_tree_root(conn: &mut Connection) -> Result<CodeTreeRoot, StoreError> {
        let tx = conn.transaction().into_store_error()?;
        let forest = ScopedCodeForest::new(SqliteForestBackend::new(&tx))?;
        Ok(forest.code_tree_root())
    }

    /// Adds the MAST forests of the packages, their procedure roots and their debug info to the
    /// code registry, and their procedure roots to the code tree, in one database transaction.
    ///
    /// A package whose forest already has debug info in the registry is skipped. A package without
    /// debug info is added without debug info. The packages to skip are found with a read, so the
    /// debug info is decoded and a write transaction is opened only when a package is missing.
    pub(crate) fn write_procedures_with_debug_info(
        conn: &mut Connection,
        packages: &[Arc<Package>],
    ) -> Result<(), StoreError> {
        let commitments = blob_array(
            packages.iter().map(|package| package.mast_forest().commitment().as_bytes()),
        );
        let stored: BTreeSet<Vec<u8>> = conn
            .prepare_cached("SELECT commitment FROM code_debug_info WHERE commitment IN rarray(?1)")
            .into_store_error()?
            .query_map(params![commitments], |row| row.get(0))
            .into_store_error()?
            .collect::<Result<_, _>>()
            .into_store_error()?;

        let mut missing = Vec::new();
        for package in packages {
            let mast = package.mast_forest();
            if stored.contains(mast.commitment().as_bytes().as_slice()) {
                continue;
            }
            let debug_info =
                package.debug_info().map_err(|err| StoreError::ParsingError(err.to_string()))?;
            missing.push((Arc::clone(mast), debug_info));
        }
        if missing.is_empty() {
            return Ok(());
        }

        with_write_tx(conn, |tx| {
            let forests: Vec<Arc<MastForest>> =
                missing.iter().map(|(mast, _)| Arc::clone(mast)).collect();
            SqliteStore::write_procedures(tx, &forests)?;
            for (mast, debug_info) in &missing {
                if let Some(debug_info) = debug_info {
                    SqliteStore::write_debug_info(tx, mast.commitment(), debug_info)?;
                }
            }
            Ok(())
        })
    }

    /// Adds the procedures of the MAST forests to the code registry and to the code tree, in the
    /// transaction `tx`.
    pub(crate) fn write_procedures(
        tx: &Transaction<'_>,
        procedures: &[Arc<MastForest>],
    ) -> Result<(), StoreError> {
        let procedure_roots: Vec<ProcedureRoot> = procedures
            .iter()
            .flat_map(|procedure| procedure.local_procedure_digests())
            .map(ProcedureRoot::from_raw)
            .collect();
        if procedure_roots.is_empty() {
            return Ok(());
        }

        let mut code_smt_forest = ScopedCodeForest::new(SqliteForestBackend::new(tx))?;
        code_smt_forest.insert_procedure_roots(&procedure_roots)?;

        let mut insert_forest = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO code_mast_forests (commitment, mast) VALUES (?1, ?2)",
            )
            .into_store_error()?;
        let mut insert_procedure = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO code_procedures (procedure_root, commitment) \
                 VALUES (?1, ?2)",
            )
            .into_store_error()?;
        for mast in procedures {
            let mut procedure_roots = mast.local_procedure_digests().peekable();
            if procedure_roots.peek().is_none() {
                continue;
            }

            let commitment = mast.commitment();
            insert_forest
                .execute(params![commitment.as_bytes(), mast.to_bytes()])
                .into_store_error()?;
            for procedure_root in procedure_roots {
                insert_procedure
                    .execute(params![procedure_root.as_bytes(), commitment.as_bytes()])
                    .into_store_error()?;
            }
        }
        Ok(())
    }

    /// Adds or replaces the debug info of the MAST forest with the given commitment.
    fn write_debug_info(
        conn: &Connection,
        mast_forest_commitment: Word,
        debug_info: &PackageDebugInfo,
    ) -> Result<(), StoreError> {
        // The foreign key of `code_debug_info` rejects a commitment without a stored forest.
        conn.execute(
            "INSERT OR REPLACE INTO code_debug_info (commitment, debug_info) VALUES (?1, ?2)",
            params![mast_forest_commitment.as_bytes(), debug_info.to_bytes()],
        )
        .into_store_error()?;
        Ok(())
    }

    /// Returns the MAST forest that holds the procedure, with the debug info of the forest if the
    /// registry has it.
    pub(crate) fn query_procedure(
        conn: &Connection,
        procedure_root: ProcedureRoot,
    ) -> Result<Option<LoadedMastForest>, StoreError> {
        const QUERY: &str = "SELECT f.mast, d.debug_info FROM code_procedures p \
             JOIN code_mast_forests f ON f.commitment = p.commitment \
             LEFT JOIN code_debug_info d ON d.commitment = p.commitment \
             WHERE p.procedure_root = ?1";

        let row: Option<(Vec<u8>, Option<Vec<u8>>)> = conn
            .prepare_cached(QUERY)
            .into_store_error()?
            .query_row([procedure_root.as_word().as_bytes()], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
            .into_store_error()?;
        let Some((mast, debug_info)) = row else {
            return Ok(None);
        };

        let mast = Arc::new(MastForest::read_from_bytes(&mast)?);
        let debug_info =
            debug_info.map(|bytes| PackageDebugInfo::read_from_bytes(&bytes)).transpose()?;
        Ok(Some(LoadedMastForest::with_package_debug_info(mast, Ok(debug_info))))
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::vec::Vec;

    use miden_client_core::store::Store;
    use miden_protocol::ProtocolLib;
    use miden_protocol::block::code_tree::CodeTree;
    use miden_standards::StandardsLib;

    use super::*;
    use crate::tests::create_test_store;

    fn protocol_package() -> Arc<Package> {
        Arc::new(ProtocolLib::default().as_ref().clone())
    }

    fn procedure_roots_of(procedure: &MastForest) -> BTreeSet<ProcedureRoot> {
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

        store.insert_procedures(vec![protocol.clone()]).await.unwrap();

        for procedure_root in procedure_roots_of(&protocol) {
            let stored = store.get_procedure(procedure_root).await.unwrap().unwrap();
            assert_eq!(stored.mast_forest().commitment(), protocol.commitment());
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

        store.insert_procedures(vec![protocol.clone()]).await.unwrap();
        store.insert_procedures(vec![standards.clone()]).await.unwrap();

        let roots: BTreeSet<_> = procedure_roots_of(&protocol)
            .union(&procedure_roots_of(&standards))
            .copied()
            .collect();
        let code_tree = CodeTree::with_entries(roots.iter().copied()).unwrap();
        assert_eq!(store.get_code_tree_root().await.unwrap(), code_tree.root());

        let present = *roots.first().unwrap();
        for procedure_root in [present, absent_procedure_root()] {
            let witness = store.get_procedure_witness(procedure_root).await.unwrap();
            assert_eq!(witness, code_tree.open(&procedure_root));
        }
    }

    /// A procedure that two forests share stays bound to the forest that was added first.
    #[tokio::test]
    async fn shared_procedure_keeps_the_first_forest() {
        let protocol = ProtocolLib::default().mast_forest().clone();
        let standards = StandardsLib::default().mast_forest().clone();
        let shared_roots: Vec<ProcedureRoot> = procedure_roots_of(&protocol)
            .intersection(&procedure_roots_of(&standards))
            .copied()
            .collect();
        assert!(!shared_roots.is_empty(), "the forests must share a procedure");

        for (first, second) in [(&protocol, &standards), (&standards, &protocol)] {
            let store = create_test_store().await;
            store.insert_procedures(vec![first.clone()]).await.unwrap();
            store.insert_procedures(vec![second.clone()]).await.unwrap();

            for procedure_root in &shared_roots {
                let stored = store.get_procedure(*procedure_root).await.unwrap().unwrap();
                assert_eq!(stored.mast_forest().commitment(), first.commitment());
            }
        }
    }

    #[tokio::test]
    async fn adding_the_same_procedure_twice_changes_nothing() {
        let store = create_test_store().await;
        let protocol = ProtocolLib::default().mast_forest().clone();

        store.insert_procedures(vec![protocol.clone()]).await.unwrap();
        let root = store.get_code_tree_root().await.unwrap();
        let counts = row_counts(&store).await;

        store.insert_procedures(vec![protocol]).await.unwrap();

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

        // Drop the procedures table, so that the registry insert of `insert_procedures` fails after
        // the code tree update
        store
            .interact_with_connection(|conn| {
                conn.execute_batch("DROP TABLE code_procedures").into_store_error()
            })
            .await
            .unwrap();

        // Add a forest and check that it fails
        assert!(store.insert_procedures(vec![protocol]).await.is_err());

        // Check that the code tree did not change
        assert_eq!(store.get_code_tree_root().await.unwrap(), empty_root);
    }

    /// Debug info is served with its forest, and stays when the same forest is added again.
    #[tokio::test]
    async fn get_procedure_serves_debug_info() {
        let store = create_test_store().await;
        let package = protocol_package();
        let mast = package.mast_forest().clone();
        let procedure_root =
            ProcedureRoot::from_raw(mast.local_procedure_digests().next().unwrap());

        store.insert_procedures(vec![mast.clone()]).await.unwrap();
        store
            .upsert_debug_info(mast.commitment(), package.debug_info().unwrap().unwrap())
            .await
            .unwrap();

        let loaded = store.get_procedure(procedure_root).await.unwrap().unwrap();
        assert_eq!(loaded.mast_forest().commitment(), mast.commitment());
        assert!(loaded.package_debug_info().unwrap().is_some());

        store.insert_procedures(vec![mast]).await.unwrap();

        let loaded = store.get_procedure(procedure_root).await.unwrap().unwrap();
        assert!(loaded.package_debug_info().unwrap().is_some());
    }

    /// A forest is added with its debug info, and its procedure roots are added to the code tree.
    /// Adding it again changes nothing.
    #[tokio::test]
    async fn insert_procedures_with_debug_info_adds_debug_info() {
        let store = create_test_store().await;
        let package = protocol_package();
        let procedure_root = ProcedureRoot::from_raw(
            package.mast_forest().local_procedure_digests().next().unwrap(),
        );

        store.insert_procedures_with_debug_info(vec![package.clone()]).await.unwrap();

        let loaded = store.get_procedure(procedure_root).await.unwrap().unwrap();
        assert!(loaded.package_debug_info().unwrap().is_some());
        let code_tree = CodeTree::with_entries(procedure_roots_of(package.mast_forest())).unwrap();
        assert_eq!(store.get_code_tree_root().await.unwrap(), code_tree.root());

        let counts = row_counts(&store).await;
        store.insert_procedures_with_debug_info(vec![package]).await.unwrap();
        assert_eq!(row_counts(&store).await, counts);
        assert_eq!(store.get_code_tree_root().await.unwrap(), code_tree.root());
    }

    #[tokio::test]
    async fn insert_procedures_with_debug_info_adds_a_package_without_debug_info() {
        let store = create_test_store().await;
        let mut package = ProtocolLib::default().as_ref().clone();
        package.sections.retain(|section| !section.id.is_debug());
        let package = Arc::new(package);
        let procedure_root = ProcedureRoot::from_raw(
            package.mast_forest().local_procedure_digests().next().unwrap(),
        );

        store.insert_procedures_with_debug_info(vec![package.clone()]).await.unwrap();

        let loaded = store.get_procedure(procedure_root).await.unwrap().unwrap();
        assert!(loaded.package_debug_info().unwrap().is_none());
        let code_tree = CodeTree::with_entries(procedure_roots_of(package.mast_forest())).unwrap();
        assert_eq!(store.get_code_tree_root().await.unwrap(), code_tree.root());
    }

    #[tokio::test]
    async fn debug_info_requires_a_stored_forest() {
        let store = create_test_store().await;
        let package = protocol_package();

        let result = store
            .upsert_debug_info(
                package.mast_forest().commitment(),
                package.debug_info().unwrap().unwrap(),
            )
            .await;

        assert!(result.is_err());
    }
}
