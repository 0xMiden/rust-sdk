//! Generates the genesis fixtures (`.mac` account files + `genesis.toml`) used to bootstrap a
//! testing node from the standalone node executables.
//!
//! Usage: `gen-genesis [OUTPUT_DIR]` (defaults to `./genesis`), or `gen-genesis --check-env` to
//! apply the `MIDEN_TEST_NODE_VERIFICATION_BASE_FEE` rules and exit without writing anything.
//!
//! Setting the `AGGLAYER_GENESIS` env var additionally emits the agglayer genesis accounts
//! (bridge admin, GER manager, bridge, and faucet).
//!
//! Setting `MIDEN_TEST_NODE_VERIFICATION_BASE_FEE` to a non-zero value makes the generated chain
//! charge that fee and declares MIDEN-funded funder wallets in the config. No wallet file is
//! written here: the node creates those accounts while building genesis and writes them to its own
//! accounts directory. Unset or `0` leaves fees off; anything else that is not a `u32` aborts
//! rather than falling back.

use std::ffi::OsStr;
use std::path::PathBuf;

use anyhow::bail;
use test_node_genesis::{FUNDER_COUNT, verification_base_fee_from_env};

const USAGE: &str = "usage: gen-genesis [OUTPUT_DIR]   (defaults to ./genesis)\n\
                     \x20      gen-genesis --check-env";

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let arg = args.next();
    if let Some(extra) = args.next() {
        bail!("unexpected argument \"{}\"\n{USAGE}", extra.display());
    }

    // Applies the fee rules without writing anything, so a caller can reject a malformed value
    // before doing something it cannot undo. `scripts/start-test-node.sh` asks first, because its
    // next step wipes the previous chain.
    if arg.as_deref() == Some(OsStr::new("--check-env")) {
        verification_base_fee_from_env()?;
        return Ok(());
    }

    // Every other argument names a directory to create, so a near-miss of `--check-env` must not
    // become one: it would write a faucet secret key into a directory named after the typo. A
    // directory whose name really does start with a dash is still reachable as `./-name`.
    match &arg {
        Some(arg) if arg.as_encoded_bytes().starts_with(b"-") => {
            bail!("unrecognized flag \"{}\"\n{USAGE}", arg.display())
        },
        Some(arg) if arg.is_empty() => bail!("OUTPUT_DIR is empty\n{USAGE}"),
        _ => {},
    }

    let output_dir = arg.map_or_else(|| PathBuf::from("./genesis"), PathBuf::from);

    let include_agglayer = std::env::var("AGGLAYER_GENESIS").is_ok();
    if include_agglayer {
        println!("Agglayer genesis accounts enabled");
    }

    let verification_base_fee = verification_base_fee_from_env()?;

    test_node_genesis::write_genesis_config(&output_dir, include_agglayer, verification_base_fee)?;

    // Printed when off too: someone who asked for a fee and sees "Fees off" knows the value never
    // reached this process.
    match verification_base_fee {
        Some(base_fee) => println!(
            "Fees on: verification_base_fee = {base_fee}, \
             {FUNDER_COUNT} MIDEN-funded funder wallets declared"
        ),
        None => println!("Fees off: verification_base_fee = 0"),
    }
    println!("Wrote genesis config to {}", output_dir.display());

    Ok(())
}
