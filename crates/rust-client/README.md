# Rust Client Library

Rust library, which can be used by other project to programmatically interact with the Miden rollup.

## Adding miden-client as a dependency

In order to utilize the `miden-client` library, you can add the dependency to your project's `Cargo.toml` file:

````toml
miden-client = { version = "0.16.0-alpha.1", features = ["tonic"] }
````

Talking to a node requires the `tonic` feature, which is not part of the default set. Leave it out only when supplying your own `NodeRpcClient` and `TransactionProver` implementations.

## Crate Features

| Features  | Description |
| --------- | ----------- |
| `tonic`   | Includes the gRPC pieces that communicate with a Miden node: `GrpcClient`, `RemoteTransactionProver`, the gRPC note transport client, and the `ClientBuilder` methods that wire them up (`for_testnet`, `for_devnet`, `for_localhost`, `grpc_client`). Uses `tonic` transport with TLS on native targets and `tonic-web-wasm-client` on `wasm32`. **Disabled by default.** |
| `std`     | Enables `std` support and concurrent execution in `miden-tx`. Enabled by default for native targets. It turns on the `tonic` dependency's transport and TLS features, which is not the same as the `tonic` feature above: gRPC support still has to be requested explicitly. |
| `concurrent` | Enables Rayon-parallel proving in `miden-tx` without the rest of `std`, for `wasm32` consumers that cannot enable it. Native builds get this through `std`. |
| `testing` | Enables functions meant for testing environments. **Disabled by default.** |
| `dap`     | Enables running a transaction under a Debug Adapter Protocol client instead of proving it. Implies `std`. **Disabled by default.** |

### Store and RpcClient implementations

The library user can provide their own implementations of `Store` and `RpcClient` traits, which can be used as components of `Client`, though it is not necessary. The `Store` trait is used to persist the state of the client, while the `RpcClient` trait is used to communicate via [gRPC](https://grpc.io/) with the Miden node.

Storage backends are provided as separate crates:
- SQLite: `miden-client-sqlite-store`, based on SQLite. For `std`-compatible environments.
- Web (WASM): See [0xMiden/web-sdk](https://github.com/0xMiden/web-sdk) for browser storage.

## License
This project is [MIT licensed](../../LICENSE).
