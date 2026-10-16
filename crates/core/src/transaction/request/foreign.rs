//! Contains structures and functions related to FPI (Foreign Procedure Invocation) transactions.
use alloc::string::{String, ToString};
use core::cmp::Ordering;
use core::fmt::Write as _;

use miden_protocol::account::{AccountId, PartialAccount};
use miden_protocol::transaction::{AccountInputs, TransactionScript};
use miden_protocol::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};
use miden_protocol::vm::MIN_STACK_DEPTH;
use miden_protocol::{Felt, Word};
use miden_standards::code_builder::CodeBuilder;

use super::TransactionRequestError;
use crate::rpc::AccountStorageRequirements;

// FPI SCRIPT
// ================================================================================================

/// Builds a transaction script that invokes the procedure with the given root on a foreign account.
///
/// `args` are the procedure's inputs, pushed so that `args[0]` ends up on top of the stack. The
/// kernel reads them as a fixed window of [`MIN_STACK_DEPTH`] felts, so no more may be passed.
///
/// The script leaves the procedure's outputs on top of the stack and drops the rest.
pub fn build_fpi_script(
    code_builder: CodeBuilder,
    foreign_account_id: AccountId,
    procedure_root: Word,
    args: &[Felt],
) -> Result<TransactionScript, TransactionRequestError> {
    if args.len() > MIN_STACK_DEPTH {
        return Err(TransactionRequestError::ForeignProcedureInputsTooLong {
            max: MIN_STACK_DEPTH,
            actual: args.len(),
        });
    }

    let mut script = String::from(
        "use miden::protocol::tx\nuse miden::core::sys\n\n@transaction_script\npub proc main\n",
    );

    // Fill the unused input slots with zeros, then push the args on top of them.
    let pad_count = MIN_STACK_DEPTH - args.len();
    for _ in 0..pad_count / 4 {
        script.push_str("    padw\n");
    }
    for _ in 0..pad_count % 4 {
        script.push_str("    push.0\n");
    }
    for arg in args.iter().rev() {
        writeln!(script, "    push.{arg}").expect("writing to a string never fails");
    }

    writeln!(script, "    push.{}", procedure_root.to_hex())
        .expect("writing to a string never fails");
    writeln!(script, "    push.{}", foreign_account_id.prefix().as_u64())
        .expect("writing to a string never fails");
    writeln!(script, "    push.{}", foreign_account_id.suffix())
        .expect("writing to a string never fails");

    script.push_str("    exec.tx::execute_foreign_procedure\n");
    script.push_str("    exec.sys::truncate_stack\n");
    script.push_str("end\n");

    Ok(code_builder.compile_tx_script(&script)?)
}

// FOREIGN ACCOUNT
// ================================================================================================

/// Account types for foreign procedure invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum ForeignAccount {
    /// Account with public visibility whose state and code will be retrieved from the network at
    /// execution time. Declaring it upfront lets you specify [`AccountStorageRequirements`] so the
    /// correct storage map entries are fetched in a single RPC call. If not declared, the account
    /// is lazily loaded with empty storage requirements, and any storage map accesses will trigger
    /// additional RPC calls during execution.
    Public(AccountId, AccountStorageRequirements),
    /// Private account that requires a [`PartialAccount`] to be provided by the caller. An account
    /// witness will be retrieved from the network at execution time so that it can be used as
    /// inputs to the transaction kernel.
    Private(PartialAccount),
    /// Account whose state and inclusion witness the caller supplies, so nothing is fetched for it
    /// at execution time.
    ///
    /// The witness opens against the account tree of exactly one block, so inputs fetched at block
    /// `N` are only valid for a transaction whose reference block is `N`: the anchor's block under
    /// `Client::execute_transaction_at`, or the sync height at execution time otherwise. Storage
    /// map keys and vault assets absent from the inputs are still resolved lazily during execution.
    Prefetched(AccountInputs),
}

impl ForeignAccount {
    /// Creates a new [`ForeignAccount::Public`]. The account's components (code, storage header and
    /// inclusion proof) will be retrieved at execution time, alongside particular storage slot maps
    /// correspondent to keys passed in `indices`.
    pub fn public(
        account_id: AccountId,
        storage_requirements: AccountStorageRequirements,
    ) -> Result<Self, TransactionRequestError> {
        if !account_id.is_public() {
            return Err(TransactionRequestError::InvalidForeignAccountId(account_id));
        }

        Ok(Self::Public(account_id, storage_requirements))
    }

    /// Creates a new [`ForeignAccount::Private`]. A proof of the account's inclusion will be
    /// retrieved at execution time.
    pub fn private(account: impl Into<PartialAccount>) -> Result<Self, TransactionRequestError> {
        let partial_account: PartialAccount = account.into();
        if partial_account.id().is_public() {
            return Err(TransactionRequestError::InvalidForeignAccountId(partial_account.id()));
        }

        Ok(Self::Private(partial_account))
    }

    pub fn storage_slot_requirements(&self) -> AccountStorageRequirements {
        match self {
            ForeignAccount::Public(_, account_storage_requirements) => {
                account_storage_requirements.clone()
            },
            ForeignAccount::Private(_) | ForeignAccount::Prefetched(_) => {
                AccountStorageRequirements::default()
            },
        }
    }

    /// Returns the foreign account's [`AccountId`].
    pub fn account_id(&self) -> AccountId {
        match self {
            ForeignAccount::Public(account_id, _) => *account_id,
            ForeignAccount::Private(partial_account) => partial_account.id(),
            ForeignAccount::Prefetched(inputs) => inputs.id(),
        }
    }
}

impl From<AccountInputs> for ForeignAccount {
    fn from(inputs: AccountInputs) -> Self {
        Self::Prefetched(inputs)
    }
}

impl Ord for ForeignAccount {
    fn cmp(&self, other: &Self) -> Ordering {
        self.account_id().cmp(&other.account_id())
    }
}

impl PartialOrd for ForeignAccount {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Serializable for ForeignAccount {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        match self {
            ForeignAccount::Public(account_id, storage_requirements) => {
                target.write(0u8);
                account_id.write_into(target);
                storage_requirements.write_into(target);
            },
            ForeignAccount::Private(partial_account) => {
                target.write(1u8);
                partial_account.write_into(target);
            },
            ForeignAccount::Prefetched(inputs) => {
                target.write(2u8);
                inputs.write_into(target);
            },
        }
    }
}

impl Deserializable for ForeignAccount {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let account_type: u8 = source.read_u8()?;
        match account_type {
            0 => {
                let account_id = AccountId::read_from(source)?;
                let storage_requirements = AccountStorageRequirements::read_from(source)?;
                Ok(ForeignAccount::Public(account_id, storage_requirements))
            },
            1 => {
                let foreign_inputs = PartialAccount::read_from(source)?;
                Ok(ForeignAccount::Private(foreign_inputs))
            },
            2 => Ok(ForeignAccount::Prefetched(AccountInputs::read_from(source)?)),
            _ => Err(DeserializationError::InvalidValue("Invalid account type".to_string())),
        }
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET;

    use super::*;

    /// The inputs are read as a fixed window, so a longer argument list is rejected.
    #[test]
    fn build_fpi_script_rejects_more_args_than_the_input_window() {
        let foreign_id: AccountId = ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET.try_into().unwrap();
        let arg = Felt::new(1).expect("one is a valid field element");
        let args = vec![arg; MIN_STACK_DEPTH + 1];

        let err = build_fpi_script(CodeBuilder::new(), foreign_id, Word::empty(), &args)
            .expect_err("a longer argument list must be rejected");

        assert!(matches!(
            err,
            TransactionRequestError::ForeignProcedureInputsTooLong { max, actual }
                if max == MIN_STACK_DEPTH && actual == MIN_STACK_DEPTH + 1
        ));
    }
}
