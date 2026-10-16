# Miden Client Core

Core traits and types shared by the miden-client crates. This crate holds the contract between the client and its backends.

- The `Store` trait, which a storage backend implements
- The types in the `Store` signatures: note, account and transaction records, filters, settings, and state sync and transaction updates
- The errors of these types, in the `errors` module
- The RPC data that a store persists: RPC limits and the transaction encryption key
- The note transport cursor

Applications that use the client should depend on `miden-client`, which re-exports the types of its API. Depend on this crate to implement a store, or to use the store contract directly.

## Quick Start

Add to `Cargo.toml`:

```toml
miden-client-core = { version = "0.17" }
```

To use it in a `no_std` environment, turn off the default features:

```toml
miden-client-core = { version = "0.17", default-features = false }
```

## Features

| Feature   | Description                                                                         |
|-----------|-------------------------------------------------------------------------------------|
| `std`     | Enables the Rust standard library. On by default.                                   |
| `testing` | Enables test utilities, such as building a transaction encryption key without an attestation. |

## License
This project is licensed under the MIT License. See the [LICENSE](../../LICENSE) file for details.
