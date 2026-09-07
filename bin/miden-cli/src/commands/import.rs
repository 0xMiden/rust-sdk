use std::fs;
use std::path::PathBuf;

use miden_client::Client;
use miden_client::account::{AccountFile, AccountId};
use miden_client::keystore::Keystore;
use miden_client::note::NoteFile;
use miden_client::utils::Deserializable;
use tracing::info;

use crate::commands::account::{account_code_has_basic_wallet, set_default_account_if_unset};
use crate::errors::CliError;
use crate::{FilesystemKeyStore, Parser};

#[derive(Debug, Parser, Clone)]
#[command(about = "Import notes or accounts")]
pub struct ImportCmd {
    /// Paths to the files that contains the account/note data.
    #[arg()]
    filenames: Vec<PathBuf>,
    /// Only relevant for accounts. If set, the account will be overwritten if it already exists.
    #[arg(short, long, default_value_t = false)]
    overwrite: bool,
}

impl ImportCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
        keystore: FilesystemKeyStore,
    ) -> Result<(), CliError> {
        validate_paths(&self.filenames)?;
        for filename in &self.filenames {
            let contents = fs::read(filename)?;

            if let Ok(note_file) = NoteFile::read_from_bytes(&contents) {
                match client.import_notes(&[note_file]).await?.first() {
                    Some(commitment) => println!(
                        "Successfully imported note with details commitment {}",
                        commitment.to_hex()
                    ),
                    None => println!("Note was already up to date; nothing to import."),
                }
            } else {
                info!(
                    "Attempting to import account data from {}...",
                    fs::canonicalize(filename)?.as_path().display()
                );
                let Ok(account_file) = AccountFile::read_from_bytes(&contents) else {
                    return Err(CliError::Import(format!(
                        "failed to read `{}` as a note or as an account",
                        filename.to_string_lossy()
                    )));
                };
                let account_id =
                    import_account(&mut client, &keystore, account_file, self.overwrite).await?;

                println!("Successfully imported account {account_id}");

                // Only basic wallets are eligible to become the default account; faucets and
                // other account kinds are skipped.
                if let Some(code) = client.get_account_code(account_id).await?
                    && account_code_has_basic_wallet(account_id, &code)
                {
                    set_default_account_if_unset(&mut client, account_id).await?;
                }
            }
        }
        Ok(())
    }
}

// IMPORT ACCOUNT
// ================================================================================================

/// Imports an account file to the client.
///
/// This implies:
///
/// - Reading all secret keys, and importing them to the CLI keystore with account association
/// - Adding the [account][`miden_client::account::Account`] to the client
async fn import_account<AUTH>(
    client: &mut Client<AUTH>,
    keystore: &FilesystemKeyStore,
    account_file: AccountFile,
    overwrite: bool,
) -> Result<AccountId, CliError> {
    let account_id = account_file.account.id();
    let AccountFile { account, auth_secret_keys } = account_file;

    for key in auth_secret_keys {
        // Use the Keystore trait method which handles both key storage and account association
        keystore.add_key(&key, account_id).await.map_err(CliError::KeyStore)?;
    }

    client.add_account(&account, overwrite).await?;

    Ok(account_id)
}

// HELPERS
// ================================================================================================

/// Checks that all files exist, otherwise returns an error. It also ensures that all files have a
/// specific extension.
fn validate_paths(paths: &[PathBuf]) -> Result<(), CliError> {
    let invalid_path = paths.iter().find(|path| !path.exists());

    if let Some(path) = invalid_path {
        Err(CliError::Input(format!("The path `{}` does not exist", path.to_string_lossy())))
    } else {
        Ok(())
    }
}
