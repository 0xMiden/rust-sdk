---
title: CLI
---

The following document lists the commands that the CLI currently supports.

:::tip
Use `--help` as a flag on any command for more information.
:::

## Usage

Call a command on the `miden-client` like this:

```sh
miden-client <command> <flags> <arguments>
```

Optionally, you can include the `--debug` flag to run the command with debug mode, which enables debug output logs from scripts that were compiled in this mode:

```sh
miden-client --debug <flags> <arguments>
```

Note that the debug flag overrides the `MIDEN_DEBUG` environment variable.

## Commands

### `init`

Creates a configuration file for the client in the current directory. Running this command is optional, as the client will self-initialize by default. By default, the command uses the Testnet network.

```sh
# This will create a config file named `miden-client.toml` using default values
# This file contains information useful for the CLI like the RPC provider and database path
miden-client init

# You can set up the CLI for any of the default networks
miden-client init --network testnet
miden-client init --network devnet
miden-client init --network localhost

# You can also specify a custom network
miden-client init --network 18.203.155.106
# You can specify the port
miden-client init --network 18.203.155.106:8080
# You can also specify the protocol (http/https)
miden-client init --network https://18.203.155.106
# You can specify both
miden-client init --network https://18.203.155.106:1234

# You can use the --store-path flag to override the default store config
miden-client init --store-path db/store.sqlite3

# You can use the --block-delta flag to set maximum number of blocks the client can be behind
miden-client init --block-delta 250

# You can provide both flags
miden-client init --network 18.203.155.106 --store-path db/store.sqlite3

# You can set a remote prover to offload the proving process (along with the `--delegate-proving` flag in transaction commands)
miden-client init --remote-prover-endpoint <PROVER_URL>

# To enable the transport layer, specify the endpoint
miden-client init --note-transport-endpoint <MIDEN_NOTE_TRANSPORT_URL>
```

More information on the configuration file can be found in the [configuration section](cli-config.md).

### `account`

Inspect account details.

#### Action Flags

| Flags            | Description                                      | Short Flag |
| ---------------- | ------------------------------------------------ | ---------- |
| `--list`         | List all accounts monitored by this client       | `-l`       |
| `--show <ID>`    | Show details of the account for the specified ID | `-s`       |
| `--default <ID>` | Manage the setting for the default account       | `-d`       |

The `--show` flag also accepts a partial ID instead of the full ID. For example, instead of:

```sh
miden-client account --show 0x8fd4b86a6387f8d8
```

You can call:

```sh
miden-client account --show 0x8fd4b86
```

For the `--default` flag, if `<ID>` is "none" then the previous default account is cleared. If no `<ID>` is specified then the default account is shown.

### `new-wallet`

Creates a new wallet account.

A basic wallet is comprised of a basic authentication component (for RPO Falcon signature verification), alongside a basic wallet component (for sending and receiving assets).

This command has three optional flags:

- `-t, --account-type <ACCOUNT_TYPE>`: Used to select the account visibility (private if not specified). It may receive "private" or "public". This is the only thing the protocol's `AccountType` encodes.
- `--extra-packages <PACKAGES>`: Specifies a list of file paths for packages holding account components to include in the account. If the packages contain placeholders, the CLI will prompt the user to enter the required data for instantiating storage appropriately.
- `--init-storage-data-path <INIT_STORAGE_DATA_PATH>`: Specifies an optional file path to a TOML file containing key/value pairs used for initializing storage. Each key should map to a placeholder within the packages' component metadata. The CLI will prompt for any keys that are not present in the file.

After creating an account with the `new-wallet` command, it is automatically stored and tracked by the client. This means the client can execute transactions that modify the state of accounts and track related changes by synchronizing with the Miden network.

### `new-account`

Creates a new account and saves it locally.

An account may be composed of one or more components, each with its own storage and distinct functionality. This command lets you build a custom account by selecting an account type and optionally adding extra component packages.

This command has four flags:

- `-t, --account-type <ACCOUNT_TYPE>`: Specifies the account visibility. It accepts either "private" or "public", with "private" as the default. This is the only thing the protocol's `AccountType` encodes.

There is no `--faucet` flag: faucet-vs-regular is derived from the packages. If any package contributes the `FungibleFaucet` component, the resulting account is treated as a fungible faucet and an implicit `TokenPolicyManager` is installed when one is not already provided. `--account-type` only selects visibility.
- `--packages <PACKAGES>`: Specifies a list of file paths for packages holding account components to include in the account. If the packages contain placeholders, the CLI will prompt the user to enter the required data for instantiating storage appropriately.
- `--init-storage-data-path <INIT_STORAGE_DATA_PATH>`: Specifies an optional file path to a TOML file containing key/value pairs used for initializing storage. Each key should map to a placeholder within the packages' component metadata. The CLI will prompt for any keys that are not present in the file.

After creating an account with the `new-account` command, the account is stored locally and tracked by the client, enabling it to execute transactions and synchronize state changes with the Miden network.

#### Examples

```bash
# Create a new wallet with default settings (private visibility, no extra components)
miden-client new-wallet

# Create a new wallet with public visibility
miden-client new-wallet -t public

# Create a new wallet that includes custom packages
miden-client new-wallet --extra-packages packages/custom-package.masp

# Create a fungible faucet with interactive input
# (the resulting account is a faucet because basic-fungible-faucet.masp contributes the
# `FungibleFaucet` component — no extra flag is needed)
miden-client new-account --packages packages/basic-fungible-faucet.masp

# Create a fungible faucet with preset fields
miden-client new-account --packages packages/basic-fungible-faucet.masp --init-storage-data-path init_data.toml
```

where `init_data.toml` is a TOML file with the following example content:
```toml
token_metadata.max_supply = 1000000000
token_metadata.decimals = 6
token_metadata.ticker = "TEST"
```

### `info`

View a summary of the current client state.

#### Action Flags

| Flag           | Description                                  | Short Flag |
| -------------- | -------------------------------------------- | ---------- |
| `--rpc-status` | Display detailed RPC node status information | `-r`       |

When using the `--rpc-status` flag, the command displays additional information about the RPC node including:

- Node version
- Genesis commitment
- Store connection status and chain tip
- Block producer status and chain tip

### `notes`

View and manage notes. Also, exchange private notes using the note transport network.

#### Action Flags

| Flags                   | Description                                              | Short Flag |
| ----------------------- | -------------------------------------------------------- | ---------- |
| `--list [<filter>]`     | List input notes                                         | `-l`       |
| `--show <ID>`           | Show details of the input note for the specified note ID | `-s`       |
| `--send <ID> <address>` | Send a note using the note transport network             |            |
| `--fetch`               | Fetch notes from the note transport network              |            |

The `--list` flag receives an optional filter: - expected: Only lists expected notes. - committed: Only lists committed notes. - consumed: Only lists consumed notes. - processing: Only lists processing notes. - consumable: Only lists consumable notes. An additional `--account-id <ID>` flag may be added to only show notes consumable by the specified account.
If no filter is specified then all notes are listed.

The `--show` flag also accepts a partial ID instead of the full ID. For example, instead of:

```sh
miden-client notes --show 0x70b7ecba1db44c3aa75e87a3394de95463cc094d7794b706e02a9228342faeb0
```

You can call:

```sh
miden-client notes --show 0x70b7ec
```

To send a private note, the `--send` flag sends a note using the note transport network.
The note ID (hex, in full or a prefix) and recipient's address (bech32) must be provided.
The note is assumed to be stored in the store (e.g., imported using [`import`](#import)).

You can call:

```sh
miden-client notes --send 0xc1234567 mm1qpkdyek2c0ywwvzupakc7zlzty8qn2qnfc
```

To fetch private notes, the `--fetch` allows to download notes from the note transport network.
Only notes for tracked tags will be fetched (e.g. `miden-client tags --list`).
The downloaded notes will be added to the store.

```sh
miden-client notes --fetch
```

### `network-note-status`

Query the network for the processing status of a note. This is useful for diagnosing issues with network transactions (NTX), such as notes that are stuck or have been discarded.

```sh
miden-client network-note-status <NOTE_ID>
```

The command displays a table with the following information:

- **Status**: The current processing state of the note (`Pending`, `Processed`, `Discarded`, or `Committed`).
- **Attempt Count**: The number of times the node has attempted to process the note.
- **Last Error**: The last error encountered during processing, if any.
- **Last Attempt Block**: The block number of the most recent processing attempt.

The note ID must be provided as a full hex string:

```sh
miden-client network-note-status 0x70b7ecba1db44c3aa75e87a3394de95463cc094d7794b706e02a9228342faeb0
```

:::note
This command queries the Miden node directly and does not require the note to be tracked locally.
:::

### `sync`

Sync the client with the latest state of the Miden network. Shows a brief summary at the end.

### `tags`

View and add tags.

#### Action Flags

| Flag             | Description                                                 | Aliases |
| ---------------- | ----------------------------------------------------------- | ------- |
| `--list`         | List all tags monitored by this client                      | `-l`    |
| `--add <tag>`    | Add a new tag to the list of tags monitored by this client  | `-a`    |
| `--remove <tag>` | Remove a tag from the list of tags monitored by this client | `-r`    |

### `tx`

View transactions.

#### Action Flags

| Command  | Description               | Aliases |
| -------- | ------------------------- | ------- |
| `--list` | List tracked transactions | -l      |

After a transaction gets executed, two entities start being tracked:

- The transaction itself: It follows a lifecycle from `Pending` (initial state) and `Committed` (after the node receives it). It may also be `Discarded` if the transaction was not included in a block.
- Output notes that might have been created as part of the transaction (for example, when executing a pay-to-id transaction).

### Transaction creation commands

#### `mint`

Creates a note that contains a specific amount tokens minted by a faucet, that the target Account ID can consume.

Usage: `miden-client mint --target <TARGET ACCOUNT ID> --asset <AMOUNT>::<FAUCET ID> --note-type <NOTE_TYPE>`

#### `consume-notes`

Account ID consumes a list of notes, specified by their Note ID.

Usage: `miden-client consume-notes --account <ACCOUNT ID> [NOTES]`

For this command, you can also provide a partial ID instead of the full ID for each note. So instead of

```sh
miden-client consume-notes --account <some-account-id> 0x70b7ecba1db44c3aa75e87a3394de95463cc094d7794b706e02a9228342faeb0 0x80b7ecba1db44c3aa75e87a3394de95463cc094d7794b706e02a9228342faeb0
```

You can do:

```sh
miden-client consume-notes --account <some-account-id> 0x70b7ecb 0x80b7ecb
```

Additionally, you can optionally not specify note IDs, in which case any note that is known to be consumable by the executor account ID will be consumed.

Either `Expected` or `Committed` notes may be consumed by this command, changing their state to `Processing`. It's state will be updated to `Consumed` after the next sync.

#### `transfer`

Transfers assets to another account. Sender Account creates a note that a target Account ID can consume. The asset is identified by the tuple `(FAUCET ID, AMOUNT)`. The note can be configured to be recallable making the sender able to consume it after a height is reached.

Usage: `miden-client transfer --sender <SENDER ACCOUNT ID> --target <TARGET ACCOUNT ID> --asset <AMOUNT>::<FAUCET ID> --note-type <NOTE_TYPE> <RECALL_HEIGHT>`

#### `swap`

The source account creates a `SWAP` note that offers some asset in exchange for some other asset. When another account consumes that note, it will receive the offered asset amount and the requested asset will removed from its vault (and put into a new note which the first account can then consume). Consuming the note will fail if the account doesn't have enough of the requested asset.

Usage: `miden-client swap --source <SOURCE ACCOUNT ID> --offered-asset <OFFERED AMOUNT>::<OFFERED FAUCET ID> --requested-asset <REQUESTED AMOUNT>::<REQUESTED FAUCET ID> --note-type <NOTE_TYPE>`

### `address`

View and manage addresses.

#### Action Subcommands

| Subcommand                          | Description                                                                                      |
| ----------------------------------- | -------------------------------------------------------------------------------------------------|
| `list <ID>`                         | List all addresses or only for the specified account ID (default command)                        |
| `add <ID> <ADDRESS>`                | Track a bech32-encoded address on the specified account ID                                       |
| `remove <ID> <ADDRESS>`             | Remove a bech32-encoded address from the specified account ID                                    |
| `encode <ID> <INTERFACE> <TAG_LEN>` | Produce a bech32 address from the account ID, interface, and optional tag length                 |

The `list` subcommand optionally takes an account ID to only show the addresses of that account, if it is not provided, it will show all addresses of all accounts.

```sh
miden-client address list 0x17f13f4f83a8e8100c19d2961dfda2
```

`add` and `remove` take the account ID and a bech32-encoded address as arguments. `add` validates that the bech32 address encodes the same account ID and that its network matches the CLI's configured network.

Use `encode` to produce a bech32 address from its fields — this output is what `add` expects. The interface can be:
- `basic-wallet`: The basic wallet interface.

Note: `Unspecified` (shown by `address list`) denotes an address not bound to any interface, it's the default address for every account created.

```sh
# Produce a bech32 address for the given account, interface, and tag length
miden-client address encode 0x17f13f4f83a8e8100c19d2961dfda2 basic-wallet 10

# Track that address on the account
miden-client address add 0x17f13f4f83a8e8100c19d2961dfda2 mlcl1qple0ejnutx8zyp0cm0pme9wjfgqz0u9djq_qruqqypuyph
```

```sh
miden-client address remove 0x17f13f4f83a8e8100c19d2961dfda2 mlcl1qple0ejnutx8zyp0cm0pme9wjfgqz0u9djq
```

#### Tips

For `transfer` and `consume-notes`, you can omit the `--sender` and `--account` flags to use the default account defined in the [config](cli-config.md). If you omit the flag but have no default account defined in the config, you'll get an error instead.

For every command which needs an account ID (either wallet or faucet), you can also provide a partial ID instead of the full ID for each account. So instead of

```sh
miden-client transfer --sender 0x80519a1c5e3680fc --target 0x8fd4b86a6387f8d8 --asset 100::0xa99c5c8764d4e011
```

You can do:

```sh
miden-client transfer --sender 0x80519 --target 0x8fd4b --asset 100::0xa99c5c8764d4e011
```

!!! note
The only exception is for using IDs as part of the asset, those should have the full faucet's account ID.

#### Transaction confirmation

When creating a new transaction, a summary of the transaction updates will be shown and confirmation for those updates will be prompted:

```sh
miden-client <tx command> ...

TX Summary:

...

Continue with proving and submission? Changes will be irreversible once the proof is finalized on the network (y/N)
```

This confirmation can be skipped in non-interactive environments by providing the `--force` flag (`miden-client transfer --force ...`).

#### Delegated proving

If a remote prover is configured, the CLI can offload the proving process to it. This is done by providing the `--delegate-proving` flag when creating a transaction. The CLI will then send the transaction to the remote prover for processing.

### Importing and exporting

#### `export`

Export input note data to a binary file .

| Flag                          | Description                           | Aliases |
| ----------------------------- | ------------------------------------- | ------- |
| `--filename <FILENAME>`       | Desired filename for the binary file. | `-f`    |
| `--export-type <EXPORT_TYPE>` | Exported note type.                   | `-e`    |

##### Export type

The user needs to specify how the note should be exported via the `--export-type` flag. The following options are available:

- `id`: Only the note ID is exported. When importing, if the note ID is already tracked by the client, the note will be updated with missing information fetched from the node. This works for both public and private notes. If the note isn't tracked and the note is public, the whole note is fetched from the node and is stored for later use.
- `full`: The note is exported with all of its information (metadata and inclusion proof). When importing, the note is considered unverified. The note may not be consumed directly after importing as its block header will not be stored in the client. The block header will be fetched and be used to verify the note during the next sync. At this point the note will be committed and may be consumed.
- `partial`: The note is exported with minimal information and may be imported even if the note is not yet committed on chain. At the moment of importing the note, the client will check the state of the note by doing a note sync, using the note's tag. Depending on the response, the note will be either stored as "Expected" or "Committed".

#### `import`

Import entities managed by the client, such as accounts and notes. The type of entities is inferred.

The `--overwrite` flag can be used when importing accounts. It allows the user to overwrite existing accounts with the same ID. This is useful when you want to update the account's information or replace it with a new version.

### Executing scripts

#### `exec`

Execute the specified program against the specified account.

| Flag                          | Description                                  | Aliases |
| ----------------------------- | -------------------------------------------- | ------- |
| `--account <ACCOUNT_ID>`      | Account ID to use for the program execution. | `-a`    |
| `--script-path <SCRIPT_PATH>` | Path to script's source code to be executed. | `-s`    |
| `--inputs-path <INPUTS_PATH>` | Path to the inputs file.                     | `-i`    |
| `--hex-words`                 | Print the output stack grouped into words.   |         |

The file referenced by `--inputs-path` should contain a TOML array of inline tables, where each table has two fields: - `key`: a 256-bit hexadecimal string representing a word to be used as a key for the input entry. The hexadecimal value must be prefixed with 0x. - `values`: an array of 64-bit unsigned integers representing field elements to be used as values for the input entry. Each integer must be written as a separate string, within double quotes.

The input file should contain a TOML table called `inputs`, as in the following example:

```toml
inputs = [ { key = "0x0000000000000000000000000000000000000000000000000000001000000000", values = ["13", "9"]}, { key = "0x0000000000000000000000000000000000000000000000000000000000000000" , values = ["1", "2"]}, ]
```

### `note-transport`

Send and fetch private notes using the transport layer.
