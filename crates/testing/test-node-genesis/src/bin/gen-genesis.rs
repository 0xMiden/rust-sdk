//! Generates the genesis fixtures (`.mac` account files + `accounts.toml`) that `miden-validator
//! genesis` builds a testing node's genesis block from.
//!
//! Usage: `gen-genesis [OUTPUT_DIR]` (defaults to `./genesis`).
//!
//! The chain charges fees in the `MIDEN` native faucet's asset: every transaction pays out of its
//! own account vault. The funding account holds the asset the node's funding service hands out. The
//! fee itself is not a fixture: `start-test-node.sh` passes it to `miden-validator genesis`.

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let output_dir = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("./genesis"), PathBuf::from);

    test_node_genesis::write_genesis_config(&output_dir)?;
    println!("Wrote genesis fixtures to {}", output_dir.display());

    Ok(())
}
