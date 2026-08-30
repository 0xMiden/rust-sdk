# Genesis fixtures generator (testing only)

Generates the genesis fixtures used to bootstrap a testing node for the Miden client integration
tests. This crate is NOT intended for production use.

The testing node itself is run from the standalone Miden node executables (`miden-validator`,
`miden-node`, `miden-ntx-builder`); see `scripts/start-test-node.sh` and the `start-node` /
`stop-node` Make targets. This crate only produces the genesis content those executables consume.

## `gen-genesis`

```bash
gen-genesis [OUTPUT_DIR]   # defaults to ./genesis
gen-genesis --check-env    # applies the fee rules below, writes nothing
```

`--check-env` exists so a caller can reject a malformed
`MIDEN_TEST_NODE_VERIFICATION_BASE_FEE` before doing something it cannot undo;
`scripts/start-test-node.sh` asks before it wipes the previous chain.

Writes, into `OUTPUT_DIR`:

- `tst_faucet.mac` — the TST genesis faucet, written **with** its secret key so tests can mint.
- `test_account_NNNN.mac` — the test faucets and the `too_many_assets` account (read-only
  fixtures, no secret keys).
- `genesis.toml` — references every `.mac` file via `[[account]]` entries, and carries the fee
  parameters described below.

The genesis block is then built with:

```bash
miden-validator genesis --config OUTPUT_DIR/genesis.toml \
    --genesis-block-directory BLOCK_DIR --accounts-directory ACCOUNTS_DIR --validator.key KEY
```

and each component seeds its database from the resulting `genesis.dat` via its own `bootstrap`.

## Fees

`verification_base_fee` defaults to `0`, so fees are never charged and existing tests keep their
exact balances. Setting `MIDEN_TEST_NODE_VERIFICATION_BASE_FEE` to a non-zero value (in the fee
asset's smallest denomination) emits that fee instead, along with two public `[[wallet]]` entries
holding 5,000 MIDEN each:

```bash
MIDEN_TEST_NODE_VERIFICATION_BASE_FEE=500 make start-node
```

Those wallets exist because the generated native MIDEN faucet is a network account, written without
a signing key: minting from it means driving a network transaction rather than signing one locally,
so a wallet holding MIDEN at genesis is the practical way for a test to get hold of the fee asset.
The node also generates an operator account that owns the native faucet, but that is the node's own
account — some node versions leave its vault empty and some pre-fund it so it can pay for the first
mint requests — and this harness neither exports nor spends it either way. The node generates the
funders too, so only it knows their ids, and it writes them with their signing keys to its
`--accounts-directory`; `scripts/start-test-node.sh` copies them into `./data/` as
`wallet_<n>.mac`, one per funder.

Their balance appears in `genesis.toml` as `5000000000`: the node documents `amount` as full token
units but passes it on unscaled, so the manifest has to carry the smallest denomination, and the
native faucet has 6 decimals.

Leaving the variable unset is the only way to ask for a fee-free chain by omission. A value that is
present but not a `u32`, the empty string included, aborts genesis generation rather than falling
back to `0`, so a typo cannot quietly bring up a fee-free chain that a fee test then passes against.
`AGGLAYER_GENESIS` below takes the opposite convention — any value, empty included, turns it on.

The switch is not enough on its own to make the integration suite pass against a fee-charging node:

- accounts referenced by `[[account]]`, the TST faucet among them, are loaded verbatim and hold no
  MIDEN, so the harness has to get MIDEN to them from a funder. That need not be a separate funding
  transaction: note scripts run before authentication, so a consumed note can deliver the fee the
  same transaction goes on to pay, which `miden-client-tests`' `fees.rs` covers;
- the agglayer bridge and faucet are network accounts, which pay the fee from their own vaults.
  Those vaults are funded per transaction rather than up front: the single asset a
  `FeeSponsorshipNote` carries is moved into the vault as the account collects fees for the feature
  note that sponsorship names, and both accounts allowlist that script by default. But these
  fixtures are built with `miden_agglayer::testing::zero_fee_policy_manager`, which zeroes the price
  of every note they allowlist and sets the fee asset to the mock-chain fee faucet rather than the
  generated MIDEN one. The verification fee is charged independently of that pricing, so a MIDEN
  sponsorship is rejected as the wrong asset. Nothing rewrites that asset id in place — it is fixed
  when the policy manager is built — so pointing these fixtures at the chain's own fee faucet means
  rebuilding them once genesis has assigned its id. `network_transaction.rs` already builds its own
  network accounts that way, reading `genesis.fee_parameters().fee_faucet_id()` first; the two
  places that rebuild these agglayer fixtures at runtime, `agglayer/mod.rs` and
  `agglayer_bridge_in_out.rs`, still pass the mock-faucet helper. No test helper builds a
  sponsorship note either way;
- a signature-authenticated account only transacts on a fee-charging chain when its request commits
  fee conversion info (`TransactionRequestBuilder::fee_conversion_info`), which neither the shared
  test helpers nor `bin/miden-bench` yet do — and CI runs the bench smoke tests against this node
  right after `make start-node-background`;
- the CLI integration tests drive `miden-client` as a subprocess, and the CLI exposes no way to
  attach fee conversion info at all, so those need a change in `bin/miden-cli` rather than in a
  test helper.

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
as `.mac` files. `genesis.toml` is a thin manifest the node's own `miden-validator genesis`
consumes, so this crate stays decoupled from the node's internal crates.

## License

This project is [MIT licensed](../../../LICENSE).
