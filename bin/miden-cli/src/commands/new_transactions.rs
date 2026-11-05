use std::collections::{BTreeMap, VecDeque};
use std::io;
#[cfg(feature = "dap")]
use std::net::SocketAddr;
#[cfg(feature = "dap")]
use std::path::PathBuf;
use std::sync::Arc;

use clap::builder::RangedU64ValueParser;
use clap::{Parser, Subcommand, ValueEnum};
use miden_client::account::AccountId;
use miden_client::asset::AssetAmount;
use miden_client::keystore::Keystore;
use miden_client::note::{
    BlockNumber,
    Note,
    NoteScriptRoot,
    NoteTag,
    NoteType as MidenNoteType,
    StandardNote,
    get_input_note_with_id_prefix,
};
use miden_client::store::{NoteRecordError, TransactionFilter};
use miden_client::transaction::{
    PaymentNoteDescription,
    ProvenTransaction,
    PswapTransactionData,
    RawOutputNote,
    SwapTransactionData,
    TransactionDetails,
    TransactionProver,
    TransactionProverError,
    TransactionRequest,
    TransactionRequestBuilder,
    TransactionResult,
};
use miden_client::{Client, ClientError, MAX_INPUT_NOTES_PER_TX, RemoteTransactionProver, Word};
use tokio::task::JoinHandle;
use tracing::info;

use crate::config::CliConfig;
use crate::create_dynamic_table;
use crate::errors::CliError;
use crate::utils::{
    SHARED_TOKEN_DOCUMENTATION,
    get_input_acc_id_by_prefix_or_default,
    load_faucet_metadata_resolver,
    parse_account_id,
    print_executed_transaction,
};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum NoteType {
    Public,
    Private,
}

impl From<&NoteType> for MidenNoteType {
    fn from(note_type: &NoteType) -> Self {
        match note_type {
            NoteType::Public => MidenNoteType::Public,
            NoteType::Private => MidenNoteType::Private,
        }
    }
}

/// Mint tokens from a fungible faucet to a wallet.
#[derive(Debug, Parser, Clone)]
pub struct MintCmd {
    /// Target account ID or its hex prefix.
    #[arg(short = 't', long = "target")]
    target_account_id: String,

    /// Asset to be minted.
    #[arg(short, long, help=format!("Asset to be minted.\n{SHARED_TOKEN_DOCUMENTATION}"))]
    asset: String,

    #[arg(short, long, value_enum)]
    note_type: NoteType,
    /// Flag to submit the executed transaction without asking for confirmation.
    #[arg(long, default_value_t = false)]
    force: bool,

    /// Flag to delegate proving to the remote prover specified in the config file.
    #[arg(long, default_value_t = false)]
    delegate_proving: bool,
}

impl MintCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let force = self.force;
        let resolver = load_faucet_metadata_resolver()?;

        let fungible_asset = resolver.parse_fungible_asset(&client, &self.asset).await?;

        let target_account_id = parse_account_id(&client, self.target_account_id.as_str()).await?;

        let transaction_request = TransactionRequestBuilder::new()
            .build_mint_fungible_asset(
                fungible_asset,
                target_account_id,
                (&self.note_type).into(),
                client.rng(),
            )
            .map_err(|err| {
                CliError::Transaction(err.into(), "Failed to build mint transaction".to_string())
            })?;

        execute_transaction(
            &mut client,
            fungible_asset.faucet_id(),
            transaction_request,
            force,
            self.delegate_proving,
        )
        .await
    }
}

/// Create a pay-to-id transaction.
#[derive(Debug, Parser, Clone)]
pub struct TransferCmd {
    /// Sender account ID or its hex prefix. If none is provided, the default account's ID is used
    /// instead.
    #[arg(short = 's', long = "sender")]
    sender_account_id: Option<String>,
    /// Target account ID or its hex prefix.
    #[arg(short = 't', long = "target")]
    target_account_id: String,

    /// Asset to be sent.
    #[arg(short, long, help=format!("Asset to be sent.\n{SHARED_TOKEN_DOCUMENTATION}"))]
    asset: String,

    #[arg(short, long, value_enum)]
    note_type: NoteType,
    /// Flag to submit the executed transaction without asking for confirmation
    #[arg(long, default_value_t = false)]
    force: bool,
    /// Set the recall height for the transaction. If the note wasn't consumed by this height, the
    /// sender may consume it back.
    ///
    /// Setting this flag turns the transaction from a `PayToId` to a `PayToIdWithRecall`.
    #[arg(short, long)]
    recall_height: Option<u32>,

    /// Set the timelock height for the transaction. The note will not be consumable until this
    /// height is reached.
    #[arg(short = 'i', long)]
    timelock_height: Option<u32>,

    /// Flag to delegate proving to the remote prover specified in the config file
    #[arg(long, default_value_t = false)]
    delegate_proving: bool,
}

impl TransferCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let force = self.force;

        let resolver = load_faucet_metadata_resolver()?;

        let fungible_asset = resolver.parse_fungible_asset(&client, &self.asset).await?;

        // try to use either the provided argument or the default account
        let sender_account_id =
            get_input_acc_id_by_prefix_or_default(&client, self.sender_account_id.clone()).await?;
        let target_account_id = parse_account_id(&client, self.target_account_id.as_str()).await?;

        let mut payment_description = PaymentNoteDescription::new(
            vec![fungible_asset.into()],
            sender_account_id,
            target_account_id,
        );

        if let Some(recall_height) = self.recall_height {
            payment_description =
                payment_description.with_reclaim_height(BlockNumber::from(recall_height));
        }

        if let Some(timelock_height) = self.timelock_height {
            payment_description =
                payment_description.with_timelock_height(BlockNumber::from(timelock_height));
        }

        let transaction_request = TransactionRequestBuilder::new()
            .build_pay_to_id(payment_description, (&self.note_type).into(), client.rng())
            .map_err(|err| {
                CliError::Transaction(err.into(), "Failed to build payment transaction".to_string())
            })?;

        execute_transaction(
            &mut client,
            sender_account_id,
            transaction_request,
            force,
            self.delegate_proving,
        )
        .await
    }
}

/// Create a swap transaction.
#[derive(Debug, Parser, Clone)]
pub struct SwapCmd {
    /// Sender account ID or its hex prefix. If none is provided, the default account's ID is used
    /// instead.
    #[arg(short = 's', long = "source")]
    sender_account_id: Option<String>,

    /// Asset offered.
    #[arg(short = 'o', long = "offered-asset", help=format!("Asset offered.\n{SHARED_TOKEN_DOCUMENTATION}"))]
    offered_asset: String,

    /// Asset requested.
    #[arg(short, long, help=format!("Asset requested.\n{SHARED_TOKEN_DOCUMENTATION}"))]
    requested_asset: String,

    /// Visibility of the swap note to be created.
    #[arg(short, long, value_enum)]
    note_type: NoteType,

    /// Visibility of the payback note produced when the swap is consumed. Defaults to private
    /// (cheaper, and the swap already records the trade in the consuming transaction).
    #[arg(long, value_enum, default_value_t = NoteType::Private)]
    payback_note_type: NoteType,

    /// Flag to submit the executed transaction without asking for confirmation.
    #[arg(long, default_value_t = false)]
    force: bool,

    /// Flag to delegate proving to the remote prover specified in the config file.
    #[arg(long, default_value_t = false)]
    delegate_proving: bool,
}

impl SwapCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let force = self.force;

        let resolver = load_faucet_metadata_resolver()?;

        let offered_fungible_asset =
            resolver.parse_fungible_asset(&client, &self.offered_asset).await?;
        let requested_fungible_asset =
            resolver.parse_fungible_asset(&client, &self.requested_asset).await?;

        // try to use either the provided argument or the default account
        let sender_account_id =
            get_input_acc_id_by_prefix_or_default(&client, self.sender_account_id.clone()).await?;

        let swap_transaction = SwapTransactionData::new(
            sender_account_id,
            offered_fungible_asset.into(),
            requested_fungible_asset.into(),
        );

        let transaction_request = TransactionRequestBuilder::new()
            .build_swap(
                &swap_transaction,
                (&self.note_type).into(),
                (&self.payback_note_type).into(),
                client.rng(),
            )
            .map_err(|err| {
                CliError::Transaction(err.into(), "Failed to build swap transaction".to_string())
            })?;

        execute_transaction(
            &mut client,
            sender_account_id,
            transaction_request,
            force,
            self.delegate_proving,
        )
        .await?;

        let payback_note_tag: u32 = NoteTag::with_account_target(sender_account_id).into();
        println!(
            "To receive updates about the payback note run `miden-client tags --add {payback_note_tag}`",
        );

        Ok(())
    }
}

/// Default value of the largest number of notes that one note-consumption transaction consumes.
///
/// Proving time and memory double each time the execution trace grows past a power of two. With
/// fees and assets that have callbacks, 40 notes keep the trace below 2^19 rows. More notes can
/// double the proving time and memory.
const DEFAULT_NOTES_PER_TX: usize = 40;

/// The largest value of `--in-flight`.
const MAX_IN_FLIGHT: usize = 8;

/// Consume with the account corresponding to `account_id` all of the notes from `list_of_notes`. If
/// no account ID is provided, the default one is used. If no notes are provided, any notes that are
/// identified to be owned by the account ID are consumed. If there are more notes than
/// `notes_per_tx`, the notes are consumed in more than one transaction.
#[derive(Debug, Parser, Clone)]
pub struct ConsumeNotesCmd {
    /// The account ID to be used to consume the note or its hex prefix. If none is provided, the
    /// default account's ID is used instead.
    #[arg(short = 'a', long = "account")]
    account_id: Option<String>,
    /// A list of note IDs or the hex prefixes of their corresponding IDs.
    list_of_notes: Vec<String>,
    /// Flag to submit the executed transaction without asking for confirmation.
    #[arg(short, long, default_value_t = false)]
    force: bool,

    /// Flag to delegate proving to the remote prover specified in the config file.
    #[arg(long, default_value_t = false)]
    delegate_proving: bool,

    /// The largest number of notes that one transaction consumes. If there are more notes, the
    /// command consumes them in more than one transaction. A larger value makes fewer transactions,
    /// but each transaction needs more time and memory to prove. The node rejects a transaction
    /// that is larger than its request size limit, so a large value can fail with large notes.
    #[arg(
        long,
        default_value_t = DEFAULT_NOTES_PER_TX,
        value_parser = RangedU64ValueParser::<usize>::new().range(1..=MAX_INPUT_NOTES_PER_TX as u64),
    )]
    notes_per_tx: usize,

    /// The number of transactions that the command keeps in flight when it consumes the notes in
    /// more than one transaction. With a value above 1, the command executes a transaction on the
    /// account state that the previous transaction leaves while the previous transaction is being
    /// proven, so that up to this many proofs run at the same time. Only a remote prover proves
    /// transactions at the same time, so a value above 1 needs `--delegate-proving`. If a
    /// transaction fails, the transactions executed after it are not submitted. The next sync after
    /// they expire discards them, and their notes can be consumed again.
    #[arg(
        long,
        default_value_t = 1,
        value_parser = RangedU64ValueParser::<usize>::new().range(1..=MAX_IN_FLIGHT as u64),
    )]
    in_flight: usize,

    /// Consume only the notes that use one of these note scripts. Each value is the name of a
    /// standard note (e.g., "P2ID" or "P2IDE") or a hex note script root. Separate the values with
    /// commas or use the flag more than one time. This flag cannot be used with note IDs.
    #[arg(
        long,
        value_name = "SCRIPT",
        value_delimiter = ',',
        value_parser = parse_note_script_filter,
        conflicts_with = "list_of_notes",
        long_help = filter_long_help(),
    )]
    filter: Vec<NoteScriptRoot>,

    /// Debug the note-consumption transaction instead of proving and submitting it: start a DAP
    /// debug adapter server on the given address (e.g. "127.0.0.1:4711") and wait for a DAP client
    /// to connect before executing.
    #[cfg(feature = "dap")]
    #[arg(long = "start-debug-adapter")]
    start_debug_adapter: Option<SocketAddr>,

    /// Write a replay snapshot of the debug session to this file once it ends, for offline replay
    /// with `miden-debug --replay <FILE>`. Only meaningful together with `--start-debug-adapter`.
    #[cfg(feature = "dap")]
    #[arg(long = "record", value_name = "FILE", requires = "start_debug_adapter")]
    record: Option<PathBuf>,
}

impl ConsumeNotesCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let force = self.force;

        if self.in_flight > 1 && !self.delegate_proving {
            return Err(CliError::Input(
                "--in-flight needs --delegate-proving: only a remote prover proves transactions at the same time"
                    .to_string(),
            ));
        }

        let mut input_notes = Vec::new();

        for note_id in &self.list_of_notes {
            input_notes.push(resolve_input_note(&client, note_id).await?);
        }

        let account_id =
            get_input_acc_id_by_prefix_or_default(&client, self.account_id.clone()).await?;

        if input_notes.is_empty() {
            info!("No input note IDs provided, getting all notes consumable by {}", account_id);
            let consumable_notes = client.get_consumable_notes(Some(account_id)).await?;
            for (note_record, _) in consumable_notes {
                let note: Note = note_record.try_into().map_err(|err: NoteRecordError| {
                    CliError::Transaction(err.into(), "Failed to convert note record".to_string())
                })?;
                if self.filter.is_empty() || self.filter.contains(&note.script().root()) {
                    input_notes.push(note);
                }
            }
        }

        if input_notes.is_empty() {
            if self.filter.is_empty() {
                println!("Did not find any consumable notes for {account_id}.");
            } else {
                println!(
                    "Did not find any consumable notes for {account_id} that match the filter."
                );
            }
            return Ok(());
        }

        print_note_scripts(&input_notes);

        if input_notes.len() > self.notes_per_tx {
            return self.consume_in_transactions(&mut client, account_id, &input_notes).await;
        }

        let transaction_request = consume_notes_request(&input_notes)?;

        #[cfg(feature = "dap")]
        if let Some(addr) = self.start_debug_adapter.as_ref() {
            return debug_transaction(
                &mut client,
                account_id,
                transaction_request,
                addr,
                self.record.as_deref(),
            )
            .await;
        }

        execute_transaction(
            &mut client,
            account_id,
            transaction_request,
            force,
            self.delegate_proving,
        )
        .await
    }

    /// Consumes `input_notes` in more than one transaction of at most `notes_per_tx` notes, after
    /// one confirmation for all of the transactions.
    async fn consume_in_transactions<AUTH: Keystore + Sync + 'static>(
        &self,
        client: &mut Client<AUTH>,
        account_id: AccountId,
        input_notes: &[Note],
    ) -> Result<(), CliError> {
        #[cfg(feature = "dap")]
        if self.start_debug_adapter.is_some() {
            return Err(CliError::Input(format!(
                "the debug adapter runs one transaction, but {} notes need more than one transaction of up to {} notes. Pass fewer notes or a larger --notes-per-tx",
                input_notes.len(),
                self.notes_per_tx
            )));
        }

        println!(
            "Consuming {} notes in {} transactions of up to {} notes each.",
            input_notes.len(),
            input_notes.len().div_ceil(self.notes_per_tx),
            self.notes_per_tx
        );
        if !self.force
            && !user_confirms(
                "\nContinue with executing, proving and submitting all transactions? Changes will be irreversible once the proofs are finalized on the network (y/N)",
            )
        {
            println!("Note consumption was cancelled.");
            return Ok(());
        }

        let prover = transaction_prover(client, self.delegate_proving)?;
        if self.in_flight > 1 {
            return consume_notes_pipelined(
                client,
                account_id,
                input_notes,
                self.notes_per_tx,
                prover,
                self.in_flight,
            )
            .await;
        }
        consume_notes_in_chunks(client, account_id, input_notes, self.notes_per_tx, prover).await
    }
}

/// Standard notes that `--filter` accepts by name.
const STANDARD_NOTES: [StandardNote; 19] = [
    StandardNote::P2ID,
    StandardNote::P2IDE,
    StandardNote::SWAP,
    StandardNote::PSWAP,
    StandardNote::MINT,
    StandardNote::BURN,
    StandardNote::CONSTANT_FEE_POLICY_CONFIG,
    StandardNote::FAUCET_POLICY_CONFIG,
    StandardNote::FAUCET_METADATA_CONFIG,
    StandardNote::MIN_BURN_AMOUNT_CONFIG,
    StandardNote::ALLOWLIST_CONFIG,
    StandardNote::BLOCKLIST_CONFIG,
    StandardNote::PAUSE_CONFIG,
    StandardNote::OWNER_CONFIG,
    StandardNote::RBAC_CONFIG,
    StandardNote::NETWORK_ACCOUNT_CONFIG,
    StandardNote::UPGRADE,
    StandardNote::FEE_SPONSORSHIP,
    StandardNote::TX_FEE,
];

/// Returns the names of [`STANDARD_NOTES`], separated by commas.
fn standard_note_names() -> String {
    STANDARD_NOTES.iter().map(StandardNote::name).collect::<Vec<_>>().join(", ")
}

/// Returns the help of `--filter` that `--help` prints. It lists the standard notes that the flag
/// accepts by name.
fn filter_long_help() -> String {
    format!(
        "Consume only the notes that use one of these note scripts. Each value is the name of a \
         standard note (e.g., \"P2ID\" or \"P2IDE\") or a hex note script root. Separate the \
         values with commas or use the flag more than one time. This flag cannot be used with \
         note IDs.\n\nStandard notes: {}.",
        standard_note_names()
    )
}

/// Parses a `--filter` value: the name of a standard note in any letter case, or a hex note script
/// root.
fn parse_note_script_filter(value: &str) -> Result<NoteScriptRoot, String> {
    if let Some(standard_note) =
        STANDARD_NOTES.iter().find(|note| note.name().eq_ignore_ascii_case(value))
    {
        return Ok(standard_note.script_root());
    }

    Word::try_from(value).map(NoteScriptRoot::from_raw).map_err(|_| {
        format!(
            "expected a hex note script root or the name of a standard note: {}",
            standard_note_names()
        )
    })
}

/// Prints a table with the number of notes in `notes` for each note script.
fn print_note_scripts(notes: &[Note]) {
    let mut num_notes_by_script = BTreeMap::<NoteScriptRoot, usize>::new();
    for note in notes {
        *num_notes_by_script.entry(note.script().root()).or_default() += 1;
    }

    let mut table = create_dynamic_table(&["Standard note", "Script root", "Notes"]);
    for (script_root, num_notes) in num_notes_by_script {
        let standard_note =
            StandardNote::from_script_root(script_root).map_or("Unknown", |note| note.name());
        table.add_row(vec![standard_note.to_string(), script_root.to_hex(), num_notes.to_string()]);
    }
    println!("{table}");
}

/// Builds a transaction request that consumes `input_notes`.
fn consume_notes_request(input_notes: &[Note]) -> Result<TransactionRequest, CliError> {
    TransactionRequestBuilder::new()
        .input_notes(input_notes.iter().cloned().map(|note| (note, None)))
        .build()
        .map_err(|err| {
            CliError::Transaction(
                err.into(),
                "Failed to build consume notes transaction".to_string(),
            )
        })
}

/// Consumes `input_notes` with the account `account_id`, in transactions of at most `notes_per_tx`
/// notes each.
///
/// Each transaction starts from the account state that the previous transaction leaves. The client
/// syncs the chain before the first transaction. After each transaction, the client syncs again
/// only if the next transaction can expire before the node accepts it. The sync does not fetch
/// notes from the note transport layer, because the run consumes only the notes in `input_notes`.
/// The function stops at the first transaction that fails. The notes of that transaction and of the
/// transactions after it stay unconsumed, so a new run of the command can consume them.
async fn consume_notes_in_chunks<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
    account_id: AccountId,
    input_notes: &[Note],
    notes_per_tx: usize,
    prover: Arc<dyn TransactionProver + Send + Sync>,
) -> Result<(), CliError> {
    let num_transactions = input_notes.len().div_ceil(notes_per_tx);
    let mut num_consumed = 0;
    let mut sync_before_transaction = true;
    let mut previous_submission_height = BlockNumber::GENESIS;

    for (index, notes) in input_notes.chunks(notes_per_tx).enumerate() {
        println!(
            "Transaction {}/{num_transactions}: consuming {} notes...",
            index + 1,
            notes.len()
        );

        let result = async {
            if sync_before_transaction {
                client.sync_chain().await?;
            }
            let transaction_request = consume_notes_request(notes)?;
            let transaction_id = client
                .submit_new_transaction_with_prover(account_id, transaction_request, prover.clone())
                .await?;
            let details = client
                .get_transactions(TransactionFilter::Ids(vec![transaction_id]))
                .await?
                .pop()
                .map(|record| record.details);
            Ok::<_, CliError>((transaction_id, details))
        }
        .await;

        let (transaction_id, details) = match result {
            Ok(result) => result,
            Err(err) => {
                println!(
                    "Consumed {num_consumed} of {} notes in {index} transactions. To consume the other notes, run `consume-notes` again without note IDs.",
                    input_notes.len()
                );
                return Err(err);
            },
        };

        num_consumed += notes.len();
        println!("Transaction ID: {transaction_id}");

        sync_before_transaction = match details {
            Some(details) => {
                let can_expire = next_transaction_can_expire(&details, previous_submission_height);
                previous_submission_height = details.submission_height;
                can_expire
            },
            None => true,
        };
    }

    println!("Consumed {num_consumed} notes in {num_transactions} transactions.");
    Ok(())
}

/// The smallest number of blocks between the chain tip and the expiration block of a transaction
/// that the node accepts. The node rejects a transaction that expires at most 2 blocks after the
/// chain tip.
const MIN_BLOCKS_BEFORE_EXPIRATION: u32 = 3;

/// Returns `true` if a transaction with the same reference block as the transaction of `details`
/// can expire before the node accepts it.
///
/// `previous_submission_height` is the chain tip at the submission of the transaction before the
/// transaction of `details`. The estimate assumes that the next transaction takes as many blocks as
/// the transaction of `details`, and it adds the same number of blocks as a margin.
fn next_transaction_can_expire(
    details: &TransactionDetails,
    previous_submission_height: BlockNumber,
) -> bool {
    // The chain tip when the transaction started. After a sync, it is the reference block.
    // Otherwise, it is the submission height of the previous transaction.
    let start_height = details.block_num.max(previous_submission_height).as_u32();
    let submission_height = details.submission_height.as_u32();
    let num_blocks = submission_height.saturating_sub(start_height);

    submission_height
        .saturating_add(num_blocks.saturating_mul(2))
        .saturating_add(MIN_BLOCKS_BEFORE_EXPIRATION)
        > details.expiration_block_num.as_u32()
}

/// Consumes `input_notes` with the account `account_id`, in transactions of at most `notes_per_tx`
/// notes each, with up to `in_flight` transactions being proven at the same time.
///
/// The function applies each executed transaction to the store before its proof exists, so that the
/// next transaction can be executed on the account state that it leaves while the proof runs. It
/// submits the proven transactions in execution order. The node accepts a transaction that starts
/// from the state that a transaction in its mempool leaves.
///
/// The function stops at the first transaction that fails to prove or to submit. That transaction
/// and the transactions executed after it stay in the store as pending transactions that the node
/// never received. The next sync after they expire discards them and releases their notes, so a new
/// run of the command can consume the notes.
async fn consume_notes_pipelined<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
    account_id: AccountId,
    input_notes: &[Note],
    notes_per_tx: usize,
    prover: Arc<dyn TransactionProver + Send + Sync>,
    in_flight: usize,
) -> Result<(), CliError> {
    let chunks: Vec<&[Note]> = input_notes.chunks(notes_per_tx).collect();
    let num_transactions = chunks.len();
    let mut num_executed = 0;
    let mut num_submitted = 0;
    let mut num_consumed = 0;
    let mut in_flight_transactions = VecDeque::new();

    let result = async {
        let mut reference_block = client.sync_chain().await?.block_num;
        // The chain tip that the node reported last.
        let mut chain_tip = reference_block;
        // The expiration block of the last executed transaction.
        let mut expiration_block = None;
        // The number of blocks that the last submission took.
        let mut blocks_per_submission = 0;

        loop {
            while in_flight_transactions.len() < in_flight && num_executed < num_transactions {
                if let Some(expiration) = expiration_block
                    && pipelined_transaction_can_expire(
                        chain_tip,
                        in_flight,
                        blocks_per_submission,
                        expiration,
                    )
                {
                    reference_block = client.sync_chain().await?.block_num;
                    chain_tip = reference_block;
                }

                let notes = chunks[num_executed];
                println!(
                    "Transaction {}/{num_transactions}: executing with {} notes...",
                    num_executed + 1,
                    notes.len()
                );
                let transaction_request = consume_notes_request(notes)?;
                let tx_result = client.execute_transaction(account_id, transaction_request).await?;
                // The submission height is not known yet. The reference block stands in for it.
                client.apply_transaction(&tx_result, reference_block).await?;
                expiration_block = Some(tx_result.executed_transaction().expiration_block_num());

                let prover = prover.clone();
                let tx_inputs = tx_result.executed_transaction().clone().into();
                let proof = tokio::spawn(async move { prover.prove(tx_inputs).await });
                in_flight_transactions.push_back((num_executed, tx_result, proof));
                num_executed += 1;
            }

            let Some((index, tx_result, proof)) = in_flight_transactions.pop_front() else {
                break;
            };
            let proven_transaction = proven_transaction(&tx_result, proof).await?;
            let submission_height =
                client.submit_proven_transaction(proven_transaction, &tx_result).await?;
            blocks_per_submission = submission_height.as_u32().saturating_sub(chain_tip.as_u32());
            chain_tip = submission_height;
            num_submitted += 1;
            num_consumed += chunks[index].len();
            println!(
                "Transaction {}/{num_transactions} submitted. Transaction ID: {}",
                index + 1,
                tx_result.executed_transaction().id()
            );
        }
        Ok::<_, CliError>(())
    }
    .await;

    if let Err(err) = result {
        for (_, _, proof) in in_flight_transactions {
            proof.abort();
        }
        println!(
            "Consumed {num_consumed} of {} notes in {num_submitted} transactions. {} executed transactions were not submitted. The next `sync` after they expire discards them, and their notes can be consumed again. To consume the other notes, run `consume-notes` again without note IDs.",
            input_notes.len(),
            num_executed - num_submitted
        );
        return Err(err);
    }

    println!("Consumed {num_consumed} notes in {num_transactions} transactions.");
    Ok(())
}

/// Waits for the proof task `proof` and checks that the proof is for the transaction of
/// `tx_result`.
async fn proven_transaction(
    tx_result: &TransactionResult,
    proof: JoinHandle<Result<ProvenTransaction, TransactionProverError>>,
) -> Result<ProvenTransaction, CliError> {
    let proven_transaction = proof
        .await
        .map_err(|err| CliError::Transaction(err.into(), "the proving task failed".to_string()))?
        .map_err(ClientError::TransactionProvingError)?;

    let executed_transaction = tx_result.executed_transaction();
    if proven_transaction.id() != executed_transaction.id() {
        return Err(ClientError::MismatchedProvenTransaction {
            requested: executed_transaction.id(),
            returned: proven_transaction.id(),
        }
        .into());
    }

    Ok(proven_transaction)
}

/// Returns `true` if a transaction that is executed now with the reference block of a transaction
/// that expires at `expiration_block` can expire before the node accepts it.
///
/// `chain_tip` is the chain tip that the node reported last. The node accepts the transaction after
/// the `in_flight` transactions ahead of it. The estimate assumes that each of them takes
/// `blocks_per_submission` blocks, and it adds the same number of blocks as a margin.
fn pipelined_transaction_can_expire(
    chain_tip: BlockNumber,
    in_flight: usize,
    blocks_per_submission: u32,
    expiration_block: BlockNumber,
) -> bool {
    let num_submissions = u32::try_from(in_flight).unwrap_or(u32::MAX);
    chain_tip
        .as_u32()
        .saturating_add(blocks_per_submission.saturating_mul(num_submissions).saturating_mul(2))
        .saturating_add(MIN_BLOCKS_BEFORE_EXPIRATION)
        > expiration_block.as_u32()
}

// PSWAP COMMANDS
// ================================================================================================

/// Partial swap (PSWAP) commands.
#[derive(Debug, Parser, Clone)]
#[command(about = "Create, consume, or cancel partial swap notes")]
pub struct PswapCmd {
    #[command(subcommand)]
    action: PswapAction,
}

#[derive(Debug, Subcommand, Clone)]
pub enum PswapAction {
    /// Create a new partial swap note.
    Create(PswapCreateCmd),

    /// Consume (fill) an existing partial swap note.
    Consume(PswapConsumeCmd),

    /// Cancel an existing partial swap note.
    Cancel(PswapCancelCmd),
}

impl PswapCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        client: Client<AUTH>,
    ) -> Result<(), CliError> {
        match &self.action {
            PswapAction::Create(cmd) => Box::pin(cmd.execute(client)).await,
            PswapAction::Consume(cmd) => Box::pin(cmd.execute(client)).await,
            PswapAction::Cancel(cmd) => Box::pin(cmd.execute(client)).await,
        }
    }
}

/// Create a partial swap note offering one fungible asset in exchange for another.
#[derive(Debug, Parser, Clone)]
pub struct PswapCreateCmd {
    /// Sender account ID or its hex prefix. If none is provided, the default account is used.
    #[arg(short = 's', long = "sender")]
    sender_account_id: Option<String>,

    /// Asset offered.
    #[arg(short = 'o', long = "offered-asset", help=format!("Asset offered.\n{SHARED_TOKEN_DOCUMENTATION}"))]
    offered_asset: String,

    /// Asset requested.
    #[arg(short, long, help=format!("Asset requested.\n{SHARED_TOKEN_DOCUMENTATION}"))]
    requested_asset: String,

    /// Visibility of the PSWAP note to be created.
    #[arg(short, long, value_enum)]
    note_type: NoteType,

    /// Visibility of the payback note produced when the PSWAP is filled. Defaults to private
    /// (cheaper, and the fill amount is already recorded in the executing transaction).
    #[arg(long, value_enum, default_value_t = NoteType::Private)]
    payback_note_type: NoteType,

    /// Flag to submit the executed transaction without asking for confirmation.
    #[arg(long, default_value_t = false)]
    force: bool,

    /// Flag to delegate proving to the remote prover specified in the config file.
    #[arg(long, default_value_t = false)]
    delegate_proving: bool,
}

impl PswapCreateCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let sender_id =
            get_input_acc_id_by_prefix_or_default(&client, self.sender_account_id.clone()).await?;

        let resolver = load_faucet_metadata_resolver()?;
        let offered_fungible_asset =
            resolver.parse_fungible_asset(&client, &self.offered_asset).await?;
        let requested_fungible_asset =
            resolver.parse_fungible_asset(&client, &self.requested_asset).await?;

        let pswap_data =
            PswapTransactionData::new(sender_id, offered_fungible_asset, requested_fungible_asset);

        let tx_request = TransactionRequestBuilder::new()
            .build_pswap_create(
                &pswap_data,
                (&self.note_type).into(),
                (&self.payback_note_type).into(),
                None,
                client.rng(),
            )
            .map_err(|err| {
                CliError::Transaction(
                    err.into(),
                    "Failed to build PSWAP create transaction".to_string(),
                )
            })?;

        execute_transaction(&mut client, sender_id, tx_request, self.force, self.delegate_proving)
            .await
    }
}

/// Consume (partially fill) an existing partial swap note.
#[derive(Debug, Parser, Clone)]
pub struct PswapConsumeCmd {
    /// Consumer account ID or its hex prefix.
    #[arg(short = 'a', long = "account")]
    account: String,

    /// Note ID or hex prefix of the PSWAP note to consume.
    #[arg(long)]
    note: String,

    /// Amount of the requested asset the consumer account is providing to fill the swap.
    #[arg(long)]
    fill_amount: u64,

    /// Flag to submit the executed transaction without asking for confirmation.
    #[arg(long, default_value_t = false)]
    force: bool,

    /// Flag to delegate proving to the remote prover specified in the config file.
    #[arg(long, default_value_t = false)]
    delegate_proving: bool,
}

impl PswapConsumeCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let consumer_id = parse_account_id(&client, &self.account).await?;
        let note = resolve_input_note(&client, &self.note).await?;

        let fill_amount = AssetAmount::new(self.fill_amount)
            .map_err(|err| CliError::Parse(err.into(), "Invalid fill amount".to_string()))?;

        // The CLI does not yet support note-supplied fills (in-flight fills routed through other
        // notes), so pass 0 for `note_fill_amount`.
        let tx_request = TransactionRequestBuilder::new()
            .build_pswap_consume(&note, consumer_id, fill_amount, AssetAmount::ZERO)
            .map_err(|err| {
                CliError::Transaction(
                    err.into(),
                    "Failed to build PSWAP consume transaction".to_string(),
                )
            })?;

        execute_transaction(&mut client, consumer_id, tx_request, self.force, self.delegate_proving)
            .await
    }
}

/// Cancel an existing partial swap note, reclaiming the offered asset.
#[derive(Debug, Parser, Clone)]
pub struct PswapCancelCmd {
    /// Account ID or its hex prefix of the note creator. If none is provided, the default account
    /// is used.
    #[arg(short = 's', long = "sender")]
    sender_account_id: Option<String>,

    /// Note ID or hex prefix of the PSWAP note to cancel.
    #[arg(long)]
    note: String,

    /// Flag to submit the executed transaction without asking for confirmation.
    #[arg(long, default_value_t = false)]
    force: bool,

    /// Flag to delegate proving to the remote prover specified in the config file.
    #[arg(long, default_value_t = false)]
    delegate_proving: bool,
}

impl PswapCancelCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let sender_id =
            get_input_acc_id_by_prefix_or_default(&client, self.sender_account_id.clone()).await?;
        let note = resolve_input_note(&client, &self.note).await?;

        let tx_request = TransactionRequestBuilder::new()
            .build_pswap_cancel(note, sender_id)
            .map_err(|err| {
                CliError::Transaction(
                    err.into(),
                    "Failed to build PSWAP cancel transaction".to_string(),
                )
            })?;

        execute_transaction(&mut client, sender_id, tx_request, self.force, self.delegate_proving)
            .await
    }
}

// HELPERS
// ================================================================================================

/// Resolves a note ID prefix to a fully-qualified [`Note`].
async fn resolve_input_note<AUTH: Keystore + Sync>(
    client: &Client<AUTH>,
    note_id_prefix: &str,
) -> Result<Note, CliError> {
    let note_record = get_input_note_with_id_prefix(client, note_id_prefix)
        .await
        .map_err(|_| {
            CliError::Input(format!(
                "Input note ID {note_id_prefix} is neither a valid Note ID nor a prefix of a known Note ID"
            ))
        })?;

    note_record.try_into().map_err(|err: NoteRecordError| {
        CliError::Transaction(err.into(), "Failed to convert note record".to_string())
    })
}

// EXECUTE TRANSACTION
// ================================================================================================

async fn execute_transaction<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
    account_id: AccountId,
    transaction_request: TransactionRequest,
    force: bool,
    delegated_proving: bool,
) -> Result<(), CliError> {
    println!("Executing transaction...");
    let transaction_result = client.execute_transaction(account_id, transaction_request).await?;

    let executed_transaction = transaction_result.executed_transaction().clone();

    // Show delta and ask for confirmation
    print_executed_transaction(client, &executed_transaction).await?;
    if !force
        && !user_confirms(
            "\nContinue with proving and submission? Changes will be irreversible once the proof is finalized on the network (y/N)",
        )
    {
        println!("Transaction was cancelled.");
        return Ok(());
    }

    let transaction_id = executed_transaction.id();
    let output_notes = executed_transaction
        .output_notes()
        .iter()
        .map(RawOutputNote::id)
        .collect::<Vec<_>>();

    println!("Proving transaction...");

    let prover = transaction_prover(client, delegated_proving)?;

    let proven_transaction = client.prove_transaction_with(&transaction_result, prover).await?;

    println!("Submitting transaction to node...");

    let submission_height = client
        .submit_proven_transaction(proven_transaction, &transaction_result)
        .await?;
    println!("Applying transaction to store...");
    client.apply_transaction(&transaction_result, submission_height).await?;

    println!("Successfully created transaction.");
    println!("Transaction ID: {transaction_id}");

    if output_notes.is_empty() {
        println!("The transaction did not generate any output notes.");
    } else {
        println!("Output notes:");
        for note_id in &output_notes {
            println!("\t- {note_id}");
        }
    }

    Ok(())
}

/// Returns the remote prover from the configuration file if `delegated_proving` is set. Otherwise,
/// returns the prover of `client`.
fn transaction_prover<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    delegated_proving: bool,
) -> Result<Arc<dyn TransactionProver + Send + Sync>, CliError> {
    if !delegated_proving {
        return Ok(client.prover());
    }

    let cli_config = CliConfig::load()?;
    let remote_prover_endpoint =
        cli_config.remote_prover_endpoint.as_ref().ok_or(CliError::Config(
            "Remote prover endpoint".to_string().into(),
            "remote prover endpoint is not set in the configuration file".to_string(),
        ))?;

    Ok(Arc::new(
        RemoteTransactionProver::new(remote_prover_endpoint.to_string())
            .with_timeout(cli_config.remote_prover_timeout),
    ))
}

/// Prints `question` and returns `true` if the user answers `y`.
fn user_confirms(question: &str) -> bool {
    println!("{question}");
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).expect("Should read line");

    answer.trim().to_lowercase() == "y"
}

/// Runs `transaction_request` under a DAP debug adapter instead of proving and submitting it, so a
/// DAP client can attach and step through the full transaction (kernel, note scripts, account
/// code). Optionally writes a replay snapshot for offline replay with `miden-debug --replay`.
#[cfg(feature = "dap")]
async fn debug_transaction<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
    account_id: AccountId,
    transaction_request: TransactionRequest,
    addr: &SocketAddr,
    record: Option<&std::path::Path>,
) -> Result<(), CliError> {
    let mut config = miden_debug::DapConfig::new(addr.to_string());
    // The DAP executor is created and consumed inside the transaction executor, so recorded advice
    // mutations are read back through this shared handle once the session ends.
    let recorder = config.record_event_mutations();
    let snapshot_recorder = record.map(|path| (config.record_snapshot(path.to_path_buf()), path));
    let config_handle = config.clone();
    miden_debug::DapConfig::set_global(config);

    println!(
        "Starting debug session on {addr}; connect a DAP client to step through the transaction..."
    );
    let result = loop {
        let result = client
            .execute_transaction_with_dap(account_id, transaction_request.clone())
            .await
            .map(|_| ())
            .map_err(|err| {
                CliError::Transaction(err.into(), "error debugging the transaction".to_string())
            });

        if config_handle.restart_requested() {
            config_handle.reset_restart();
            println!("Rebuilding transaction and restarting debug session...");
            continue;
        }

        break result;
    };

    let mutation_sets = recorder.take();
    if !mutation_sets.is_empty() {
        println!(
            "Recorded {} advice mutation set(s) from event handlers during the debug session.",
            mutation_sets.len()
        );
    }
    if let Some((snapshot_recorder, path)) = &snapshot_recorder
        && let Err(err) = super::report_replay_snapshot_write(snapshot_recorder, path)
    {
        if result.is_err() {
            eprintln!("{err}");
        } else {
            return Err(err);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use clap::Parser;
    use miden_client::account::{Account, AccountId};
    use miden_client::asset::{Asset, FungibleAsset};
    use miden_client::builder::ClientBuilder;
    use miden_client::crypto::eddsa_25519_sha512::KeyExchangeKey;
    use miden_client::keystore::FilesystemKeyStore;
    use miden_client::note::{BlockNumber, Note, NoteFile, NoteScriptRoot, NoteType, StandardNote};
    use miden_client::rpc::encryption::TransactionEncryptionKey;
    use miden_client::store::{NoteFilter, TransactionFilter};
    use miden_client::testing::account_id::{
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        ACCOUNT_ID_PRIVATE_SENDER,
    };
    use miden_client::testing::common::create_test_store_path;
    use miden_client::testing::mock::MockRpcApi;
    use miden_client::testing::{Auth, MockChainBuilder};
    use miden_client::transaction::{
        LocalTransactionProver,
        ProvenTransaction,
        TransactionInputs,
        TransactionProver,
        TransactionProverError,
        TransactionStatus,
    };
    use miden_client::{Client, ClientError, Word};
    use miden_client_sqlite_store::ClientBuilderSqliteExt;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    use super::{ConsumeNotesCmd, DEFAULT_NOTES_PER_TX, parse_note_script_filter};
    use crate::errors::CliError;

    const NOTE_AMOUNT: u64 = 10;

    /// A prover that fails on one call and sends the other calls to the local prover.
    struct FailingProver {
        calls: AtomicUsize,
        failing_call: usize,
    }

    #[async_trait::async_trait]
    impl TransactionProver for FailingProver {
        async fn prove(
            &self,
            tx_inputs: TransactionInputs,
        ) -> Result<ProvenTransaction, TransactionProverError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == self.failing_call {
                return Err(TransactionProverError::other("the test prover fails this call"));
            }
            LocalTransactionProver::default().prove(tx_inputs)
        }
    }

    fn faucet_id() -> AccountId {
        AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap()
    }

    /// Returns the RPC API of a mock chain that has `num_p2id_notes` P2ID notes and
    /// `num_extended_notes` P2IDE notes for one wallet, the wallet, and a file for each note.
    ///
    /// The notes are in the genesis block, which the client does not sync. A client must import the
    /// note files to track the notes.
    fn mock_chain_with_notes(
        num_p2id_notes: usize,
        num_extended_notes: usize,
    ) -> (MockRpcApi, Account, Vec<NoteFile>) {
        let sender_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_SENDER).unwrap();
        let asset = Asset::from(FungibleAsset::new(faucet_id(), NOTE_AMOUNT).unwrap());

        let mut builder = MockChainBuilder::new();
        let wallet = builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
        let mut note_files = Vec::new();
        for _ in 0..num_p2id_notes {
            let note = builder
                .add_p2id_note(sender_id, wallet.id(), &[asset], NoteType::Public)
                .unwrap();
            note_files.push(NoteFile::NoteId(note.id()));
        }
        for _ in 0..num_extended_notes {
            let note = builder
                .add_p2ide_note(
                    sender_id,
                    wallet.id(),
                    None,
                    &[asset],
                    NoteType::Public,
                    None,
                    None,
                )
                .unwrap();
            note_files.push(NoteFile::NoteId(note.id()));
        }
        let mut chain = builder.build().unwrap();
        chain.prove_next_block().unwrap();

        (MockRpcApi::new(chain), wallet, note_files)
    }

    /// Opens a client on the store at `store_path`.
    async fn open_client(
        rpc: &MockRpcApi,
        store_path: PathBuf,
        prover: Option<Arc<dyn TransactionProver + Send + Sync>>,
    ) -> Client<FilesystemKeyStore> {
        let mut builder = ClientBuilder::new()
            .rpc(Arc::new(rpc.clone()))
            .sqlite_store(store_path)
            .authenticator(Arc::new(FilesystemKeyStore::new(std::env::temp_dir()).unwrap()))
            .tx_discard_delta(Some(crate::TX_DISCARD_DELTA));
        if let Some(prover) = prover {
            builder = builder.prover(prover);
        }

        builder.build().await.unwrap()
    }

    /// Opens a client on a new store, makes it track `wallet` and the notes of `note_files`, and
    /// syncs it.
    async fn new_wallet_client(
        rpc: &MockRpcApi,
        store_path: PathBuf,
        wallet: &Account,
        note_files: &[NoteFile],
        prover: Option<Arc<dyn TransactionProver + Send + Sync>>,
    ) -> Client<FilesystemKeyStore> {
        let mut client = open_client(rpc, store_path, prover).await;
        client.ensure_genesis_in_place().await.unwrap();
        client.seed_protocol_config(rpc.protocol_config()).await.unwrap();
        let genesis_commitment = client
            .get_block_header_by_num(BlockNumber::GENESIS)
            .await
            .unwrap()
            .unwrap()
            .0
            .commitment();
        client
            .seed_transaction_encryption_key(TransactionEncryptionKey::new_unattested(
                b"mock-key-id".to_vec(),
                KeyExchangeKey::with_rng(&mut StdRng::seed_from_u64(0)).public_key(),
                genesis_commitment,
            ))
            .await
            .unwrap();
        client.add_account(wallet, false).await.unwrap();
        client.import_notes(note_files).await.unwrap();
        client.sync_state().await.unwrap();

        client
    }

    /// Runs `consume-notes --force` for `wallet` with `extra_args`.
    async fn consume_notes(
        client: Client<FilesystemKeyStore>,
        wallet: &Account,
        extra_args: &[&str],
    ) -> Result<(), CliError> {
        let account_id = wallet.id().to_hex();
        let args = ["consume-notes", "--force", "--account", account_id.as_str()];
        let cmd = ConsumeNotesCmd::try_parse_from(args.iter().chain(extra_args)).unwrap();
        Box::pin(cmd.execute(client)).await
    }

    async fn num_input_notes(client: &Client<FilesystemKeyStore>, filter: NoteFilter) -> usize {
        client.get_input_notes(filter).await.unwrap().len()
    }

    async fn balance(client: &Client<FilesystemKeyStore>, wallet: &Account) -> u64 {
        client
            .account_reader(wallet.id())
            .get_balance(faucet_id())
            .await
            .unwrap()
            .as_u64()
    }

    /// The command consumes more notes than one transaction can hold, in transactions of at most
    /// the default number of notes.
    #[tokio::test]
    #[ignore = "proves 50 transactions of 40 notes, which takes minutes and more than 2 GB of memory"]
    async fn consume_notes_consumes_more_notes_than_one_transaction_holds() {
        const NUM_NOTES: usize = 2_000;

        let (rpc, wallet, note_files) = mock_chain_with_notes(NUM_NOTES, 0);
        let store_path = create_test_store_path();
        let client = new_wallet_client(&rpc, store_path.clone(), &wallet, &note_files, None).await;
        assert_eq!(num_input_notes(&client, NoteFilter::Committed).await, NUM_NOTES);
        let initial_balance = balance(&client, &wallet).await;

        consume_notes(client, &wallet, &[]).await.unwrap();

        let client = open_client(&rpc, store_path, None).await;
        assert_eq!(num_input_notes(&client, NoteFilter::Committed).await, 0);
        assert_eq!(num_input_notes(&client, NoteFilter::Processing).await, NUM_NOTES);
        assert_eq!(
            client.get_transactions(TransactionFilter::All).await.unwrap().len(),
            NUM_NOTES.div_ceil(DEFAULT_NOTES_PER_TX)
        );
        assert_eq!(
            balance(&client, &wallet).await,
            initial_balance + NUM_NOTES as u64 * NOTE_AMOUNT
        );
    }

    /// The command stops at the first transaction that fails. A new run consumes the notes that the
    /// failed run did not consume.
    #[tokio::test]
    async fn consume_notes_resumes_after_a_failed_transaction() {
        const NUM_NOTES: usize = 5;
        const NOTES_PER_TX: usize = 2;
        let notes_per_tx = NOTES_PER_TX.to_string();

        let (rpc, wallet, note_files) = mock_chain_with_notes(NUM_NOTES, 0);
        let store_path = create_test_store_path();
        let prover = Arc::new(FailingProver {
            calls: AtomicUsize::new(0),
            failing_call: 1,
        });
        let client =
            new_wallet_client(&rpc, store_path.clone(), &wallet, &note_files, Some(prover)).await;
        assert_eq!(num_input_notes(&client, NoteFilter::Committed).await, NUM_NOTES);

        let result = consume_notes(client, &wallet, &["--notes-per-tx", &notes_per_tx]).await;
        assert!(matches!(
            result,
            Err(CliError::Client {
                error: ClientError::TransactionProvingError(_),
                ..
            })
        ));

        let client = open_client(&rpc, store_path.clone(), None).await;
        assert_eq!(num_input_notes(&client, NoteFilter::Processing).await, NOTES_PER_TX);
        assert_eq!(num_input_notes(&client, NoteFilter::Committed).await, NUM_NOTES - NOTES_PER_TX);

        consume_notes(client, &wallet, &["--notes-per-tx", &notes_per_tx])
            .await
            .unwrap();

        let client = open_client(&rpc, store_path, None).await;
        assert_eq!(num_input_notes(&client, NoteFilter::Committed).await, 0);
        assert_eq!(num_input_notes(&client, NoteFilter::Processing).await, NUM_NOTES);
        assert_eq!(client.get_transactions(TransactionFilter::All).await.unwrap().len(), 3);
    }

    #[test]
    fn filter_accepts_standard_note_names_and_script_roots() {
        assert_eq!(parse_note_script_filter("p2ide"), Ok(StandardNote::P2IDE.script_root()));
        assert_eq!(parse_note_script_filter("P2ID"), Ok(StandardNote::P2ID.script_root()));
        let custom_root = NoteScriptRoot::from_raw(Word::from([1u32, 2, 3, 4]));
        assert_eq!(parse_note_script_filter(&custom_root.to_hex()), Ok(custom_root));
        assert!(parse_note_script_filter("p2id2").is_err());

        let cmd =
            ConsumeNotesCmd::try_parse_from(["consume-notes", "--filter", "p2id,p2ide"]).unwrap();
        assert_eq!(
            cmd.filter,
            [StandardNote::P2ID.script_root(), StandardNote::P2IDE.script_root()]
        );
        assert!(
            ConsumeNotesCmd::try_parse_from(["consume-notes", "--filter", "p2id", "0x1234"])
                .is_err()
        );
    }

    /// With `--filter`, the command consumes only the notes that use a matching note script.
    #[tokio::test]
    async fn consume_notes_consumes_only_the_notes_that_match_the_filter() {
        let (rpc, wallet, note_files) = mock_chain_with_notes(2, 2);
        let store_path = create_test_store_path();
        let client = new_wallet_client(&rpc, store_path.clone(), &wallet, &note_files, None).await;
        assert_eq!(num_input_notes(&client, NoteFilter::Committed).await, 4);

        // The one-transaction path prints the transaction effects, which reads the CLI config file.
        // One note per transaction makes two transactions, so the test does not need that file.
        consume_notes(client, &wallet, &["--filter", "p2ide", "--notes-per-tx", "1"])
            .await
            .unwrap();

        let client = open_client(&rpc, store_path, None).await;
        assert_eq!(num_input_notes(&client, NoteFilter::Processing).await, 2);
        let committed_notes = client.get_input_notes(NoteFilter::Committed).await.unwrap();
        assert_eq!(committed_notes.len(), 2);
        assert!(
            committed_notes
                .iter()
                .all(|note| note.details().script().root() == StandardNote::P2ID.script_root())
        );
    }

    /// Consumes all committed notes of `wallet` with `in_flight` transactions in flight.
    async fn consume_notes_pipelined(
        client: &mut Client<FilesystemKeyStore>,
        wallet: &Account,
        notes_per_tx: usize,
        in_flight: usize,
        prover: Arc<dyn TransactionProver + Send + Sync>,
    ) -> Result<(), CliError> {
        let notes: Vec<Note> = client
            .get_input_notes(NoteFilter::Committed)
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.try_into().unwrap())
            .collect();
        Box::pin(super::consume_notes_pipelined(
            client,
            wallet.id(),
            &notes,
            notes_per_tx,
            prover,
            in_flight,
        ))
        .await
    }

    /// With `--in-flight` above 1, the command proves transactions at the same time and submits
    /// them in execution order, so the node commits all of them.
    #[tokio::test]
    async fn consume_notes_pipelined_submits_the_transactions_in_order() {
        const NUM_NOTES: usize = 10;
        const NOTES_PER_TX: usize = 2;

        let (rpc, wallet, note_files) = mock_chain_with_notes(NUM_NOTES, 0);
        let mut client =
            new_wallet_client(&rpc, create_test_store_path(), &wallet, &note_files, None).await;
        let initial_balance = balance(&client, &wallet).await;

        let prover = Arc::new(LocalTransactionProver::default());
        consume_notes_pipelined(&mut client, &wallet, NOTES_PER_TX, 2, prover)
            .await
            .unwrap();

        assert_eq!(num_input_notes(&client, NoteFilter::Committed).await, 0);
        assert_eq!(num_input_notes(&client, NoteFilter::Processing).await, NUM_NOTES);
        assert_eq!(client.get_transactions(TransactionFilter::Uncommitted).await.unwrap().len(), 5);

        rpc.prove_block();
        client.sync_state().await.unwrap();
        let transactions = client.get_transactions(TransactionFilter::All).await.unwrap();
        assert_eq!(transactions.len(), 5);
        assert!(
            transactions
                .iter()
                .all(|tx| matches!(tx.status, TransactionStatus::Committed { .. }))
        );
        assert_eq!(
            balance(&client, &wallet).await,
            initial_balance + NUM_NOTES as u64 * NOTE_AMOUNT
        );
    }

    /// When a transaction fails, the transactions executed after it are not submitted. The sync
    /// after they expire discards them and releases their notes, and a new run consumes the notes.
    #[tokio::test]
    async fn consume_notes_pipelined_releases_the_notes_of_unsubmitted_transactions() {
        const NUM_NOTES: usize = 10;
        const NOTES_PER_TX: usize = 2;

        let (rpc, wallet, note_files) = mock_chain_with_notes(NUM_NOTES, 0);
        let store_path = create_test_store_path();
        let mut client =
            new_wallet_client(&rpc, store_path.clone(), &wallet, &note_files, None).await;
        // The third proof fails.
        let prover = Arc::new(FailingProver {
            calls: AtomicUsize::new(0),
            failing_call: 2,
        });

        let result = consume_notes_pipelined(&mut client, &wallet, NOTES_PER_TX, 2, prover).await;
        assert!(matches!(
            result,
            Err(CliError::Client {
                error: ClientError::TransactionProvingError(_),
                ..
            })
        ));

        // Transactions 1 and 2 were submitted. Transaction 3 failed to prove, and transaction 4 was
        // executed while transaction 3 was being proven. The node never received 3 and 4.
        assert_eq!(client.get_transactions(TransactionFilter::Uncommitted).await.unwrap().len(), 4);
        assert_eq!(num_input_notes(&client, NoteFilter::Processing).await, 4 * NOTES_PER_TX);
        assert_eq!(
            num_input_notes(&client, NoteFilter::Committed).await,
            NUM_NOTES - 4 * NOTES_PER_TX
        );

        rpc.prove_block();
        rpc.advance_blocks(crate::TX_DISCARD_DELTA + 2);
        client.sync_state().await.unwrap();
        let transactions = client.get_transactions(TransactionFilter::All).await.unwrap();
        assert_eq!(
            transactions
                .iter()
                .filter(|tx| matches!(tx.status, TransactionStatus::Committed { .. }))
                .count(),
            2
        );
        assert_eq!(
            transactions
                .iter()
                .filter(|tx| matches!(tx.status, TransactionStatus::Discarded(_)))
                .count(),
            2
        );
        // The notes of the committed transactions are consumed. The discarded transactions released
        // their notes, which are consumable again.
        assert_eq!(num_input_notes(&client, NoteFilter::Consumed).await, 2 * NOTES_PER_TX);
        assert_eq!(num_input_notes(&client, NoteFilter::Processing).await, 0);
        assert_eq!(
            num_input_notes(&client, NoteFilter::Committed).await,
            NUM_NOTES - 2 * NOTES_PER_TX
        );

        let notes_per_tx = NOTES_PER_TX.to_string();
        consume_notes(client, &wallet, &["--notes-per-tx", &notes_per_tx])
            .await
            .unwrap();

        let client = open_client(&rpc, store_path, None).await;
        assert_eq!(num_input_notes(&client, NoteFilter::Committed).await, 0);
        assert_eq!(
            num_input_notes(&client, NoteFilter::Processing).await,
            NUM_NOTES - 2 * NOTES_PER_TX
        );
        // The new transactions are pending. Two of them consume the released notes from the same
        // account state as the discarded transactions, so they have the same IDs and replace the
        // discarded records.
        assert_eq!(client.get_transactions(TransactionFilter::Uncommitted).await.unwrap().len(), 3);
    }

    /// `--in-flight` takes 1 to 8, and a value above 1 needs `--delegate-proving`.
    #[tokio::test]
    async fn consume_notes_validates_in_flight() {
        assert!(ConsumeNotesCmd::try_parse_from(["consume-notes", "--in-flight", "0"]).is_err());
        assert!(ConsumeNotesCmd::try_parse_from(["consume-notes", "--in-flight", "9"]).is_err());

        let (rpc, wallet, _) = mock_chain_with_notes(1, 0);
        let client = open_client(&rpc, create_test_store_path(), None).await;
        let result = consume_notes(client, &wallet, &["--in-flight", "2"]).await;
        assert!(matches!(result, Err(CliError::Input(_))));
    }
}
