//! Contains the transaction script that upgrades the code of the executing account.
use miden_protocol::transaction::TransactionScript;
use miden_standards::code_builder::CodeBuilder;

use crate::utils::LazyLock;

// ACCOUNT CODE UPGRADE SCRIPT
// ================================================================================================

/// Source of the transaction script that upgrades the code of the executing account.
///
/// The script reads the new code commitment from `TX_SCRIPT_ARGS`, so one script serves every
/// upgrade.
const ACCOUNT_CODE_UPGRADE_SCRIPT: &str = "
use miden::standards::account_upgrade

#! Upgrades the code of the executing account to the code with NEW_CODE_COMMITMENT.
#!
#! Inputs:  [NEW_CODE_COMMITMENT, pad(12)]
#! Outputs: [pad(16)]
@transaction_script
pub proc main
    padw swapw
    # => [NEW_CODE_COMMITMENT, STORAGE_UPGRADE_COMMITMENT, pad(12)]

    call.account_upgrade::upgrade
    # => [pad(20)]

    dropw
    # => [pad(16)]
end
";

static ACCOUNT_CODE_UPGRADE_TX_SCRIPT: LazyLock<TransactionScript> = LazyLock::new(|| {
    CodeBuilder::new()
        .compile_tx_script(ACCOUNT_CODE_UPGRADE_SCRIPT)
        .expect("the account code upgrade script should compile")
});

/// Returns the transaction script that upgrades the code of the executing account.
///
/// The script calls the `upgrade` procedure of the
/// [`UpgradeManager`](miden_standards::account::upgrade::UpgradeManager) component with the new
/// code commitment from `TX_SCRIPT_ARGS` and an empty storage upgrade commitment.
pub(super) fn account_code_upgrade_script() -> TransactionScript {
    ACCOUNT_CODE_UPGRADE_TX_SCRIPT.clone()
}
