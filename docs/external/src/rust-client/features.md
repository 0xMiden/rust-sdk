---
title: Features
sidebar_position: 3
---

The Miden client offers a range of functionality for interacting with the Miden rollup.

### Transaction execution

The Miden client facilitates the execution of transactions on the Miden rollup; allowing users to transfer assets, mint new tokens, and perform various other operations.

### Proof generation

The Miden rollup supports user-generated proofs which are key to ensuring the validity of transactions on the Miden rollup.

To enable such proofs, the client contains the functionality for executing, proving, and submitting transactions.

### Miden network interactivity

The Miden client enables users to interact with the Miden network. This includes syncing with the latest blockchain data and managing account information.

__Note transport__ The client also supports connectivity with the Miden Note Transport network for the exchange of private notes (end-to-end encryption coming soon).

### Note screening

The Miden client supports screening notes against tracked accounts to determine whether they are relevant and when they can be consumed. Applications can use this to filter input notes and prepare consume transactions before execution. More information can be found in the [Note screening section](./library.md#note-screening).

### Account generation and tracking

The Miden client provides features for generating and tracking accounts within the Miden rollup ecosystem. Users can create accounts and track their transaction status.

### Crate features

The `miden-client` crate gates some of the functionality above behind Cargo features:

| Feature | Description |
| ------- | ----------- |
| `tonic` | Includes the gRPC pieces that talk to a node: `GrpcClient`, `RemoteTransactionProver`, the gRPC note transport client, and the `ClientBuilder` methods that wire them up (`for_testnet`, `for_devnet`, `for_localhost`, `grpc_client`). Uses `tonic` transport with TLS on native targets and `tonic-web-wasm-client` on `wasm32`. **Disabled by default.** |
| `std` | Enables `std` support and concurrent execution in `miden-tx`. Enabled by default for native targets. This turns on the `tonic` dependency's transport and TLS features, which is not the same as the `tonic` feature above: gRPC support still has to be requested explicitly. |
| `concurrent` | Enables Rayon-parallel proving without pulling in the rest of `std`, for `wasm32` consumers that cannot use it. Native builds get it through `std`. |
| `testing` | Enables mocks and helpers meant for test environments. **Disabled by default.** |
| `dap` | Enables running a transaction under a Debug Adapter Protocol client instead of proving it. Implies `std`. **Disabled by default.** |
