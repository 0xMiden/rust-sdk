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

    /// Desired filename for the binary file. Defaults to the note ID if not provided.
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
            export_account(&client, &keystore, self.id.as_str(), self.filename.clone()).await?;
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

async fn export_account<AUTH>(
    client: &Client<AUTH>,
    keystore: &FilesystemKeyStore,
    account_id: &str,
    filename: Option<PathBuf>,
) -> Result<File, CliError> {
    let account_id = parse_account_id(client, account_id).await?;

    let account: Account = client
        .get_account(account_id)
        .await?
        .ok_or_else(|| CliError::Export(format!("Account with ID {account_id} not found")))?;

    // Use the Keystore trait method to get all keys for this account
    let key_pairs = keystore.get_keys_for_account(&account_id).await.map_err(CliError::KeyStore)?;

    if key_pairs.is_empty() {
        return Err(CliError::Export("No keys found for account".to_string()));
    }

    let account_data = AccountFile::new(account, key_pairs);

    let file_path = if let Some(filename) = filename {
        filename
    } else {
        let current_dir = std::env::current_dir()?;
        current_dir.join(format!("{account_id}.mac"))
    };

    info!("Writing file to {}", file_path.to_string_lossy());
    let mut file = create_secret_data_file(&file_path)?;
    account_data.write_into(&mut file);
    #[cfg(unix)]
    restrict_secret_data_file_permissions(&file_path)?;

    println!("Successfully exported account {account_id}");
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

// HELPERS
// ================================================================================================

/// Creates a file for writing account secret data (the exported `.mac` file's `auth_secret_keys`),
/// restricted to owner-only access (`0600`) on Unix from the moment of creation.
fn create_secret_data_file(path: &std::path::Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)
    }
    #[cfg(not(unix))]
    {
        File::create(path)
    }
}

/// Forces `path` to owner-only (`0600`) permissions on Unix, in case a file already existed at
/// that path with looser permissions before this export - `OpenOptions::mode` only applies when
/// a file is newly created, not when an existing one is truncated and reopened for writing.
#[cfg(unix)]
fn restrict_secret_data_file_permissions(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{create_secret_data_file, restrict_secret_data_file_permissions};

    fn unique_temp_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("miden-cli-export-test-{label}-{}", std::process::id()))
    }

    /// A freshly created export file must be `0600` from the start.
    #[test]
    fn newly_created_secret_data_file_is_0600() {
        let path = unique_temp_path("new");
        let _ = std::fs::remove_file(&path);

        let file = create_secret_data_file(&path).expect("failed to create file");
        let mode = file.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600 on creation, got {mode:o}");

        let _ = std::fs::remove_file(&path);
    }

    /// Overwriting a pre-existing, loosely-permissioned export file must still end up `0600`
    /// - `OpenOptions::mode` alone (as used by `create_secret_data_file`) would silently leave a
    /// pre-existing file's looser permissions in place, which is exactly why `export_account`
    /// also calls `restrict_secret_data_file_permissions` afterwards.
    #[test]
    fn overwriting_an_existing_export_file_still_ends_up_0600() {
        let path = unique_temp_path("overwrite");
        std::fs::write(&path, b"stale contents").expect("failed to pre-create file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("failed to set initial permissions");

        let _file = create_secret_data_file(&path).expect("failed to open file");
        restrict_secret_data_file_permissions(&path).expect("failed to restrict permissions");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600 after overwrite, got {mode:o}");

        let _ = std::fs::remove_file(&path);
    }
}
