extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use anyhow::{Context, Result, ensure};
use miden_agglayer::{
    ExitRoot,
    GlobalIndex,
    Keccak256Output,
    LeafData,
    MetadataHash,
    ProofData,
    SmtNode,
};
use miden_client::agglayer::{EthAddress, EthAmount, EthEmbeddedAccountId};
use miden_client::utils::hex_to_bytes;
use miden_protocol::account::AccountId;
use serde::Deserialize;

// SERDE HELPERS
// ================================================================================================

/// Deserializes a JSON value that may be either a number or a string into a `String`.
///
/// Foundry's `vm.serializeUint` outputs JSON numbers for uint256 values.
/// This deserializer accepts both `"100"` (string) and `100` (number) forms.
fn deserialize_uint_to_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(serde::de::Error::custom("expected a number or string for amount")),
    }
}

// TEST VECTOR TYPES
// ================================================================================================

/// Deserialized leaf value test vector from Solidity-generated JSON.
#[derive(Debug, Deserialize)]
pub struct LeafValueVector {
    pub origin_network: u32,
    pub origin_token_address: String,
    pub destination_network: u32,
    pub destination_address: String,
    #[serde(deserialize_with = "deserialize_uint_to_string")]
    pub amount: String,
    pub metadata_hash: String,
    #[allow(dead_code)]
    pub leaf_value: String,
}

impl LeafValueVector {
    /// Converts this test vector into a `LeafData` instance.
    pub fn to_leaf_data(&self) -> LeafData {
        LeafData {
            origin_network: self.origin_network,
            origin_token_address: EthAddress::from_hex(&self.origin_token_address)
                .expect("valid origin token address hex"),
            destination_network: self.destination_network,
            destination_address: EthAddress::from_hex(&self.destination_address)
                .expect("valid destination address hex"),
            amount: EthAmount::from_uint_str(&self.amount).expect("valid amount uint string"),
            metadata_hash: MetadataHash::new(
                hex_to_bytes(&self.metadata_hash).expect("valid metadata hash hex"),
            ),
        }
    }
}

/// Deserialized proof value test vector from Solidity-generated JSON.
/// Contains SMT proofs, exit roots, global index, and expected global exit root.
#[derive(Debug, Deserialize)]
pub struct ProofValueVector {
    pub smt_proof_local_exit_root: Vec<String>,
    pub smt_proof_rollup_exit_root: Vec<String>,
    pub global_index: String,
    pub mainnet_exit_root: String,
    pub rollup_exit_root: String,
    /// Expected global exit root: keccak256(mainnetExitRoot || rollupExitRoot)
    #[allow(dead_code)]
    pub global_exit_root: String,
}

impl ProofValueVector {
    /// Converts this test vector into a `ProofData` instance.
    pub fn to_proof_data(&self) -> ProofData {
        let smt_proof_local: [SmtNode; 32] = self
            .smt_proof_local_exit_root
            .iter()
            .map(|s| SmtNode::new(hex_to_bytes(s).expect("valid smt proof hex")))
            .collect::<Vec<_>>()
            .try_into()
            .expect("expected 32 SMT proof nodes for local exit root");

        let smt_proof_rollup: [SmtNode; 32] = self
            .smt_proof_rollup_exit_root
            .iter()
            .map(|s| SmtNode::new(hex_to_bytes(s).expect("valid smt proof hex")))
            .collect::<Vec<_>>()
            .try_into()
            .expect("expected 32 SMT proof nodes for rollup exit root");

        ProofData {
            smt_proof_local_exit_root: smt_proof_local,
            smt_proof_rollup_exit_root: smt_proof_rollup,
            global_index: GlobalIndex::from_hex(&self.global_index)
                .expect("valid global index hex"),
            mainnet_exit_root: Keccak256Output::new(
                hex_to_bytes(&self.mainnet_exit_root).expect("valid mainnet exit root hex"),
            ),
            rollup_exit_root: Keccak256Output::new(
                hex_to_bytes(&self.rollup_exit_root).expect("valid rollup exit root hex"),
            ),
        }
    }
}

/// Deserialized claim asset test vector from Solidity-generated JSON.
/// Contains both LeafData and ProofData from a real claimAsset transaction.
#[derive(Debug, Deserialize)]
pub struct ClaimAssetVector {
    #[serde(flatten)]
    pub proof: ProofValueVector,

    #[serde(flatten)]
    pub leaf: LeafValueVector,
}

// FOUNDRY TEST VECTOR GENERATION
// ================================================================================================

/// Maximum random deposit offset to keep foundry execution fast.
/// Each offset adds one hash operation in the Solidity test.
const MAX_DEPOSIT_OFFSET: u32 = 1000;

/// Path to the foundry project directory, relative to the crate's manifest directory.
const FOUNDRY_PROJECT_SUBDIR: &str = "foundry-vectors";

/// Path to the generated test vectors JSON file within the foundry project.
const FOUNDRY_OUTPUT_JSON: &str = "test-vectors/claim_asset_vectors_local_tx.json";

/// Runs the foundry test to generate claim asset test vectors for a given destination account.
///
/// This function:
/// 1. Converts the `AccountId` to an Ethereum address format (0x-prefixed hex)
/// 2. Invokes `forge test` with the `DESTINATION_ADDRESS` environment variable
/// 3. Optionally passes `ORIGIN_TOKEN_ADDRESS` if provided (e.g. from a genesis faucet)
/// 4. Reads and parses the generated JSON file
/// 5. Returns the `(ProofData, LeafData, ExitRoot)` tuple
pub fn generate_claim_data_for_account(
    account_id: AccountId,
    origin_token_address: Option<&EthAddress>,
) -> Result<(ProofData, LeafData, ExitRoot)> {
    let destination_address: EthAddress = EthEmbeddedAccountId::from_account_id(account_id).into();
    let destination_hex = destination_address.to_hex();
    println!(
        "[foundry] Generating claim data for account {:?} (eth address: {})",
        account_id, destination_hex
    );

    // Determine the foundry project directory using CARGO_MANIFEST_DIR.
    // This ensures the path is correct regardless of the test binary's working directory.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let foundry_dir = std::path::Path::new(manifest_dir).join(FOUNDRY_PROJECT_SUBDIR);
    ensure!(
        foundry_dir.join("foundry.toml").exists(),
        "Foundry project not found at {}. Run `forge install` in that directory first.",
        foundry_dir.display()
    );

    // Ensure the test-vectors output directory exists (it is gitignored so may not
    // be present in a fresh checkout).
    let output_dir = foundry_dir.join("test-vectors");
    std::fs::create_dir_all(&output_dir).with_context(|| {
        format!("failed to create test-vectors directory at {}", output_dir.display())
    })?;

    // Generate a random deposit offset so each test run gets a unique leaf_index.
    // This prevents the bridge from rejecting the CLAIM as already spent when
    // running the test multiple times against the same node instance.
    let deposit_offset: u32 = rand::random::<u32>() % MAX_DEPOSIT_OFFSET;
    println!("[foundry] Using deposit offset: {}", deposit_offset);

    // Run forge test with the destination address as an environment variable
    let mut cmd = std::process::Command::new("forge");
    cmd.arg("test")
        .arg("-vv")
        .arg("--match-contract")
        .arg("ClaimAssetTestVectorsLocalTx")
        .env("DESTINATION_ADDRESS", &destination_hex)
        .env("DEPOSIT_OFFSET", deposit_offset.to_string())
        .current_dir(&foundry_dir);

    if let Some(addr) = origin_token_address {
        let addr_hex = addr.to_hex();
        println!("[foundry] Using origin token address: {}", addr_hex);
        cmd.env("ORIGIN_TOKEN_ADDRESS", &addr_hex);
    }

    let output = cmd.output().context("failed to execute `forge test` - is foundry installed?")?;

    ensure!(
        output.status.success(),
        "forge test failed!\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    println!(
        "[foundry] forge test completed successfully:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Read and parse the generated JSON
    let json_path = foundry_dir.join(FOUNDRY_OUTPUT_JSON);
    let json_content = std::fs::read_to_string(&json_path).with_context(|| {
        format!("failed to read generated test vectors from {}", json_path.display())
    })?;

    let vector: ClaimAssetVector = serde_json::from_str(&json_content)
        .context("failed to parse foundry-generated claim asset vectors JSON")?;

    let ger = ExitRoot::new(
        hex_to_bytes(&vector.proof.global_exit_root).context("invalid global exit root hex")?,
    );

    println!(
        "[foundry] Claim data generated successfully for destination: {}",
        destination_hex
    );

    Ok((vector.proof.to_proof_data(), vector.leaf.to_leaf_data(), ger))
}
