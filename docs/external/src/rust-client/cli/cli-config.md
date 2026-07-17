---
title: Config
sidebar-position: 2
---

After [installation](../install-and-run.md#install-the-client), use the client by running the following and adding the [relevant commands](index.md#commands):

```sh
miden-client
```

:::tip
Run `miden-client --help` for information on `miden` commands.
:::

## Client Configuration

We configure the client using a [TOML](https://en.wikipedia.org/wiki/TOML) file ([`miden-client.toml`]). The file gets created when running `miden-client init`, which creates a `.miden` directory structure to organize all client-related files. By default, this directory is located in the HOME path, i.e. at `~/.miden`. Running this command is optional, but can be done if you want to have more fine-grained control over the configuration of the `miden-client`. The TOML file can also be edited to use a different configuration for the client.

```sh
store_filepath = ".miden/store.sqlite3"
secret_keys_directory = ".miden/keystore"
default_account_id = "0x012345678"
token_symbol_map_filepath = ".miden/token_symbol_map.toml"
remote_prover_endpoint = "http://localhost:8080"
package_directory = ".miden/packages"
max_block_number_delta = 256

[rpc]
endpoint = { protocol = "http", host = "localhost", port = 57291 }
timeout_ms = 10000

[note-transport] # optional
endpoint = "http://localhost:57292"
timeout_ms = 10000
```

### Configuration Location and Priority

The client supports both **global** and **local** configuration with intelligent priority handling:

1. **Global Configuration** (default): Located at `~/.miden/miden-client.toml` in your home directory. The global directory location can be overridden with the `MIDEN_CLIENT_HOME` environment variable (see [Environment variables](#environment-variables)).
2. **Local Configuration** (project-specific): Located at `./.miden/miden-client.toml` in your current working directory

**Priority Order**: Local configuration takes precedence over global configuration. If both exist, the client will use the local configuration and ignore the global one.

### Initialization Options

```bash
# Create global configuration (default behavior)
miden-client init

# Create local configuration in current directory
miden-client init --local
```

The global configuration approach reduces per-project setup overhead while still allowing project-specific customization when needed.

### Configuration Management

#### Clear Command

The `clear` command helps manage configuration by removing existing setups:

```bash
# Remove local config if present, otherwise remove global config
miden-client clear

# Force removal of global configuration only
miden-client clear --global
```

**Priority Behavior**: The clear command follows the same priority logic as config loading - it will remove the local configuration first if it exists, and only remove the global configuration if no local configuration is found. This ensures you don't accidentally lose both configurations at once.

**Use Cases**:
- Resetting configuration between releases when changes require clean state
- Switching from local to global configuration (or vice versa)
- Troubleshooting configuration-related issues

### RPC

An `rpc` section is used to configure the connection to the Miden node. It contains the following fields:

- `endpoint`: The endpoint of the Miden node. It can be a specific url (like `"https://rpc.devnet.miden.io"`) or a table with the following fields:
  - `protocol`: The protocol used to connect to the node. It can be either `http` or `https`.
  - `host`: The host of the node. It can be either an IP address or a domain name.
  - `port`: The port of the node. It is an integer.

This field can be set with the `--network` flag when running the `miden-client init` command. For example, to set the testnet endpoint, you can run: `miden-client init --network testnet`.

:::note

- Running the node locally for development is encouraged.
- However, the endpoint can point to any remote node.
  :::

### Store and keystore

The `store_filepath` field is used to configure the path to the SQLite database file used by the client. The `secret_keys_directory` field is used to configure the path to the directory where the keystore files are stored. The default values are `.miden/store.sqlite3` and `.miden/keystore`, respectively, organizing these files within the `.miden` directory structure.

The store filepath can be set when running the `miden-client init` command with the `--store-path` flag.

### Default account ID

The `default_account_id` field contains the default account ID to be used by the client's command when no `account` is provided. It is a hexadecimal string that represents the account ID. The field is optional, and if not set, the client will set it once the first account is created.

By default none is set, but you can set and unset it with:

```sh
miden-client account --default <ACCOUNT_ID> #Sets default account
miden-client account --default none #Unsets default account
```

:::note
The account must be tracked by the client in order to be set as the default account.
:::

You can also see the current default account ID with:

```sh
miden-client account --default
```

### Token symbol map

The `token_symbol_map_filepath` field is used to configure the path to the TOML file that contains the token symbol map. The token symbol map stores the faucet details for different token symbols. The default value is `.miden/token_symbol_map.toml`.

This file must be updated manually with known token symbol mappings. A sample token symbol map file looks like this:

```toml
# This addresses in this file are not real and are only for demonstration purposes.
ETH = { id = "0xa031cc137adecd54", decimals = 18 }
BTC = { id = "0x2f3c4b5e6a7b8c9d", decimals = 8 }
```

The `id` field is the faucet account ID and the `decimals` field is the number of decimals used by the token.

When the client is configured with a token symbol map, any transaction command that specifies an asset can use the token symbol instead of the asset ID. For example, when specifying an asset normally you would use something like:
`1::0x2f3c4b5e6a7b8c9d`

But if the faucet is included in the token symbol map (using the sample above as the mapping), you would use:
`0.00000001::BTC`

Notice how the amount specified when using the token symbol takes into account the decimals of the token (`1` base unit of the token is `0.00000001` for BTC as it uses 8 decimals).

### Remote prover endpoint

The `remote_prover_endpoint` field is used to configure the usage of a remote prover. You can set a remote prover when calling the `miden-client prover` command with the `--remote-prover-endpoint` flag. The prover will be used for all transactions that are executed with the `miden` command. By default, no remote prover is used and all transactions are executed locally.

### Package directory
`Packages` are Miden's native packaging format.
This structure contains the outputs of a compiled project, with all of its corresponding metadata. Specifically, a `Package` may contain the compiled MAST for an `Account Component` in the form of a `Library`.

The `package_directory` field is used to configure the path to the directory where the account components are stored in package (`.masp`) form. The default value is `.miden/packages`.

In this directory you can place the packages used to create the account components. These define the interface of the account that will be created.

For more information on miden packages, see:
- [The mast-package crate](https://github.com/0xMiden/miden-vm/blob/next/crates/mast-package/README.md)
- [The Miden package's status article on the Miden compiler](https://docs.miden.xyz/core-concepts/compiler/)

### Block Delta

The `max_block_number_delta` is an optional field that is used to configure the maximum number of blocks the client can be behind the network.

If not set, the default behavior is to ignore the block difference between the client and the network. If set, the client will check this difference is within the specified maximum when validating a transaction.

```sh
miden-client init --block-delta 256
```

### Environment variables

- `MIDEN_CLIENT_HOME`: Overrides the default global `.miden` directory (`~/.miden`). When set, all commands that reference the global directory will use the specified path instead. This is useful for keeping separate environments or storing the client data in a non-default location. For example:

  ```sh
  export MIDEN_CLIENT_HOME=/path/to/custom/miden
  miden-client init
  ```

  Note that this only affects the **global** directory. If a local `./.miden` directory exists, it still takes precedence over the global one (whether default or overridden).

- `MIDEN_DEBUG`: When set to `true`, enables debug mode on the transaction executor. Any `debug.*` instructions in an executed script then print VM state to standard output to facilitate MASM debugging (see [MASM Debug Output](../debug-output.md)). This variable can be overridden by the `--debug` CLI flag.

### Note Transport

A `note-transport` section is used to configure the connection to the Miden Note Transport node used in the exchange of private notes. It contains the following fields:
- `endpoint`: The endpoint of the Miden Note Transport node;
- `timeout-ms`: The timeout employed in client requests to the node.

> [!Note]
> - Running the node locally for development is encouraged.
> - However, the endpoint can point to any remote node.
