# Enforce state-consistency preconditions on every store update

Store mutations today apply updates computed against an in-memory snapshot without verifying that the stored state still matches the snapshot the update was derived from. The headline example: `apply_transaction` records `init_account_state = executed_transaction.initial_account().initial_commitment()` in the transaction details, and passes the transaction's *claimed* initial account state as the patch basis to `apply_account_patch` — but never checks `stored_latest.commitment == tx.initial_state` before applying. The update is applied onto whatever happens to be in `latest_account_headers` at write time. Between execution and persistence, a sync, another client process, or a second transaction can have advanced the stored state; the write then silently produces a state that corresponds to no on-chain reality. The same pattern (guards are absent, nonce-only, or trust the caller) recurs across the mutation surface.

The invariant: **every store mutation that was derived from a prior state must name that state, and the store must refuse to apply it if the named basis doesn't match what's stored — a compare-and-swap discipline at the store boundary.**

## Motivation

- An update applied onto a state it wasn't derived from persists data corresponding to **no on-chain reality** — silent, compounding corruption that surfaces much later as a wrong balance or an inexplicable proof failure, with the causal write long gone.
- The interleavings are real, not theoretical: sync and transaction-apply race within one process, two processes can share one database file (the faucet incident), and multi-device usage is coming.
- The cost asymmetry makes this a bargain: the basis data already flows through the write path (`TransactionDetails.init_account_state` exists today) — the check is one comparison inside an already-open SQL transaction. The only expensive part is deciding to enforce it, and that decision gets more expensive after external stores implement the unguarded contract.
- Typed conflicts convert "mysterious wrong state later" into "retry with fresh state now" — the difference between a support ticket and a handled error.

## Plan

- **Audit every mutating store entry point** (`apply_transaction`, `apply_transaction_batch`, `apply_state_sync`, `update_account`, `upsert_input_notes`, note-state updates, `undo_account_state`) and classify: (a) has a basis and checks it, (b) has a basis and doesn't check it, (c) is genuinely absolute (idempotent upserts, e.g. code/scripts by content hash) and needs no check. Document the classification on the `Store` trait docs as part of the atomicity contract.
- **Account CAS:** `apply_transaction` verifies `stored_latest.commitment == update.init_account_state` inside the same SQL transaction as the write (the data is already in `TransactionDetails` — the check is nearly free). `apply_sync_account_patch` gets the same commitment-basis check (currently nonce-only; detailed in WORK-04).
- **Note-state transitions:** `upsert_input_notes` and the sync path overwrite note state unconditionally; the note state machine defines legal transitions — enforce expected-current-state on write so a stale writer can't regress a `Consumed` note to `Committed`.
- **Typed conflict errors:** violations return `StaleStateBasis { entity, expected, found }`-style variants (joins WORK-02's structured `StoreError` work), never generic `DatabaseError` strings. Callers can then distinguish retry-after-refresh from corruption.
- **Atomicity of check + write:** the precondition read must happen inside the same SQL transaction (native) / same store transaction (WASM) as the mutation — a check-then-write across transactions is still a race. On native this is already the shape (`with_forest_snapshot` wraps a tx); the check just moves inside it.
- **Client-side recovery:** on `StaleStateBasis` from a transaction apply, the client surfaces a typed error telling the user to re-execute against fresh state (do NOT auto-re-execute — the transaction was proven against the old state). On sync-path conflicts, self-heal via full-state fetch (WORK-04).
- **Tests:** for each mutation with a basis, a test that advances the stored state between derivation and apply and asserts a typed conflict with the store untouched. The WORK-04 proptest's interleavings cover the account paths; extend the model to note-state writes.

## Relationship to WORK-04

WORK-04 owns the account status state machine and sync convergence; this generalizes its CAS discipline into a store-wide write contract covering notes, transactions, and any future mutation. Implement the account checks once — under whichever lands first — and cross-reference.

A shared implementation vehicle: restructure store updates (starting with `StateSyncUpdate`) as a guarded changeset — plain upserts only for append-only/content-addressed data, `{basis, final}` CAS entries for stateful rows, post-write verification where a node commitment exists. The precondition becomes a property of the update type rather than a per-writer discipline, and re-applying a changeset after a crash is idempotent (stored already equals final → no-op). Note the in-memory transition enforcement that already exists (input-note state objects, `TransactionRecord::commit_transaction`/`discard_transaction` refusing to leave finalized states) protects only single-process flows — the store-boundary check is what covers two writers on one database file.

Cross-links: WORK-02 (typed StoreError variants), WORK-04 (account basis checks + recovery), WORK-07 (same never-partially-apply discipline for sync).
