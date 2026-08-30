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

Read when the local node is started — by `scripts/start-test-node.sh` and the `gen-genesis` it runs — and not by the tests; setting these alongside a test command has no effect:

- `MIDEN_TEST_NODE_VERIFICATION_BASE_FEE` - A non-zero value makes the local chain charge that fee and puts two funder wallets holding MIDEN in genesis; the script copies them into `./data` as `wallet_<n>.mac`, secret keys included. See `crates/testing/test-node-genesis/README.md` for what that switch does and does not yet make possible
- `AGGLAYER_GENESIS` - Any value, empty included, adds the AggLayer accounts to genesis and exports them to `./data`, where `AGGLAYER_ACCOUNTS_DIR=./data` picks them up
- `MIDEN_TEST_NODE_USE_INSTALLED` - Any non-empty value runs the node binaries already installed under `target/test-node/install` instead of checking them against the pinned rev, aborting rather than falling back to the pin if one of them is missing

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

### Two genesis modes

AggLayer tests support two modes, depending on whether the node has agglayer accounts pre-deployed at genesis:

1. **Empty genesis (runtime setup)**: All accounts (bridge admin, GER manager, bridge, faucet) are created at runtime. This is the default and works against any node started with `make start-node`.

2. **Complete genesis (pre-deployed)**: Agglayer accounts are included in the genesis block. Start the node with `make start-node-agglayer`, then point the tests at the `.mac` files:

```bash
# Start node with agglayer genesis accounts
make start-node-agglayer

# Run genesis-aware tests
AGGLAYER_ACCOUNTS_DIR=./data/ miden-client-integration-tests --contains "agglayer"
```

### Environment variables

- `AGGLAYER_ACCOUNTS_DIR` - Path to directory containing agglayer `.mac` account files. Setting it switches the agglayer tests to complete-genesis mode (otherwise they default to runtime setup). The node writes these files to `./data/` when started with `AGGLAYER_GENESIS=1`. On devnet, this would point to wherever the devnet account files are stored.

### Genesis account files

When the node is started with `make start-node-agglayer` (or `AGGLAYER_GENESIS=1`), the following files are written to the data directory:

- `bridge_admin.mac` - Bridge admin wallet (includes secret key)
- `ger_manager.mac` - GER manager wallet (includes secret key)
- `bridge.mac` - AggLayer bridge account (no secret key, NoAuth)
- `agglayer_faucet.mac` - AggLayer faucet account (no secret key, NoAuth)

The genesis faucet uses a deterministic test origin token address (`0xAAAA...AA`) and is pre-registered in the bridge's faucet registry.

### Testing against devnet

The same tests work against devnet by setting `AGGLAYER_ACCOUNTS_DIR` to the directory containing devnet-specific `.mac` files and using the appropriate RPC endpoint:

```bash
AGGLAYER_ACCOUNTS_DIR=/path/to/devnet/accounts \
  miden-client-integration-tests --network devnet --contains "agglayer"
```

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
