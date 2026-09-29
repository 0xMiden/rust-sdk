use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use clap::Parser;
use miden_client::account::AccountId;
use miden_client::keystore::Keystore;
use miden_client::rpc::domain::account::AccountStorageRequirements;
use miden_client::transaction::{
    AdviceInputs,
    ForeignAccount,
    TransactionRequestBuilder,
    TransactionRequestError,
    build_fpi_script,
};
use miden_client::vm::typed::TypedProcInfo;
use miden_client::vm::{ExecutionError, MIN_STACK_DEPTH, OperationError, error_code_from_msg};
use miden_client::{Client, ClientError, Felt, TransactionExecutorError, Word};

use crate::call_resolution::{
    CallCode,
    LocalAccount,
    ResolvedCall,
    classify_local_account,
    generate_tx_script,
    resolve_call,
};
use crate::commands::account::DEFAULT_ACCOUNT_ID_KEY;
use crate::errors::CliError;
use crate::utils::{print_executed_program_stack, print_executed_transaction};

// CALL COMMAND
// ================================================================================================

/// The transaction kernel's assertion message for a transaction that changes nothing.
///
/// The message is hashed into the error code the assertion carries, and compared against it. It
/// only decides whether an explanatory line is printed: if the kernel ever rewords it, the line
/// stops appearing and nothing else changes.
const EMPTY_TRANSACTION_ASSERTION: &str =
    "executed transaction neither changed the account state, nor consumed any notes";

#[derive(Debug, Clone, Parser)]
#[command(
    about = "Call a procedure on an account and display the result and state delta. Accounts \
             that aren't tracked locally are read from the network and the call is read-only."
)]
pub struct CallCmd {
    /// Account and procedure in the form `<ACCOUNT_ID>:<PROCEDURE>`.
    #[arg(
        value_name = "ACCOUNT_ID:PROCEDURE",
        long_help = "Account and procedure in the form `<ACCOUNT_ID>:<PROCEDURE>`.\n\n\
                     The procedure name is matched against the package's exports with `_` and `-` \
                     treated as equivalent, so it can be written in either snake_case or \
                     kebab-case (e.g. `get_count` matches the WIT export `get-count`)."
    )]
    target: String,

    /// Positional arguments to push onto the stack before calling the procedure.
    #[arg(value_name = "args")]
    args: Vec<String>,

    /// Path to the package (.masp) file containing the procedure. If omitted, `<PROCEDURE>` must be
    /// a hex digest and the output stack is shown as raw felts.
    #[arg(long, short)]
    package: Option<PathBuf>,

    /// Path to a TOML file with advice map entries, in the same format as the `exec` command.
    #[arg(long, short, long_help = crate::advice_inputs::INPUTS_PATH_LONG_HELP)]
    inputs_path: Option<PathBuf>,

    /// A note script package (.masp) for a note the procedure creates. Repeatable.
    ///
    /// The procedure gives only the script root. The package lets the client build the full note.
    #[arg(long = "note-script", value_name = "PACKAGE")]
    note_scripts: Vec<PathBuf>,
}

impl CallCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let ResolvedCall { target_id, call_code, advice_entries } = resolve_call(
            &client,
            &self.target,
            self.package.as_ref(),
            &self.args,
            self.inputs_path.as_ref(),
            &self.note_scripts,
            true,
        )
        .await?;

        let call_target = resolve_call_target(&client, target_id).await?;

        match call_target {
            CallTarget::Local(account_id) => {
                run_local_call(&client, account_id, call_code, advice_entries).await
            },
            CallTarget::Remote { target_id, executor_id, foreign_account } => {
                run_remote_call(
                    &client,
                    target_id,
                    executor_id,
                    foreign_account,
                    call_code,
                    advice_entries,
                )
                .await
            },
        }
    }
}

// HELPERS
// ================================================================================================

/// Prints the values the procedure returned, rendered as their declared types when the package
/// describes them and as raw stack felts otherwise.
fn print_call_result(output_stack: &[Felt; MIN_STACK_DEPTH], typed: Option<&TypedProcInfo>) {
    let Some(typed) = typed else {
        // Nothing says where the results end, so the dump runs to the last non-zero value.
        print_executed_program_stack(output_stack, None);
        return;
    };

    match typed.decode_result(output_stack.as_slice()) {
        // A procedure that returns nothing has no result to show.
        Ok(None) => {},
        Ok(Some(rendered)) => println!("Result: {rendered}"),
        Err(err) => {
            println!("The result is not a valid value of the procedure's return type: {err}");
            print_executed_program_stack(output_stack, typed.output_felt_count());
        },
    }
}

/// Runs a remote call via FPI. FPI cannot mutate the foreign account, so there is no state delta to
/// compute — only the read phase runs.
async fn run_remote_call<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    target_id: AccountId,
    executor_id: AccountId,
    foreign_account: Box<ForeignAccount>,
    call_code: CallCode,
    advice_entries: Vec<(Word, Vec<Felt>)>,
) -> Result<(), CliError> {
    let CallCode { builder, digest, args, typed } = call_code;
    let tx_script =
        build_fpi_script(builder, target_id, digest, &args).map_err(|err| match err {
            TransactionRequestError::ForeignProcedureInputsTooLong { max, actual } => {
                CliError::InvalidArgument(format!(
                    "A call on an account read from the network takes at most {max} input felts; \
                     got {actual}"
                ))
            },
            other => {
                CliError::Transaction(other.into(), "Failed to build the call script".to_string())
            },
        })?;

    let output_stack = client
        .execute_program(
            executor_id,
            tx_script,
            AdviceInputs::default().with_map(advice_entries),
            BTreeMap::from([(target_id, *foreign_account)]),
        )
        .await?;

    print_call_result(&output_stack, typed.as_ref());

    println!("\nA call on an account read from the network can only read it; no state delta.");
    Ok(())
}

/// Runs a local call: a read phase for the return values, then a transaction for the state delta.
/// The account runs the call itself, so the procedure may mutate it.
async fn run_local_call<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    account_id: AccountId,
    call_code: CallCode,
    advice_entries: Vec<(Word, Vec<Felt>)>,
) -> Result<(), CliError> {
    let CallCode { builder, digest, args, typed } = call_code;
    let tx_script = generate_tx_script(builder, &digest, &args)?;

    // 1) Read-only execution to get return values.
    let output_stack = client
        .execute_program(
            account_id,
            tx_script.clone(),
            AdviceInputs::default().with_map(advice_entries.clone()),
            BTreeMap::new(),
        )
        .await?;
    print_call_result(&output_stack, typed.as_ref());

    // 2) Transaction execution to get the state delta.
    let tx_request = TransactionRequestBuilder::new()
        .custom_script(tx_script)
        .extend_advice_map(advice_entries)
        .build()
        .map_err(|err| {
            CliError::Transaction(err.into(), "Failed to build transaction".to_string())
        })?;

    match client.execute_transaction(account_id, tx_request).await {
        Ok(tx_result) => {
            print_executed_transaction(client, tx_result.executed_transaction()).await?;
        },
        Err(e) => report_failed_delta(&e),
    }
    Ok(())
}

/// Resolved call target.
enum CallTarget {
    /// The account is tracked locally, so it runs the call itself and may be mutated by it.
    Local(AccountId),
    /// The account is read from the network and the call runs from a local account.
    Remote {
        target_id: AccountId,
        executor_id: AccountId,
        foreign_account: Box<ForeignAccount>,
    },
}

async fn resolve_call_target<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    target_id: AccountId,
) -> Result<CallTarget, CliError> {
    match classify_local_account(client, target_id).await? {
        LocalAccount::Usable => return Ok(CallTarget::Local(target_id)),
        // A locked account holds outdated state and is always private, so it can't be read from the
        // network either.
        LocalAccount::Locked => {
            return Err(CliError::InvalidArgument(format!(
                "Account {target_id} is locked: its local state doesn't match the network's, so \
                 the call can't run on it."
            )));
        },
        LocalAccount::Untracked => {},
    }

    let foreign_account = ForeignAccount::public(target_id, AccountStorageRequirements::default())
        .map_err(|err| match err {
            TransactionRequestError::InvalidForeignAccountId(_) => {
                CliError::InvalidArgument(format!(
                    "Account {target_id} isn't tracked locally and its state isn't public, so it \
                     can't be read from the network."
                ))
            },
            other => CliError::InvalidArgument(format!(
                "Account {target_id} can't be read from the network: {other}"
            )),
        })?;

    let executor_id = pick_local_executor(client).await?;

    println!(
        "Account {target_id} isn't tracked locally; reading its state from the network and \
         running the call from your account {executor_id}."
    );

    Ok(CallTarget::Remote {
        target_id,
        executor_id,
        foreign_account: Box::new(foreign_account),
    })
}

/// Picks the local account the FPI call runs from, preferring the default account.
///
/// Any account works: the script calls the foreign procedure, not the native account's code. Locked
/// accounts are skipped because their local state doesn't match the node's.
async fn pick_local_executor<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
) -> Result<AccountId, CliError> {
    let default_id: Option<AccountId> =
        client.get_setting(DEFAULT_ACCOUNT_ID_KEY.to_string()).await?;
    if let Some(default_id) = default_id
        && let Some((_, status)) = client.get_account_header(default_id).await?
        && !status.is_locked()
    {
        return Ok(default_id);
    }

    let local_accounts = client.get_account_headers().await?;
    local_accounts
        .iter()
        .find(|(_, status)| !status.is_locked())
        .map(|(header, _)| header.id())
        .ok_or_else(|| {
            CliError::InvalidArgument(
                "Calling an account that isn't tracked locally needs one of your own accounts to \
                 run the call from, and none is usable. Create one with `miden-client new-wallet` \
                 and re-run."
                    .to_string(),
            )
        })
}

/// Returns true when `error` is the transaction kernel's rejection of a transaction that neither
/// changed the account state nor consumed a note.
pub(crate) fn is_empty_transaction_error(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::TransactionExecutorError(
            TransactionExecutorError::TransactionProgramExecutionFailed(
                ExecutionError::OperationError {
                    err: OperationError::FailedAssertion { err_code, .. },
                    ..
                },
            ),
        ) if *err_code == error_code_from_msg(EMPTY_TRANSACTION_ASSERTION)
    )
}

/// Reports a transaction that did not execute, in place of the state delta it would have shown.
///
/// The read-only execution has already printed the result by this point, so this never fails the
/// command: the call itself succeeded, only its effects could not be reported.
fn report_failed_delta(error: &ClientError) {
    if is_empty_transaction_error(error) {
        // A procedure that only reads, on an account whose components write nothing, leaves the
        // transaction with no effects at all, and the kernel refuses those. For a read-only call
        // that is the expected outcome rather than a fault, so it is reported instead of dumping
        // the assertion chain. The kernel rejects only when the account was left unchanged and
        // nothing was consumed, so that is all this can report; it says nothing about created
        // notes.
        println!();
        println!("The transaction was rejected because it had no effects:\n");
        println!("No notes were consumed.");
        println!();
        println!("Account Storage was not changed.");
        println!("Account Vault was not changed.");
        println!("Account nonce was not changed.");
        return;
    }

    let mut report = String::new();
    let mut cause = std::error::Error::source(error);
    while let Some(err) = cause {
        writeln!(report, "  caused by: {err}").unwrap();
        cause = err.source();
    }

    println!("\n(Could not compute state delta: {error})");
    print!("{report}");
}
