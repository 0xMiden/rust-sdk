use alloc::collections::BTreeMap;

use miden_protocol::Word;
use miden_protocol::account::AccountCode;
use miden_tx::LoadedMastForest;

use crate::utils::RwLock;

// ACCOUNT CODE STORE
// ================================================================================================

/// The MAST forests of the account code that a data store serves, keyed by the roots of their local
/// procedures.
///
/// The code registry does not necessarily hold account code, so the data store keeps it in memory
/// for the duration of its executions. It is kept apart from the client's MAST forest cache, so
/// that it is never evicted during an execution.
#[derive(Default)]
pub struct AccountCodeStore {
    forests: RwLock<BTreeMap<Word, LoadedMastForest>>,
}

impl AccountCodeStore {
    /// Returns the forest that holds the procedure, if it is account code that was loaded.
    pub fn get(&self, procedure_root: &Word) -> Option<LoadedMastForest> {
        self.forests.read().get(procedure_root).cloned()
    }

    /// Adds the account code.
    pub fn load_account_code(&self, code: &AccountCode) {
        let forest = code.loaded_mast_forest();
        let mut forests = self.forests.write();
        for procedure_root in forest.mast_forest().local_procedure_digests() {
            forests.insert(procedure_root, forest.clone());
        }
    }
}
