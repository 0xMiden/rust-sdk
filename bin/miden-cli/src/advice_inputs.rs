use std::path::Path;

use miden_client::{Felt, Word};
use serde::{Deserialize, Deserializer, de};

use crate::errors::CliError;

// ADVICE MAP INPUTS
// ================================================================================================

/// Long-form `--help` text shared by the `inputs_path` argument of the `exec` and `call` commands,
/// describing the advice-inputs TOML file format.
pub const INPUTS_PATH_LONG_HELP: &str = "\
Path to a TOML file whose entries are loaded into the VM's advice map.

The file must contain a TOML array named `inputs` of inline tables, where each table has two \
fields:
- `key`: a 256-bit hexadecimal string (prefixed with `0x`) used as the advice-map key.
- `values`: an array of 64-bit unsigned integers, each written as a separate string within double \
quotes.

Example:
    inputs = [
        { key = \"0x0000000000000000000000000000000000000000000000000000001000000000\", values = [\"13\", \"9\"] },
        { key = \"0x0000000000000000000000000000000000000000000000000000000000000000\", values = [\"1\", \"2\"] },
    ]";

/// Struct that holds a single key-values pair from the provided file inputs file. These will be
/// aggregated in the [`CliAdviceInputs`] struct
#[derive(Deserialize)]
struct CliAdviceInput {
    key: String,
    #[serde(deserialize_with = "string_to_u64")]
    values: Vec<u64>,
}

/// Struct that holds every key-values pair present in the provided inputs file.
#[derive(Deserialize)]
struct CliAdviceInputs {
    inputs: Vec<CliAdviceInput>,
}

/// Since the toml crate has problems parsing u64 values (see
/// [issue](https://github.com/toml-rs/toml/issues/705), we store the values as Strings. Then, when
/// deserializing, we turn those Strings to u64 in order to then turn them to Felts.
fn string_to_u64<'de, D>(deserializer: D) -> Result<Vec<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)?
        .into_iter()
        .map(|a| a.parse::<u64>())
        .collect::<Result<Vec<u64>, _>>()
        .map_err(|_| {
            de::Error::custom(
                "invalid type: expected u64 in between double quotes. For example: values = [\"13\", \"9\"]",
            )
        })
}

/// Reads an advice inputs TOML file and parses it into advice-map key-values entries.
///
/// The file should contain a single `inputs` array of inline tables, each with a `key` (a 256-bit
/// hex word, prefixed with `0x`) and `values` (an array of `u64`s written as quoted strings):
///
/// ```toml
/// inputs = [
///     { key = "0x...", values = ["13", "9"] },
/// ]
/// ```
pub fn load_advice_map_from_file(path: &Path) -> Result<Vec<(Word, Vec<Felt>)>, CliError> {
    if !path.exists() {
        return Err(CliError::Input(format!(
            "the advice inputs file at path {} does not exist",
            path.display()
        )));
    }
    let contents = std::fs::read_to_string(path)?;
    parse_advice_map(&contents)
}

/// Parses advice-map key-values entries from the contents of an advice inputs TOML file.
fn parse_advice_map(serialized: &str) -> Result<Vec<(Word, Vec<Felt>)>, CliError> {
    let cli_inputs: CliAdviceInputs = toml::from_str(serialized)
        .map_err(|err| CliError::Input(format!("failed to parse advice inputs: {err}")))?;
    cli_inputs
        .inputs
        .into_iter()
        .map(|input| {
            let word = Word::try_from(input.key).map_err(|err| err.to_string())?;
            let felts: Vec<Felt> = input
                .values
                .into_iter()
                .map(|v| Felt::new(v).map_err(|err| err.to_string()))
                .collect::<Result<_, _>>()?;
            Ok((word, felts))
        })
        .collect::<Result<Vec<_>, String>>()
        .map_err(CliError::Input)
}
