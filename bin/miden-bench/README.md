# miden-bench

Benchmarking tool for the Miden client library. This binary measures performance of transactions and of the `SQLite` store to establish baselines and identify optimization opportunities.

## Installation

```bash
# Using make
make install-bench

# Or directly with cargo
cargo install --path bin/miden-bench --locked
```

After installation, the `miden-bench` binary will be available in your PATH.

## Commands

### `deploy`

Deploys a public wallet with empty storage maps to the network. This is the first step in preparing an account for benchmarking. Storage maps are created empty; use `expand` to fill them with entries.

```bash
miden-bench --network localhost deploy --maps 2
```

The command outputs the account ID and a ready-to-copy `expand` command:

```
Account ID: 0xabcdef1234567890...

Expand storage with:
  miden-bench expand --account-id 0xabcdef1234567890 --map-idx 0 --offset 0 --count 100
```

### `expand`

Fills entries into a specific storage map of a deployed account. The account must have been deployed with `deploy` first, which installs the expansion procedures needed to write entries.

```bash
miden-bench expand --account-id 0x... --map-idx 0 --offset 0 --count 200
```

Entries are batched into transactions of up to 280 entries each. Keys and values are generated deterministically from the map index and entry offset, so repeated runs with the same parameters produce the same data.

To fill multiple maps, run `expand` once per map:

```bash
miden-bench expand --account-id 0x... --map-idx 0 --offset 0 --count 100
miden-bench expand --account-id 0x... --map-idx 1 --offset 0 --count 100
```

### `transaction`

Benchmarks transaction operations that read storage from an account (requires a running Miden node):

- **execute** - Measures transaction execution time (without proving) reading all storage entries
- **prove** - Measures transaction proving time and proof size
- **full** - Measures full transaction (execute + prove + submit)

The benchmark executes transactions that read all storage map entries from the specified account. Accounts deployed via `deploy` automatically have the signing key persisted, enabling all three benchmarks (`execute`, `prove`, and `full`). If the signing key is not found (e.g., account was imported from the network without deploying), only `execute` is benchmarked.

The number of storage maps is auto-detected from the account.

```bash
miden-bench --network localhost transaction --account-id 0x...
```

### `store`

Benchmarks the `SQLite` store as the database grows.

```bash
miden-bench store --notes 1000,10000 --accounts 100,1000 --iterations 5
```

Two tables come out of a run, one per sweep. Each row is a store method, each column a size, and the last column the growth from the first size to the last. That growth is the number to read: a method served by an index stays near `1.00x` while the database grows, and one that falls back to a scan grows with it.

The note sweep seeds three quarters of the notes as consumed by a single account, spread over blocks and transaction orders, and the rest as unspent notes carrying a nullifier. It measures:

- `get_input_notes` for the `Unspent`, `Consumed`, `List`, `Nullifiers` and `ScriptRoots` filters
- `get_unspent_input_note_nullifiers` and `get_tracked_block_headers`
- `get_input_note_after` seeded with a cursor near the end of the account's history
- a full `InputNoteReader` walk, reported as a total and per note
- `upsert_input_notes` and `apply_state_sync` for a fixed batch of new notes

The account sweep seeds wallets that share one account code, and measures `SqliteStore::new` (which rebuilds the SMT forest on open), `get_account_headers`, `get_account_header` and `prune_account_history`.

Reads run before writes within a size, so every read measures exactly the seeded database.

```bash
# A deeper sweep. Seeding is linear in the sizes, so the run time is too.
miden-bench store --notes 10000,100000 --accounts 1000,10000
```

`make bench-store` runs the same sweep CI does. Override `STORE_BENCH_ARGS` to change the sizes:

```bash
make bench-store STORE_BENCH_ARGS="--notes 5000,50000 --accounts 500,5000"
```

### `import`

Imports an account into the local store. Two mutually exclusive modes:

- `--filename <path>` reads a `.mac` file via `AccountFile::read`, inserts its auth secret keys into the keystore, and adds the account to the store.
- `--account-id <hex>` downloads a public account from the network via `Client::import_account_by_id`.

```bash
# From a .mac file
miden-bench import --filename account.mac

# From the network
miden-bench --network testnet import --account-id 0x...
```

The command reports import time and the imported size: input `.mac` file size for the file mode (which includes auth secret keys), and the post-import serialized account size for the network mode.

### `export`

Exports an account from the local store to a `.mac` file. The file contains the account alongside its auth secret keys retrieved from the keystore.

```bash
# Default output path: <account_id>.mac in the current directory
miden-bench export --account-id 0x...

# Custom output path
miden-bench export --account-id 0x... --filename ./out/account.mac
```

The command reports export time and the resulting `.mac` file size.

## Workflow

The typical workflow is: **deploy** -> **expand** -> **transaction**.

All commands share a persistent store directory (default: `./miden-bench-store`). The `deploy` command saves the account and signing key there, and `expand` and `transaction` reuse them automatically.

```bash
# 1. Deploy an account with 2 empty storage maps
miden-bench --network localhost deploy --maps 2

# 2. Fill each map with entries (copy account-id from deploy output)
miden-bench expand --account-id 0x... --map-idx 0 --offset 0 --count 100
miden-bench expand --account-id 0x... --map-idx 1 --offset 0 --count 100

# 3. Benchmark transactions against the account
miden-bench transaction --account-id 0x...
```

## Global Options

These options apply to all commands and must be specified before the subcommand.

### Network Environment (`-n, --network`)

Specifies the network environment for benchmarks that require a Miden node. Default: `localhost`.

| Value | Endpoint |
|-------|----------|
| `localhost` (or `local`) | `http://localhost:57291` |
| `devnet` (or `dev`) | `https://rpc.devnet.miden.io` |
| `testnet` (or `test`) | `https://rpc.testnet.miden.io` |
| Custom URL | Any valid RPC endpoint |

```bash
# Use local development node
miden-bench --network localhost deploy

# Use Miden devnet
miden-bench --network devnet deploy
```

You can also set the network via the `MIDEN_NETWORK` environment variable:

```bash
export MIDEN_NETWORK=devnet
miden-bench deploy
```

### Persistent Store (`--store`)

Path to a directory for persistent store data. Default: `./miden-bench-store`.

All commands use this directory for the `SQLite` database and filesystem keystore. The `deploy` command creates and populates it; `expand` and `transaction` reuse it.

```bash
# Use a custom store directory
miden-bench --store ./my-bench-data deploy --maps 2
miden-bench --store ./my-bench-data expand --account-id 0x... --map-idx 0 --offset 0 --count 100
miden-bench --store ./my-bench-data transaction --account-id 0x...
```

### CPU Profiling with Samply

Use [`samply`](https://github.com/mstange/samply) to inspect CPU time by function, stack, thread, or source line. `miden-bench` timings remain the benchmark numbers.

```bash
cargo install --locked samply
CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release -p miden-client-bench

samply record target/release/miden-bench --store ./miden-bench-store deploy --maps 1
samply record target/release/miden-bench --store ./miden-bench-store transaction --account-id 0x... --iterations 3
```

On Linux, Samply may require perf event access if recording is denied.

### Command Options

#### Deploy

- `-m, --maps <N>` - Number of empty storage maps in the account (1-100, default: 1)

```bash
# Deploy account with 3 empty storage maps
miden-bench deploy --maps 3
```

#### Expand

- `-a, --account-id <ID>` - Public account ID to expand (required, hex format)
- `-m, --map-idx <N>` - Storage map index to fill (0-based, must be less than the deploy `--maps` count)
- `-o, --offset <N>` - Starting entry offset (0-based)
- `-c, --count <N>` - Number of entries to add starting from offset

#### Transaction

- `-a, --account-id <ID>` - Public account ID to benchmark against (required, hex format)
- `-r, --reads <N>` - Maximum storage reads per transaction. When total entries exceed this limit, reads are split across multiple transactions per benchmark iteration. Each iteration's time is the sum across all transactions. When omitted, all entries are read in a single transaction.
- `-i, --iterations <N>` - Number of benchmark iterations (default: 5)

#### Store

- `--notes <N[,N...]>` - Note counts to sweep over (default: `1000,10000`)
- `--accounts <M[,M...]>` - Account counts to sweep over (default: `100,1000`)
- `-i, --iterations <N>` - Number of benchmark iterations per measurement (default: 5)

#### Import

Exactly one of the following must be provided:

- `-f, --filename <PATH>` - Path to a `.mac` account file
- `-a, --account-id <ID>` - Public account ID to download from the network (hex format)

#### Export

- `-a, --account-id <ID>` - Account ID to export (required, hex format)
- `-f, --filename <PATH>` - Output `.mac` file path (defaults to `<account_id>.mac` in the current directory)

## Metrics

Each benchmark reports a table with columns:

- **Mean** - Average duration across all iterations
- **Min** - Fastest iteration
- **Max** - Slowest iteration

Proving benchmarks also display the proof output size alongside the benchmark name.
