//! Trust policy applied to input-note scripts when executing a user transaction request.
//!
//! See [`NoteScriptTrustPolicy`] for the available variants and their semantics.

use alloc::collections::BTreeSet;
use alloc::string::ToString;
use core::fmt;

use miden_protocol::Word;
use miden_protocol::note::NoteScriptRoot;
use miden_standards::note::StandardNote;
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

/// Per-request trust policy controlling which input-note scripts may be included in a
/// user-authorized transaction.
///
/// The policy is checked at the start of [`Client::submit_new_transaction`] and
/// [`Client::execute_transaction`] by validating each input note's script root before
/// executing the requested transaction. A script previously imported into the local store
/// does not bypass the policy.
///
/// This gate applies to user-authorized transaction execution only. Speculative
/// consumability probes, such as [`crate::note::NoteScreener`] during sync or
/// `consume-notes` auto-discovery, may execute scripts but discard their effects and are
/// outside this policy.
///
/// [`Client::submit_new_transaction`]: crate::Client::submit_new_transaction
/// [`Client::execute_transaction`]: crate::Client::execute_transaction
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NoteScriptTrustPolicy {
    /// Only allow scripts that match a known [`StandardNote`] (P2ID, P2IDE, SWAP, MINT, BURN).
    ///
    /// This is the default and rejects any custom or unknown script the executor encounters,
    /// even if it is already in the local store.
    #[default]
    StandardScriptsOnly,
    /// Allow scripts whose root is in the provided set, in addition to standard scripts. Use this
    /// to opt in to specific note scripts the caller has independently verified.
    TrustedScriptRoots(BTreeSet<Word>),
    /// Allow any script root. Equivalent to disabling the trust gate entirely.
    ///
    /// Intended for clients that surface unknown scripts to the user behind their own approval
    /// flow before submitting the transaction.
    AllowUnlistedAfterApproval,
}

impl NoteScriptTrustPolicy {
    /// Returns whether the policy permits a user transaction to include an input note with the
    /// given script root.
    pub fn allows(&self, root: Word) -> bool {
        match self {
            Self::StandardScriptsOnly => is_standard_script(root),
            Self::TrustedScriptRoots(set) => is_standard_script(root) || set.contains(&root),
            Self::AllowUnlistedAfterApproval => true,
        }
    }
}

impl fmt::Display for NoteScriptTrustPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StandardScriptsOnly => f.write_str("StandardScriptsOnly"),
            Self::TrustedScriptRoots(set) => {
                let label = if set.len() == 1 {
                    "trusted root"
                } else {
                    "trusted roots"
                };
                write!(f, "TrustedScriptRoots ({} {label})", set.len())
            },
            Self::AllowUnlistedAfterApproval => f.write_str("AllowUnlistedAfterApproval"),
        }
    }
}

fn is_standard_script(root: Word) -> bool {
    StandardNote::from_script_root(NoteScriptRoot::from_raw(root)).is_some()
}

// SERIALIZATION
// ================================================================================================

impl Serializable for NoteScriptTrustPolicy {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        match self {
            Self::StandardScriptsOnly => target.write_u8(0),
            Self::TrustedScriptRoots(set) => {
                target.write_u8(1);
                let roots: alloc::vec::Vec<Word> = set.iter().copied().collect();
                roots.write_into(target);
            },
            Self::AllowUnlistedAfterApproval => target.write_u8(2),
        }
    }
}

impl Deserializable for NoteScriptTrustPolicy {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        match source.read_u8()? {
            0 => Ok(Self::StandardScriptsOnly),
            1 => {
                let roots = alloc::vec::Vec::<Word>::read_from(source)?;
                Ok(Self::TrustedScriptRoots(roots.into_iter().collect()))
            },
            2 => Ok(Self::AllowUnlistedAfterApproval),
            tag => Err(DeserializationError::InvalidValue(
                ["invalid NoteScriptTrustPolicy tag: ", &tag.to_string()].concat(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standard_root() -> Word {
        // Pick any standard variant; the exact one doesn't matter, only that
        // `StandardNote::from_script_root` recognizes it.
        StandardNote::P2ID.script_root().into()
    }

    fn unknown_root() -> Word {
        // A deterministic non-standard root.
        Word::from([1u32, 2, 3, 4])
    }

    #[test]
    fn default_is_standard_scripts_only() {
        assert_eq!(NoteScriptTrustPolicy::default(), NoteScriptTrustPolicy::StandardScriptsOnly);
    }

    #[test]
    fn standard_scripts_only_accepts_standards_and_rejects_unknown() {
        let policy = NoteScriptTrustPolicy::StandardScriptsOnly;
        assert!(policy.allows(standard_root()));
        assert!(!policy.allows(unknown_root()));
    }

    #[test]
    fn trusted_script_roots_accepts_listed_root_and_standards() {
        let listed = unknown_root();
        let policy = NoteScriptTrustPolicy::TrustedScriptRoots(BTreeSet::from([listed]));
        assert!(policy.allows(listed));
        assert!(policy.allows(standard_root()));

        let other_unknown = Word::from([9u32, 9, 9, 9]);
        assert!(!policy.allows(other_unknown));
    }

    #[test]
    fn allow_unlisted_accepts_anything() {
        let policy = NoteScriptTrustPolicy::AllowUnlistedAfterApproval;
        assert!(policy.allows(standard_root()));
        assert!(policy.allows(unknown_root()));
    }

    fn roundtrip(policy: &NoteScriptTrustPolicy) {
        let mut buffer = alloc::vec::Vec::new();
        policy.write_into(&mut buffer);
        let decoded = NoteScriptTrustPolicy::read_from_bytes(&buffer).unwrap();
        assert_eq!(policy, &decoded);
    }

    #[test]
    fn serialization_roundtrip_standard_scripts_only() {
        roundtrip(&NoteScriptTrustPolicy::StandardScriptsOnly);
    }

    #[test]
    fn serialization_roundtrip_trusted_script_roots() {
        let mut roots = BTreeSet::new();
        roots.insert(unknown_root());
        roots.insert(Word::from([7u32, 7, 7, 7]));
        roundtrip(&NoteScriptTrustPolicy::TrustedScriptRoots(roots));
    }

    #[test]
    fn serialization_roundtrip_trusted_script_roots_empty_set() {
        roundtrip(&NoteScriptTrustPolicy::TrustedScriptRoots(BTreeSet::new()));
    }

    #[test]
    fn serialization_roundtrip_allow_unlisted() {
        roundtrip(&NoteScriptTrustPolicy::AllowUnlistedAfterApproval);
    }
}
