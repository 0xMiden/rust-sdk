# Web3Signer Authenticator

`TransactionAuthenticator` implementation that signs Miden transactions with private keys held in a
[Web3Signer](https://docs.web3signer.consensys.io/) instance.

- Reads the signer's key list on connect and signs for any key on it
- Only the `EcdsaK256Keccak` authentication scheme, the one Web3Signer can produce
- `no_std` + `alloc`, with a built-in HTTP transport behind the default `std` feature

## Adding as a dependency

```toml
miden-client                          = { version = "0.16", features = ["tonic"] }
miden-client-web3signer-authenticator = { version = "0.16" }
```

## Quick Start

The authenticator is passed to `ClientBuilder::authenticator`, and the client signs through the
remote signer from that point on:

```rust
use std::sync::Arc;

use miden_client::ClientBuilder;
use miden_client_web3signer_authenticator::Web3SignerAuthenticator;

let authenticator = Web3SignerAuthenticator::connect("http://127.0.0.1:9000").await?;

let client = ClientBuilder::for_testnet()
    .store(store)
    .authenticator(Arc::new(authenticator))
    .build()
    .await?;
```

The authenticator provides other methods as well:

```rust
use miden_client_web3signer_authenticator::Web3SignerAuthenticator;

let mut authenticator = Web3SignerAuthenticator::connect("http://127.0.0.1:9000").await?;

// Get all public keys held by the authenticator
let public_keys = authenticator.get_public_keys();

// Get a public key by commitment
let public_key = authenticator.get_public_key_by_commitment(public_keys[0].to_commitment());

// Get a public key by the identifier the signer lists it under (its hex-encoded public key)
let public_key = authenticator.get_public_key_by_identifier("0x09b02f8a...");

// The key list is read once, when the authenticator is built, so a key added to the signer
// afterwards is only picked up by reading the list again
authenticator.update_public_keys().await?;
```

## Crate Features

| Feature | Description |
| ------- | ----------- |
| `std`   | Provides `HttpTransport`, the `reqwest`-backed transport used by `connect`. **Enabled by default.** |

## Custom transport

Targets without a `reqwest`-style HTTP client, such as `wasm32`, disable the `std` feature and
implement `SignerTransport` themselves. It has two methods, `get` and `post`, each taking a path to
resolve against the signer's base URL and returning the response body. Build the authenticator with
`Web3SignerAuthenticator::connect_with(transport)`.

## License
This project is [MIT licensed](../../LICENSE).
