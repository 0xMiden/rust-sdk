#![allow(clippy::cast_possible_truncation)]

use std::path::Path;
use std::time::Instant;

use miden_client::account::{AccountFile, AccountId};
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::{Client, Serializable};

use crate::report::format_size;

/// Imports an account from a `.mac` file. The file is read with [`AccountFile::read`], the auth
/// secret keys are inserted into the filesystem keystore, and the account is added to the client's
/// store. Fails if the account already exists in the store.
pub async fn import_from_file(
    client: &mut Client<FilesystemKeyStore>,
    store_path: &Path,
    filename: &Path,
) -> anyhow::Result<()> {
    let file_size = std::fs::metadata(filename)?.len() as usize;

    println!("Importing account from {}...", filename.display());

    let t = Instant::now();
    let account_file = AccountFile::read(filename)?;
    let account_id = account_file.account.id();
    let AccountFile { account, auth_secret_keys } = account_file;

    let keystore_path = store_path.join("keystore");
    let keystore = FilesystemKeyStore::new(keystore_path)
        .map_err(|e| anyhow::anyhow!("Failed to create keystore: {e}"))?;
    for key in auth_secret_keys {
        keystore.add_key(&key, account_id).await?;
    }

    client.add_account(&account, false).await?;
    let elapsed = t.elapsed();

    println!();
    println!("Account ID: {account_id}");
    println!("Account file size: {}", format_size(file_size));
    println!("Import time: {elapsed:.2?}");

    Ok(())
}

/// Imports a public account from the network by its ID via [`Client::import_account_by_id`]. After
/// the import, the account is read back from the store to report the serialized account size.
pub async fn import_from_network(
    client: &mut Client<FilesystemKeyStore>,
    account_id_str: &str,
) -> anyhow::Result<()> {
    let account_id = AccountId::from_hex(account_id_str)?;

    println!("Importing account {account_id} from network...");

    let t = Instant::now();
    client.import_account_by_id(account_id).await?;
    let elapsed = t.elapsed();

    let account = client
        .get_account(account_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Imported account {account_id} not found in store"))?;
    let serialized_size = account.to_bytes().len();

    println!();
    println!("Account ID: {account_id}");
    println!("Serialized account size: {}", format_size(serialized_size));
    println!("Import time: {elapsed:.2?}");

    Ok(())
}
