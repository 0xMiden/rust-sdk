#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]

use std::fmt::Write;

use miden_client::Felt;
use rand::{Rng, RngExt};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;

/// Describes a storage slot for reader procedure generation.
///
/// Used to generate MASM reader component code that provides procedures
/// to read from both value and map storage slots.
#[derive(Clone, Debug)]
pub struct SlotDescriptor {
    /// The full slot name (e.g., `miden::bench::map_slot_0`)
    pub name: String,
    /// Whether this is a map slot (`true`) or value slot (`false`)
    pub is_map: bool,
}

/// Generates MASM code for a storage reader component with procedures for each slot.
///
/// For map slots: generates `get_map_item_slot_N` (reads a key-value entry).
/// For value slots: generates `get_value_slot_N` (reads the slot value directly).
///
/// These procedures must be called from within account context (via `call` from a
/// transaction script), because the kernel verifies the caller is an account procedure.
pub fn generate_reader_component_code(slots: &[SlotDescriptor]) -> String {
    let mut code = String::new();

    for (i, slot) in slots.iter().enumerate() {
        let slot_name = &slot.name;
        if slot.is_map {
            write!(
                code,
                r#"const MAP_SLOT_{i} = word("{slot_name}")

# Reads an item from map storage slot {i}.
# Stack input: [KEY]
# Stack output: [VALUE]
@account_procedure
pub proc get_map_item_slot_{i}
    push.MAP_SLOT_{i}[0..2]
    exec.::miden::protocol::active_account::get_map_item
end

"#
            )
            .expect("writing to String should not fail");
        } else {
            write!(
                code,
                r#"const SLOT_{i} = word("{slot_name}")

# Reads the value from storage slot {i}.
# Stack input: []
# Stack output: [VALUE]
@account_procedure
pub proc get_value_slot_{i}
    push.SLOT_{i}[0..2]
    exec.::miden::protocol::active_account::get_item
end

"#
            )
            .expect("writing to String should not fail");
        }
    }

    code
}

/// Generates a random non-zero `[Felt; 4]` value suitable for storage map entries.
///
/// Values must be non-zero because the SMT treats zero values as deletions.
/// The probability of generating an all-zero word is astronomically small (~2^-256),
/// but we guard against it for correctness.
pub fn random_word(rng: &mut impl Rng) -> [Felt; 4] {
    loop {
        let word: [Felt; 4] =
            std::array::from_fn(|_| Felt::new_unchecked(rng.random::<u64>() >> 1));
        if word.iter().any(|f| f.as_canonical_u64() != 0) {
            return word;
        }
    }
}

/// Creates an RNG seeded from a slot index, for deterministic random value generation.
pub fn slot_rng(seed: u32) -> ChaCha20Rng {
    let mut rng_seed = [0u8; 32];
    rng_seed[0..4].copy_from_slice(&seed.to_le_bytes());
    ChaCha20Rng::from_seed(rng_seed)
}

#[cfg(test)]
mod tests {
    use miden_client::account::component::{
        AccountComponent,
        AccountComponentMetadata,
        BasicWallet,
    };
    use miden_client::account::{
        Account,
        AccountBuilder,
        AccountBuilderSchemaCommitmentExt,
        AccountType,
        StorageMap,
        StorageMapKey,
        StorageSlot,
        StorageSlotName,
    };
    use miden_client::assembly::CodeBuilder;
    use miden_client::auth::{Approver, AuthSchemeId, AuthSecretKey, AuthSingleSig};

    use super::*;

    /// Configuration for generating large accounts.
    #[derive(Clone, Debug)]
    struct LargeAccountConfig {
        num_storage_map_entries: usize,
        num_map_slots: usize,
        seed: [u8; 32],
    }

    impl LargeAccountConfig {
        fn with_seed(maps: usize, entries_per_map: usize, seed: [u8; 32]) -> Self {
            Self {
                num_storage_map_entries: entries_per_map,
                num_map_slots: maps,
                seed,
            }
        }
    }

    /// Creates a large account with the specified configuration.
    fn create_large_account(
        config: &LargeAccountConfig,
    ) -> anyhow::Result<(Account, AuthSecretKey)> {
        let sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(&mut ChaCha20Rng::from_seed(
            config.seed,
        ));

        // Create storage map slots
        let mut storage_slots = Vec::new();
        for i in 0..config.num_map_slots {
            let slot_name = format!("miden::bench::map_slot_{i}");
            storage_slots.push(create_large_storage_slot(
                slot_name.as_str(),
                config.num_storage_map_entries,
                i as u32,
            ));
        }

        // Reader component: owns storage slots and provides get_map_item_slot_N procedures
        let descriptors: Vec<SlotDescriptor> = (0..config.num_map_slots)
            .map(|i| SlotDescriptor {
                name: format!("miden::bench::map_slot_{i}"),
                is_map: true,
            })
            .collect();
        let reader_code = generate_reader_component_code(&descriptors);
        let reader_component_code = CodeBuilder::default()
            .compile_component_code("miden::bench::storage_reader", &reader_code)
            .map_err(|e| anyhow::anyhow!("Failed to compile reader component: {e}"))?;
        let reader_component = AccountComponent::new(
            reader_component_code,
            storage_slots,
            AccountComponentMetadata::new("miden::testing::storage_reader"),
        )
        .map_err(|e| anyhow::anyhow!("Failed to create reader component: {e}"))?;

        // Wallet component: provides standard wallet operations (no storage slots)
        let wallet_component = AccountComponent::new(
            BasicWallet::code().as_library().clone(),
            vec![],
            BasicWallet::component_metadata(),
        )
        .expect("basic wallet component should satisfy account component requirements");

        let account = AccountBuilder::new(config.seed)
            .with_auth_component(AuthSingleSig::new(Approver::new(
                sk.public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            )))
            .account_type(AccountType::Public)
            .with_component(wallet_component)
            .with_component(reader_component)
            .build_with_schema_commitment()?;

        Ok((account, sk))
    }

    /// Creates a storage slot with many map entries.
    fn create_large_storage_slot(name: &str, num_entries: usize, seed: u32) -> StorageSlot {
        let mut rng = slot_rng(seed);

        let map_entries: Vec<_> = (0..num_entries as u32)
            .map(|i| {
                let key_val = seed.wrapping_mul(1000).wrapping_add(i);
                let key = [Felt::new_unchecked(key_val as u64); 4];
                let value = random_word(&mut rng);
                (StorageMapKey::new(key.into()), value.into())
            })
            .collect();

        StorageSlot::with_map(
            StorageSlotName::new(name).expect("slot name should be valid"),
            StorageMap::with_entries(map_entries).expect("map entries should be valid"),
        )
    }

    #[test]
    fn test_create_small_account() {
        let config = LargeAccountConfig::with_seed(1, 10, [0x01; 32]);
        let result = create_large_account(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_large_storage_slot() {
        let slot = create_large_storage_slot("test::slot", 100, 0);
        assert!(matches!(slot.slot_type(), miden_client::account::StorageSlotType::Map));
    }
}
