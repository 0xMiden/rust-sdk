use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use miden_client::Client;
use miden_client::account::{Account, AccountFile};
use miden_client::keystore::Keystore;
use miden_client::store::NoteExportType;
use miden_client::utils::Serializable;
use tracing::info;

use crate::errors::CliError;
use crate::utils::parse_account_id;
use crate::{FilesystemKeyStore, Parser, get_output_note_with_id_prefix};

#[derive(Debug, Parser, Clone)]
#[command(about = "Export client output notes, or account data")]
pub struct ExportCmd {
    /// ID (or a valid prefix) of the output note or account to export.
    #[clap()]
    id: String,

    /// Desired filename for the binary file. Defaults to the note ID for a note, and to the account
    /// ID for an account.
    #[arg(short, long)]
    filename: Option<PathBuf>,

    /// Export account data (cannot be used with --note).
    #[arg(long, conflicts_with = "note")]
    account: bool,

    /// Export note data (cannot be used with --account).
    #[arg(long, requires = "export_type", conflicts_with = "account")]
    note: bool,

    /// Exported note type.
    #[arg(short, long, value_enum, conflicts_with = "account")]
    export_type: Option<ExportType>,

    /// Leave the account secret keys out of the exported account file.
    #[arg(
        long,
        default_value_t = false,
        requires = "account",
        conflicts_with_all = ["note", "export_type"]
    )]
    no_keys: bool,
}

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum ExportType {
    Id,
    Full,
    Partial,
}

impl From<&ExportType> for NoteExportType {
    fn from(export_type: &ExportType) -> NoteExportType {
        match export_type {
            ExportType::Id => NoteExportType::NoteId,
            ExportType::Full => NoteExportType::NoteWithProof,
            ExportType::Partial => NoteExportType::NoteDetails,
        }
    }
}

impl ExportCmd {
    pub async fn execute<AUTH: Keystore + Sync>(
        &self,
        mut client: Client<AUTH>,
        keystore: FilesystemKeyStore,
    ) -> Result<(), CliError> {
        if self.account {
            export_account(
                &client,
                &keystore,
                self.id.as_str(),
                self.filename.clone(),
                self.no_keys,
            )
            .await?;
        } else if let Some(export_type) = &self.export_type {
            export_note(&mut client, self.id.as_str(), self.filename.clone(), export_type).await?;
        } else {
            return Err(CliError::Export(
                "Export type is required when exporting a note".to_string(),
            ));
        }
        Ok(())
    }
}

// EXPORT ACCOUNT
// ================================================================================================

/// Writes the account to an account file.
///
/// The file carries the secret keys that the keystore holds for the account. The keystore can hold
/// no key for the account, and `no_keys` leaves the keys out of a file that would otherwise carry
/// them. Both cases produce a file that describes the account state without granting the right to
/// sign for it.
///
/// The account itself is still sensitive. The file carries the account seed while the account is
/// undeployed, and the seed is needed to deploy the account.
async fn export_account<AUTH>(
    client: &Client<AUTH>,
    keystore: &FilesystemKeyStore,
    account_id: &str,
    filename: Option<PathBuf>,
    no_keys: bool,
) -> Result<File, CliError> {
    let account_id = parse_account_id(client, account_id).await?;

    let account: Account = client
        .get_account(account_id)
        .await?
        .ok_or_else(|| CliError::Export(format!("Account with ID {account_id} not found")))?;

    let key_pairs = if no_keys {
        Vec::new()
    } else {
        keystore.get_keys_for_account(&account_id).await.map_err(CliError::KeyStore)?
    };

    let exported_key_count = key_pairs.len();
    let account_data = AccountFile::new(account, key_pairs);

    let file_path = if let Some(filename) = filename {
        filename
    } else {
        let current_dir = std::env::current_dir()?;
        current_dir.join(format!("{account_id}.mac"))
    };

    info!("Writing file to {}", file_path.to_string_lossy());
    let mut file = File::create(file_path)?;
    account_data.write_into(&mut file);

    if exported_key_count == 0 {
        println!("Successfully exported account {account_id} without secret keys");
    } else {
        println!(
            "Successfully exported account {account_id} with {exported_key_count} secret key(s)"
        );
    }
    Ok(file)
}

// EXPORT NOTE
// ================================================================================================

async fn export_note<AUTH: Keystore + Sync>(
    client: &mut Client<AUTH>,
    note_id: &str,
    filename: Option<PathBuf>,
    export_type: &ExportType,
) -> Result<File, CliError> {
    let note_id = get_output_note_with_id_prefix(client, note_id)
        .await
        .map_err(|err| CliError::Export(err.to_string()))?
        .id();

    let output_note = client
        .get_output_notes(miden_client::store::NoteFilter::Unique(note_id))
        .await?
        .pop()
        .expect("should have an output note");

    let note_file = output_note
        .into_note_file(&export_type.into())
        .map_err(|err| CliError::Export(err.to_string()))?;

    let file_path = if let Some(filename) = filename {
        filename
    } else {
        let current_dir = std::env::current_dir()?;
        current_dir.join(format!("{}.mno", note_id.to_hex()))
    };

    info!("Writing file to {}", file_path.to_string_lossy());
    let mut file = File::create(file_path)?;
    file.write_all(&note_file.to_bytes()).map_err(CliError::IO)?;

    println!("Successfully exported note {note_id}");
    Ok(file)
}
