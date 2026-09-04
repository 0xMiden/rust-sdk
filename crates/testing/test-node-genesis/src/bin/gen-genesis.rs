//! Generates the genesis fixtures (`.mac` account files + `genesis.toml`) used to bootstrap a
//! testing node from the standalone node executables.
//!
//! Usage: `gen-genesis [OUTPUT_DIR]` (defaults to `./genesis`).
//!
//! The chain charges fees by default: every transaction pays out of its own account vault,
//! denominated in the `MIDEN` native faucet's asset, and genesis declares the funder wallets the
//! integration tests draw that asset from. `MIDEN_VERIFICATION_BASE_FEE` overrides the base fee
//! (`0` gives a fee-free chain, which declares no funder wallets) and `MIDEN_NUM_FUNDER_WALLETS`
//! overrides how many funders are declared.

use std::path::PathBuf;

use anyhow::Context;
use test_node_genesis::DEFAULT_NUM_FUNDER_WALLETS;

/// Env var overriding the genesis `verification_base_fee`.
const VERIFICATION_BASE_FEE_ENV: &str = "MIDEN_VERIFICATION_BASE_FEE";

/// Genesis `verification_base_fee` when the env var is unset. Matches the base fee the protocol's
/// own fee tests use, and is large enough that the computed fee is never zero.
const DEFAULT_VERIFICATION_BASE_FEE: u32 = 500;

/// Env var overriding the number of funder wallets emitted by a fee-charging genesis.
const NUM_FUNDER_WALLETS_ENV: &str = "MIDEN_NUM_FUNDER_WALLETS";

fn main() -> anyhow::Result<()> {
    let output_dir = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("./genesis"), PathBuf::from);

    let verification_base_fee =
        parse_env(VERIFICATION_BASE_FEE_ENV)?.unwrap_or(DEFAULT_VERIFICATION_BASE_FEE);

    // Funders exist only to hand the native asset to accounts created at test time, so a fee-free
    // chain declares none.
    let num_funder_wallets = if verification_base_fee == 0 {
        0
    } else {
        parse_env(NUM_FUNDER_WALLETS_ENV)?.unwrap_or(DEFAULT_NUM_FUNDER_WALLETS)
    };
    println!(
        "verification_base_fee = {verification_base_fee}, funder wallets = {num_funder_wallets}"
    );

    test_node_genesis::write_genesis_config(
        &output_dir,
        verification_base_fee,
        num_funder_wallets,
    )?;
    println!("Wrote genesis config to {}", output_dir.display());

    Ok(())
}

/// Reads `name` from the environment and parses it as a `u32`, returning `None` when it is unset.
fn parse_env(name: &str) -> anyhow::Result<Option<u32>> {
    match std::env::var(name) {
        Ok(value) => value
            .trim()
            .parse::<u32>()
            .map(Some)
            .with_context(|| format!("{name} must be a u32, got {value:?}")),
        Err(_) => Ok(None),
    }
}
