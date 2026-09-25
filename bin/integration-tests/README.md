# Miden Client Integration Tests

This directory contains integration tests for the Miden client library. These tests verify the functionality of the client against a running Miden node.

## Running

The tests run under `cargo nextest`, which gives each test its own process. The Make targets set
the environment the tests need:

```bash
make start-node-background
make integration-test-non-agglayer   # everything except agglayer
make integration-test-agglayer       # only agglayer, serialized
make stop-node
```

To run a subset directly, use a nextest filter expression:

```bash
cargo nextest run --workspace --release --test=integration -E 'test(/fpi/)'
```

## Environment Variables

The following environment variables configure the generated test wrappers:

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
AGGLAYER_ACCOUNTS_DIR=./data make integration-test-agglayer
```

### Testing against devnet

The same tests work against devnet by setting `AGGLAYER_ACCOUNTS_DIR` to the directory containing devnet-specific `.mac` files and using the appropriate RPC endpoint:

```bash
AGGLAYER_ACCOUNTS_DIR=/path/to/devnet/accounts TEST_MIDEN_NETWORK=devnet \
  cargo nextest run --workspace --release --test=integration -E 'test(/agglayer/)' 
```

## Fees

The testing node charges a fee for every transaction, as a real chain does: its genesis sets
`verification_base_fee = 500`. To run it fee-free instead:

```bash
MIDEN_VERIFICATION_BASE_FEE=0 make start-node-background
```

The suite never mints. It draws the native asset from the node's funding service, named by
`MIDEN_FUNDING_SERVICE_URL`, which owns one account and hands out notes
over HTTP. `start-test-node.sh` runs one on `http://127.0.0.1:50401` whenever the chain charges
fees, and the Make targets point at it for you. Naming no service leaves the run without a funder,
which is all a fee-free chain needs.

```bash
# Local node. The Makefile targets pass the URL for you.
make integration-test-non-agglayer

# Deployed network, which needs no wallets funded out of band.
TEST_MIDEN_NETWORK=testnet MIDEN_FUNDING_SERVICE_URL=https://funding.example \
  cargo nextest run --workspace --release --test=integration
```

Three properties of the service a test can notice:

- The test process proves nothing itself, and every request that reaches the service inside one
  short window shares a single transaction, across processes.
- The service answers a request with the note as soon as it queues it, before it builds the
  transaction which creates the note. Every funding note is consumed as an unauthenticated input,
  so a test never needs the block. A test can submit its transaction before the funding
  transaction reaches the node. The node then rejects it, and the test client's RPC layer
  resubmits the same proven transaction until the node accepts it. An expired funding transaction
  keeps its notes queued in the service, which puts them into a later transaction.
- Its notes are **public**. A sync therefore imports a funding note as a tracked input note of the
  account it targets, so a test must identify a note by ID rather than by position or by counting
  the committed notes.

The `insert_new_*` helpers pay each account they create. The account is deployed by whichever
transaction first consumes that note, as described below. An account a test builds itself can be
funded and deployed on the spot with `TestClient::deploy_account`, whose deploy transaction
consumes the funding note and thereby settles its own fee.

The service builds one transaction at a time and waits for each to commit before it builds the
next, so accounts are funded in batches wherever a test creates more than one: the `setup_*` helpers create their accounts with the `insert_new_*_unfunded`
variants and then pass the whole set to `TestClient::fund_if_needed`, which asks for them all at
once so they share one transaction. A test creating several accounts of its own should do the same
rather than calling the funding `insert_new_*` helpers in a row.

Funding costs no transaction of its own. Each account's note is held until the account's next
transaction and folded into it, so that one transaction deploys the account, funds it and does the
test's work. `TestClient::submit_new_transaction` does the folding. A request going somewhere else
needs `TestClient::fund_request` first, notably a batch, which borrows the client for as long as it
lives. A test that needs the funding to land in a particular transaction, one asserting on what a
sync reports for instance, should call `TestClient::take_funding` and consume the note itself.

### Environment variables

- `MIDEN_FUNDING_SERVICE_URL` - funding service base URL. Unset or
  empty leaves the run without a funder
- `MIDEN_VERIFICATION_BASE_FEE` - genesis `verification_base_fee` for the testing node (default
  `500`, `0` runs the node fee-free and declares no funding wallet)

## Test Case Generation

The build script scans `src/tests/` recursively for functions named `test_*` and generates a
`#[tokio::test]` wrapper for each in `OUT_DIR/integration_tests.rs`. A new test needs no
registration.

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
