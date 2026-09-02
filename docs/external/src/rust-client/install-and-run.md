---
title: Installation
sidebar_position: 1
---

## Software prerequisites

- [Rust installation](https://www.rust-lang.org/tools/install) minimum version 1.96.

## Install the client

Run the following command to install the miden-client:

```sh
cargo install miden-client-cli --locked
```

This installs the `miden-client` binary (at `~/.cargo/bin/miden-client`).

## Run the client

If the install worked correctly, you should be able to check the version by running:

```sh
miden-client --version
```

Once installed, you may run:
```sh
miden-client --help
```

This will show you the available commands and options for the client.

An more in depth tutorial can be fund in the [Getting started section](./get-started).
