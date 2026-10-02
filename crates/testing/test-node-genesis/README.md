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

- `native_faucet.mac`: the native fee faucet, a network account owned by the operator below (no
  secret key of its own).
- `faucet_operator.mac`: the wallet owning the native faucet, written **with** its secret key. It
  is what `miden-faucet init --import` takes to run a faucet dispensing the native asset.
- `funding_account.mac`: the public funding account `miden-validator genesis` requires, written
  **with** its secret key. The node's funding service pays registered accounts out of it.
- `tst_faucet.mac`: the TST genesis faucet, written **with** its secret key so tests can mint.
- `test_account_NNNN.mac`: the test faucets and the `too_many_assets` account (read-only
  fixtures, no secret keys).
- `accounts.toml`: references the accounts above (except the native faucet and the funding
  account) via named `[[account]]` entries and declares the funder wallets.

The genesis block is then built with:

```bash
miden-validator genesis \
  --native-faucet OUTPUT_DIR/native_faucet.mac \
  --funding-account OUTPUT_DIR/funding_account.mac \
  --accounts-config OUTPUT_DIR/accounts.toml \
  --verification-base-fee <FEE> --timestamp <UNIX_SECONDS> ...
```

The fee and the timestamp are genesis parameters, not fixtures, so `start-test-node.sh` passes
them on the command line.

## Fees and funding

Every transaction settles its fee out of the vault of the account it runs against, so the native
faucet is generated here rather than by the node: its ID has to be known while the other accounts
are built, or their vaults could not reference it. `MIDEN_VERIFICATION_BASE_FEE` overrides the base
fee (`0` gives a fee-free chain) and `MIDEN_NUM_FUNDER_WALLETS` how many funders are declared.

Seeded with the native asset: the `[[wallet]]` funders, which the node writes as
`wallet_<index>.mac` and `start-test-node.sh` copies to `./data/funders/`, the funding account,
and every genesis account that transacts, which nothing can top up afterwards.

## AggLayer genesis

The pre-deployed AggLayer accounts are always emitted:

- `bridge_admin.mac`: bridge admin wallet (with secret key)
- `ger_manager.mac`: GER manager wallet (with secret key)
- `bridge.mac`: AggLayer bridge account (unconfigured, configured at test time)
- `agglayer_faucet.mac`: AggLayer faucet (token symbol "AGG")

`start-test-node.sh` copies these into `./data/`, where the tests load them via
`AGGLAYER_ACCOUNTS_DIR`. Genesis always carries them because the bridge and faucet are network
accounts, which no client transaction can deploy.

## Why a TOML manifest

The accounts are built in Rust (depending only on `miden-protocol` / `miden-standards`) and emitted
as `.mac` files. `accounts.toml` is a thin manifest the node's own `miden-validator genesis`
consumes, so this crate stays decoupled from the node's internal crates.

## License

This project is [MIT licensed](../../../LICENSE).
