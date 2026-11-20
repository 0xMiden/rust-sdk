use alloc::collections::BTreeMap;
use core::num::NonZeroUsize;

use lru::LruCache;
use miden_protocol::Word;
use miden_tx::LoadedMastForest;

use crate::utils::RwLock;

// MAST FOREST LRU CACHE
// ================================================================================================

/// MAST forests kept in memory between executions, with at most a fixed number of forests.
///
/// The client keeps one cache for all its data stores. A forest that a data store reads from the
/// store is added to the cache, so the next executions find it in memory. When the cache is full,
/// adding a forest removes the forest that was used least recently.
///
/// A forest has one entry, keyed by its commitment, and a lookup entry for each of its procedure
/// roots. Thus, the capacity counts forests, not procedures.
pub struct MastForestCache {
    inner: RwLock<Inner>,
}

struct Inner {
    /// The forests, keyed by their commitment, from the most to the least recently used.
    forests: LruCache<Word, LoadedMastForest>,
    /// The commitment of the forest that holds each procedure root.
    roots: BTreeMap<Word, Word>,
}

impl MastForestCache {
    /// The number of forests that the cache keeps by default: the known packages, the account code
    /// and the code of the registry that recent transactions used.
    pub const DEFAULT_CAPACITY: NonZeroUsize = NonZeroUsize::new(32).unwrap();

    /// Creates an empty cache that keeps at most `capacity` forests.
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            inner: RwLock::new(Inner {
                forests: LruCache::new(capacity),
                roots: BTreeMap::new(),
            }),
        }
    }

    /// Returns the forest that holds the procedure, if the cache has it, and marks the forest as
    /// the most recently used.
    pub fn get(&self, procedure_root: &Word) -> Option<LoadedMastForest> {
        let mut inner = self.inner.write();
        let Inner { forests, roots } = &mut *inner;
        let commitment = roots.get(procedure_root)?;
        forests.get(commitment).cloned()
    }

    /// Adds the forest as the most recently used one. If the cache is full, the least recently used
    /// forest and the lookup entries of its procedure roots are removed.
    pub fn insert(&self, forest: &LoadedMastForest) {
        let commitment = forest.mast_forest().commitment();
        let mut inner = self.inner.write();
        let Inner { forests, roots } = &mut *inner;

        for procedure_root in forest.mast_forest().local_procedure_digests() {
            roots.insert(procedure_root, commitment);
        }
        if let Some((evicted, _)) = forests.push(commitment, forest.clone())
            && evicted != commitment
        {
            roots.retain(|_, forest_commitment| *forest_commitment != evicted);
        }
    }
}

impl Default for MastForestCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    use miden_protocol::{MastForest, ProtocolLib};
    use miden_standards::StandardsLib;

    use super::*;

    fn loaded(forest: &Arc<MastForest>) -> LoadedMastForest {
        LoadedMastForest::new(forest.clone())
    }

    fn roots_of(forest: &MastForest) -> Vec<Word> {
        forest.local_procedure_digests().collect()
    }

    fn capacity(forests: usize) -> NonZeroUsize {
        NonZeroUsize::new(forests).unwrap()
    }

    #[test]
    fn any_procedure_root_finds_the_forest() {
        let protocol = ProtocolLib::default().mast_forest().clone();
        let cache = MastForestCache::default();

        cache.insert(&loaded(&protocol));

        for root in roots_of(&protocol) {
            let found = cache.get(&root).unwrap();
            assert_eq!(found.mast_forest().commitment(), protocol.commitment());
        }
        assert!(cache.get(&Word::from([1u32, 2, 3, 4])).is_none());
    }

    #[test]
    fn a_full_cache_removes_the_least_recently_used_forest() {
        let protocol = ProtocolLib::default().mast_forest().clone();
        let standards = StandardsLib::default().mast_forest().clone();
        let cache = MastForestCache::new(capacity(1));
        let protocol_only_root = roots_of(&protocol)
            .into_iter()
            .find(|root| !roots_of(&standards).contains(root))
            .unwrap();

        cache.insert(&loaded(&protocol));
        cache.insert(&loaded(&standards));

        assert!(cache.get(&protocol_only_root).is_none());
        assert!(cache.get(&roots_of(&standards)[0]).is_some());
    }

    #[test]
    fn a_read_makes_the_forest_the_most_recently_used() {
        let protocol = ProtocolLib::default().mast_forest().clone();
        let standards = StandardsLib::default().mast_forest().clone();
        let core = miden_protocol::CoreLibrary::default().package().mast_forest().clone();
        let cache = MastForestCache::new(capacity(2));
        let protocol_only_root = roots_of(&protocol)
            .into_iter()
            .find(|root| !roots_of(&standards).contains(root))
            .unwrap();

        cache.insert(&loaded(&protocol));
        cache.insert(&loaded(&standards));
        // The read makes protocol the most recently used forest, so adding core removes standards.
        assert!(cache.get(&protocol_only_root).is_some());
        cache.insert(&loaded(&core));

        assert!(cache.get(&protocol_only_root).is_some());
        assert!(cache.get(&roots_of(&core)[0]).is_some());
        let standards_only_root = roots_of(&standards)
            .into_iter()
            .find(|root| !roots_of(&protocol).contains(root))
            .unwrap();
        assert!(cache.get(&standards_only_root).is_none());
    }

    #[test]
    fn adding_a_forest_again_keeps_its_roots() {
        let protocol = ProtocolLib::default().mast_forest().clone();
        let cache = MastForestCache::new(capacity(1));

        cache.insert(&loaded(&protocol));
        cache.insert(&loaded(&protocol));

        assert!(cache.get(&roots_of(&protocol)[0]).is_some());
    }
}
