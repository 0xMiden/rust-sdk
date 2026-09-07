use alloc::collections::BTreeMap;

use miden_protocol::Word;
use miden_protocol::crypto::merkle::MerkleError;
use miden_protocol::crypto::merkle::smt::{PartialSmt, SmtProof};

// STAGED SMT
// ================================================================================================

/// One account SMT (the vault or a storage map slot) as a batch in progress sees it: proofs
/// anchored at the tree's committed root plus the absolute values of the keys in-batch
/// transactions wrote.
///
/// Replaying the writes onto the proofs ([`Self::staged_view`]) yields the current in-batch tree,
/// from which a witness for any key can be opened — the key only needs its committed-root proof
/// supplied, which the store can always serve since the committed state is never mutated while a
/// batch is being built. The one exception is a tree anchored at the empty root (a fresh or
/// removed map slot): there every key is implicitly provable and no proofs are needed at all.
#[derive(Clone)]
pub(crate) struct StagedSmt {
    /// Committed-root proofs for every key an in-batch transaction has written so far. Its root
    /// is the committed root the view is anchored at.
    committed: PartialSmt,
    /// Absolute leaf values written in-batch, keyed by hashed leaf key.
    entries: BTreeMap<Word, Word>,
    /// The in-batch root: the committed root with `entries` replayed on top.
    current_root: Word,
}

impl StagedSmt {
    /// Creates a staged view anchored at `committed_root`, with no in-batch writes yet.
    pub fn new(committed_root: Word) -> Self {
        Self {
            committed: PartialSmt::new(committed_root),
            entries: BTreeMap::new(),
            current_root: committed_root,
        }
    }

    /// Creates a staged view anchored at the empty tree, for a storage map slot an in-batch
    /// transaction removed.
    pub fn empty() -> Self {
        Self::new(PartialSmt::EMPTY_ROOT)
    }

    /// Returns the committed root the view is anchored at.
    pub fn committed_root(&self) -> Word {
        self.committed.root()
    }

    /// Returns the current in-batch root.
    pub fn current_root(&self) -> Word {
        self.current_root
    }

    /// Folds a transaction's writes into the view and returns the new in-batch root.
    ///
    /// `committed_proofs` must cover every written key (except keys whose committed path is
    /// already tracked, e.g. by an earlier write or an empty subtree).
    pub fn apply_entries(
        &mut self,
        committed_proofs: impl IntoIterator<Item = SmtProof>,
        entries: impl IntoIterator<Item = (Word, Word)>,
    ) -> Result<Word, MerkleError> {
        for proof in committed_proofs {
            self.committed.add_proof(proof)?;
        }
        self.entries.extend(entries);
        self.current_root = self.staged_view(None)?.root();
        Ok(self.current_root)
    }

    /// Returns the in-batch tree: the committed view (plus any extra committed-root proofs, e.g.
    /// for the key about to be opened) with all in-batch writes replayed. The returned tree's
    /// root equals [`Self::current_root`], so witnesses opened from it are valid against the
    /// in-batch state.
    pub fn staged_view(
        &self,
        extra_committed_proofs: impl IntoIterator<Item = SmtProof>,
    ) -> Result<PartialSmt, MerkleError> {
        let mut staged = self.committed.clone();
        for proof in extra_committed_proofs {
            staged.add_proof(proof)?;
        }
        for (key, value) in &self.entries {
            staged.insert(*key, *value)?;
        }
        Ok(staged)
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::crypto::merkle::smt::Smt;
    use miden_protocol::{Felt, ONE, ZERO};

    use super::*;

    fn felt(n: u64) -> Felt {
        Felt::new(n).unwrap()
    }

    fn key(n: u64) -> Word {
        [felt(n), ZERO, ZERO, felt(n)].into()
    }

    fn value(n: u64) -> Word {
        [felt(n), felt(n), ONE, ONE].into()
    }

    /// A committed tree with three entries used as the baseline for the tests below.
    fn committed_tree() -> Smt {
        Smt::with_entries([(key(1), value(1)), (key(2), value(2)), (key(3), value(3))]).unwrap()
    }

    #[test]
    fn roots_track_the_full_tree_across_writes() {
        let mut full = committed_tree();
        let committed = full.clone();
        let mut staged = StagedSmt::new(full.root());

        // First write: update an existing key and add a new one.
        let proofs = [committed.open(&key(1)), committed.open(&key(4))];
        full.insert(key(1), value(10)).unwrap();
        full.insert(key(4), value(4)).unwrap();
        let root = staged.apply_entries(proofs, [(key(1), value(10)), (key(4), value(4))]).unwrap();
        assert_eq!(root, full.root());

        // Second write: re-supplying a committed proof for an already-written key is harmless.
        full.insert(key(1), value(11)).unwrap();
        let root = staged.apply_entries([committed.open(&key(1))], [(key(1), value(11))]).unwrap();
        assert_eq!(root, full.root());
    }

    #[test]
    fn witness_for_untouched_key_opens_at_in_batch_root() {
        let mut full = committed_tree();
        let committed = full.clone();
        let mut staged = StagedSmt::new(full.root());

        full.insert(key(2), value(20)).unwrap();
        staged.apply_entries([committed.open(&key(2))], [(key(2), value(20))]).unwrap();

        // key(3) was never written in-batch; its committed proof anchors the staged view.
        let view = staged.staged_view([committed.open(&key(3))]).unwrap();
        assert_eq!(view.root(), staged.current_root());
        assert_eq!(view.open(&key(3)).unwrap(), full.open(&key(3)));

        // An absent key gets an emptiness proof the same way.
        let view = staged.staged_view([committed.open(&key(9))]).unwrap();
        assert_eq!(view.open(&key(9)).unwrap(), full.open(&key(9)));
    }

    #[test]
    fn empty_anchored_view_serves_and_accepts_any_key_without_proofs() {
        let mut staged = StagedSmt::empty();

        let view = staged.staged_view(None).unwrap();
        assert_eq!(view.root(), PartialSmt::EMPTY_ROOT);
        view.open(&key(7)).unwrap();

        let mut full = Smt::new();
        full.insert(key(7), value(7)).unwrap();
        let root = staged.apply_entries(None, [(key(7), value(7))]).unwrap();
        assert_eq!(root, full.root());
    }
}
