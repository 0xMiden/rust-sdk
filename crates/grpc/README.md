# Miden Client gRPC

Default gRPC clients for the services that a Miden client uses: the node, the note transport service and the remote prover.

- `GrpcClient`, which implements the `NodeRpcClient` trait from `miden-client-core` and talks to a Miden node over gRPC. It rejects node responses that do not match the request
- `RpcMethod` and the `v1` module, with one type per endpoint of the node RPC API version, for callers that choose the API version of each call
- `RpcStreamMethod` and `GrpcClient::subscribe`, which open a server-streaming endpoint and validate each item of the stream
- `Endpoint`, the address of the node, with presets for the public networks
- The protobuf code generated from the node RPC API, and the conversions between its messages and the `miden-client-core` types
- The parsing of the typed errors that the node returns
- `note_transport::GrpcNoteTransportClient`, which implements the `NoteTransportClient` trait from `miden-client-core` and exchanges private notes with the note transport service
- `remote_prover::RemoteTransactionProver`, which sends a transaction witness to a remote prover and returns the proven transaction

On native targets, the clients use the `tonic` transport with TLS. On `wasm32` targets, they use `tonic-web-wasm-client`.

## Quick Start

Add to `Cargo.toml`:

```toml
miden-client-core = { version = "0.18" }
miden-client-grpc = { version = "0.18" }
```

To build for `wasm32`, turn off the default features:

```toml
miden-client-grpc = { version = "0.18", default-features = false }
```

## Usage

```rust
use miden_client_core::rpc::NodeRpcClient;
use miden_client_grpc::{Endpoint, GrpcClient};

let client = GrpcClient::new(&Endpoint::testnet(), 10_000);
let (block_header, _) = client.get_block_header_by_number(None, false).await?;
```

`NodeRpcClient` uses the newest API version that the client supports. To call one endpoint of a specific API version, pass its type to `GrpcClient::call`:

```rust
use miden_client_grpc::{Endpoint, GrpcClient, v1};

let client = GrpcClient::new(&Endpoint::testnet(), 10_000);
let response = client
    .call(v1::GetBlockHeaderByNumber {
        block_num: None,
        include_mmr_proof: true,
        include_protocol_config: false,
    })
    .await?;
```

## Features

| Feature | Description                                                                        |
|---------|------------------------------------------------------------------------------------|
| `std`   | Enables the native `tonic` transport with TLS and the `std` features of the Miden dependencies. On by default. Turn it off for `wasm32`. |

## License
This project is licensed under the MIT License. See the [LICENSE](../../LICENSE) file for details.
