# Miden client

[![LICENSE](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/0xMiden/rust-sdk/blob/HEAD/LICENSE)
[![test](https://github.com/0xMiden/rust-sdk/actions/workflows/test.yml/badge.svg)](https://github.com/0xMiden/rust-sdk/actions/workflows/test.yml)
[![build](https://github.com/0xMiden/rust-sdk/actions/workflows/build.yml/badge.svg)](https://github.com/0xMiden/rust-sdk/actions/workflows/build.yml)
[![RUST_VERSION](https://img.shields.io/badge/rustc-1.88+-lightgray.svg)](https://www.rust-lang.org/tools/install)
[![crates.io](https://img.shields.io/crates/v/miden-client)](https://crates.io/crates/miden-client)

This repository contains the Miden client, which provides a way to execute and prove transactions, facilitating the interaction with the Miden rollup.

### Status

The Miden client is still under heavy development and the project can be considered to be in an *alpha* stage. Many features are yet to be implemented and there is a number of limitations which we will lift in the near future.

## Overview

The Miden client currently consists of two components:

- `miden-client` library, which can be used by other project to programmatically interact with the Miden rollup. You can find more information about the library in the [Rust client Library](./crates/rust-client/README.md) section.
- `miden-client-cli`, which is a wrapper around the library exposing its functionality via a simple command-line interface (CLI). You can find more information about the CLI in the [Miden client CLI](./bin/miden-cli/README.md) section.

The browser-focused SDK (`@miden-sdk/miden-sdk`, the React hooks library `@miden-sdk/react`, and the `@miden-sdk/vite-plugin`) lives in a dedicated repository at [0xMiden/web-sdk](https://github.com/0xMiden/web-sdk).

The client's main responsibility is to maintain a partial view of the blockchain which allows for locally executing and proving transactions. It keeps a local store of various entities that periodically get updated by syncing with the node.

For more info check:

- [Getting started](https://docs.miden.xyz/builder/tools/clients/rust-client/get-started/)
- [CLI Reference](https://docs.miden.xyz/builder/tools/clients/rust-client/cli/)
- [Configuration](https://docs.miden.xyz/builder/tools/clients/rust-client/cli/cli-config)
- [Online Documentation](https://docs.miden.xyz/builder/tools/clients/)

## Workspace structure

The workspace is organized as follows:
- The `bin` folder contains crates that are meant to be compiled into binaries (like the CLI).
- The `crates` folder contains the library crates that are meant to be used as dependencies (like the Rust client library).

### Makefile

We use `make` to encapsulate some tasks, such as running lints and tests. You can check out [Makefile](./Makefile) for all available tasks or just run the following command:

```bash
make
```

## Testing

To test the project's code, we provide both unit tests (which can be run with `cargo test`) and integration tests. For more info on integration tests, refer to the [integration testing document](./bin/integration-tests/README.md)

## Contributing

Interested in contributing? Check [CONTRIBUTING.md](./CONTRIBUTING.md).

## License
This project is [MIT licensed](./LICENSE).
