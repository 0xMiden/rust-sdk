---
title: Client
sidebar_position: 1
---

# Miden client

The Miden client has two main components:

1. [Miden client library](#miden-client-library).
2. [Miden client CLI](#miden-client-cli).

A browser SDK is also available. It is maintained in a separate repository. See [Web SDK](#web-sdk).

### Miden client library

The Miden client library is a Rust library that can be integrated into projects, allowing developers to interact with the Miden rollup.

The library provides a set of APIs and functions for executing transactions, generating proofs, and managing activity on the Miden network.

### Miden client CLI

The Miden client also includes a command-line interface (CLI) that serves as a wrapper around the library, exposing its basic functionality in a user-friendly manner.

The CLI provides commands for interacting with the Miden rollup, such as submitting transactions, syncing with the network, and managing account data.

More information about the CLI can be found in the [CLI section](./rust-client/cli).

### Web SDK

The browser SDK (`@miden-sdk/miden-sdk`, the React hooks library `@miden-sdk/react`, and `@miden-sdk/vite-plugin`) wraps the Rust library for use in web applications. It is maintained in the [0xMiden/web-sdk](https://github.com/0xMiden/web-sdk) repository.
