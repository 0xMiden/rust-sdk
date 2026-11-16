use miden_protocol::block::code_tree::{CodeTree, CodeTreeRoot, CodeWitness};
use miden_protocol::code::ProcedureRoot;
use miden_protocol::crypto::merkle::smt::{
    Backend,
    BackendReader,
    LargeSmtForest,
    LineageId,
    Smt,
    SmtForestUpdateBatch,
    TreeId,
    VersionId,
};
use miden_protocol::{Felt, Hasher, MastForest, Word};

use super::StoreError;
use super::smt_forest::forest_error;

/// The value that marks a procedure root as present in the code tree. It is the same value that
/// [`CodeTree`] uses.
const PRESENT_VALUE: Word = Word::new([Felt::ONE, Felt::ZERO, Felt::ZERO, Felt::ZERO]);

/// Returns the lineage identifier of the code tree.
fn code_tree_lineage_id() -> LineageId {
    LineageId::new(Hasher::hash(b"miden-client:code-tree").as_bytes())
}

// CODE SMT FOREST
// ================================================================================================

/// Persists the code tree in an SMT forest.
///
/// The code tree is one lineage of the forest. It has the same keys and values as a [`CodeTree`]:
/// each key is a procedure root and each value marks the root as present. Thus, its root and
/// witnesses are the same as those of a [`CodeTree`] with the same procedure roots.
///
/// The code tree only holds procedure roots. The code of the procedures is not stored here.
pub struct CodeSmtForest<B: BackendReader> {
    forest: LargeSmtForest<B>,
}

impl<B: BackendReader> CodeSmtForest<B> {
    /// Creates a forest over the provided backend, loading tree metadata from it.
    pub fn new(backend: B) -> Result<Self, StoreError> {
        Ok(Self {
            forest: LargeSmtForest::new(backend).map_err(forest_error)?,
        })
    }

    // READERS
    // --------------------------------------------------------------------------------------------

    /// Returns the root of the code tree.
    ///
    /// Before the first procedure is added, this is the root of the empty tree.
    pub fn code_tree_root(&self) -> CodeTreeRoot {
        match self.forest.latest_root(code_tree_lineage_id()) {
            Some(root) => CodeTreeRoot::from_raw(root),
            None => CodeTree::<Smt>::default().root(),
        }
    }

    /// Returns a witness that proves the presence or absence of the procedure root in the code
    /// tree.
    pub fn get_procedure_witness(
        &self,
        procedure_root: ProcedureRoot,
    ) -> Result<CodeWitness, StoreError> {
        let lineage = code_tree_lineage_id();
        let Some(version) = self.forest.latest_version(lineage) else {
            return Ok(CodeTree::<Smt>::default().open(&procedure_root));
        };

        let proof = self
            .forest
            .open(TreeId::new(lineage, version), procedure_root.as_word())
            .map_err(forest_error)?;
        Ok(CodeWitness::new(proof))
    }
}

impl<B: Backend> CodeSmtForest<B> {
    // MUTATORS
    // --------------------------------------------------------------------------------------------

    /// Adds the local procedures of `procedure` to the code tree, at the given version.
    ///
    /// The procedures that are only external references are not added, because their code is not in
    /// `procedure`. A procedure root that is already in the tree is left as it is.
    ///
    /// `new_version` must be greater than the latest version of the code tree.
    pub fn add_procedure(
        &mut self,
        new_version: VersionId,
        procedure: &MastForest,
    ) -> Result<(), StoreError> {
        let mut batch = SmtForestUpdateBatch::empty();
        let operations = batch.operations(code_tree_lineage_id());
        let mut is_empty = true;
        for procedure_root in procedure.local_procedure_digests() {
            operations.add_insert(procedure_root, PRESENT_VALUE);
            is_empty = false;
        }
        if is_empty {
            return Ok(());
        }

        let mutations =
            self.forest.compute_forest_mutations(new_version, batch).map_err(forest_error)?;
        self.forest.apply_mutations(mutations).map_err(forest_error)?;

        Ok(())
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use miden_protocol::ProtocolLib;
    use miden_protocol::crypto::merkle::smt::ForestInMemoryBackend;
    use miden_standards::StandardsLib;

    use super::*;

    fn forest() -> CodeSmtForest<ForestInMemoryBackend> {
        CodeSmtForest::new(ForestInMemoryBackend::new()).unwrap()
    }

    fn procedure_roots(procedures: &[&MastForest]) -> Vec<ProcedureRoot> {
        let mut roots: Vec<ProcedureRoot> = procedures
            .iter()
            .flat_map(|procedure| procedure.local_procedure_digests())
            .map(ProcedureRoot::from_raw)
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }

    #[test]
    fn empty_forest_matches_empty_code_tree() {
        let forest = forest();
        let code_tree = CodeTree::<Smt>::default();
        let procedure_root = ProcedureRoot::from_raw(Word::from([1u32, 2, 3, 4]));

        assert_eq!(forest.code_tree_root(), code_tree.root());
        assert_eq!(
            forest.get_procedure_witness(procedure_root).unwrap(),
            code_tree.open(&procedure_root)
        );
    }

    /// The forest must produce the same root and witnesses as the protocol's code tree, since both
    /// are checked against the block header.
    #[test]
    fn added_procedures_match_code_tree() {
        let protocol = ProtocolLib::default();
        let standards = StandardsLib::default();
        let mut forest = forest();

        forest.add_procedure(1, protocol.mast_forest()).unwrap();
        forest.add_procedure(2, standards.mast_forest()).unwrap();

        let roots = procedure_roots(&[protocol.mast_forest(), standards.mast_forest()]);
        let code_tree = CodeTree::with_entries(roots.iter().copied()).unwrap();
        assert_eq!(forest.code_tree_root(), code_tree.root());

        let present = roots[0];
        let absent = ProcedureRoot::from_raw(Word::from([1u32, 2, 3, 4]));
        for procedure_root in [present, absent] {
            assert_eq!(
                forest.get_procedure_witness(procedure_root).unwrap(),
                code_tree.open(&procedure_root)
            );
        }
    }

    #[test]
    fn adding_the_same_procedure_twice_keeps_the_root() {
        let protocol = ProtocolLib::default();
        let mut forest = forest();

        forest.add_procedure(1, protocol.mast_forest()).unwrap();
        let root = forest.code_tree_root();
        forest.add_procedure(2, protocol.mast_forest()).unwrap();

        assert_eq!(forest.code_tree_root(), root);
    }
}
