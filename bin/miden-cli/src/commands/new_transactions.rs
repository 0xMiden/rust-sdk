use std::io;
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use miden_client::account::AccountId;
use miden_client::keystore::Keystore;
use miden_client::note::{
    BlockNumber,
    Note,
    NoteType as MidenNoteType,
    SwapNote,
    get_input_note_with_id_prefix,
};
use miden_client::store::NoteRecordError;
use miden_client::transaction::{
    NoteArgs,
    PaymentNoteDescription,
    PswapTransactionData,
    RawOutputNote,
    SwapTransactionData,
    TransactionRequest,
    TransactionRequestBuilder,
};
use miden_client::{Client, RemoteTransactionProver};
use tracing::info;

use crate::config::CliConfig;
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
                MidenNoteType::Private,
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

        let payback_note_tag: u32 = SwapNote::build_tag(
            (&self.note_type).into(),
            &swap_transaction.offered_asset(),
            &swap_transaction.requested_asset(),
        )
        .into();
        println!(
            "To receive updates about the payback Swap Note run `miden-client tags --add {payback_note_tag}`",
        );

        Ok(())
    }
}

/// Consume with the account corresponding to `account_id` all of the notes from `list_of_notes`.
/// If no account ID is provided, the default one is used. If no notes are provided, any notes
/// that are identified to be owned by the account ID are consumed.
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

    /// Trust the script roots of the input notes that are not recognized standard scripts. By
    /// default, the client only executes notes whose scripts match a known standard. Pass this
    /// flag after independently verifying the note scripts you are about to run.
    #[arg(long, default_value_t = false)]
    allow_unlisted_note_scripts: bool,
}

impl ConsumeNotesCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let force = self.force;

        let mut input_notes: Vec<(Note, Option<NoteArgs>)> = Vec::new();

        for note_id in &self.list_of_notes {
            input_notes.push((resolve_input_note(&client, note_id).await?, None));
        }

        let account_id =
            get_input_acc_id_by_prefix_or_default(&client, self.account_id.clone()).await?;

        if input_notes.is_empty() {
            info!("No input note IDs provided, getting all notes consumable by {}", account_id);
            let consumable_notes = client.get_consumable_notes(Some(account_id)).await?;
            for (note_record, _) in consumable_notes {
                input_notes.push((
                    note_record.try_into().map_err(|err: NoteRecordError| {
                        CliError::Transaction(
                            err.into(),
                            "Failed to convert note record".to_string(),
                        )
                    })?,
                    None,
                ));
            }
        }

        if input_notes.is_empty() {
            println!("Did not find any consumable notes for {account_id}.");
            return Ok(());
        }

        let mut builder = TransactionRequestBuilder::new();
        if self.allow_unlisted_note_scripts {
            builder = builder.allow_unlisted_note_scripts();
        }
        let transaction_request = builder.input_notes(input_notes).build().map_err(|err| {
            CliError::Transaction(
                err.into(),
                "Failed to build consume notes transaction".to_string(),
            )
        })?;

        execute_transaction(
            &mut client,
            account_id,
            transaction_request,
            force,
            self.delegate_proving,
        )
        .await
    }
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
            PswapAction::Create(cmd) => cmd.execute(client).await,
            PswapAction::Consume(cmd) => cmd.execute(client).await,
            PswapAction::Cancel(cmd) => cmd.execute(client).await,
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

    /// Visibility of the payback note produced when the PSWAP is filled. Defaults
    /// to private (cheaper, and the fill amount is already recorded in the
    /// executing transaction).
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

        // The CLI does not yet support note-supplied fills (in-flight fills routed through
        // other notes), so pass 0 for `note_fill_amount`.
        let tx_request = TransactionRequestBuilder::new()
            .build_pswap_consume(&note, consumer_id, self.fill_amount, 0)
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
    /// Account ID or its hex prefix of the note creator. If none is provided, the default
    /// account is used.
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
    if !force {
        println!(
            "\nContinue with proving and submission? Changes will be irreversible once the proof is finalized on the network (y/N)"
        );
        let mut proceed_str: String = String::new();
        io::stdin().read_line(&mut proceed_str).expect("Should read line");

        if proceed_str.trim().to_lowercase() != "y" {
            println!("Transaction was cancelled.");
            return Ok(());
        }
    }

    let transaction_id = executed_transaction.id();
    let output_notes = executed_transaction
        .output_notes()
        .iter()
        .map(RawOutputNote::id)
        .collect::<Vec<_>>();

    println!("Proving transaction...");

    let prover = if delegated_proving {
        let cli_config = CliConfig::load()?;
        let remote_prover_endpoint =
            cli_config.remote_prover_endpoint.as_ref().ok_or(CliError::Config(
                "Remote prover endpoint".to_string().into(),
                "remote prover endpoint is not set in the configuration file".to_string(),
            ))?;

        Arc::new(
            RemoteTransactionProver::new(remote_prover_endpoint.to_string())
                .with_timeout(cli_config.remote_prover_timeout),
        )
    } else {
        client.prover()
    };

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
