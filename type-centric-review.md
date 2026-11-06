# Type-centric API review of rust-sdk `next` (107 PRs, 2026-08-10 → 2026-10-06)

## Status as of 2026-11-05

Plan progress against section F (one PR per group):

| PR | Status | Landed as | Covers |
|---|---|---|---|
| 1. perf(store): fix redundant store reads | done | [#2770](https://github.com/0xMiden/rust-sdk/pull/2770), merged 2026-11-04 | P-1 (K15 native `get_unique_note_tags` / `get_account_note_tags`), P-2 (Q4 single-slot witness reads, Q12 `query_storage_header`, Q2+Q3 `StoredAccountHeader` from `query_latest_account_header`, Q6 single `INSERT…SELECT` undo) |
| 2. fix: close correctness gaps | open | — | C-1, C-2, C-3, C-4. C-4's waiter is partly covered by #2683 (`wait_for_consumable_notes` returns the notes and fails on timeout); verify the async sleeps before starting. C-1 still needs the settled-note tag predicate decision. |
| 3. chore: remove unused code | done | [#2681](https://github.com/0xMiden/rust-sdk/pull/2681), merged 2026-10-07 | R-1 |
| 4. refactor(rust-client): type-centric cleanup | open | — | R-2..R-8, R-10, R-11, R-12. Split 4a (sync + store rules) / 4b (transaction + rpc) if huge. |
| 5. refactor(store): sqlite account writer context | open | — | R-9. PR 1 is merged, so the same-files constraint is cleared. |
| 6. refactor(cli): type-centric cleanup | open | — | R-13, R-14, R-15. After PR 4 (uses R-12). Decide `-f` behaviour. |
| 7. test: harness cleanup | done | [#2683](https://github.com/0xMiden/rust-sdk/pull/2683), merged 2026-10-08 | R-16 (node waits and redundant syncs removed), R-17 (fee helpers and `import_account_file` on `TestClient`, `AgglayerScenario`, bench reads the keystore from the client, pre-seeded `mock_client_builder`) |

Related work outside the plan: [#2729](https://github.com/0xMiden/rust-sdk/pull/2729) (merged
2026-10-22) makes `SqliteStore::get_account` read header, vault, storage and code from one
deferred transaction. It touches the same sqlite account read path as PR 5.

Remaining order: 2 → 4 (a then b) → 5 → 6. P-3 (diff-based full account update) and the
deferred items (F1, X12, X7, X15, C16/C17) stay out.

The per-area raw notes (`report-*.md`, `verdict-*.md`) referenced below were session scratch
files and no longer exist. Sections A–F are the reconciled result and are self-contained.

Method: six area reviewers (rpc, transaction, client core, sqlite-store, CLI, tests) read the
current code at `next` 1bcdb1aee for every file touched by the last 107 merged PRs, then six
adversarial defenders re-verified each finding. Only what survived is listed. web-sdk was
grepped at ~/repos/web-sdk ca4b922 where relevant. Per-area raw notes: report-*.md and
verdict-*.md in this directory.

Side observation: `origin/main` has #2663 (remove NTL relay outbox) and #2667 (chunk transport
tag requests) that are not on `origin/next`; next has 5 commits main lacks. The note_transport
findings below were checked against main.

## Principle added 2026-10-06 (applies to every item below)

miden-client is a library. Public API that an integrator could plausibly call is NOT dead code
just because nothing in this repo or web-sdk calls it. In particular `StateSync`,
`Client::apply_state_sync`, `Client::validate_request`, `SyncSummary` helpers and the transport
`health_check` are user-facing building blocks (users may drive a sync and update the store
themselves). Consequences for the findings:

- Removals are limited to items no external user can reach or use: `pub(crate)`/private items,
  unusable duplicates (`store::AccountUpdates` cannot be passed anywhere), impls on types that are
  private outside the `testing` feature (`rpc::generated`), write-only test fields.
- Reshapes that move a public free fn onto its type (F4, F5, C7/K14, K9 get-side) keep the entry
  point and are fine, but each is a BREAKING changelog entry.
- X1 is now "keep as is": `validate_request` is a documented pre-flight check; its sender check
  stays for the error hint. Only the internal duplication claim was wrong, nothing to delete.
- K18 is now "keep and align": `Client::apply_state_sync` must behave like the internal
  `apply_chain_updates` tail (cache the partial MMR), not be removed.
- K2 must keep `StateSync` store-free (witness accounts arrive through `StateSyncInput`); K1 must
  not change the `OnNoteReceived` trait; F8/F1 must not change `SyncedNote::new` / `NoteInfo`
  shape without a changelog entry; X12 ctor `Result` change breaks web-sdk callers.
- Section C below is re-filtered accordingly.

## A. Defects found along the way (fix regardless of the rule)

1. **sqlite `get_unique_note_tags` regression (#2593, HEAD).** The PR deleted SqliteStore's
   native query; the `Store` default (`store/mod.rs:592-611`) now loads a partial account per
   tracked account, i.e. 2N+2 connection interactions per call. Called per sync
   (`sync/mod.rs:334`) and per untracked public note in the screener
   (`note_screener.rs:329`). Fix: restore the native sqlite query.
2. **Storage-map witness over-read (sqlite `rows.rs:193-231`).** For
   `AccountStorageFilter::Root(_)` the second `match` on the filter yields `None`, and
   `query_storage_maps` loads every map entry of every map slot, keeping one. Only caller is
   `data_store/mod.rs:146` on each witness cache miss during tx execution. Fix: derive the map
   slot names from `storage_values`; keep the `StorageMap::new()` fallback for empty maps.
3. **Settled-note tag removal drifts across stores.** sqlite `sync.rs:102-115` removes tags
   for `!is_inclusion_pending()`; web-sdk idxdb `sync/mod.rs:193-197` uses `is_committed()`.
   rust-client already has `TryInto<NoteTagRecord> for &InputNoteRecord` (`tag.rs:165`) but it
   ignores `ExpectedNoteState.tag`, so import.rs:162 special-cases it and four more sites
   rebuild the record. Home: `InputNoteRecord::tag_record()` + `tags_to_remove()` on the
   tracker, used by both stores.
4. **CLI codec skips the network check** (`codecs/mod.rs:47`, #2515 written the same day
   before #2547). `keys --associate/--disassociate` and typed `call` account-id args accept a
   foreign-network bech32 that every other path rejects. Config is in scope at both
   construction points; S.
5. **`std::thread::sleep` inside async fns** in the harness: `common.rs:665`,
   `client.rs:649/689/709`. On the current_thread test runtime this freezes the connection
   driver and all timers 3s per iteration. Siblings already use `tokio::time::sleep`.
6. **Panic reachable from deserialized `TransactionRequest` bytes**
   (`request/mod.rs:190-208` `expect("Recipient should be included…")`): `validate()` checks
   only duplicate input notes; a SendNotes partial whose recipient is absent from
   `expected_output_recipients` panics in `get_outgoing_assets`, `validate_output_note_senders`
   and the web-sdk wrapper. Extend `validate()`; no format change.
7. **CLI faucet metadata** (`account.rs:606-626`) hand-destructures the token config and skips
   the `MAX_DECIMALS` check the library's private `faucet_metadata_from_token_config` enforces;
   `account --show`'s vault table re-implements `format_asset` with a different miss fallback
   (prefix hex vs bech32). Expose the library fn + a tracked-faucet source in the resolver.
8. **Full public-account sync writes O(full state) history rows**
   (`accounts.rs:852-968`): `PublicAccountUpdate::Full` is the common sync path; the path
   archives unchanged rows and runs a NULL-marker follow-up pass. A store-local diff feeding the
   existing patch writers is feasible (AccountPatch::new rejects nonce 0, so not via
   AccountPatch). L.
9. **`NullifierUpdate`** (`rpc/domain/nullifier.rs:13-25`): manual `PartialEq` on nullifier
   only vs derived `Ord` over both fields, stored in a `BTreeSet`. Harmless today; derive both.
10. **Import-by-proof marks `ConsumedExternal` beyond sync height** (`import.rs:295-300`,
    no guard) while `mark_externally_consumed` (`:430-469`, #2453) guards. Transient today,
    but it is what makes inverting the screener/tracker order (see B.K1) unsafe.

Claimed by the first pass, rejected on defence: the "foreign sender escapes execute" bug (the
rule is enforced upstream in `SendNotesTransactionScript::new`; the client copy is dead), the
wasm32 panic on `chain_length` (it is `fixed32`), the mock `advance_blocks` snapshot gap
(unreachable: sync always pins to the live tip).

## B. Rule violations worth fixing (confirmed), ranked within area

### rust-client core (sync / store / note / account)
- **K4** "apply a committed note to a record" written twice (tracker :437-441/:453-457 with an
  `expect`, import.rs:395-403). → `InputNoteRecord::committed_note_received(proof, metadata,
  attachments, header)`; return `&mut` from `expected_note_matching` to delete the expect. S.
- **K5/Q15** tag record home (see A.3). S.
- **K3** three ways to trim+track a block in the partial MMR (`block_header.rs:219-285` hand
  trim with a panicking `expect` at :226, `state_sync.rs:885-926` nodes() diff,
  `data_store/mod.rs:730-768` `MmrProof::with_forest`). `with_forest` trims identically and
  returns an error where the client panics; indices are recoverable from the `nodes()` diff.
  → one `track_block(partial_mmr, header, proof)`; delete `adjust_merkle_path_for_forest`,
  `track_block_in_mmr`, `authenticated_block_nodes`, `fetch_block_header`. Callers:
  transaction/mod.rs:420, batch/mod.rs:325, data_store/mod.rs:426. M.
- **K2** account sync context: `(block_from, chain_tip_header)` through 5–6 fns, proof-pinning
  check copied 3× (sync/mod.rs:533, state_sync.rs:1069, :1255), witness collection on `Client`
  with post-hoc dedupe against `AccountUpdates::with_account_witnesses`. Client placement is
  deliberate (store-free StateSync), so fix via `StateSyncInput` carrying
  `Vec<(AccountId, has_stored_witness)>` and a `SyncTarget` struct inside StateSync. M-L.
- **K1 (reduced)** `NoteId::new(commitment, metadata) == id` in 4 places → 
  `InputNoteRecord::is_committed_as`; drop the redundant `Expected` store scan in the screener
  (:305-315; every metadata-less Expected is Unspent → already in the tracker). Do NOT ask the
  tracker first: the screener's `Unique` lookup is what stops consumed/invalid records being
  re-inserted as Committed.
- **K6** import-by-details matches notes to requests twice (import.rs:519 and :380). S.
- **K11** `FilesystemKeyStore::get_key_sync` does not check file-name commitment == key;
  `list_keys`/`associate_key` re-check, `get_signature`/`get_public_key` do not (#2559). S.
- **K8** `register_account` / `is_allowlist_gated` re-derive "new non-network account". S, low.
- **K12** execution-chain grouping computed twice per sync (`derive_account_commitments`,
  `compute_ordered_nullifiers`). S.
- **K13** `DataStoreCache::ref_block` precondition: dead `ChainTip` fallback (contradicts its
  own comment) at :206/:274 vs `expect` at :521. One `account_state_at() -> Result`. S.
- **K9** `load_protocol_config(&dyn Store, ..)` → `Store::get_protocol_config` default; the
  key fn must stay pub (sqlite writes it inside its sync transaction). S.
- **K14** `get_input_note_with_id_prefix(client, ..)` is a public free fn with 3 CLI callers →
  `Client` method (see C7). `ensure_not_processing` pub with 2 in-file callers.
- Upstream candidates: `validate_account_witness(&AccountWitness, AccountId, &BlockHeader)`
  (#2476), `block_num_from_forest(&PartialMmr)` (#2453).

### transaction / pswap
- **X8** `get_outgoing_assets` (mod.rs:1540-1563) merges a set with itself: the SendNotes
  partials and the rebuilt own notes have identical ids and assets (verified in protocol
  0.17.0). → `TransactionRequest::outgoing_assets()` over the partials; removes an expect. S.
- **X4+X10** `build_input_notes` has one caller which already filters consumed/processing and
  retains authenticated, so its checks are unreachable and its `expect`s depend on the caller;
  `chain_anchor_for_request` re-derives pinned-vs-inferred via `pub(super)
  explicit_input_notes`. → `TransactionRequest::build_input_notes(stored_records)` owns the
  classification; `authenticated_note_blocks()` on top. S.
- **X2** execution tail duplicated between `execute_transaction_with_mode` (:528-571) and
  `execute_transaction_for_batch` (batch/mod.rs:440-468); #2629 had to edit both. No borrow
  obstacle. → `Client::execute_prepared<STORE>(..)` + `PreparedTransaction::register_on`;
  `InMemoryBatchDataStore` needs an `inner()` accessor. M.
- **X3** fee conversion: `FeeAuth::of` runs twice per execution; three free fns over the
  request; `validate_fee_conversion_info_support` and the `Ignored` arm at :1618 are the same
  rule (documented duplicate). → classify once per prepare, `TransactionRequest::attach_…(&FeeAuth, ..)`;
  keep the validate for `validate_request`'s deserialized-only path. M.
- **X1 (as deletion)** `validate_request` has zero callers (workspace, CLI, web-sdk);
  `validate_output_note_senders`, `OutputNoteSenderMismatch` and its `ErrorHint`
  (errors.rs:412) are dead. Delete rather than re-home.
- **X11** `ensure_account_allowed(id, is_allowed: Result<bool>)` + 3-line gate duplicated in
  both submit paths → `Client::ensure_account_allowed(id)`. S.
- **X16** observer notification loop written twice, diverged. S. **X18** infallible
  `TransactionResult::new` returns `Result`, stale doc. S. **X19** `execute_transaction_at`
  takes `&mut self` for no reason. S.
- **X12 (narrow)** zero-asset rule: fits `SwapTransactionData::new` / `PswapTransactionData::new`
  only (P2ID and mint are different rules); web-sdk calls these ctors directly, so flag the
  break.
- **X7 (narrow)** `fetch_public_account_inputs` / `local_account_inputs` take `(store, rpc_api)`;
  home is a small `{store, rpc_api}` struct, not `ClientDataStore` (which carries a cache and
  is not built before `get_foreign_account_inputs` runs).
- **X15** partial-blockchain assembly duplicated between `chain_anchor_at_tip` and
  `BatchBuilder::submit`; batch hand-rolls a missing-header error the shared helper handles.
  `lower_refs` is always empty in practice. M, low.
- Upstream: `extract_pswap_attachment` re-implements `TryFrom<&NoteAttachment> for
  PswapNoteAttachment` that miden-standards 0.17.0 ships (stricter; genuine notes pass both) —
  replace body, drop 3 tests. `notes_from_output(&RawOutputNotes)` pub free fn. `create_tag` +
  Subscription tag built twice (pswap/mod.rs:105, store.rs:245).

### sqlite-store
- **Q4** see A.2. **Q15** see A.3. **Q5** see A.8.
- **Q7** `apply_transaction_in_txn` (transaction.rs:194-223) builds `TransactionRecord` from
  `TransactionStoreUpdate` field by field; web-sdk idxdb does the same today. →
  `TransactionStoreUpdate::to_transaction_record(created_at)` in rust-client. S.
- **Q8** `get_transactions` matches the filter, then `if let TransactionFilter::Query` after
  the query (#2589); ordering contract is documented on the rust-client type. →
  `TransactionFilterQuery::apply(&mut Vec<_>)`. S.
- **Q9** `NoteFilter` → state-discriminant sets inline in the store (web-sdk repeats them);
  only `UNSPENT_STATES` lives in rust-client. → `COMMITTED_STATES`/`CONSUMED_STATES`/… consts.
  S.
- **Q1** account-state writers thread `(tx, account_id_bytes, nonce_val, …)` through 7 fns;
  archive-then-write copied 3× (key arity differs, so per-table statement sets). →
  `AccountStateWriter<'a>`. M.
- **Q2+Q3** latest-header-by-id lookup spelled 5×; `replace_account_header` re-SELECTs
  seed/locked/watched because `require_latest_account_header` discards them. Widen the helper
  to return the full row; KEEP the nonce check at :1099 (sole ordering check on the local-tx
  path). S.
- **Q12** `AccountStorageHeader` assembled twice + ad-hoc slot lookup; `map_slot_roots()` and
  `find_slot_header_by_name()` exist upstream. Keep the sort (`new` requires it). S.
- **Q14** `apply_account_patch` inlines allocate+apply that `apply_forest_update` wraps
  (2-line fix). **Q6** COUNT before INSERT…SELECT (trivial). **Q10** serializer field dup
  (DRY). **Q11** dead `has_client_notes` bool + two statements for one upsert.

### rpc / note_transport
- **F13** `note_id_from_proto` / `note_inclusion_proof_from_proto` are `Ok(v.decode_and_verify()?)`
  wrappers (#2549); delete, 4 sites type-check. Trivial.
- **F3** dead `TryFrom<GetAccountResponse> for AccountProof` (#2661 missed it; #2651 edited it);
  `get_account` builds a 0/1-entry BTreeMap + clones `StorageMapFetch` for
  `AccountDetails::into_domain` → `into_domain(self, &GetAccountRequest)`. S.
- **F10** dead `GrpcNoteTransportClient::health_check` → also removes `health_api`,
  `HealthClient` and the `tonic-health` dependency. S.
- **F7** `AccountStateAt` → `Option<BlockNumber>` matched in tonic_client:641,
  verifying_client:280, plus if-let at transaction/mod.rs:1824 and the inverse `map_or` at
  data_store:207/:275 → `block_num()` + `From<Option<BlockNumber>>`. S.
- **F5** `attestation_commitment` (5 args, only caller passes its own fields) and
  `seal_transaction_inputs(rng, &key, ..)` → methods on the key types. S.
- **F2 (narrow)** `BlockPagination::block_range()` (5 inline copies) and
  `advance_page(Option<PaginationInfo>)` for the 3 loops that unwrap it inline. S.
- **F11** `ApiClient` duplicated per cfg target; `note_transport/grpc.rs` already has the
  `type Service` pattern; 4-5 forwarded fields → a `ConnectConfig`. M.
- **F4** `parse_node_error`/`parse_status_error` are public free fns via re-export →
  associated fns on `EndpointError`; keep the two matches (two different inputs). S.
- **F8** `sync_notes_with_content` default method holds `NoteContentFetch::requests` and the
  attachments fallback that belong on their types. S.
- **F1 (hold)** note transport decodes, re-encodes and re-decodes each fetched note with two
  commitment checks; the redundant copy today is the grpc-side `Verify`/`FetchedNoteMismatch`,
  not `rejoin_note` (e2ee reservation needs the client-side check). Decide e2ee direction first.
- **F14** storage-map cross-page merge in the client loop (vault already uses
  `AccountVaultPatch::merge`). S, low.

### miden-cli
- **C1** `CliConfig::load()` 11×, 8 redundant (root, `network_note_status`, public
  `CliClient::new` are legitimate); network id re-read from TOML per bech32 parse; resolver
  re-parsed per command. Root cause: `from_config` consumes a non-`Clone` config. →
  `CliContext { client, config, network_id, faucets }` built once in `Cli::execute`;
  resolver stores `network_id` (keep `client` as a parameter). L, folds C21/C4:650/C2.
- **C3** `show_note` carries `(Option<Input>, Option<Output>)`: 5 `expect` + 3 `panic!`, three
  result checks, `send` uses a second lookup variant. → `enum StoredNote` + one
  `find_by_id_prefix`; keep the same-prefix-two-notes check. M.
- **C7** three id-prefix lookups in lib.rs + the library's fourth (#2589 added the third) →
  generic helper + `Client::get_*_by_id_prefix` (library-visible). S/M.
- **C4** `execute_transaction` 5-param free fn ×7 call sites, `force`/`delegate_proving`
  declared on 7 structs → flattened `SubmitArgs`; note `consume-notes` alone has `-f`. M.
- **C5** `create_client_account` 7 params; two cmd structs declare 4 identical fields →
  flattened `AccountCreationArgs`. M.
- **C8** see A.7. **C6** see A.4.
- **C9** `load_packages(cli_config, paths)` mixes resolution and reading; `&CliConfig::default()`
  used as a "skip lookup" sentinel; two `slice::from_ref(..).pop().expect`. S.
- **C10** `Cli::execute` matches `Command` twice with a dead "already handled" arm (#2559);
  clap 4.6.7 supports `#[command(flatten)]` on subcommand variants. M.
- **C11** `GrpcClient` built from `RpcConfig` 5× (one with 6 MiB max message, four with the
  4 MiB default) → `RpcConfig::grpc_client()`. Library candidate
  `Client::get_account_or_fetch` removes the two fallbacks. S.
- **C12** import re-reads account code it just wrote. **C13** `export_note` re-fetches +
  expect. **C14** `--list consumable` classified then `try_into().expect`. **C15**
  `Network` → `Endpoint` via String round-trip; note-transport-per-network match in `init`.
  All S.
- **C16** library candidates from `tx --show` (#2589): `StandardTransactionScript::from_root`,
  `TransactionDetails::expiration_block() -> Option`, `input_note_nullifiers: Vec<Nullifier>`.
- **C17** "has BasicWallet" computed twice; no `Address::for_account` in the library. **C19**
  default-account setting raw in 4 files (8 sites). **C21** four address fns share
  `(client, account_id, network_id)`, `.expect` at :122 where the sibling maps the error.
  **C22/C23** `CallTarget::run`, `InfoCmd::execute`. **C18 (narrow)** shared key-length
  classifier; SPKI hack is an upstream gap.

### tests / harness
- **T3** `wait_for_node()` is a no-op at 52 of 54 sites (`into_client` already syncs; the other
  2 are covered by the node script's readiness wait), plus ~26 redundant `sync_state` right
  after `into_client`. Redundant since #1204. Delete. S.
- **T1** 18 hand-rolled `ClientBuilder` chains + 23 copies of the genesis/encryption-key
  epilogue, two already drifted → `create_test_client_builder_for(chain)` +
  `build_mock_client(builder)`. S-M.
- **T4** `funding::fee_faucet_id(&Client<AUTH>)` free fn on a foreign type (#2593);
  `TestClient::chain_charges_fees` reads genesis the same way; `pass_through_fee_asset` reads it
  twice → `TestClient::fee_faucet_id()`. Library candidate `Client::fee_asset_id()`. S.
- **T2 (narrow)** see A.5; plus `TestClient::wait_until(max_blocks, pred)` for ~10 ad-hoc
  loops, drop the two-client `wait_for_committed_note`, `wait_for_blocks(1)` + `sync_state`
  double syncs, `assert!` in a `Result` fn. S-M.
- **T5** 13 manual `add_key` + `add_account` pairs where `AccountSetup::prebuilt` exists;
  `insert_new_ecdsa_*` redundant outright (ECDSA is the default); `foreign_account_with_code`
  re-implements `auth_component()`; two FPI fixture types share ~60 lines. S.
- **T6** agglayer: 3-tuple of clients + `setup_core_accounts(config, 3 clients)` + same
  preamble in 3 tests; `import_account` searches 4 files by an id the caller has →
  `AgglayerScenario`. S.
- **T8** `MockRpcApi::account_commitment_updates` written, never read (since #1074). **T10**
  two `stored_account` helpers re-implement `Client::get_account` (#2642). **T11**
  `is_deployed` nonce rule duplicated (#2455/#2545). **T12** `wait_for_consumable_notes`
  returns a tuple both callers re-convert. **T7** bench re-opens the keystore the client owns
  (deploy/import/export). **T14** mock node `add_note*` dup. **T13** stale docs cite CLI args
  the crate does not have (fee.rs:55, funding.rs:29, transport.rs:21/:127).

## C. Dead code #2661 ("remove unused code") missed — re-filtered

Removable (no external reach): `store::AccountUpdates` (+ re-export; wrong type for every
consumer, web-sdk included), `TryFrom<GetAccountResponse> for AccountProof` (generated module is
private outside `testing`), `MockRpcApi::account_commitment_updates` (private, write-only),
`note_id_from_proto` / `note_inclusion_proof_from_proto` (`pub(crate)`), infallible
`TransactionResult::new -> Result` (signature fix), `execute_transaction_at(&mut self)` → `&self`.
These are PR 3 (branch `igamigo-more-unused-code`).

Keep (public API with plausible external use, unused in-repo is not a reason):
`Client::apply_state_sync` (see K18), `SyncSummary::new_empty/combine_with`,
`Client::validate_request` + `OutputNoteSenderMismatch` hint (see X1),
`GrpcNoteTransportClient::health_check` + `tonic-health`, `CliConfig::is_local/is_global`,
`ForeignAccount::storage_slot_requirements` (web-sdk wraps it).

## D. Rejected on defence
C20 (`note_record_type` is not `Display`: capitalised + handles None), K16 (borrow-forced
setup), Q16 (readers deliberately pub per #2418 review), F9 (superseded by #2663/#2667 on
main), X1-as-bug, F6 wasm panic, X19 `storage_slot_requirements` (web-sdk wraps it), T9
(unreachable), X6/X13/F12/Q13 (private helpers the rule permits), X17 (recipients need two
owners), Q17/K17 (style).

## E. PR plan (grouped: correctness / performance / readability)

Base every branch on `next`. Codes refer to sections A–D above.

### Correctness
| PR | Covers | Crates | Size | Notes |
|---|---|---|---|---|
| C-1 fix(store): one rule for settled-note tag removal | Q15, K5 | rust-client, sqlite-store | S | Pick the predicate (sqlite `!is_inclusion_pending` vs web-sdk `is_committed`); `InputNoteRecord::tag_record()` + `tags_to_remove()` on the tracker; web-sdk follow-up PR. |
| C-2 fix(rust-client): close invariant gaps | X5a, K7, K11, K13, NullifierUpdate | rust-client | S | `validate()` covers recipients; one `mark_externally_consumed` with the sync-height guard; commitment check in `get_key_sync`; `account_state_at() -> Result`; derive `PartialEq`. Changelog. |
| C-3 fix(cli): network check in codec, faucet metadata from library | C6, C8 (+C17) | miden-cli, rust-client | S/M | Expose `FaucetMetadata::from_token_config` + tracked-faucet resolver source; codec takes `network_id`. |
| C-4 fix(tests): async sleeps and one waiter | T2 narrow | test_utils, integration-tests | S | `tokio::time::sleep`; `TestClient::wait_until`; drop two-client waiter; `assert!` → `Err`. |

### Performance
| PR | Covers | Crates | Size | Notes |
|---|---|---|---|---|
| P-1 perf(store): restore native `get_unique_note_tags` | K15 | sqlite-store (+ Store trait) | S | Regression on HEAD (#2593). Optionally return `ClientAccountType` with headers so the default is cheap too. Do first. |
| P-2 perf(store): trim redundant account reads | Q4, Q12, Q2+Q3, Q6 | sqlite-store | S/M | Map slots from `storage_values`; `query_storage_header`; widen `require_latest_account_header`, drop re-SELECT, KEEP nonce check. |
| P-3 perf(store): diff-based full account update | Q5 | sqlite-store | L | Store-local diff into the patch writers (not `AccountPatch`). Write a short design note first. Last. |

### Readability / rule
| PR | Covers | Crates | Size | Notes |
|---|---|---|---|---|
| R-1 chore: remove unused code (follow-up to #2661) | K10, F3 dead impl, T8, F13, X18, X19 | rust-client | S | Open as #2681 (2026-10-06). F10 `health_check`, X1 `validate_request`, K18 `apply_state_sync`, `SyncSummary` helpers are public API and stay. |
| R-2 refactor(sync): note record owns committed-note transitions | K4, K1 reduced, K6 | rust-client | M | `committed_note_received`, `is_committed_as`, drop Expected scan; keep screener-before-tracker order. |
| R-3 refactor(sync): track blocks via `MmrProof::with_forest` | K3 | rust-client | M | One `track_block`; deletes four fns + the panicking expect. |
| R-4 refactor(sync): account sync context | K2, K12 | rust-client | M-L | `SyncTarget` inside StateSync; witness accounts via `StateSyncInput`; `ExecutionOrder` built once. |
| R-5 refactor(transaction): request owns input-note classification | X4, X10, X8 | rust-client | S/M | `build_input_notes(stored_records)`, `authenticated_note_blocks`, `outgoing_assets`; `explicit_input_notes` private again. |
| R-6 refactor(transaction): shared execution tail | X2, X11, X16 (+X15 opt.) | rust-client | M | `execute_prepared` + `register_on`; `Client::ensure_account_allowed`; `notify_transaction_observers`. After R-5 (same file). |
| R-7 refactor(transaction): classify fee auth once | X3 | rust-client | M | `FeeAuth` once per prepare; move (not delete) the deserialized-only validate. |
| R-8 refactor(store): rust-client owns store-side rules | Q7, Q8, Q9, K9 | rust-client, sqlite-store | S/M | `to_transaction_record`, `TransactionFilterQuery::apply`, state consts, `Store::get_protocol_config`. Pair with C-1 for one web-sdk adoption. |
| R-9 refactor(store): sqlite account writer context | Q1, Q14, Q11, Q10 | sqlite-store | M | `AccountStateWriter`; `apply_forest_update` at :498; single upsert for headers. After P-2. |
| R-10 refactor(rpc): move free functions onto their types | F5, F4, F7, F8, F2 narrow, F14, F3 reshape | rust-client | M | Key-type methods, `EndpointError::parse*`, `AccountStateAt::block_num`, `BlockPagination::block_range/advance_page`, `into_domain(&GetAccountRequest)`. |
| R-11 refactor(rpc): one `ApiClient` across targets | F11 | rust-client | M | `type Service` + `ConnectConfig`, pattern from note_transport/grpc.rs. |
| R-12 refactor(rust-client): id-prefix lookups as `Client` methods | C7, K14 | rust-client, miden-cli | S | Generic helper; deletes three CLI copies. Public API change. |
| R-13 refactor(cli): build the command context once | C1, C2 (network_id), C21, C11, C19 | miden-cli | L | `CliContext`; resolver stores `network_id`; `RpcConfig::grpc_client()`; default-account accessors. Do before R-14/R-15. |
| R-14 refactor(cli): `StoredNote` and small command cleanups | C3, C14, C12, C13, C15, C22, C23 | miden-cli | M | |
| R-15 refactor(cli): flatten shared args, package loading, dispatch | C4, C5, C9, C10 | miden-cli | M | `-f` currently only on consume-notes: pick one behaviour, changelog. |
| R-16 test: delete redundant node waits and syncs | T3 | integration-tests, test_utils | S | Mechanical, ~80 lines, zero risk. Any time. |
| R-17 test: harness fixtures | T1, T4, T5, T12, T10, T11, T6, T7, T14 | testing crates, bench | M | Builder helpers; `TestClient::fee_faucet_id`; `AccountSetup::prebuilt`; `AgglayerScenario`; bench uses client keystore. |

Deferred / needs a decision: F1 (e2ee direction for transport note parsing), X12 (ctor `Result` breaks web-sdk calls), X7, X15, C16/C17 library additions.

Suggested order: P-1 → R-1 → C-1..C-4 → P-2 → R-2 ∥ R-5 → R-3, R-6, R-7 → R-8 (+web-sdk) → R-4 → R-10, R-11 → R-12 → R-13 → R-14, R-15 → R-16, R-17 (any time) → P-3.

## F. Agreed grouping (2026-10-06): one PR per group unless huge

Section E's rows are the checklist items inside these PRs, not PRs of their own.

| PR | Contains | Est. size | Notes |
|---|---|---|---|
| 1. perf(store): fix redundant store reads | P-1, P-2 | ~300 lines, sqlite-store + Store trait | First. P-3 (diff-based full update) excluded: L and a design change; own PR later. |
| 2. fix: close correctness gaps across client, store and CLI | C-1, C-2, C-3, C-4 | ~400 lines, 5 crates | C-1 needs the tag predicate decision; C-1 changes rust-client API web-sdk implements → web-sdk follow-up. |
| 3. chore: remove unused code (follow-up to #2661) | R-1 | -154/+37 lines | Open as [#2681](https://github.com/0xMiden/rust-sdk/pull/2681). Public API kept per the principle above. |
| 4. refactor(rust-client): type-centric cleanup of sync, transaction and rpc | R-2..R-8, R-10, R-11, R-12 | huge (2–3k lines) → split in two if needed: 4a sync+store rules (R-2, R-3, R-4, R-8), 4b transaction+rpc (R-5, R-6, R-7, R-10, R-11, R-12) | R-8 pairs with C-1 for one web-sdk adoption; R-12 is a public API change the CLI PR consumes. |
| 5. refactor(store): sqlite account writer context | R-9 | ~500 lines | After PR 1 (same files). |
| 6. refactor(cli): type-centric cleanup | R-13, R-14, R-15 | ~1.5k lines | After PR 4 (uses R-12). Decide `-f` behaviour. |
| 7. test: harness cleanup | R-16, R-17 | ~1k lines, mostly deletions | Any time; independent. |

Order: 1 → 3 → 2 → 4 (a then b) → 5 → 6; 7 whenever. P-3 and the deferred items (F1, X12, X7, X15, C16/C17) stay out.

## G. Resume guide for the pending PRs (written 2026-11-05)

Everything needed to pick up PR 2, 4a, 4b, 5 and 6 without re-reading sections A–F. Each
checklist item names its finding code; the code's paragraph above has the file references
(valid for `next` at 1bcdb1aee, 2026-10-06, re-locate with `git grep` on the identifiers).

### Working rules for every PR
- Branch from `origin/next`, rebase instead of merge. Commits `type(scope): description`.
- Reshapes that move a public free fn onto its type keep the entry point and get a
  `[BREAKING]` changelog entry (F4, F5, C7/K14, K9, R-12).
- Keep: `Client::validate_request` + `OutputNoteSenderMismatch` hint, `Client::apply_state_sync`
  (must cache the partial MMR like `apply_chain_updates`), `SyncSummary::new_empty/combine_with`,
  `GrpcNoteTransportClient::health_check`, `ForeignAccount::storage_slot_requirements`.
- Do not change the `OnNoteReceived` trait (K1), the shape of `SyncedNote::new` / `NoteInfo`
  without a changelog entry (F8/F1), or make `StateSync` depend on the store (K2).
- web-sdk implements `Store` and calls the pswap/swap ctors: any change to `Store` or to
  those ctors needs a web-sdk follow-up (C-1, R-8, X12).
- Verify: `make lint` (clippy on stable, typos, taplo), `make test`, `make test-docs`.
  Mock-chain tests in `crates/testing/miden-client-tests` are package
  `miden-client-unit-tests`; run them one at a time. Unit tests under `sync/` need
  `--features "std dap testing"`. Run `make format` last.
- No changelog entry for test_utils / integration-tests / bench changes; describe them in the
  PR body instead.

### PR 2. fix: close correctness gaps (C-1, C-2, C-3, C-4)
Order inside the PR: C-2 → C-3 → C-4 → C-1 (C-1 carries the web-sdk follow-up).

C-1 settled-note tag removal (A.3, K5/Q15), rust-client + sqlite-store:
- [ ] Decide the predicate: sqlite `sync.rs` removes tags for `!is_inclusion_pending()`,
      web-sdk idxdb `sync/mod.rs` for `is_committed()`. Pick one and write it down in the PR.
- [ ] Add `InputNoteRecord::tag_record() -> Option<NoteTagRecord>` that honours
      `ExpectedNoteState.tag`; replace the `TryInto<NoteTagRecord> for &InputNoteRecord` in
      `note/tag.rs` and the special case in `import.rs` plus the four sites that rebuild the
      record by hand.
- [ ] Add `tags_to_remove()` on the note update tracker; sqlite's `apply_state_sync` and the
      idxdb store call it instead of filtering records themselves.
- [ ] web-sdk follow-up PR: adopt the two helpers in the idxdb store.

C-2 invariant gaps, rust-client:
- [ ] A.6: extend `TransactionRequest::validate()` so every SendNotes partial has its recipient
      in `expected_output_recipients`; today a deserialized request panics in
      `get_outgoing_assets` / `validate_output_note_senders` / the web-sdk wrapper. No wire
      format change. Add a deserialization test with a missing recipient.
- [ ] A.10 + K7: one `mark_externally_consumed` with the sync-height guard; `import.rs`
      (import by proof) currently marks `ConsumedExternal` without the guard that
      `mark_externally_consumed` (#2453) has.
- [ ] K11: `FilesystemKeyStore::get_key_sync` must check that the file-name commitment matches
      the loaded key; `list_keys` / `associate_key` already do, `get_signature` /
      `get_public_key` do not.
- [ ] K13: `DataStoreCache::account_state_at() -> Result`; delete the dead `ChainTip` fallback
      that contradicts its own comment and the later `expect`.
- [ ] A.9: derive `PartialEq` (and keep derived `Ord`) on `rpc::domain::nullifier::NullifierUpdate`;
      drop the manual nullifier-only `PartialEq`.
- [ ] Changelog entries for the validate() hardening and the keystore check.

C-3 CLI, miden-cli + rust-client:
- [ ] A.4 / C6: the bech32 codec in `codecs/mod.rs` takes `network_id` and rejects foreign
      networks, so `keys --associate/--disassociate` and typed `call` account-id args behave
      like every other path. Config is in scope at both construction points.
- [ ] A.7 / C8: expose `FaucetMetadata::from_token_config` from the library (it enforces
      `MAX_DECIMALS`); the CLI stops hand-destructuring the token config in `account.rs`.
- [ ] C8 / C17: give the faucet resolver a tracked-faucet source so `account --show`'s vault
      table uses the same `format_asset` and miss fallback (bech32, not prefix hex).
- [ ] CLI test for a foreign-network id on `keys --associate`.

C-4 harness, test_utils + integration-tests (no changelog):
- [ ] A.5: replace `std::thread::sleep` inside async fns (`test_utils/common.rs`,
      `test_utils/client.rs`) with `tokio::time::sleep`.
- [ ] T2: add `TestClient::wait_until(max_blocks, pred)` and migrate the ~10 ad-hoc loops;
      delete the two-client `wait_for_committed_note`; remove `wait_for_blocks(1)` +
      `sync_state` double syncs; `assert!` inside `Result` fns becomes `Err`.
- [ ] Check first what #2683 already did to `wait_for_consumable_notes` (returns the notes,
      fails on timeout) and to the sync calls, so this does not redo it.

### PR 4a. refactor(sync): type-centric cleanup of sync and store rules (R-2, R-3, R-4, R-8)
Order: R-8 → R-2 → R-3 → R-4 (R-4 is the risky one, keep it last and reviewable on its own
commit). Pair R-8 with C-1 if PR 2 has not landed, so web-sdk adopts once.

R-8 rust-client owns store-side rules:
- [ ] Q7: `TransactionStoreUpdate::to_transaction_record(created_at)`; sqlite
      `apply_transaction_in_txn` and idxdb stop building `TransactionRecord` field by field.
- [ ] Q8: `TransactionFilterQuery::apply(&mut Vec<TransactionRecord>)`; sqlite
      `get_transactions` applies it instead of matching `TransactionFilter::Query` after the
      query. The ordering contract stays documented on the rust-client type.
- [ ] Q9: `COMMITTED_STATES` / `CONSUMED_STATES` / … consts next to `UNSPENT_STATES`; stores
      stop inlining discriminant sets.
- [ ] K9: `Store::get_protocol_config` default method replaces `load_protocol_config(&dyn Store, ..)`;
      the settings key fn stays `pub` because sqlite writes it inside its sync transaction.

R-2 note record owns committed-note transitions:
- [ ] K4: `InputNoteRecord::committed_note_received(proof, metadata, attachments, header)`;
      use it from the tracker and from `import.rs`; `expected_note_matching` returns `&mut` so
      the tracker's `expect` goes away.
- [ ] K1: `InputNoteRecord::is_committed_as(&NoteId)` replaces the four
      `NoteId::new(commitment, metadata) == id` sites; delete the redundant `Expected` scan in
      the screener (every metadata-less Expected is Unspent, so it is already in the tracker).
- [ ] K6: import-by-details matches notes to requests once, not in both `import.rs` sites.
- [ ] HARD CONSTRAINT: keep the screener-before-tracker order in sync. The screener's `Unique`
      lookup is what stops consumed or invalid records from being re-inserted as Committed.
      Do not ask the tracker first.

R-3 one way to track a block in the partial MMR:
- [ ] K3: `track_block(partial_mmr, header, proof)` built on `MmrProof::with_forest` (trims
      identically, returns an error instead of panicking; node indices come from the `nodes()`
      diff).
- [ ] Delete `adjust_merkle_path_for_forest` (panicking `expect`), `track_block_in_mmr`,
      `authenticated_block_nodes`, `fetch_block_header`. Callers to migrate: `transaction/mod.rs`,
      `batch/mod.rs`, `data_store/mod.rs`.
- [ ] Changelog: the panic on an inconsistent forest becomes an error.

R-4 account sync context:
- [ ] K2: `SyncTarget { block_from, chain_tip_header }` inside `StateSync`; the proof-pinning
      check lives once on it (today copied in `sync/mod.rs`, `state_sync.rs` ×2).
- [ ] K2: `StateSyncInput` carries `Vec<(AccountId, has_stored_witness)>`; delete the post-hoc
      dedupe against `AccountUpdateTracker::with_account_witnesses` on `Client`. `StateSync`
      stays store-free.
- [ ] K12: build the execution-chain grouping once per sync and share it between
      `derive_account_commitments` and `compute_ordered_nullifiers`.
- [ ] Mind PR #2660 (`AccountUpdateTracker`, same-nonce race fix) which touches
      `state_sync.rs` and `state_sync_update.rs`: rebase on it or wait for it.

### PR 4b. refactor(transaction, rpc): type-centric cleanup (R-5, R-6, R-7, R-10, R-11, R-12)
Order: R-5 → R-6 → R-7 (same file, `transaction/mod.rs`), then R-10 → R-11 → R-12.

R-5 request owns input-note classification:
- [ ] X4 + X10: `TransactionRequest::build_input_notes(stored_records)` owns the
      consumed/processing/authenticated classification; the current free fn has one caller that
      pre-filters, so its checks are unreachable and its `expect`s depend on the caller.
- [ ] `TransactionRequest::authenticated_note_blocks()` replaces the pinned-vs-inferred
      re-derivation in `chain_anchor_for_request`; `explicit_input_notes` becomes private again.
- [ ] X8: `TransactionRequest::outgoing_assets()` over the SendNotes partials;
      `get_outgoing_assets` merged a set with itself (identical ids and assets, verified on
      protocol 0.17.0). Removes an `expect`.

R-6 shared execution tail:
- [ ] X2: `Client::execute_prepared<STORE>(..)` + `PreparedTransaction::register_on`; used by
      `execute_transaction_with_mode` and `execute_transaction_for_batch` (#2629 had to edit
      both). `InMemoryBatchDataStore` gets an `inner()` accessor. No borrow obstacle.
- [ ] X11: `Client::ensure_account_allowed(id)` replaces `ensure_account_allowed(id, Result<bool>)`
      plus the 3-line gate copied in both submit paths.
- [ ] X16: one `notify_transaction_observers`; the two loops have diverged, diff them first.
- [ ] X15 optional: share the partial-blockchain assembly between `chain_anchor_at_tip` and
      `BatchBuilder::submit` (`lower_refs` is always empty in practice).

R-7 fee auth classified once:
- [ ] X3: `FeeAuth::of` once per prepare; `TransactionRequest::attach_fee_conversion_info(&FeeAuth, ..)`
      replaces the three free fns over the request.
- [ ] Move (do not delete) `validate_fee_conversion_info_support`: `validate_request` needs it
      for the deserialized-only path. Drop the duplicated `Ignored` arm.

R-10 rpc free functions onto their types (all `[BREAKING]` where the fn was public):
- [ ] F5: `attestation_commitment` (5 args, sole caller passes its own fields) and
      `seal_transaction_inputs(rng, &key, ..)` become methods on the key types.
- [ ] F4: `parse_node_error` / `parse_status_error` become associated fns on `EndpointError`
      (keep both matches, two different inputs).
- [ ] F7: `AccountStateAt::block_num() -> Option<BlockNumber>` + `From<Option<BlockNumber>>`;
      replaces the matches in `tonic_client`, `verifying_client`, `transaction/mod.rs` and the
      inverse `map_or` in `data_store`.
- [ ] F2: `BlockPagination::block_range()` (5 inline copies) and `advance_page(Option<PaginationInfo>)`
      for the 3 paging loops.
- [ ] F8: `NoteContentFetch::requests()` and the attachments fallback move out of the
      `sync_notes_with_content` default method onto their types.
- [ ] F3: `AccountDetails::into_domain(self, &GetAccountRequest)`; `get_account` stops building a
      0/1-entry BTreeMap and cloning `StorageMapFetch`.
- [ ] F14 optional: storage-map cross-page merge on the patch type, as the vault already does
      with `AccountVaultPatch::merge`.

R-11 one `ApiClient`:
- [ ] F11: `type Service` per cfg target (pattern in `note_transport/grpc.rs`) and a
      `ConnectConfig` for the forwarded fields; delete the duplicated struct.

R-12 id-prefix lookups:
- [ ] C7 + K14: generic prefix helper and `Client::get_input_note_by_id_prefix` /
      `get_account_by_id_prefix` / `get_transaction_by_id_prefix` (names to settle); delete the
      three CLI copies in `lib.rs` and the public free fn `get_input_note_with_id_prefix`.
      `[BREAKING]`. `ensure_not_processing` becomes private.
- [ ] Upstream candidates, not for this PR: `validate_account_witness(..)` (#2476),
      `block_num_from_forest(&PartialMmr)` (#2453), `TryFrom<&NoteAttachment> for
      PswapNoteAttachment` from miden-standards replacing `extract_pswap_attachment`.

### PR 5. refactor(store): sqlite account writer context (R-9)
- [ ] Q1: `AccountStateWriter<'a>` owning `(tx, account_id, nonce, ..)`; the archive-then-write
      sequence exists 3× with different key arity, so keep per-table statement sets inside the
      writer. Expect ~500 lines in `crates/sqlite-store/src/account/`.
- [ ] Q14: `apply_account_patch` calls `apply_forest_update` instead of inlining allocate+apply.
- [ ] Q11: delete the dead `has_client_notes` bool; one upsert statement for headers.
- [ ] Q10: deduplicate the serializer fields.
- [ ] Rebase note: #2770 (PR 1) and #2729 (one-snapshot `get_account`) already changed the
      account read path in this directory; diff against them before starting.
- [ ] Out of scope: P-3 / A.8 (diff-based `PublicAccountUpdate::Full` writes). Needs a design
      note first; a store-local diff feeding the patch writers, not via `AccountPatch::new`
      (rejects nonce 0).

### PR 6. refactor(cli): type-centric cleanup (R-13, R-14, R-15)
Needs R-12 from PR 4b. Order: R-13 → R-14 → R-15.

R-13 command context built once:
- [ ] C1: `CliContext { client, config, network_id, faucets }` built in `Cli::execute`; root
      cause is `from_config` consuming a non-`Clone` config. The three legitimate extra
      `CliConfig::load()` sites are the root, `network_note_status` and the public `CliClient::new`.
- [ ] C1: the address resolver stores `network_id`; `client` stays a parameter.
- [ ] C21: the four address fns share `(client, account_id, network_id)`; map the error instead
      of the `expect`.
- [ ] C11: `RpcConfig::grpc_client()`; today 5 constructions, one with a 6 MiB message limit and
      four with the 4 MiB default. Library candidate `Client::get_account_or_fetch` removes the
      two fallbacks.
- [ ] C19: one accessor pair for the default account setting (raw in 4 files, 8 sites).

R-14 `StoredNote` and small command cleanups:
- [ ] C3: `enum StoredNote { Input(..), Output(..) }` + one `find_by_id_prefix`; keep the
      same-prefix-two-notes check; removes 5 `expect` + 3 `panic!` from `show_note`; `send`
      uses the same lookup.
- [ ] C12: import does not re-read the account code it just wrote. C13: `export_note` does not
      re-fetch + expect. C14: `--list consumable` keeps the classified value instead of
      `try_into().expect`. C15: `Network` → `Endpoint` without the String round trip; one
      note-transport-per-network match in `init`. C22/C23: `CallTarget::run`, `InfoCmd::execute`.

R-15 shared args, package loading, dispatch:
- [ ] C4: `SubmitArgs` (flattened) replaces the 5-param `execute_transaction` free fn at 7 call
      sites and the `force` / `delegate_proving` fields declared on 7 structs.
- [ ] DECISION: `-f` exists only on `consume-notes`. Either add it everywhere through
      `SubmitArgs` or drop it; changelog either way.
- [ ] C5: `AccountCreationArgs` (flattened) replaces the 7-param `create_client_account`.
- [ ] C9: split `load_packages` into resolution and reading; remove the `&CliConfig::default()`
      sentinel and the two `slice::from_ref(..).pop().expect`.
- [ ] C10: one `match` in `Cli::execute`; clap 4.6.7 supports `#[command(flatten)]` on
      subcommand variants, so the dead "already handled" arm goes.
- [ ] Not in this PR (library additions, decide separately): C16 `StandardTransactionScript::from_root`,
      `TransactionDetails::expiration_block() -> Option`, `input_note_nullifiers: Vec<Nullifier>`;
      C17 `Address::for_account`; C18 shared key-length classifier.

### Deferred, needs a decision before any PR
- F1: note transport decodes, re-encodes and re-decodes each fetched note with two commitment
  checks. The redundant copy is the grpc-side `Verify` / `FetchedNoteMismatch`; `rejoin_note`'s
  client-side check is reserved for e2ee. Decide the e2ee direction first.
- X12: zero-asset rule into `SwapTransactionData::new` / `PswapTransactionData::new` only;
  the ctors would return `Result`, which breaks web-sdk callers.
- X7: `fetch_public_account_inputs` / `local_account_inputs` take `(store, rpc_api)`; home is a
  small `{ store, rpc_api }` struct, not `ClientDataStore`.
