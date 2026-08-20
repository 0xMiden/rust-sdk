# Genesis fixtures generator (testing only)

Generates the genesis fixtures used to bootstrap a testing node for the Miden client integration
tests. This crate is NOT intended for production use.

The testing node itself is run from the standalone Miden node executables (`miden-validator`,
`miden-node`, `miden-ntx-builder`); see `scripts/start-test-node.sh` and the `start-node` /
`stop-node` Make targets. This crate only produces the genesis content those executables consume.

## `gen-genesis`

```bash
gen-genesis [OUTPUT_DIR]   # defaults to ./genesis
```

Writes, into `OUTPUT_DIR`:

- `tst_faucet.mac` — the TST genesis faucet, written **with** its secret key so tests can mint.
- `test_account_NNNN.mac` — the test faucets and the `too_many_assets` account (read-only
  fixtures, no secret keys).
- `genesis.toml` — references every `.mac` file via `[[account]]` entries, with
  `verification_base_fee = 0`.

The node is then bootstrapped with:

```bash
miden-validator bootstrap --genesis-config-file OUTPUT_DIR/genesis.toml ...
```

## AggLayer genesis

Setting the `AGGLAYER_GENESIS` env var additionally emits the pre-deployed AggLayer accounts:

- `bridge_admin.mac` — bridge admin wallet (with secret key)
- `ger_manager.mac` — GER manager wallet (with secret key)
- `bridge.mac` — AggLayer bridge account (unconfigured; configured at test time)
- `agglayer_faucet.mac` — AggLayer faucet (token symbol "AGG")

The `start-node-agglayer` Make target starts the node this way, and
`scripts/start-test-node.sh` copies the files into `./data/` so the integration tests can load
them via `AGGLAYER_ACCOUNTS_DIR=./data`.

## Why a TOML manifest

The accounts are built in Rust (depending only on `miden-protocol` / `miden-standards`) and emitted
as `.mac` files. `genesis.toml` is a thin manifest the node's own `miden-validator bootstrap`
consumes, so this crate stays decoupled from the node's internal crates.

## License

This project is [MIT licensed](../../../LICENSE).
