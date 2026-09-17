use std::fs;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, ValueEnum};
use miden_client::account::AccountId;
use miden_client::auth::{AuthSchemeId, AuthSecretKey, PublicKeyCommitment};
use miden_client::crypto::{ecdsa_k256_keccak, rpo_falcon512};
use miden_client::keystore::FilesystemKeyStore;
use miden_client::utils::{ByteReader, Deserializable, hex_to_bytes};
use miden_client::{SliceReader, Word};

use crate::errors::CliError;
use crate::{Parser, create_dynamic_table};

const ECDSA_PUBLIC_KEY_BYTES: usize = 33;
const FALCON_PUBLIC_KEY_BYTES: usize = 897;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum KeyScheme {
    #[value(name = "falcon512-poseidon2")]
    Falcon512Poseidon2,
    #[value(name = "ecdsa-k256-keccak")]
    EcdsaK256Keccak,
}

impl KeyScheme {
    fn name(self) -> &'static str {
        match self {
            Self::Falcon512Poseidon2 => "falcon512-poseidon2",
            Self::EcdsaK256Keccak => "ecdsa-k256-keccak",
        }
    }
}

impl From<KeyScheme> for AuthSchemeId {
    fn from(value: KeyScheme) -> Self {
        match value {
            KeyScheme::Falcon512Poseidon2 => Self::Falcon512Poseidon2,
            KeyScheme::EcdsaK256Keccak => Self::EcdsaK256Keccak,
        }
    }
}

#[derive(Clone, Debug, Parser)]
#[command(
    about = "Manage authentication keys. Defaults to --list",
    group(ArgGroup::new("action").args([
        "list",
        "generate",
        "import",
        "commitment",
        "associate",
        "disassociate",
    ])),
    group(ArgGroup::new("scheme_action").args(["generate", "commitment"])),
    group(ArgGroup::new("association_action").args(["associate", "disassociate"])),
)]
pub struct KeysCmd {
    /// List all keys in the keystore.
    #[arg(long)]
    list: bool,

    /// Generate and store a new key.
    #[arg(long, requires = "scheme")]
    generate: bool,

    /// Import a serialized authentication secret key.
    #[arg(long, value_name = "FILE")]
    import: Option<PathBuf>,

    /// Calculate the commitment of a serialized public key.
    #[arg(long, value_name = "PUBLIC_KEY", requires = "scheme")]
    commitment: Option<String>,

    /// Associate a stored key with an account.
    #[arg(long, value_name = "COMMITMENT", requires = "account_id")]
    associate: Option<String>,

    /// Remove an association between a stored key and an account.
    #[arg(long, value_name = "COMMITMENT", requires = "account_id")]
    disassociate: Option<String>,

    /// Authentication scheme for key generation or commitment calculation.
    #[arg(long, value_enum, requires = "scheme_action")]
    scheme: Option<KeyScheme>,

    /// Full hexadecimal account ID for an association operation.
    #[arg(long, value_name = "ACCOUNT_ID", requires = "association_action")]
    account_id: Option<String>,
}

impl KeysCmd {
    pub fn execute(&self, keystore: &FilesystemKeyStore) -> Result<(), CliError> {
        match self {
            Self { generate: true, scheme: Some(scheme), .. } => generate_key(keystore, *scheme),
            Self { import: Some(file), .. } => import_key(keystore, file),
            Self {
                commitment: Some(public_key),
                scheme: Some(scheme),
                ..
            } => print_commitment(*scheme, public_key),
            Self {
                associate: Some(commitment),
                account_id: Some(account_id),
                ..
            } => associate_key(keystore, commitment, account_id),
            Self {
                disassociate: Some(commitment),
                account_id: Some(account_id),
                ..
            } => disassociate_key(keystore, commitment, account_id),
            _ => list_keys(keystore),
        }
    }
}

fn list_keys(keystore: &FilesystemKeyStore) -> Result<(), CliError> {
    let mut table = create_dynamic_table(&["Commitment", "Scheme", "Associated accounts"]);

    for key in keystore.list_keys().map_err(CliError::KeyStore)? {
        let account_ids = if key.account_ids.is_empty() {
            "-".to_string()
        } else {
            key.account_ids
                .iter()
                .map(|account_id| account_id.to_hex())
                .collect::<Vec<_>>()
                .join(", ")
        };
        table.add_row(vec![
            Word::from(key.commitment).to_hex(),
            scheme_name(key.scheme),
            account_ids,
        ]);
    }

    println!("\n{table}");
    println!("Associated keys are included in account exports unless --no-keys is used.");
    Ok(())
}

fn associate_key(
    keystore: &FilesystemKeyStore,
    commitment: &str,
    account_id: &str,
) -> Result<(), CliError> {
    let commitment = parse_commitment(commitment)?;
    let account_id = parse_account_id(account_id)?;
    keystore.associate_key(commitment, account_id).map_err(CliError::KeyStore)?;
    println!(
        "Associated key {} with account {}.",
        Word::from(commitment).to_hex(),
        account_id.to_hex()
    );
    Ok(())
}

fn disassociate_key(
    keystore: &FilesystemKeyStore,
    commitment: &str,
    account_id: &str,
) -> Result<(), CliError> {
    let commitment = parse_commitment(commitment)?;
    let account_id = parse_account_id(account_id)?;
    keystore.disassociate_key(commitment, account_id).map_err(CliError::KeyStore)?;
    println!(
        "Removed the association between key {} and account {}.",
        Word::from(commitment).to_hex(),
        account_id.to_hex()
    );
    Ok(())
}

fn generate_key(keystore: &FilesystemKeyStore, scheme: KeyScheme) -> Result<(), CliError> {
    let key = AuthSecretKey::with_scheme(scheme.into())
        .map_err(|err| CliError::Input(format!("failed to generate key: {err}")))?;
    store_and_report_key(keystore, &key, "Generated")
}

fn import_key(keystore: &FilesystemKeyStore, file: &Path) -> Result<(), CliError> {
    let bytes = fs::read(file)?;
    let mut reader = SliceReader::new(&bytes);
    let key = AuthSecretKey::read_from(&mut reader).map_err(|err| {
        CliError::Input(format!(
            "failed to decode authentication secret key from {}: {err}",
            file.display()
        ))
    })?;
    if reader.has_more_bytes() {
        return Err(CliError::Input(format!(
            "authentication secret key in {} contains trailing bytes",
            file.display()
        )));
    }
    store_and_report_key(keystore, &key, "Imported")
}

fn store_and_report_key(
    keystore: &FilesystemKeyStore,
    key: &AuthSecretKey,
    action: &str,
) -> Result<(), CliError> {
    keystore.store_key(key).map_err(CliError::KeyStore)?;
    let commitment = Word::from(key.public_key().to_commitment()).to_hex();
    println!("{action} {} key.", scheme_name(key.auth_scheme()));
    println!("Public key commitment: {commitment}");
    Ok(())
}

fn print_commitment(scheme: KeyScheme, public_key: &str) -> Result<(), CliError> {
    let commitment = match scheme {
        KeyScheme::Falcon512Poseidon2 => {
            let bytes = hex_to_bytes::<FALCON_PUBLIC_KEY_BYTES>(public_key)
                .map_err(|err| invalid_public_key(scheme, err))?;
            rpo_falcon512::PublicKey::read_from_bytes(&bytes)
                .map_err(|err| invalid_public_key(scheme, err))?
                .to_commitment()
        },
        KeyScheme::EcdsaK256Keccak => {
            let bytes = hex_to_bytes::<ECDSA_PUBLIC_KEY_BYTES>(public_key)
                .map_err(|err| invalid_public_key(scheme, err))?;
            ecdsa_k256_keccak::PublicKey::read_from_bytes(&bytes)
                .map_err(|err| invalid_public_key(scheme, err))?
                .to_commitment()
        },
    };

    println!("{}", commitment.to_hex());
    Ok(())
}

fn invalid_public_key(scheme: KeyScheme, err: impl std::fmt::Display) -> CliError {
    CliError::Input(format!("invalid {} public key: {err}", scheme.name()))
}

fn parse_commitment(value: &str) -> Result<PublicKeyCommitment, CliError> {
    Word::try_from(value)
        .map(PublicKeyCommitment::from)
        .map_err(|err| CliError::Input(format!("invalid public key commitment `{value}`: {err}")))
}

fn parse_account_id(value: &str) -> Result<AccountId, CliError> {
    AccountId::from_hex(value)
        .map_err(|err| CliError::Input(format!("invalid account ID `{value}`: {err}")))
}

fn scheme_name(scheme: AuthSchemeId) -> String {
    match scheme {
        AuthSchemeId::Falcon512Poseidon2 => "falcon512-poseidon2".to_string(),
        AuthSchemeId::EcdsaK256Keccak => "ecdsa-k256-keccak".to_string(),
        _ => scheme.to_string(),
    }
}
