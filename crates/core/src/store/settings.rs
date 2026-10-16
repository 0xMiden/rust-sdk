use alloc::string::String;
use alloc::vec::Vec;

use miden_protocol::Word;

// SETTING SCOPE
// ================================================================================================

/// Which side of the client/user boundary a `settings` row belongs to.
///
/// The discriminants are what a store persists, so they are part of its schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SettingScope {
    /// Owned by the client itself. A store persists these rows but the public settings API of the
    /// client never reaches them.
    Client = 0,
    /// Owned by the user of the client.
    User = 1,
}

impl SettingScope {
    /// Returns the value this scope is stored as.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

// SETTING MUTATION
// ================================================================================================

/// A single mutation against the `settings` KV store, applied as part of an atomic batch via
/// [`Store::apply_settings_mutations`](super::Store::apply_settings_mutations).
#[derive(Debug, Clone)]
pub enum SettingMutation {
    /// Insert or overwrite `key` with `value`.
    Set { key: String, value: Vec<u8> },
    /// Delete `key`.
    Remove { key: String },
}

// SETTING KEYS
// ================================================================================================

/// Returns the settings key that holds the protocol configuration for `commitment`.
///
/// A [`Store`](super::Store) implementation needs this key to persist a configuration a sync
/// returned.
pub fn protocol_config_setting_key(commitment: Word) -> String {
    format!("protocol_config:{commitment}")
}
