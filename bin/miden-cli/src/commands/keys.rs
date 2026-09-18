use std::fs;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, ValueEnum};
use miden_client::auth::{AuthSchemeId, AuthSecretKey, PublicKeyCommitment};
use miden_client::crypto::{ecdsa_k256_keccak, rpo_falcon512};
use miden_client::keystore::FilesystemKeyStore;
use miden_client::utils::{ByteReader, Deserializable, hex_to_bytes};
use miden_client::{SliceReader, Word};

use crate::codecs::parse_account_id_token;
use crate::errors::CliError;
use crate::{Parser, create_dynamic_table};

/// Length of a serialized ECDSA public key. It matches the compressed SEC1 form that
/// `ecdsa_k256_keccak::PublicKey` reads.
const ECDSA_PUBLIC_KEY_BYTES: usize = 33;
/// Length of a serialized Falcon public key. It matches the form that `rpo_falcon512::PublicKey`
/// reads.
const FALCON_PUBLIC_KEY_BYTES: usize = 897;

/// Name of the Falcon scheme in the command line and in the command output.
const FALCON_SCHEME_NAME: &str = "falcon512-poseidon2";
/// Name of the ECDSA scheme in the command line and in the command output.
const ECDSA_SCHEME_NAME: &str = "ecdsa-k256-keccak";

#[derive(Clone, Copy, Debug, ValueEnum)]
enum KeyScheme {
    #[value(name = FALCON_SCHEME_NAME)]
    Falcon512Poseidon2,
    #[value(name = ECDSA_SCHEME_NAME)]
    EcdsaK256Keccak,
}

impl KeyScheme {
    fn name(self) -> &'static str {
        match self {
            Self::Falcon512Poseidon2 => FALCON_SCHEME_NAME,
            Self::EcdsaK256Keccak => ECDSA_SCHEME_NAME,
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

impl TryFrom<AuthSchemeId> for KeyScheme {
    type Error = ();

    fn try_from(value: AuthSchemeId) -> Result<Self, Self::Error> {
        match value {
            AuthSchemeId::Falcon512Poseidon2 => Ok(Self::Falcon512Poseidon2),
            AuthSchemeId::EcdsaK256Keccak => Ok(Self::EcdsaK256Keccak),
            _ => Err(()),
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
    group(ArgGroup::new("association_action").args(["associate", "disassociate"])),
)]
pub struct KeysCmd {
    /// List all keys in the keystore.
    #[arg(long)]
    list: bool,

    /// Generate and store a new key.
    #[arg(long, value_name = "SCHEME")]
    generate: Option<KeyScheme>,

    /// Import a serialized authentication secret key.
    #[arg(long, value_name = "FILE")]
    import: Option<PathBuf>,

    /// Calculate the commitment of a serialized public key.
    #[arg(long, value_name = "PUBLIC_KEY")]
    commitment: Option<String>,

    /// Associate a stored key with an account so account exports include it.
    #[arg(long, value_name = "COMMITMENT", requires = "account_id")]
    associate: Option<String>,

    /// Disassociate a stored key from an account so account exports omit it.
    #[arg(long, value_name = "COMMITMENT", requires = "account_id")]
    disassociate: Option<String>,

    /// Account ID for an association operation, as a hexadecimal ID or a bech32 address.
    #[arg(long, value_name = "ACCOUNT_ID", requires = "association_action")]
    account_id: Option<String>,
}

impl KeysCmd {
    pub fn execute(&self, keystore: &FilesystemKeyStore) -> Result<(), CliError> {
        match self {
            Self { generate: Some(scheme), .. } => generate_key(keystore, *scheme),
            Self { import: Some(file), .. } => import_key(keystore, file),
            Self { commitment: Some(public_key), .. } => print_commitment(public_key),
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
    let account_id = parse_account_id_token(account_id)?;
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
    let account_id = parse_account_id_token(account_id)?;
    let removed = keystore.disassociate_key(commitment, account_id).map_err(CliError::KeyStore)?;
    if removed {
        println!(
            "Association between key {} and account {} removed.",
            Word::from(commitment).to_hex(),
            account_id.to_hex()
        );
    } else {
        println!(
            "Key {} wasn't associated with account {}.",
            Word::from(commitment).to_hex(),
            account_id.to_hex()
        );
    }
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

fn print_commitment(public_key: &str) -> Result<(), CliError> {
    let encoded_key = public_key.strip_prefix("0x").ok_or_else(|| {
        CliError::Input("public key must use a 0x-prefixed hexadecimal encoding".to_string())
    })?;
    let scheme = match encoded_key.len() {
        length if length == ECDSA_PUBLIC_KEY_BYTES * 2 => KeyScheme::EcdsaK256Keccak,
        length if length == FALCON_PUBLIC_KEY_BYTES * 2 => KeyScheme::Falcon512Poseidon2,
        length => {
            return Err(CliError::Input(format!(
                "unsupported public key length: expected {} or {} hexadecimal digits, got {}",
                ECDSA_PUBLIC_KEY_BYTES * 2,
                FALCON_PUBLIC_KEY_BYTES * 2,
                length
            )));
        },
    };

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

/// Returns the command line name of an authentication scheme.
///
/// A scheme that the command cannot generate has no command line name. The upstream name is used
/// for it, so that `--list` still reports the key.
fn scheme_name(scheme: AuthSchemeId) -> String {
    KeyScheme::try_from(scheme).map_or_else(|()| scheme.to_string(), |scheme| scheme.name().into())
}
