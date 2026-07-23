---
title: Library
sidebar_position: 5
---

To use the Miden client library in a Rust project, include it as a dependency.

In your project's `Cargo.toml`, add:

```toml
miden-client = { version = "0.11" }
```

## Client instantiation

The recommended way to create a client is using the `ClientBuilder`. For standard networks, use the pre-configured constructors:

```rust
use std::sync::Arc;
use miden_client::builder::ClientBuilder;
use miden_client_sqlite_store::SqliteStore;

// Create store
let sqlite_store = SqliteStore::new("path/to/store".try_into()?).await?;
let store = Arc::new(sqlite_store);

// Build client for testnet (pre-configured RPC, prover, and note transport)
let client = ClientBuilder::for_testnet()
    .store(store)
    .filesystem_keystore("path/to/keys")?
    .build()
    .await?;
```

Other network constructors are available:
- `ClientBuilder::for_testnet()` - Pre-configured for Miden testnet
- `ClientBuilder::for_devnet()` - Pre-configured for Miden devnet
- `ClientBuilder::for_localhost()` - Pre-configured for local development

For custom configurations, use `ClientBuilder::new()` and configure each component:

```rust
use std::sync::Arc;
use miden_client::builder::ClientBuilder;
use miden_client::rpc::{Endpoint, GrpcClient};
use miden_client_sqlite_store::SqliteStore;

// Create store
let sqlite_store = SqliteStore::new("path/to/store".try_into()?).await?;
let store = Arc::new(sqlite_store);

// Setup the gRPC endpoint
let endpoint = Endpoint::new("https".into(), "localhost".into(), Some(57291));

let client = ClientBuilder::new()
    .grpc_client(&endpoint, None)
    .store(store)
    .filesystem_keystore("path/to/keys")?
    // Optional: custom prover via .prover(Arc::new(prover))
    // Optional: note transport via .note_transport(Arc::new(nt_client))
    // Optional: custom source manager via .source_manager(Arc::new(sm)) — only
    //   needed when compiling scripts outside the client with an external
    //   `Assembler`; pass the same `Arc` to both so source spans align.
    .build()
    .await?;
```

## Create local account

With the Miden client, you can create and track any number of public and local accounts. For local accounts, the state is tracked locally, and the rollup only keeps commitments to the data, which in turn guarantees privacy.

The `AccountBuilder` can be used to create a new account with the specified parameters and components. The following code creates a new local account:

```rust
let key_pair = SecretKey::with_rng(client.rng());

let new_account = AccountBuilder::new(init_seed) // Seed should be random for each account
    .account_type(AccountType::Private)
    .with_auth_component(AuthRpoFalcon512::new(key_pair.public_key()))
    .with_component(BasicWallet)
    .build()?;
keystore.add_key(&AuthSecretKey::RpoFalcon512(key_pair), new_account.id()).await?;
client.add_account(&new_account, false).await?;
```
Once an account is created, it is kept locally and its state is automatically tracked by the client.

To create a public account, specify `AccountType::Public`:

```Rust
let key_pair = SecretKey::with_rng(client.rng());
let anchor_block = client.get_latest_epoch_block().await.unwrap();

let new_account = AccountBuilder::new(init_seed) // Seed should be random for each account
    .anchor((&anchor_block).try_into().unwrap())
    .account_type(AccountType::Public)
    .with_auth_component(AuthRpoFalcon512::new(key_pair.public_key()))
    .with_component(BasicWallet)
    .build()?;
keystore.add_key(&AuthSecretKey::RpoFalcon512(key_pair), new_account.id()).await?;
client.add_account(&new_account, false).await?;
```

The account's state is also tracked locally, but during sync the client updates the account state by querying the node for the most recent account data.

### Network accounts

A network account is a public account that the node drives automatically: it consumes matching notes on the network's behalf via network transactions (NTX). A network account is built by using the `NetworkAccount::builder` method, which takes a standardized allowlist of note script roots. The node uses that allowlist to identify the account and route only allowlisted notes to it; the auth procedure additionally enforces that consumed notes are allowlisted and that no transaction script runs.

```rust
let network_account = NetworkAccount::builder(init_seed, allowed_note_script_roots)?
    .with_component(/* your contract component */)
    .build_with_schema_commitment()?;
client.add_account(&network_account, false).await?;

// Deploy with an empty (scriptless) transaction: `AuthNetworkAccount` forbids transaction
// scripts, and its auth procedure bumps the nonce from 0 to 1, which registers the account.
let deploy = TransactionRequestBuilder::new().build()?;
let tx_id = client.submit_new_transaction(network_account.id(), deploy).await?;
```

After deployment the account is a network account, so the node rejects user-submitted transactions against it; all further state changes happen through network transactions.

## Execute transaction

In order to execute a transaction, you first need to define which type of transaction is to be executed. This may be done with the `TransactionRequest` which represents a general definition of a transaction. Some standardized constructors are available for common transaction types.

Here is an example for a `pay-to-id` transaction type:

```rust
// Define asset
let faucet_id = AccountId::from_hex(faucet_id)?;
let fungible_asset = FungibleAsset::new(faucet_id, *amount)?.into();

let sender_account_id = AccountId::from_hex(bob_account_id)?;
let target_account_id = AccountId::from_hex(alice_account_id)?;
let payment_description = PaymentNoteDescription::new(
    vec![fungible_asset.into()],
    sender_account_id,
    target_account_id,
);

let transaction_request = TransactionRequestBuilder::new().build_pay_to_id(
    payment_description,
    None,
    NoteType::Private,
    client.rng(),
)?;

// Execute transaction. No information is tracked after this.
let transaction_execution_result = client.new_transaction(sender_account_id, transaction_request.clone()).await?;

// Prove and submit the transaction, which is stored alongside created notes (if any)
client.submit_transaction(transaction_execution_result).await?
```

You can decide whether you want the note details to be public or private through the `note_type` parameter.
You may also customize the transaction request with the other `TransactionRequestBuilder` methods. This allows you to run custom code, with custom note arguments and additional output/input notes as well.

### Input note script trust policy

When the transaction consumes input notes, by default the client only executes notes whose scripts match one of the protocol's [standard note types](https://docs.rs/miden-standards/latest/miden_standards/note/enum.StandardNote.html) (P2ID, P2IDE, SWAP, MINT, or BURN). Notes carrying any other script are rejected before execution. To consume notes with custom scripts, opt in on the request builder:

- `TransactionRequestBuilder::trusted_input_note_script_roots(roots)` to allow specific script roots.
- `TransactionRequestBuilder::allow_unlisted_note_scripts()` to allow any script root, intended for clients that surface unknown scripts to the user behind their own approval flow.

See `NoteScriptTrustPolicy` for the full set of variants.

## Note screening

### When to use note screening

You can use note screening when you need to decide whether a note is relevant to the accounts tracked by the client. Screening checks whether each tracked account can consume the note now or at a future block.

Screening may run trial transaction executions, so it is not free. Use it when you need consumability information for planning, filtering, or building a consume transaction.

### Use the Client helpers first

For notes already tracked by the client, you should usually start with the helper methods on `Client`. These cover the common case without creating a `NoteScreener` directly.

```rust
use miden_client::note::NoteConsumptionStatus;

// Return all committed input notes that at least one tracked account may consume.
let consumable_notes = client.get_consumable_notes(None).await?;

for (note, accounts) in consumable_notes {
    for (account_id, status) in accounts {
        if matches!(status, NoteConsumptionStatus::Consumable) {
            // This account can consume the note at the current sync height.
            println!("{} can consume {}", account_id, note.id().to_hex());
        }
    }
}
```

### Obtain a screener

When you need to check notes that are not already covered by the client helpers, you can obtain a screener from the client.

```rust
// Build a screener configured with the client's store and RPC client.
let screener = client.note_screener();
```

You can also pass custom transaction arguments to the screener via `with_transaction_args`. The screener uses them during its trial executions, which lets it evaluate consumability under the same conditions you will use when actually consuming. For example:

```rust
use std::collections::BTreeMap;

use miden_client::Word;
use miden_client::note::NoteId;
use miden_client::transaction::{AdviceMap, TransactionArgs};

// Per-note arguments passed to the note script.
let note_args: BTreeMap<NoteId, Word> = BTreeMap::from([(note_id, custom_args)]);
let tx_args = TransactionArgs::new(AdviceMap::default()).with_note_args(note_args);

let screener_with_args = client.note_screener().with_transaction_args(tx_args);
```

### Check one note

To check one note, call `can_consume`.

```rust
use miden_client::note::{Note, NoteConsumptionStatus};

// Fetch the input note from the store.
let input_note_record = client.get_input_note(note_id).await?.unwrap();
let note: Note = input_note_record.try_into()?;

let account_statuses = screener.can_consume(&note).await?;

for (account_id, status) in account_statuses {
    match status {
        NoteConsumptionStatus::Consumable => {
            // The note can be consumed now by this account.
            println!("{account_id} can consume {}", note.id().to_hex());
        },
        NoteConsumptionStatus::ConsumableAfter(block_number) => {
            // The note becomes consumable at a later block.
            println!("{account_id} can consume this note after block {block_number}");
        },
        _ => {
            // Other statuses explain why the note is not immediately consumable.
            println!("{account_id}: {status:?}");
        },
    }
}
```

### Check many notes

When you have several notes, use `can_consume_batch` to check them all in one pass.

```rust
use std::collections::BTreeMap;

use miden_client::note::{Note, NoteConsumability, NoteId};
use miden_client::store::NoteFilter;

// Fetch committed input notes from the store.
let input_note_records = client.get_input_notes(NoteFilter::Committed).await?;

let notes: Vec<Note> = input_note_records
    .iter()
    .cloned()
    .map(TryInto::try_into)
    .collect::<Result<_, _>>()?;

// Check all notes with one executor setup.
let notes_by_id: BTreeMap<NoteId, Vec<NoteConsumability>> =
    screener.can_consume_batch(&notes).await?;

for (note_id, account_statuses) in notes_by_id {
    println!("{} has {} possible consumers", note_id.to_hex(), account_statuses.len());
}
```

### Check consumability for one account

If you already know which account will consume the notes, use `check_notes_consumability`. This is useful when planning a multi-note consume transaction for a known account.

```rust
use miden_client::note::{Note, NoteConsumptionInfo};
use miden_client::store::NoteFilter;

// Fetch committed input notes from the store.
let input_note_records = client.get_input_notes(NoteFilter::Committed).await?;

let notes: Vec<Note> = input_note_records
    .iter()
    .cloned()
    .map(TryInto::try_into)
    .collect::<Result<_, _>>()?;

// Find the largest subset that can execute together for this account.
let consumption_info: NoteConsumptionInfo = screener
    .check_notes_consumability(account_id, notes)
    .await?;

for successful_note in &consumption_info.successful {
    // These notes can be included together in the consume transaction.
    println!("can consume {}", successful_note.id().to_hex());
}

for failed_note in &consumption_info.failed {
    // Failed notes include the note and the execution error.
    println!("cannot consume {}: {}", failed_note.note.id().to_hex(), failed_note.error);
}
```

## Reading consumed notes

### When to use the note reader

Use the note reader when you need to iterate over the input notes a specific account has already consumed, for example to build a consumption history or reconcile past activity.

`InputNoteReader` reads lazily from the store, so creating a reader does not run a query. Since the reader queries the local store, sync the client first to see the latest consumptions. Notes are returned in on-chain consumption order, first by block number, then by the account's transaction order within each block.

### Iterate over an account's consumed notes

Obtain a reader from the client and call `next` until it returns `None`. Each call to `next` runs one store query.

```rust
let mut reader = client.input_note_reader(account_id);

while let Some(note) = reader.next().await? {
    // Use the consumed input note.
}
```

### Restrict to a block range

Configure the reader with `in_block_range` to return only notes consumed within an inclusive block range. `reset` returns the reader to the beginning without changing its consumer account or block range.

```rust
use miden_client::block::BlockNumber;

let mut reader = client
    .input_note_reader(account_id)
    .in_block_range(BlockNumber::from(0u32), BlockNumber::from(100u32));

while let Some(note) = reader.next().await? {
    // Use the consumed input note.
}

// Start another pass over the same notes.
reader.reset();
```
