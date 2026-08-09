# Rust Client Library

Rust library, which can be used by other project to programmatically interact with the Miden rollup.

## Adding miden-client as a dependency

In order to utilize the `miden-client` library, you can add the dependency to your project's `Cargo.toml` file:

````toml
miden-client = { version = "0.16.0-alpha.1" }
````

## Crate Features

| Features  | Description |
| --------- | ----------- |
| `tonic`   | Includes `GrpcClient`, a gRPC client to communicate with a Miden node. Uses `tonic` transport with TLS on native targets and `tonic-web-wasm-client` on `wasm32`. **Disabled by default.** |
| `std`     | Enables `std` support and concurrent execution in `miden-tx`. Enabled by default for native targets. |
| `testing` | Enables functions meant for testing environments. **Disabled by default.** |

### Store and RpcClient implementations

The library user can provide their own implementations of `Store` and `RpcClient` traits, which can be used as components of `Client`, though it is not necessary. The `Store` trait is used to persist the state of the client, while the `RpcClient` trait is used to communicate via [gRPC](https://grpc.io/) with the Miden node.

Storage backends are provided as separate crates:
- SQLite: `miden-client-sqlite-store`, based on SQLite. For `std`-compatible environments.
- Web (WASM): See [0xMiden/web-sdk](https://github.com/0xMiden/web-sdk) for browser storage.

## License
This project is [MIT licensed](../../LICENSE).
