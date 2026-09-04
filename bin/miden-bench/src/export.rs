#![allow(clippy::cast_possible_truncation)]

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use miden_client::account::{AccountFile, AccountId};
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::{Client, Serializable};

use crate::report::format_size;

/// Exports an account from the client's store to a `.mac` file. The file contains the [`Account`]
/// alongside its auth secret keys retrieved from the filesystem keystore. When `filename` is
/// `None`, the file is written to the current working directory as `<account_id>.mac`.
pub async fn export_account(
    client: &Client<FilesystemKeyStore>,
    store_path: &Path,
    account_id_str: &str,
    filename: Option<PathBuf>,
) -> anyhow::Result<()> {
    let account_id = AccountId::from_hex(account_id_str)?;

    let file_path = match filename {
        Some(path) => path,
        None => std::env::current_dir()?.join(format!("{account_id}.mac")),
    };

    println!("Exporting account {account_id} to {}...", file_path.display());

    let t = Instant::now();

    let account = client
        .get_account(account_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Account {account_id} not found in store"))?;

    let keystore_path = store_path.join("keystore");
    let keystore = FilesystemKeyStore::new(keystore_path)
        .map_err(|e| anyhow::anyhow!("Failed to create keystore: {e}"))?;
    let key_pairs = keystore.get_keys_for_account(&account_id).await?;
    if key_pairs.is_empty() {
        anyhow::bail!("No keys found for account {account_id}");
    }

    let account_data = AccountFile::new(account, key_pairs);
    let mut file = File::create(&file_path)?;
    file.write_all(&account_data.to_bytes())?;
    let elapsed = t.elapsed();

    let file_size = std::fs::metadata(&file_path)?.len() as usize;

    println!();
    println!("Account file size: {}", format_size(file_size));
    println!("Export time: {elapsed:.2?}");

    Ok(())
}
