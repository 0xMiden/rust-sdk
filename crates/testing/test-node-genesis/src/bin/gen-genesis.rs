//! Generates the genesis fixtures (`.mac` account files + `genesis.toml`) used to bootstrap a
//! testing node from the standalone node executables.
//!
//! Usage: `gen-genesis [OUTPUT_DIR]` (defaults to `./genesis`).
//!
//! Setting the `AGGLAYER_GENESIS` env var additionally emits the agglayer genesis accounts
//! (bridge admin, GER manager, bridge, and faucet).
//!
//! Setting `MIDEN_VERIFICATION_BASE_FEE` to a non-zero value makes the chain charge fees: every
//! transaction then pays out of its own account vault, denominated in the node-generated `MIDEN`
//! native faucet's asset. Defaults to `0` (fee-free chain).

use std::path::PathBuf;

use anyhow::Context;

/// Env var overriding the genesis `verification_base_fee`.
const VERIFICATION_BASE_FEE_ENV: &str = "MIDEN_VERIFICATION_BASE_FEE";

fn main() -> anyhow::Result<()> {
    let output_dir = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("./genesis"), PathBuf::from);

    let include_agglayer = std::env::var("AGGLAYER_GENESIS").is_ok();
    if include_agglayer {
        println!("Agglayer genesis accounts enabled");
    }

    let verification_base_fee = match std::env::var(VERIFICATION_BASE_FEE_ENV) {
        Ok(value) => value
            .trim()
            .parse::<u32>()
            .with_context(|| format!("{VERIFICATION_BASE_FEE_ENV} must be a u32, got {value:?}"))?,
        Err(_) => 0,
    };
    if verification_base_fee != 0 {
        println!("Fees enabled: verification_base_fee = {verification_base_fee}");
    }

    test_node_genesis::write_genesis_config(&output_dir, include_agglayer, verification_base_fee)?;
    println!("Wrote genesis config to {}", output_dir.display());

    Ok(())
}
