---
title: Design
sidebar_position: 4
---

The Miden client has the following architectural components:

- [Store](#store)
- [RPC client](#rpc-client)
- [Transaction executor](#transaction-executor)
- [Keystore](#keystore)
- [Note screener](#note-screener)
- [Note reader](#note-reader)
- [Note transport](#note-transport)

:::tip

- The RPC client and the store are Rust traits.
- This allow developers and users to easily customize their implementations.

:::

## Store

The store is central to the client's design.

It manages the persistence of the following entities:

- Accounts; including their state history and related information such as vault assets and account code.
- Transactions and their scripts.
- Notes.
- Note tags.
- Block headers and chain information that the client needs to execute transactions and consume notes.

Because Miden allows off-chain executing and proving, the client needs to know about the state of the blockchain at the moment of execution. To avoid state bloat, however, the client does not need to see the whole blockchain history, just the chain history intervals that are relevant to the user.

The store can track any number of accounts, and any number of notes that those accounts might have created or may want to consume.

## RPC client

The RPC client communicates with the node through a defined set of gRPC methods. The provided client works both in `std` and `wasm` environments.

The available gRPC methods are documented in the [Node gRPC Reference](https://docs.miden.xyz/miden-node/rpc).

## Transaction executor

The transaction executor uses the [Miden VM](https://docs.miden.xyz/core-concepts/miden-vm/) to execute transactions. All transactions run within the [transaction kernel](https://docs.miden.xyz/builder/smart-contracts/transactions/introduction).

When executing, the executor needs access to relevant blockchain history. The executor uses a `DataStore` interface for accessing this data. This means that there may be some coupling between the executor and the store.

## Keystore

The keystore is responsible for storing and managing the private keys of the accounts tracked by the client.

These private keys are used by the executor to sign and authenticate transactions. Implementations for both rust and web keystores are provided.

## Note Screener

The note screener is used to check the consumability of notes by tracked accounts. It performs fast static checks (e.g. checking the inputs for well known notes) and also dry runs of consumption transactions.

It can find the tracked accounts that can consume a note, and whether the note can be consumed at the moment or in the future.

Usage examples for note screening can be found in the [Note screening section](./library.md#note-screening).

## Note Reader

The note reader is used to iterate over the input notes a specific account has already consumed. Notes are read lazily from the store and returned in on-chain consumption order.

Usage examples for the note reader can be found in the [Reading consumed notes section](./library.md#reading-consumed-notes).

## State Sync component

The state sync component encapsulates the logic for dealing with synchronization of the client state with the network. It repeatedly queries the node with sync state requests until the chain tip is reached. On every requests it updates the provided tracked elements (accounts, notes, transactions, etc.) and returns an updated state at the end which can be used to update the store (this component does not modify the store directly).

The component also exposes a specific customizable callback which can be used to react to new note arrivals.

## Note transport

Access to the note transport network to exchange private notes is also provided.
The provided client uses gRPC methods to communicate with the note transport network, working both in `std` and `wasm` environments.

Targeting privacy, notes are primarily exchanged using their tags as identifiers. By default, when notes are created the tag is derived from the recipient account ID, however the tag can also be random.

The system is also prepared for end-to-end encryption (to be implemented).

gRPC methods include:

- `SendNote`: Sends a note to the note transport network. The recipient address is employed to encrypt the outgoing note (to be implemented).
- `FetchNotes`: Fetch notes from the network by note tag. A pagination mechanism using a monotonic-increasing cursor is also employed. The cursor is created by the network and used by the client to reduce the number of fetched notes (to avoid downloading already fetched notes).
