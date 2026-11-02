# Miden Client Core

Core traits and types shared by the miden-client crates. This crate holds the contract between the client and its backends.

- The `Store` trait, which a storage backend implements
- The `NodeRpcClient` trait, which a client for the Miden node RPC API implements, and `VerifyingRpcClient`, which wraps any implementation and rejects responses that do not match the request
- The types in the `Store` signatures: note, account and transaction records, filters, settings, and state sync and transaction updates
- The types in the `NodeRpcClient` signatures: the RPC request and response types in `rpc::domain`, `RpcEndpoint`, `AccountStateAt` and the transaction encryption types
- The errors of these types, in the `errors` module
- The RPC data that a store persists: RPC limits and the transaction encryption key
- The note transport cursor and sync state

Applications that use the client should depend on `miden-client`, which re-exports the types of its API. Depend on this crate to implement a store or an RPC client, or to use these contracts directly.

## Quick Start

Add to `Cargo.toml`:

```toml
miden-client-core = { version = "0.18" }
```

To use it in a `no_std` environment, turn off the default features:

```toml
miden-client-core = { version = "0.18", default-features = false }
```

## Features

| Feature   | Description                                                                         |
|-----------|-------------------------------------------------------------------------------------|
| `std`     | Enables the Rust standard library. On by default.                                   |
| `testing` | Enables test utilities, such as building a transaction encryption key without an attestation. |

## License
This project is licensed under the MIT License. See the [LICENSE](../../LICENSE) file for details.
