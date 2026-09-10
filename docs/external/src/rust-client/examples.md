---
title: Examples
sidebar_position: 7
---

:::note
For a complete example on how to run the client and submit transactions to the Miden node, refer to the [`Getting started documentation`](https://docs.miden.xyz/builder/tools/clients/rust-client/get-started/#prerequisites).
:::

## Prover Fallback Pattern

When using a remote prover, network issues or server errors may cause proving to fail. A common pattern is to configure the client with a remote prover by default and fall back to local proving when remote proving fails.

`RemoteTransactionProver` requires the `tonic` feature (see [Crate features](./features.md#crate-features)).

```rust
use std::sync::Arc;
use miden_client::{
    ClientError,
    RemoteTransactionProver,
    builder::ClientBuilder,
    transaction::{LocalTransactionProver, ProvingOptions},
};

// Create provers
let remote_prover = Arc::new(RemoteTransactionProver::new("https://prover.example.com"));
let local_prover = Arc::new(LocalTransactionProver::new(ProvingOptions::default()));

// Build client with remote prover as default
let mut client = ClientBuilder::new()
    .prover(remote_prover.clone())
    .store(store)
    .rpc(rpc)
    .authenticator(authenticator)
    .build()
    .await?;

// Build transaction request
let tx_request = /* ... build your transaction request ... */;

// Submit with fallback: try remote prover first, fall back to local on proving error
let tx_id = match client.submit_new_transaction(account_id, tx_request.clone()).await {
    Ok(id) => id,
    Err(ClientError::TransactionProvingError(_)) => {
        println!("Remote proving failed, falling back to local prover...");
        client
            .submit_new_transaction_with_prover(account_id, tx_request, local_prover.clone())
            .await?
    }
    Err(e) => return Err(e.into()),
};

println!("Transaction submitted: {}", tx_id);
```
