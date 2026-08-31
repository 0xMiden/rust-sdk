# Miden Client Integration Tests

This directory contains integration tests for the Miden client library. These tests verify the functionality of the client against a running Miden node.

## Features

- **Parallel Execution**: Run tests in parallel to significantly reduce total execution time
- **Test Filtering**: Filter tests by name patterns, categories, or exclude specific tests
- **Flexible Configuration**: Configurable RPC endpoints, timeouts, and parallel job counts
- **Comprehensive Reporting**: Detailed test results with timing statistics and progress tracking
- **cargo-nextest-like Experience**: Similar filtering and execution patterns as cargo-nextest

## Installation

To install the integration tests binary:

```bash
make install-tests
```

This will build and install the `miden-client-integration-tests` binary to your system.

## Usage

### Running the Binary

The integration tests binary can be run with various command-line options:

```bash
miden-client-integration-tests [OPTIONS]
```

### Command-Line Options

- `-n, --network <NETWORK>` - Network preset: `devnet`, `testnet`, `localhost`, or a custom RPC endpoint (default: `localhost`). Sets defaults for all components (RPC, prover, note transport)
- `-t, --timeout <MILLISECONDS>` - Timeout for RPC requests in milliseconds (default: `10000`)
- `--prover-url <URL>` - Override prover endpoint. Accepts `devnet`, `testnet`, `localhost`, or a custom URL. If unset, defaults based on network
- `--note-transport-url <URL>` - Override note transport endpoint. Accepts `devnet`, `testnet`, or a custom URL. If unset, defaults based on network
- `-j, --jobs <NUMBER>` - Number of tests to run in parallel (default: auto-detected CPU cores, set to `1` for sequential execution)
- `-f, --filter <REGEX>` - Filter tests by name using regex patterns
- `--contains <STRING>` - Only run tests whose names contain this substring
- `--exclude <REGEX>` - Exclude tests whose names match this regex pattern
- `--retry-count <NUMBER>` - Number of times to retry failed tests (default: `3`, set to `0` to disable retries)
- `--list` - List all available tests without running them
- `-h, --help` - Show help information
- `-V, --version` - Show version information

### Examples

Run all tests with default settings (auto-detected CPU cores):
```bash
miden-client-integration-tests
```

Run tests sequentially (no parallelism):
```bash
miden-client-integration-tests --jobs 1
```

Run tests with custom parallelism:
```bash
miden-client-integration-tests --jobs 8
```

List all available tests without running them:
```bash
miden-client-integration-tests --list
```

Run only client-related tests:
```bash
miden-client-integration-tests --filter "client"
```

Run tests containing "fpi" in their name:
```bash
miden-client-integration-tests --contains "fpi"
```

Exclude swap-related tests:
```bash
miden-client-integration-tests --exclude "swap"
```

Run tests against devnet:
```bash
miden-client-integration-tests --network devnet
```

Run tests against testnet:
```bash
miden-client-integration-tests --network testnet
```

Run tests against devnet (auto-configures remote prover):
```bash
miden-client-integration-tests --network devnet
```

Run tests against testnet with a local prover override:
```bash
miden-client-integration-tests --network testnet --prover-url localhost
```

Run tests against a custom RPC endpoint with timeout:
```bash
miden-client-integration-tests --network http://192.168.1.100:57291 --timeout 30000
```

Complex example: Run non-swap tests in parallel excluding swap tests:
```bash
miden-client-integration-tests --exclude "swap"
```

Show help:
```bash
miden-client-integration-tests --help
```

## Environment Variables

The following environment variables configure both the standalone binary and the `cargo test` generated wrappers:

- `TEST_MIDEN_NETWORK` - Network preset: `devnet`, `testnet`, `localhost`, or a custom RPC endpoint URL (default: `localhost`). Sets defaults for **all** components
- `TEST_MIDEN_RPC_URL` - Overrides the RPC endpoint from the network preset
- `TEST_MIDEN_PROVER_URL` - Overrides the prover: `devnet`, `testnet`, `localhost`, or a custom URL (default: derived from network)
- `TEST_MIDEN_NOTE_TRANSPORT_URL` - Overrides note transport: `devnet`, `testnet`, or a custom URL (default: derived from network)
- `MIDEN_TEST_TIMEOUT` - Test timeout in milliseconds (default: `10000`)

### Network Presets

| Network | RPC | Prover | Note Transport |
|---------|-----|--------|----------------|
| `testnet` | `rpc.testnet.miden.io` | `tx-prover.testnet.miden.io` | `transport.miden.io` |
| `devnet` | `rpc.devnet.miden.io` | `tx-prover.devnet.miden.io` | `transport.devnet.miden.io` |
| `localhost` | `localhost:57291` | localhost | *(none)* |

Any individual env var overrides the corresponding component from the preset. For example:

```bash
# Use testnet defaults but force local prover
TEST_MIDEN_NETWORK=testnet TEST_MIDEN_PROVER_URL=localhost cargo test

# Use devnet RPC with a custom note transport
TEST_MIDEN_NETWORK=devnet TEST_MIDEN_NOTE_TRANSPORT_URL=http://localhost:57292 cargo test
```

For the standalone binary, CLI flags (`--network`, `--prover-url`, `--note-transport-url`, `--timeout`) take precedence over environment variables.

## Test Categories

The integration tests cover several categories:

- **Client**: Basic client functionality, account management, and note handling
- **Custom Transaction**: Custom transaction types and Merkle store operations
- **FPI**: Foreign Procedure Interface tests
- **Network Transaction**: Network-level transaction processing
- **Onchain**: On-chain account and note operations
- **Swap Transaction**: Asset swap functionality
- **AggLayer**: AggLayer bridge integration (GER updates, bridge-in/out)

## AggLayer Tests

AggLayer tests verify the bridge integration flow: GER updates, faucet registration, bridge-in (claiming), and bridge-out.

### Pre-deployed accounts

The four AggLayer accounts are always supplied rather than created by the tests: they are network
accounts, which no client transaction can deploy, and on a fee-charging chain they must be seeded
with the fee asset because no note in their allowlist can carry it to them later.

`scripts/start-test-node.sh` writes them into `./data/`:

- `bridge_admin.mac` - Bridge admin wallet (includes secret key)
- `ger_manager.mac` - GER manager wallet (includes secret key)
- `bridge.mac` - AggLayer bridge account (no secret key, network account)
- `agglayer_faucet.mac` - AggLayer faucet account (no secret key, network account)

The bridge is deployed unconfigured. The tests register the faucet against it with a
`CONFIG_AGG_BRIDGE` note, using a deterministic test origin token address (`0xAAAA...AA`).

### Environment variables

- `AGGLAYER_ACCOUNTS_DIR` - Directory holding the AggLayer `.mac` account files. Required by the AggLayer tests. The Make targets point it at `./data`, where the testing node writes them. On devnet, point it at wherever the devnet account files are stored.

```bash
make start-node-background
AGGLAYER_ACCOUNTS_DIR=./data miden-client-integration-tests --contains "agglayer"
```

### Testing against devnet

The same tests work against devnet by setting `AGGLAYER_ACCOUNTS_DIR` to the directory containing devnet-specific `.mac` files and using the appropriate RPC endpoint:

```bash
AGGLAYER_ACCOUNTS_DIR=/path/to/devnet/accounts \
  miden-client-integration-tests --network devnet --contains "agglayer"
```

## Fees

The testing node charges a fee for every transaction, as a real chain does: its genesis sets
`verification_base_fee = 500`. To run it fee-free instead:

```bash
MIDEN_VERIFICATION_BASE_FEE=0 make start-node-background
```

The suite never mints. It draws the native asset from pre-funded basic wallets named by `--funders`
(or `MIDEN_FUNDER_ACCOUNTS`), either one `.mac` file or a directory of them.

```bash
# Local node: its genesis pre-funds the wallets and start-test-node.sh writes them here.
# The Makefile targets pass this for you.
MIDEN_FUNDER_ACCOUNTS=$PWD/data/funders cargo nextest run --workspace --release --test=integration

# Deployed network: supply wallets funded out of band, since nothing can be minted there.
miden-client-integration-tests --network testnet --funders ./testnet-funders
```

The `insert_new_*` helpers pay each account they create and deploy it by consuming that note, which
settles the deploy's own fee. `miden_client::testing::fee::deploy_account` does the same for an
account a test builds itself.

A funding transaction costs a fee and a proof, so accounts are funded in batches wherever a test
creates more than one: the `setup_*` helpers create their accounts with the `insert_new_*_unfunded`
variants and then pass the whole set to `TestClient::fund_if_needed`, which pays them all from one
transaction. A test creating several accounts of its own should do the same rather than calling the
funding `insert_new_*` helpers in a row.

Funding costs no transaction of its own beyond that payment. Each account's note is held until the
account's next transaction and folded into it, so that one transaction deploys the account, funds
it and does the test's work. `TestClient::submit_new_transaction` does the folding; a request going
somewhere else needs `TestClient::fund_request` first, notably a batch, which borrows the client
for as long as it lives. A test that needs the funding to land in a particular transaction — one
asserting on what a sync reports, say — should call `TestClient::take_funding` and consume the note
itself.

Funders must be **public** and carry their secret key: a public funder's state is re-read from the
chain before every payment, which is what makes sharing one between test processes safe.

### Environment variables

- `MIDEN_FUNDER_ACCOUNTS` - funder `.mac` file or directory, same as `--funders`
- `MIDEN_VERIFICATION_BASE_FEE` - genesis `verification_base_fee` for the testing node (default
  `500`; `0` runs the node fee-free and declares no funder wallets)
- `MIDEN_TEST_PROVER_THREADS` - threads a test process gives its prover in the agglayer run
  (default `2`), leaving cores for the node's own prover
- `MIDEN_NUM_FUNDER_WALLETS` - number of wallets a fee-charging genesis pre-funds (default `16`)

## Test Case Generation

The integration tests use an automatic code generation system to create both `cargo nextest` compatible tests and a standalone binary. Test functions that start with `test_` are automatically discovered during build time and used to generate:

1. **Individual `#[tokio::test]` wrappers** - These allow the tests to be run using standard `cargo test` or `cargo nextest run` commands
2. **Programmatic test access** - A `Vec<TestCase>` that enables the standalone binary to enumerate and execute tests dynamically with custom parallelism and filtering

The discovery system:
- Scans all `.rs` files in the `src/` directory recursively
- Identifies functions named `test_*` (supporting `pub async fn test_*`, `async fn test_*`, etc.)
- Generates test registry and integration test wrappers automatically

This dual approach allows the same test code to work seamlessly with both nextest (for development) and the standalone binary (for CI/CD and production testing scenarios), ensuring consistent behavior across different execution environments.

## Writing Tests

To add a new integration test:

1. Create a public async function that starts with `test_`
2. The function should take a `ClientConfig` parameter
3. The function should return `Result<()>`
4. Place the function in any `.rs` file under `src/`

Example:
```rust
pub async fn test_my_feature(client_config: ClientConfig) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;
    // test logic here
}
```

The build system will automatically discover this function and include it in both the test registry and generate tokio test wrappers.

## License
This project is [MIT licensed](../../LICENSE).
