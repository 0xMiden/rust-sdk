//! Provides various utilities that are commonly used throughout the Miden
//! client library.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::num::ParseIntError;

use miden_protocol::asset::AssetAmount;
use miden_protocol::errors::AssetError;
use miden_standards::account::faucets::FungibleFaucet;
pub use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};
pub use miden_tx::utils::sync::{LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
pub use miden_tx::utils::{ToHex, bytes_to_hex_string, hex_to_bytes};

use crate::alloc::borrow::ToOwned;

/// Converts an amount in the faucet base units to the token's decimals.
///
/// This is meant for display purposes only.
pub fn base_units_to_tokens(units: AssetAmount, decimals: u8) -> String {
    let units_str = units.as_u64().to_string();
    let len = units_str.len();

    if decimals == 0 {
        return units_str;
    }

    if decimals as usize >= len {
        // Handle cases where the number of decimals is greater than the length of units
        "0.".to_owned() + &"0".repeat(decimals as usize - len) + &units_str
    } else {
        // Insert the decimal point at the correct position
        let integer_part = &units_str[..len - decimals as usize];
        let fractional_part = &units_str[len - decimals as usize..];
        format!("{integer_part}.{fractional_part}")
    }
}

/// Errors that can occur when parsing a token represented as a decimal number in
/// a string into base units.
#[derive(thiserror::Error, Debug)]
pub enum TokenParseError {
    #[error("Number of decimals {0} must be less than or equal to {max_decimals}", max_decimals = FungibleFaucet::MAX_DECIMALS)]
    MaxDecimals(u8),
    #[error("More than one decimal point")]
    MultipleDecimalPoints,
    #[error("Failed to parse u64")]
    ParseU64(#[source] ParseIntError),
    #[error("Amount has more than {0} decimal places")]
    TooManyDecimals(u8),
    #[error("Amount is not a valid asset amount")]
    InvalidAmount(#[source] AssetError),
}

/// Converts a decimal number, represented as a string, into an integer by shifting
/// the decimal point to the right by a specified number of decimal places.
pub fn tokens_to_base_units(
    decimal_str: &str,
    n_decimals: u8,
) -> Result<AssetAmount, TokenParseError> {
    if n_decimals > FungibleFaucet::MAX_DECIMALS {
        return Err(TokenParseError::MaxDecimals(n_decimals));
    }

    // Split the string on the decimal point
    let parts: Vec<&str> = decimal_str.split('.').collect();

    if parts.len() > 2 {
        return Err(TokenParseError::MultipleDecimalPoints);
    }

    // Validate that the parts are valid numbers
    for part in &parts {
        part.parse::<u64>().map_err(TokenParseError::ParseU64)?;
    }

    // Get the integer part
    let integer_part = parts[0];

    // Get the fractional part; remove trailing zeros
    let mut fractional_part = if parts.len() > 1 {
        parts[1].trim_end_matches('0').to_string()
    } else {
        String::new()
    };

    // Check if the fractional part has more than N decimals
    if fractional_part.len() > n_decimals.into() {
        return Err(TokenParseError::TooManyDecimals(n_decimals));
    }

    // Add extra zeros if the fractional part is shorter than N decimals
    while fractional_part.len() < n_decimals.into() {
        fractional_part.push('0');
    }

    // Combine the integer and padded fractional part
    let combined = format!("{}{}", integer_part, &fractional_part[0..n_decimals.into()]);

    // Convert the combined string to an integer
    let units = combined.parse::<u64>().map_err(TokenParseError::ParseU64)?;

    AssetAmount::new(units).map_err(TokenParseError::InvalidAmount)
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::asset::AssetAmount;

    use crate::utils::{TokenParseError, base_units_to_tokens, tokens_to_base_units};

    fn amount(units: u64) -> AssetAmount {
        AssetAmount::new(units).unwrap()
    }

    #[test]
    fn convert_tokens_to_base_units() {
        assert_eq!(tokens_to_base_units("9223372.034707292160", 12).unwrap(), AssetAmount::MAX);
        assert_eq!(tokens_to_base_units("7531.2468", 8).unwrap(), amount(753_124_680_000));
        assert_eq!(tokens_to_base_units("7531.2468", 4).unwrap(), amount(75_312_468));
        assert_eq!(tokens_to_base_units("0", 3).unwrap(), AssetAmount::ZERO);
        assert_eq!(tokens_to_base_units("1234", 8).unwrap(), amount(123_400_000_000));
        assert_eq!(tokens_to_base_units("1", 0).unwrap(), amount(1));
        assert!(matches!(
            tokens_to_base_units("1.1", 0),
            Err(TokenParseError::TooManyDecimals(0))
        ),);
        assert!(matches!(
            tokens_to_base_units("18446744.073709551615", 11),
            Err(TokenParseError::TooManyDecimals(11))
        ),);
        assert!(matches!(tokens_to_base_units("123u3.23", 4), Err(TokenParseError::ParseU64(_))),);
        assert!(matches!(tokens_to_base_units("2.k3", 4), Err(TokenParseError::ParseU64(_))),);
        assert_eq!(tokens_to_base_units("12.345000", 4).unwrap(), amount(123_450));
        assert!(tokens_to_base_units("0.0001.00000001", 12).is_err());
        // Parses as a u64 but exceeds the maximum representable asset amount.
        assert!(matches!(
            tokens_to_base_units("18446744.073709551615", 12),
            Err(TokenParseError::InvalidAmount(_))
        ),);
    }

    #[test]
    fn convert_base_units_to_tokens() {
        assert_eq!(base_units_to_tokens(AssetAmount::MAX, 12), "9223372.034707292160");
        assert_eq!(base_units_to_tokens(amount(753_124_680_000), 8), "7531.24680000");
        assert_eq!(base_units_to_tokens(amount(75_312_468), 4), "7531.2468");
        assert_eq!(base_units_to_tokens(amount(75_312_468), 0), "75312468");
    }
}
