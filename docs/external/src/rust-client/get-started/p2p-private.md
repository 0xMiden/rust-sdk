---
title: Private peer-to-peer transfer
sidebar_position: 3
---

In this section, we show you how to make private transactions and send funds to another account using the Miden client.

:::info Important: Prerequisite steps

- You should have already followed the [prerequisite steps](index.md#prerequisites) and [create account](create-account-use-faucet) documents.
- You should _not_ have reset the state of your local client.
  :::

## Create a second account

:::tip
Remember to use the [Miden client documentation](https://docs.miden.xyz/builder/tools/clients/rust-client/cli/) for clarifications.
:::

1. Create a second account to send funds with. Previously, we created a wallet account (`Account A`). Now, create another wallet (`Account B`) using the following command:

   ```sh
   miden-client new-wallet
   ```

2. List and view the newly created accounts with the following command:

   ```sh
   miden-client account -l
   ```

3. You should see two accounts:

  <!-- ![Result of listing miden accounts](../img/get-started/two-accounts.png) -->

## Transfer assets between accounts

1. Now we can transfer some of the tokens we received from the faucet to our second `Account B`.

   To do this, run:

   ```sh
   miden-client transfer --sender <regular-account-id-A> --target <regular-account-id-B> --asset 50::<faucet-account-id> --note-type private
   ```

   :::note
   The faucet account ID can be found on the [Miden faucet website](https://testnet.miden.io/) under the title **Miden faucet**.
   :::

   This generates a private Pay-to-ID (`P2ID`) note containing `50` assets, transferred from one account to the other.

2. First, sync the accounts.

   ```sh
   miden-client sync
   ```

3. Get the second note id.

   ```sh
   miden-client notes
   ```

4. Have the second account consume the note.

   ```sh
   miden-client consume-notes --account <regular-account-ID-B> <input-note-id>
   ```

   :::tip
   It's possible to use a short version of the note id: 7 characters after the `0x` is sufficient, e.g. `0x6ae613a`.
   :::

   You should now see both accounts containing faucet assets with amounts transferred from `Account A` to `Account B`.

5. Check the second account:

   ```sh
   miden-client account --show <regular-account-ID-B>
   ```

   <!-- ![Result of listing miden accounts](../img/get-started/account-b.png) -->

6. Check the original account:

   ```sh
   miden-client account --show <regular-account-ID-A>
   ```

   <!-- ![Result of listing miden accounts](../img/get-started/account-a.png) -->

Wanna do more? [Sending public notes](p2p-public)

## Using the note transport network

The steps above assume that the client owns both accounts. To exchange notes with other users, the note transport network can be used.
For this the sender (`Account A`) will need the address (bech32 string) of the recipient (`Account B`).
After creating the note (step 1 above), get the created note ID with `miden-client notes --list`. Then send that note through the note transport network,
```sh
miden-client notes --send <note-id> <address-B>
```
Then the recipient can fetch that note using `miden-client sync`, or more specifically,
```sh
miden-client notes --fetch
```
The note will then be available to be consumed.

:::note

The client will fetch notes for tracked note tags.
By default, note tags are derived from the recipient's account ID. However these can also be random to increase privacy.
In this case, to track a specific tag, run `miden-client tags --add <tag>`.

:::

## Congratulations!

You have successfully configured and used the Miden client to interact with a Miden rollup and faucet.

You have performed basic Miden rollup operations like submitting proofs of transactions, generating and consuming notes.

For more information on the Miden client, refer to the [Miden client documentation](https://docs.miden.xyz/builder/tools/clients/).

## Clear data

All state is maintained in `store.sqlite3`, located in the directory defined in the `miden-client.toml` file.

To clear all state, delete this file. It recreates on any command execution.
