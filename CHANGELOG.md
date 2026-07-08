# Changelog

## Unreleased

### Features

* [FEATURE][rust] A client that only watches a public account now recovers notes the account consumed authenticated, even when it never tracked them by tag. During sync it reads the note references the node attaches to the account's transactions, fetches each note body by id, and surfaces it through `InputNoteReader`. Requires node `0.15.1` ([#2300](https://github.com/0xMiden/miden-client/pull/2300)).

### Breaking Changes

* [BREAKING][type][rust] `rpc::domain::transaction::TransactionRecord` gained a non-public field, so it can no longer be constructed with a struct literal outside the crate ([#2300](https://github.com/0xMiden/miden-client/pull/2300)).
* [BREAKING][store] The SQLite store now stores account IDs as serialized `BLOB` columns instead of hex `TEXT` ([#2309](https://github.com/0xMiden/rust-sdk/pull/2309)).
* [BREAKING][param][store] `Store::insert_block_header` now takes a `nodes` argument and persists the header with its MMR authentication nodes in a single transaction; the standalone `Store::insert_partial_blockchain_nodes` is removed. Header-only inserts (e.g. genesis) pass an empty slice ([#2294](https://github.com/0xMiden/rust-sdk/pull/2294)).
* [BREAKING][behavior][store] The `ConsumedExternal` note-metadata layout added in [#2308](https://github.com/0xMiden/rust-sdk/pull/2308) is now the only supported serialized format. The backward-compatible decoding of the older metadata-less layout is removed, so existing stores are not compatible and must be recreated ([#2313](https://github.com/0xMiden/rust-sdk/pull/2313)).

### Fixes

* [FIX][rust] Storing an authenticated block header now persists the header and its MMR authentication nodes in a single store transaction, so an interrupted write can no longer leave a tracked block without the MMR nodes needed to rebuild the `PartialMmr` ([#2294](https://github.com/0xMiden/rust-sdk/pull/2294)).
* [FIX][rust] RPC endpoint parsing now rejects endpoint strings that omit either the protocol or host. ([#2266](https://github.com/0xMiden/miden-client/pull/2266))
* [FIX][rust] State sync now re-verifies a tracked private account's commitment mismatch against the witness `get_account` returns. The witness is checked against the synced block's account root before locking the account, so a node can no longer durably lock it with a forged `sync_transactions` commitment ([#2260](https://github.com/0xMiden/rust-sdk/pull/2260)).
* [FIX][rust] State sync now range-checks `sync_transactions` records to `(current, chain_tip]`, rejecting out-of-range records that could forge transaction commit heights ([#2252](https://github.com/0xMiden/rust-sdk/pull/2252)).
* [FIX][rust] `Endpoint` parsing now strips a trailing slash from the host of no-port endpoints such as `http://host/`, matching the cleanup already applied when a port is present ([#2268](https://github.com/0xMiden/rust-sdk/pull/2268)).
* [FIX][rust] `NodeRpcClient::get_block_header_by_number` and `get_block_by_number` now reject responses whose block number does not match the requested one with `RpcError::InvalidResponse` ([#2270](https://github.com/0xMiden/rust-sdk/pull/2270)).
* [FIX][rust] `NodeRpcClient::get_notes_by_id` now rejects responses containing a note whose ID was not requested with `RpcError::InvalidResponse` ([#2283](https://github.com/0xMiden/rust-sdk/pull/2283)).
* [FIX][rust] `NodeRpcClient::sync_nullifiers` now rejects responses containing a nullifier whose prefix was not requested with `RpcError::InvalidResponse` ([#2282](https://github.com/0xMiden/rust-sdk/pull/2282)).
* [FIX][rust] `NodeRpcClient::sync_notes` now rejects responses containing a note whose tag was not requested with `RpcError::InvalidResponse` ([#2284](https://github.com/0xMiden/rust-sdk/pull/2284)).
* [FIX][rust] Public account sync now binds `get_account` responses to the SyncMMR target block, rejecting snapshots from a different block, account, or account root ([#2255](https://github.com/0xMiden/miden-client/pull/2255)).

## 0.15.4 (TBD)

### Changes

* [rust] Bumped dependencies: Miden VM crates (`miden-core`, `miden-processor`, `miden-prover`, `miden-assembly`, etc.) to `0.23.5`, and `miden-node-proto-build` and `miden-remote-prover-client` to `0.15.1` ([#2301](https://github.com/0xMiden/rust-sdk/pull/2301)).

### Fixes

* [FIX][store] Add metadata to ConsumedExternal notes so that they can be findable by their `NoteId`. The change is store-compatible because records written by older clients (the metadata-less layout) still decode, reading back with no metadata as before ([#2308](https://github.com/0xMiden/rust-sdk/pull/2308)).

## 0.15.3 (2026-07-02)

### Enhancements

* [FEATURE][cli] `miden-cli call` now accepts advice map entries supplied via `--inputs-path/-i <FILE.toml>` in the same TOML format as `exec` ([#2244](https://github.com/0xMiden/miden-client/pull/2244)).
* [FEATURE][rust] The gRPC client now accepts responses up to 15% above the node's 4 MiB payload budget by default, and `GrpcClient::with_max_decoding_message_size` lets callers raise the decode ceiling further. The CLI raises its own ceiling to 6 MiB to cover large `SyncTransactions` responses. This prevents syncs from failing with a "decoded message length too large" error when a node response slightly exceeds the previous hard 4 MiB limit ([#2298](https://github.com/0xMiden/miden-client/pull/2299)).

## 0.15.2 (2026-06-18)

### Features

* [FEATURE][rust] Added PSWAP chain tracking: the client now follows a local creator's partial-swap order across foreign partial fills during sync, surfacing each reconstructed payback as a consumable input note and letting the creator reclaim the current tip. New `Client` API: `pswap_lineages`, `pswap_lineages_for`, `pswap_active_lineages`, `pswap_lineage`, and `build_pswap_cancel_by_order` ([#2231](https://github.com/0xMiden/miden-client/pull/2231)).
* [FEATURE][rust] Added `Client::send_private_note_with_block_hint`, which relays a sender-provided `after_block_num` so recipients get deterministic delivery instead of relying on receiving side lookback. ([#2262](https://github.com/0xMiden/miden-client/issues/2262))

### Changes

* [rust] Bumped `miden-note-transport-proto-build` to `0.4.1`. Notes imported from the note transport layer now use the provided `after_block_num` when present, falling back to the 20-block lookback window otherwise. `NoteInfo` gained a `block_hint: Option<BlockNumber>` field (plus a `NoteInfo::new` constructor) and `NoteTransportClient` gained a `send_note_with_block_hint` method (defaulting to `send_note`, so existing implementors keep compiling).  ([#2262](https://github.com/0xMiden/miden-client/issues/2262))

## 0.15.1 (2026-06-16)

### Enhancements

* [FEATURE][rust] Re-exported `miden-agglayer` as `miden_client::agglayer`. ([#2253](https://github.com/0xMiden/miden-client/issues/2253))

## 0.15.0 (2026-06-12)

### Fixes

* [FIX][rust] `Client::execute_transaction` no longer writes to the store before execution: the request's input notes and output note scripts are persisted only after the transaction executes successfully and is applied, so a failed execution leaves the store unchanged ([#2222](https://github.com/0xMiden/rust-sdk/pull/2222)).
* [FIX][rust] `Client::send_private_note` is now durable across transient NTL failures: the relay payload is persisted to a durable outbox (a `Vec<NoteInfo>` under the `note_transport_outbox` settings key) before the transport call, so a failed or interrupted `send_note` no longer drops the note. `Client::sync_note_transport` retries the outbox on each sync (the receiver dedupes by note id) and a failing relay no longer blocks the sync; the new `Client::flush_relay_outbox()` lets callers drive retries directly ([#2127](https://github.com/0xMiden/rust-sdk/pull/2127)).
* [FIX] Fixed `derive_account_commitments` to return the final account commitment when multiple transactions for the same account are committed in the same block ([#2164](https://github.com/0xMiden/rust-sdk/pull/2164)).
* [FIX] Stopped state sync from aborting when the node reports a stale (non-monotonic) header for a rapidly-advancing account: such updates are now skipped instead of failing with a nonce error ([#2216](https://github.com/0xMiden/rust-sdk/pull/2216)).
* [FIX] Preserve a fungible asset's callback flag when the store replays a vault delta, fixing a `ConflictingRoots` error when consuming callback-bearing (e.g. agglayer-minted) assets ([#2225](https://github.com/0xMiden/rust-sdk/pull/2225)).
* [FIX] Fixed the `sync_notes_with_details` to fetch the attachments for private notes ([#2214](https://github.com/0xMiden/rust-sdk/pull/2214)).
* [rust] Expanded validation for output notes before executing a `TransactionRequest`. ([#89](https://github.com/0xMiden/wallet-adapter/issues/89))
* [FIX][rust] `Client::fetch_all_private_notes` now drains the full backlog across multiple server-paginated responses instead of returning after a single batch. Needed once the note-transport server (`0xMiden/note-transport-service#77`) caps each `fetch_notes` response at `FETCH_NOTES_BATCH_SIZE` rows — previously the function silently returned only the first batch, contradicting its documented "fetches all notes" semantics. Companion deterministic regression test (`fetch_all_private_notes_drains_across_batches`) uses a new `MockNoteTransportNode::with_max_batch(n)` constructor to exercise multi-batch drain. ([#2095](https://github.com/0xMiden/rust-sdk/pull/2095))
* [FIX][rust] Fixed the `dap` feature build by bumping `miden-debug`/`miden-debug-engine` to 0.8.1 and `miden-core` to 0.23.2, aligning the debugger crates with the `miden-core` APIs they call. ([#2189](https://github.com/0xMiden/rust-sdk/pull/2189))

### Changes

* [BREAKING][rust] Added a `Subscription(Word)` variant to `NoteTagSource`. ([#2248](https://github.com/0xMiden/rust-sdk/pull/2248))
* [rust] Added `Store::apply_settings_mutations` for batched `settings` writes. ([#2248](https://github.com/0xMiden/rust-sdk/pull/2248))
* [BREAKING][param][rust] `NodeRpcClient::get_block_by_number()` now takes an `include_proof: bool` parameter to control whether the block proof is included in the response. ([#1991](https://github.com/0xMiden/rust-sdk/pull/1991))
* [BREAKING][param][rust] `NodeRpcClient::sync_chain_mmr()` replaced `block_to: Option<BlockNumber>` with `upper_bound: SyncTarget` to match the RPC definition. Use `SyncTarget::CommittedChainTip` for previous default behavior (`None`), or `SyncTarget::BlockNumber(num)` for a specific block number. ([#1991](https://github.com/0xMiden/rust-sdk/pull/1991))
* [BREAKING][rust] Added `submit_proven_batch` to `NodeRpcClient` trait. ([#2075](https://github.com/0xMiden/rust-sdk/pull/2075))
* [BREAKING][param][cli] `address add` now takes `<ACCOUNT_ID> <BECH32_ADDRESS>` instead of `<ACCOUNT_ID> <INTERFACE> [TAG_LEN]`. Use the new `address encode` subcommand to build a bech32 string from `<ACCOUNT_ID> <INTERFACE> [TAG_LEN]`. ([#2115](https://github.com/0xMiden/rust-sdk/pull/2115))
* [BREAKING][rust] `StateSync` no longer takes an `Option<Arc<dyn Store>>`. `StateSyncInput::accounts` is now a `Vec<AccountSyncHint>` (header + `AccountStorageHeader`); when hints cover the account's map slots `StateSync` issues a single `get_account_proof` for non-oversized accounts, and when new map slots appear on-chain it only fetches the missing ones. The `Store` trait method `get_account_map_slot_names` was replaced with `get_account_storage_header`. ([#2132](https://github.com/0xMiden/rust-sdk/pull/2132))
* [BREAKING] `NodeRpcClient::get_account_details` now fetches a public account's storage maps in a single `/GetAccount` request and returns `Option<Account>`. No longer returns data for private accounts; instead use `NodeRpcClient::get_account` to fetch private account's commitment. ([#2215](https://github.com/0xMiden/rust-sdk/pull/2215)).
* [BREAKING] Removed the account storage-layout sync hints; `StateSyncInput::accounts` now takes a `Vec<AccountHeader>` ([#2215](https://github.com/0xMiden/rust-sdk/pull/2215)).
* [BREAKING][type][rust] `BasicFungibleFaucet` is now a unit struct; token symbol/decimals/max-supply moved to a new `FungibleTokenMetadata` component built via `FungibleTokenMetadata::builder`. ([#2145](https://github.com/0xMiden/rust-sdk/pull/2145))
* [BREAKING][behavior][cli] `account new-faucet` now requires a `[fungible-faucet-metadata]` block (typed `symbol`, `decimals`, `max_supply`, optional `name`) in the init data file passed via `-i`, replacing the previous `["miden::standards::fungible_faucets::metadata"]` section with stringly-typed values. ([#2145](https://github.com/0xMiden/rust-sdk/pull/2145))
* [BREAKING][behavior][all] Note scripts must now use the package-style header `@note_script` + `pub proc main … end` instead of the bare `begin … end`, following the upstream protocol bump. ([#2145](https://github.com/0xMiden/rust-sdk/pull/2145))
* [BREAKING][type][rust] Token-policy components in `account::component` were redesigned: removed `MintAuthControlled`, `MintOwnerControlled`, `BurnAuthControlled`, `BurnOwnerControlled` (and their `*Config` variants); faucets now install a single `TokenPolicyManager` configured with `PolicyAuthority` + `MintPolicyConfig` / `BurnPolicyConfig`, plus standalone `MintAllowAll` / `MintOwnerOnly` / `BurnAllowAll` / `BurnOwnerOnly` policy components. Construct via `AccountBuilder::with_components(TokenPolicyManager::new(...))`. ([#2145](https://github.com/0xMiden/rust-sdk/pull/2145))
* [BREAKING][type][rust] `NoteScript::root()` now returns `NoteScriptRoot` instead of `Word`. Use `Word::from(root)` (or `root.into()`) where a `Word` is required. `NoteScriptRoot` is re-exported from `miden_client::note`. ([#2145](https://github.com/0xMiden/rust-sdk/pull/2145))
* [BREAKING][rename][rust] `FeeParameters::native_asset_id()` renamed to `fee_faucet_id()`. ([#2145](https://github.com/0xMiden/rust-sdk/pull/2145))
* [BREAKING][rust] Removed `NodeRpcClient::check_nullifiers`, `RpcEndpoint::CheckNullifiers`, `EndpointError::CheckNullifiers`, and `CheckNullifiersError` after the upstream node dropped the `CheckNullifiers` gRPC method. Use `NodeRpcClient::sync_nullifiers` to retrieve nullifier updates. ([#2145](https://github.com/0xMiden/rust-sdk/pull/2145))
* [BREAKING][behavior][cli] `token_symbol_map.toml` requires the `id` field to be a bech32 address; hex `AccountId`s are no longer accepted. Convert existing entries by copying the bech32 address from `account list`. ([#2159](https://github.com/0xMiden/rust-sdk/pull/2159))
* Added a `Client::import_watched_account_by_id` method to track an external account state without syncing notes ([#2143](https://github.com/0xMiden/rust-sdk/pull/2143)).
* Removed limit on accounts and note tags that can be tracked by the client ([#2170](https://github.com/0xMiden/rust-sdk/pull/2170)).
* [BREAKING] Updated the `sync_notes` and `sync_transactions` to return directly the fetched updates. Removed `TransactionsInfo` and `NoteSyncInfo` structs ([#2170](https://github.com/0xMiden/rust-sdk/pull/2170)).
* [BREAKING][param][rust,store] `InputNoteRecord::new` takes a `NoteAttachments` argument; input notes persist attachments (new `attachments` column on `input_notes`) ([#2203](https://github.com/0xMiden/rust-sdk/pull/2203)).
* [BREAKING][param][rust] `build_wallet_id` dropped its trailing `is_mutable: bool` (code mutability isn't encoded in the account ID) ([#2203](https://github.com/0xMiden/rust-sdk/pull/2203)).
* [BREAKING][behavior][cli] `new-account`/`new-wallet` `--account-type` (`-t`) now accepts only `private`/`public`; legacy faucet/mutability values and `--mutable` are removed. Faucet-vs-regular is derived from packages — a `FungibleFaucet` component yields a fungible faucet with an implicit `TokenPolicyManager` ([#2203](https://github.com/0xMiden/rust-sdk/pull/2203)).
* [BREAKING][type][rust] `Client::import_notes` returns `Vec<NoteDetailsCommitment>` (was `Vec<NoteId>`), since metadata-less imports have no `NoteId` yet — resolve via `Client::get_input_note_by_commitment` ([#2203](https://github.com/0xMiden/rust-sdk/pull/2203), [#2235](https://github.com/0xMiden/rust-sdk/pull/2235)).
* [BREAKING] Reworked the `GetAccount` surface on `NodeRpcClient`: replaced `get_account_proof` with `get_account(account_id, GetAccountRequest)` and added `resolve_oversize_vault` / `resolve_oversize_storage_maps` helpers. `GetAccountRequest` bundles the previous positional args ([#2202](https://github.com/0xMiden/rust-sdk/pull/2202)).
* [BREAKING][rust] `NodeRpcClient::get_note_script_by_root` now returns `Option<NoteScript>` (`None` when the node has no script for the requested root) instead of erroring when the script is absent ([#1840](https://github.com/0xMiden/rust-sdk/pull/1840)).
* [BREAKING] `miden_client::note` re-exports updated to match the protocol's split of attachment data off `NoteMetadata`: removed `NoteAttachmentKind` and `NoteMetadataHeader`, added `NoteAttachmentHeader`, `NoteAttachments`, and `PartialNoteMetadata`. ([#2185](https://github.com/0xMiden/rust-sdk/pull/2185))
* [BREAKING] `CommittedNoteMetadata` simplified to a single `Full(NoteMetadata)` variant; the `Header { sender, note_type, tag, attachment_kind }` variant is removed because sync responses now always carry full metadata (attachment content is still fetched separately via `GetNotesById`, but is no longer part of `NoteMetadata`). Callers no longer need to handle the header-only case. ([#2185](https://github.com/0xMiden/rust-sdk/pull/2185))
* [BREAKING] `TransactionRequestBuilder::build_pswap_create` now takes `note_attachment: Option<NoteAttachment>` instead of `NoteAttachment`. Pass `None` when there is nothing to attach (previously `NoteAttachment::default()`). ([#2185](https://github.com/0xMiden/rust-sdk/pull/2185))
* [BREAKING] `account list`, `account show`, and `account new-faucet` now read and build the new `FungibleFaucet` component (multi-slot) instead of the standalone `TokenMetadata` storage item. Faucet accounts created with the previous component layout are no longer recognized; new faucets are constructed via `FungibleFaucet::builder` rather than the `basic-fungible-faucet` package. ([#2185](https://github.com/0xMiden/rust-sdk/pull/2185))
* [BREAKING] Note attachments are no longer carried on the note-transport wire format (only `NoteHeader` + serialized `NoteDetails`). ([#2185](https://github.com/0xMiden/rust-sdk/pull/2185))
* [BREAKING][rust] Removed the top-level `miden_client::standards` alias. Use the curated client paths as before, or the new raw upstream namespaces `miden_client::account::standards::*`, `miden_client::note::standards::*`, and `miden_client::testing::standards::*`. ([#2185](https://github.com/0xMiden/rust-sdk/pull/2185))
* Added a blanket implementation for `NodeRpcClient::get_account_details` ([#2196](https://github.com/0xMiden/rust-sdk/pull/2196)).
* [BREAKING][param][rust] `NodeRpcClient::sync_storage_maps` and `NodeRpcClient::sync_account_vault` now take a required `block_to: BlockNumber` instead of `Option<BlockNumber>`. The node rejects ranges that extend beyond the chain tip, so callers must pass an explicit upper bound (e.g. the client's sync height). ([#2229](https://github.com/0xMiden/rust-sdk/pull/2229))
* Replaced `node-builder` crate with a leaner `test-node-genesis` crate and removed the `testing-remote-prover` crate; the testing node now runs the node's own `miden-remote-prover` ([#2232](https://github.com/0xMiden/rust-sdk/pull/2232)).

### Enhancements

* [FEATURE][rust] Added `Client::sync_chain()` (on-chain sync only) and `Client::sync_note_transport()` (Note Transport Layer fetch only) for callers needing finer-grained control over sync. ([#2091](https://github.com/0xMiden/rust-sdk/pull/2091))
* [FEATURE][rust] Added `GrpcClient::with_bearer_auth(token)` to attach an `authorization: Bearer <token>` header to every outbound gRPC call, for use behind authenticating gateways. Tokens are validated at connection time and preserved across `set_genesis_commitment` updates ([#2101](https://github.com/0xMiden/rust-sdk/pull/2101)).
* Made new-account construction use merged storage schema commitment (`build_with_schema_commitment`), re-exported `AccountBuilderSchemaCommitmentExt`, added WASM `buildWithoutSchemaCommitment()`, and fixed contract `accounts.create()` to require explicit `components` ([#1996](https://github.com/0xMiden/rust-sdk/pull/1996)).
* Fixed the faucet token symbol display when showing account details ([#1985](https://github.com/0xMiden/rust-sdk/pull/1985) ([#2158](https://github.com/0xMiden/rust-sdk/pull/2158))).
* [FEATURE][rust,cli,web] Added `get_network_note_status` to `NodeRpcClient` trait for querying the processing status of notes submitted to the network (pending, nullifier-inflight, discarded, nullifier-committed), along with attempt count and error details. Exposed as `miden-client network-note-status <note_id>` CLI command and `RpcClient.getNetworkNoteStatus()` in the web client. ([#1981](https://github.com/0xMiden/rust-sdk/pull/1981))
* Remove MMR peaks from the blocks table and store them alongside the sync height in a new `blockchain_checkpoint` table ([#2100](https://github.com/0xMiden/rust-sdk/pull/2100)).
* Added `miden-cli call` command for invoking account procedures directly from the CLI ([#1943](https://github.com/0xMiden/rust-sdk/pull/1943)).
* [FEATURE][rust,store] Added `BatchBuilder` for stacking multiple transactions against multiple local accounts and submitting them as one proven batch via `SubmitProvenBatch`. Also adds `Store::apply_transaction_batch` (atomic multi-tx apply) with a `SqliteStore` implementation. ([#2109](https://github.com/0xMiden/rust-sdk/pull/2109), [#2160](https://github.com/0xMiden/rust-sdk/issues/2160))
* Made `TransactionStoreUpdate` serialization lossless ([#2112](https://github.com/0xMiden/rust-sdk/pull/2112)).
* [FEATURE][cli] Added `address encode <ACCOUNT_ID> <INTERFACE> [TAG_LEN]` subcommand that prints the bech32 encoding of an address built from the given fields (useful for producing the input to `address add`). ([#2115](https://github.com/0xMiden/rust-sdk/pull/2115))
* [FEATURE][cli] On asset display, the CLI now lazily fetches on-chain `TokenMetadata` for untracked public faucets via RPC and persists the result in the client's settings store. ([#2159](https://github.com/0xMiden/rust-sdk/pull/2159))
* [FEATURE][cli] Faucet/account IDs in human-facing CLI output (account list, `notes -s`, transaction summaries) are now rendered as bech32 addresses using the configured network instead of hex IDs. Hex remains in error messages and debug output. ([#2159](https://github.com/0xMiden/rust-sdk/pull/2159))
* Added an integration test for network-transaction public output note creation ([#2073](https://github.com/0xMiden/rust-sdk/pull/2073)).
* [FEATURE][rust,cli] Added DAP-backed transaction execution support through `DapProgramExecutor`/`ProgramExecutor`, and made `miden-client exec --start-debug-adapter` compile source scripts so DAP clients can resolve source locations. ([#2189](https://github.com/0xMiden/rust-sdk/pull/2189), [#2245](https://github.com/0xMiden/rust-sdk/pull/2245))
* [FEATURE][web] Added `StorageView` JS wrapper over WASM `AccountStorage`. `account.storage()` now returns a `StorageView` that makes `getItem()` work intuitively for both Value and StorageMap slots. WASM primitives are unchanged; the raw `AccountStorage` is accessible via `.raw` ([#1955](https://github.com/0xMiden/rust-sdk/pull/1955)).
* [FEATURE][web] Added `wordToBigInt()` utility export for losslessly converting a `Word`'s first felt to a `BigInt`. `StorageResult.toString()` is BigInt-backed, and `valueOf()` returns a JS number for values fitting in `Number.MAX_SAFE_INTEGER` and throws `RangeError` for larger u64 values — use `.toBigInt()` for exact access ([#1955](https://github.com/0xMiden/rust-sdk/pull/1955)).
* [FEATURE][rust,cli] Added partial swap (PSWAP) support: `TransactionRequestBuilder::build_pswap_create` / `build_pswap_consume` / `build_pswap_cancel` and a `miden-client pswap` CLI command (`create`, `consume`, `cancel`) for partially-fillable fungible swaps ([#2162](https://github.com/0xMiden/rust-sdk/pull/2162)).
* Added verification of MMR responses during state sync: validated the returned block range matches the requested range and checked that post-delta MMR peaks match the block header's chain commitment ([#1887](https://github.com/0xMiden/rust-sdk/pull/1887)).

## 0.14.9 (2026-05-19)

### Enhancements

* Bumped `miden-vm` workspace dependencies from 0.22.1 to 0.22.4.

## 0.14.7 (2026-06-05)

### Enhancements

* [FEATURE][rust] Added `GrpcClient::with_bearer_auth(token)` to attach an `authorization: Bearer <token>` header to every outbound gRPC call, for use behind authenticating gateways. Tokens are validated at connection time and preserved across `set_genesis_commitment` updates ([#2101](https://github.com/0xMiden/rust-sdk/pull/2101)).

## 0.14.6 (2026-05-05)

### Fixes

* [FIX] When the client submits a network note and it is also tracking the recipient network account, now the `InputNoteReader` detects the consumed note ([#2113](https://github.com/0xMiden/rust-sdk/pull/2113)).
* Changed note transport integration tests to validate note ids and avoid matching with existing notes when running against testnet ([#2148](https://github.com/0xMiden/rust-sdk/pull/2148)).

## 0.14.5 (2026-04-27)

### Breaking Changes

* [BREAKING][behavior][rust,web] `CodeBuilder::compile_note_script` now expects a library module with a single procedure annotated `@note_script` (e.g. `@note_script\npub proc main\n    ...\nend`) instead of a `begin..end` program. Inherited from `miden-standards` 0.14.5, which switched the underlying call from `assemble_program` to `assemble_library` ([#2128](https://github.com/0xMiden/rust-sdk/pull/2128)).

### Enhancements

* Added `ClientBuilder::source_manager()` to override the `SourceManager` used by the client. When not set, the client defaults to `DefaultSourceManager`. Set this when compiling scripts outside the client with an external `Assembler`, so source spans resolve against the same manager ([#2047](https://github.com/0xMiden/rust-sdk/pull/2047)).

### Fixes

* [FIX][web] Stopped the wasm-bindgen-generated array constructors (`NoteArray`, `OutputNoteArray`, `NoteAndArgsArray`, `NoteRecipientArray`, `StorageSlotArray`, `TransactionScriptInputPairArray`, `FeltArray`, `AccountIdArray`, `AccountArray`, `ForeignAccountArray`, `NoteIdAndArgsArray`) from silently moving each input element's underlying Rust value out of the caller's JS handle. The default `pub fn new(elements: Option<Vec<T>>)` path took every element by value via wasm-bindgen's `Vec<T>` ABI: the JS handle's `__wbg_ptr` was left unchanged so the object looked fine, but any subsequent method on it panicked inside WASM with the opaque `"null pointer passed to rust"` error. The auto-generated array exports are now overridden in `js/index.js` with thin wrappers that build the same array via `push(&T)` (which already borrows + clones) so callers can keep using the originals after construction. Same pattern applied to `replaceAt` on the Rust side, which now takes `elem: &T` instead of `elem: T`. Repro: `const note = new Note(...); new NoteArray([note]); note.id();` — used to panic, now succeeds.
* [FIX][rust] Fixed source manager mismatch panic (`invalid source span: starting byte is out of bounds`) in tests that compiled scripts with a standalone `SourceManager` and then executed them through the client. Test helpers now use `TransactionKernel::assembler_with_source_manager()` and the client's shared source manager ([#2047](https://github.com/0xMiden/rust-sdk/pull/2047)).
* [FIX][react] Fixed `initializeSignerAccount` (the external-keystore init path used by `MidenFiSignerProvider`, Para, Turnkey, etc.) throwing `"invalid enum value passed"` on first connect. The code reached for `AuthScheme.AuthEcdsaK256Keccak`, which only exists on the internal wasm-bindgen `AuthScheme` enum, not on the public string-valued `AuthScheme` constant exported from `@miden-sdk/miden-sdk/lazy` — at runtime it resolved to `undefined`, and passing `undefined` to `AccountComponent.createAuthComponentFromCommitment` failed at the wasm boundary. `initializeSignerAccount` now calls `resolveAuthScheme(AuthScheme.ECDSA)`, where `resolveAuthScheme` is a newly-public helper from `@miden-sdk/miden-sdk` that converts the string constants to the numeric wasm-bindgen variant.
* [FIX][react] `DEFAULTS.AUTH_SCHEME` was being initialized to `AuthScheme.AuthRpoFalcon512` — another nonexistent key on the public `AuthScheme`, silently resolving to `undefined`. Now set to `AuthScheme.Falcon`. The four hooks that read this default (`useCreateWallet`, `useCreateFaucet`, `useImportAccount`, `useSessionAccount`) now pipe the value through `resolveAuthScheme(...)` before handing it to the wasm-bindgen `newWallet` / `newFaucet` / `importPublicAccountFromSeed` calls. The public hook option types stay `authScheme?: AuthScheme`, which now correctly means `"falcon" | "ecdsa"`.

## 0.14.4 (2026-04-20)

### Features

* Added DAP-backed transaction script debugging support with `--start-debug-adapter` flag on the `exec` CLI command, `execute_program_with_dap` client method, and offline bootstrap mode for node-less execution ([#1959](https://github.com/0xMiden/rust-sdk/pull/1959)).
* [FEATURE][web] Serialize all async `WebClient` JS methods — both the explicit wrappers and every async call that falls through `createClientProxy` to the underlying WASM client (e.g. `getAccount`, `importAccountById`, `getAccountStorage`) — via an internal `_serializeWasmCall` chain. Prevents `"recursive use of an object detected"` panics when an unwrapped read/write races the auto-sync timer or any explicitly-wrapped method. Expose `waitForIdle()` on `MidenClient` so callers can drain in-flight work before mutating non-WASM state ([#2057](https://github.com/0xMiden/rust-sdk/pull/2057)).
* [FEATURE][web] Split `@miden-sdk/miden-sdk` into eager and lazy entry points. The default entry (`import from "@miden-sdk/miden-sdk"`) now awaits WASM at module top level via a small shim (`js/eager.js`) — consumers don't need `await MidenClient.ready()` / `isReady` before constructing wasm-bindgen types. The lazy entry (`import from "@miden-sdk/miden-sdk/lazy"`) preserves the previous behavior and is required for Capacitor WKWebView hosts (the custom-scheme handler hangs on TLA) and Next.js SSR. Verified empirically against the Miden Wallet's iOS E2E suite on devnet. `@miden-sdk/react` imports from `/lazy` internally and manages readiness via `isReady`.
* [FEATURE][web] Expose `lastAuthError()` on `MidenClient` for typed sign-callback failure recovery — preserves the raw thrown value from the JS signCallback so consumers can distinguish locked/rejected/IO-error failure modes ([#2058](https://github.com/0xMiden/rust-sdk/pull/2058)).
* [FEATURE][web] Added `"custom"` operation to `preview()` so users can dry-run any pre-built `TransactionRequest`, not just send/mint/consume/swap ([#2052](https://github.com/0xMiden/rust-sdk/pull/2052)).
* [FEATURE][web] Exposed `BlockHeader.nativeAssetId()` so JavaScript consumers can read the native fungible-faucet account ID from a block header. The field already rides the RPC wire and is decoded into the Rust `BlockHeader`, but no WASM accessor existed, forcing wallets and dApps to hardcode the native faucet per network ([#2070](https://github.com/0xMiden/rust-sdk/issues/2070)).

### Fixes

* [FIX][web] `proveTransactionWithProver` now takes `&TransactionProver` by reference instead of consuming by value — the old signature invalidated the JS handle after one use, silently falling back to local proving on subsequent calls ([#2062](https://github.com/0xMiden/rust-sdk/pull/2062)).

## 0.14.3 (2026-04-16)

### Fixes

* [FIX] Detect notes created and consumed on the same block that got erased from the node and mark them as consumed ([#2008](https://github.com/0xMiden/rust-sdk/pull/2008)).

## 0.14.2 (2026-04-15)

### Features

* [FEATURE][web] Added `compile.noteScript({ code, libraries? })` to `MidenClient`, filling the gap left on the resource-based surface for note-script compilation. Mirrors the existing `compile.txScript` shape ([#2044](https://github.com/0xMiden/rust-sdk/pull/2044)).
* [FEATURE][web] Exported the `CompilerResource` class so framework wrappers (e.g. React hooks) can instantiate the compile surface over a `WasmWebClient` proxy without wrapping the full `MidenClient`. The third constructor argument is now optional ([#2044](https://github.com/0xMiden/rust-sdk/pull/2044)).

### Fixes

* [FIX][web] Fixed `syncState` deterministically failing with `mmr peaks are invalid: number of one bits in leaves is N which does not equal peak length M` after importing a private note whose inclusion block pre-dates the wallet's current sync height. `get_and_store_authenticated_block` was overwriting the correct historical peaks (written by `applyStateSync`) with peaks from the caller's current `PartialMmr` forest, so subsequent reads at the same block hit the `InvalidPeaks` validation. The IndexedDB `insertBlockHeader` now uses add-if-not-exists semantics, matching the SQLite store's `INSERT OR IGNORE` in `insert_block_header_tx` ([#2039](https://github.com/0xMiden/rust-sdk/pull/2039)).
* [FIX][web] Fixed WASM worker loading under webpack 5 / Next.js consumers. v0.14.1's single classic worker rewrote `import.meta.url` → `self.location.href` (needed for Safari/WKWebView cold-start performance), which webpack's asset tracer cannot follow — consumers hit a 404 on `miden_client_web.wasm` and the SDK silently fell back to a main-thread mode that hung on `sync()`. The SDK now ships BOTH variants (`web-client-methods-worker.js` classic for Safari, `web-client-methods-worker.module.js` ES module for webpack/Vite/Parcel) and `WebClient` picks at runtime via UA detection, configurable via the new `WebClient.workerMode` (`"auto"` / `"module"` / `"classic"`) static. No consumer config changes needed for auto ([#2046](https://github.com/0xMiden/rust-sdk/issues/2046)).

## 0.14.1 (2026-04-14)

### Enhancements

* Optimized `get_account_details` so it only fetches the delta of large public accounts when syncing ([#1916](https://github.com/0xMiden/rust-sdk/pull/1916)).

### Fixes

* [FIX][web] Fixed `syncState` failure ("inconsistent partial mmr: tracked leaf at position N has no value in nodes") caused by skipping authentication node collection for blocks already tracked from the MMR delta during large catch-up syncs. Authentication nodes are now always collected for note-relevant blocks regardless of prior tracking state. ([#1997](https://github.com/0xMiden/rust-sdk/pull/1997)).
* [FIX][web] Fixed `transactions.send({ returnNote: true })` throwing `expected instance of NoteArray`. The JS wrapper was still building `OutputNoteArray` after the WASM binding for `withOwnOutputNotes` switched to `NoteArray` ([#2011](https://github.com/0xMiden/rust-sdk/issues/2011)).
* [FIX][rust] Fixed `FilesystemKeyStore::add_key` failing on Linux when the system temp dir is on a different filesystem than the keys directory ([#2009](https://github.com/0xMiden/rust-sdk/pull/2009)).
* [FIX][rust] Made source manager handling consistent when building transaction scripts. The empty fallback script is now compiled against the client's source manager instead of a fresh one, so any source information on the produced `TransactionScript` is registered with the same source manager used by the executor ([#2006](https://github.com/0xMiden/rust-sdk/pull/2006)).

## 0.14.0 (2026-04-07)

### Enhancements

* Made `GrpcNoteTransportClient` connection lazy, deferring it to the first RPC call instead of connecting eagerly at client initialization ([#1970](https://github.com/0xMiden/rust-sdk/pull/1970)).
* Updated the `GrpcClient` to fetch the RPC limits from the node ([#1724](https://github.com/0xMiden/rust-sdk/pull/1724)) ([#1737](https://github.com/0xMiden/rust-sdk/pull/1737), [#1809](https://github.com/0xMiden/rust-sdk/pull/1809)).
* Added typed error parsing for node RPC endpoints, enabling programmatic error handling instead of string parsing ([#1734](https://github.com/0xMiden/rust-sdk/pull/1734)).
* Added `--rpc-status` flag to `miden-client info` command to display RPC node status information including node version, genesis commitment, store status, and block producer status; also added `get_status_unversioned` to `NodeRpcClient` trait ([#1742](https://github.com/0xMiden/rust-sdk/pull/1742)).
* Prevent a potential unwrap panic in `insert_storage_map_nodes_for_map` ([#1750](https://github.com/0xMiden/rust-sdk/pull/1750)).
* Changed the `StateSync::sync_state()` to take a reference of the MMR ([#1764](https://github.com/0xMiden/rust-sdk/pull/1764)).
* Account storage restructured into latest/historical tables for efficient delta writes and simpler pruning ([#1775](https://github.com/0xMiden/rust-sdk/pull/1775)).
* Remove unnecessary clones of `NoteInclusionProof` and `NoteMetadata` in note import and sync paths ([#1787](https://github.com/0xMiden/rust-sdk/pull/1787)).
* [FEATURE][web] WebClient now automatically syncs state before account creation when the client has never been synced, preventing a slow full-chain scan on the next sync (#1704).
* Added `NoteScreener` constructor via `Client::note_screener()` and improved note consumability checks with batch note screening support ([#1803](https://github.com/0xMiden/rust-sdk/pull/1803), [#1814](https://github.com/0xMiden/rust-sdk/pull/1814)).
* [FEATURE][web] Added `getAccountProof` method to the web client's `RpcClient`, allowing lightweight retrieval of account header, storage slot values, and code via a single RPC call. Refactored the `NodeRpcClient::get_account_proof` signature to allow requesting just private account proofs ([#1794](https://github.com/0xMiden/rust-sdk/pull/1794), [#1814](https://github.com/0xMiden/rust-sdk/pull/1814)).
* Added `getAccountByKeyCommitment` method to `WebClient` for retrieving accounts by public key commitment ([#1729](https://github.com/0xMiden/rust-sdk/pull/1729)).
* [BREAKING][removal][web] Removed `addAccountSecretKeyToWebStore`, `getAccountAuthByPubKeyCommitment`, `getPublicKeyCommitmentsOfAccount`, and `getAccountByKeyCommitment` from `WebClient`. Use the new `client.keystore` sub-object instead (e.g. `client.keystore.insert()`, `client.keystore.get()`, `client.keystore.getCommitments()`, `client.keystore.getAccountId()` + `client.getAccount()`). ([#1947](https://github.com/0xMiden/rust-sdk/pull/1947)).
* Added automatic registration of note scripts required by network transactions (NTX): the client checks the node's script registry before submitting and registers any missing scripts. Standard note scripts are skipped since the NTX builder resolves them directly ([#1840](https://github.com/0xMiden/rust-sdk/pull/1840)).
* Added automatic retry for rate-limited (`ResourceExhausted`) and transiently unavailable RPC calls in `GrpcClient`, with up to 5 attempts and `retry-after` header support ([#1928](https://github.com/0xMiden/rust-sdk/pull/1928)).
* Added client methods to prune account history (commitments of previous nonces, alongside its orphaned account code) ([#1886](https://github.com/0xMiden/rust-sdk/pull/1886)).
* Changed the sync state to track the consumer account on externally-consumed notes, so the `InputNoteReader` can return notes even if the transaction was not locally executed ([#1973](https://github.com/0xMiden/rust-sdk/pull/1973)).

### Changes

* [BREAKING] Incremented MSRV to 1.91 ([#1798](https://github.com/0xMiden/rust-sdk/pull/1798)).
* [BREAKING] Replaced `AuthFalcon512Rpo`/`AuthEcdsaK256Keccak` with unified  `AuthSingleSig`, changed `StorageMapKey` from a type alias to a newtype, renamed note constructors to associated methods (`P2idNote::create`, `SwapNote::create`, `P2ideNote::create`), and started requiring `AccountComponentMetadata` in `AccountComponent::new`([#1798](https://github.com/0xMiden/rust-sdk/pull/1798)).
* Included Partial states in `NoteFilter::Unspent` for output notes ([#1817](https://github.com/0xMiden/rust-sdk/pull/1817)).
* [BREAKING][arch][web] Replaced the `WebClient` class with a new `MidenClient` resource-based API as the primary web SDK entry point. `WebClient` is still available as `WasmWebClient` for low-level access but is no longer part of the public API. All documentation has been updated to use `MidenClient`. Migration: replace `WebClient.createClient(rpcUrl, noteTransportUrl, seed, storeName)` with `MidenClient.create({ rpcUrl, noteTransportUrl, seed, storeName })`, and replace direct method calls (e.g. `client.newWallet(...)`, `client.submitNewTransaction(...)`, `client.getAccounts()`) with resource methods (e.g. `client.accounts.create()`, `client.transactions.send(...)`, `client.accounts.list()`). ([#1762](https://github.com/0xMiden/rust-sdk/pull/1762)).
* [BREAKING][type][web] `AccountId.fromHex()` now returns `Result` (throws on invalid hex) instead of silently panicking via `unwrap()`. ([#1762](https://github.com/0xMiden/rust-sdk/pull/1762)).
* [BREAKING] Added a `AccountReader` accessible through `Client::account_reader` to read account data without needing to load the whole `Account` ([#1713](https://github.com/0xMiden/rust-sdk/pull/1713), [#1716](https://github.com/0xMiden/rust-sdk/pull/1716)).
* [BREAKING] Added `Keystore` trait that extends `TransactionAuthenticator` to provide a unified interface for key storage, retrieval, and account-key mapping, enabling custom keystore implementations. `Keystore` replaces `TransactionAuthenticator` in `Client` and provides a way to map from account IDs to public keys (registering them separately is not required anymore). ([#1726](https://github.com/0xMiden/rust-sdk/pull/1726)).
* Refactored integration tests binary with subprocess-per-test execution; added automatic retry of failed tests (`--retry-count`), captured stdout/stderr per test, and tracing support via `RUST_LOG` ([#1743](https://github.com/0xMiden/rust-sdk/pull/1743)).
* Improved integration test logging with a `--verbose` flag for info-level tracing, routed tracing output to stderr to avoid corrupting subprocess JSON, and added `tracing::info!` instrumentation to test helpers ([#1816](https://github.com/0xMiden/rust-sdk/pull/1816)).
* Added implementation for the `get_public_key` method on the `FilesystemKeystore` and `WebKeystore` ([#1731](https://github.com/0xMiden/rust-sdk/pull/1731)).
* [BREAKING] Made the nullifiers sync optional on the `StateSync` component ([#1756](https://github.com/0xMiden/rust-sdk/pull/1756)).
* Decoupled keystore functionality from `WebStore` by moving keystore helper logic from `idxdb-store` into the `web-client` crate, also added `export_store` and `import_store` methods to the `Store` trait, enabling usage of different stores ([#1795](https://github.com/0xMiden/rust-sdk/pull/1795)).
* [BREAKING] Added `SyncStateInputs` to bundle the parameters needed to perform the sync state ([#1778](https://github.com/0xMiden/rust-sdk/pull/1778)).
* Added lazy loading for foreign accounts. Specifying `TransactionRequestBuilder::foreign_accounts()` for public accounts is no longer required ([#1812](https://github.com/0xMiden/rust-sdk/pull/1812), [#1892](https://github.com/0xMiden/rust-sdk/pull/1892)).
* [BREAKING][type][web] `AuthSecretKey.getRpoFalcon512SecretKeyAsFelts()` and `getEcdsaK256KeccakSecretKeyAsFelts()` now return `Result<Vec<Felt>, JsValue>` instead of panicking on key type mismatch ([#1833](https://github.com/0xMiden/rust-sdk/pull/1833)).
* [BREAKING][rename][cli] Renamed `CliConfig::from_system()` to `CliConfig::load()` and `CliClient::from_system_user_config()` to `CliClient::new()` for better discoverability ([#1848](https://github.com/0xMiden/rust-sdk/pull/1848)).
* Removed `SmtForest` empty-root workaround in `AccountSmtForest::safe_pop_smts`, now that the upstream fix has landed in miden-crypto v0.19.7 ([#1864](https://github.com/0xMiden/rust-sdk/pull/1864)).
* [BREAKING][rename][all] Adapted to upstream protocol renames: `Falcon512Rpo` renamed to `Falcon512Poseidon2`, `Felt::as_int()` renamed to `as_canonical_u64()`, `OutputNote::Full` replaced by `OutputNote::Public(PublicOutputNote)`, Asset now uses key-value words API.
* [BREAKING][rename][all] Adapted to upstream protocol 0.14.0 renames: `NoteHeader::commitment()` renamed to `to_commitment()`, `NoteLocation::node_index_in_block()` renamed to `block_note_tree_index()`, `StorageMapKey::inner()` removed (use `Word::from(key)`), `TransactionOutputs::expiration_block_num` field now private (use getter). ([#1926](https://github.com/0xMiden/rust-sdk/pull/1926))
* Added an `InputNoteReader` accessible through `client.input_note_reader()` that allows for lazy iterator over all the consumed input notes ([#1843](https://github.com/0xMiden/rust-sdk/pull/1843), ([#1925](https://github.com/0xMiden/rust-sdk/pull/1925))).
* Removed miden-cli template TOMLs in favor of direct serialization into packages ([#1879](https://github.com/0xMiden/rust-sdk/pull/1879)).
* [BREAKING] Updated `SyncState` to fetch multiple note updates ([#1941](https://github.com/0xMiden/node/pull/1941), [#1963](https://github.com/0xMiden/rust-sdk/pull/1963)).
* Unified test environment variables across Rust and web client test suites. `TEST_MIDEN_NETWORK` now acts as a preset that configures all components (RPC, prover, note transport) for `devnet`/`testnet`/`localhost`. Individual env vars (`TEST_MIDEN_RPC_URL`, `TEST_MIDEN_PROVER_URL`, `TEST_MIDEN_NOTE_TRANSPORT_URL`) override specific components. Removed `TEST_MIDEN_RPC_ENDPOINT`, `TEST_WITH_NOTE_TRANSPORT`, `TEST_MIDEN_NOTE_TRANSPORT_ENDPOINT`, and `REMOTE_PROVER` ([#1939](https://github.com/0xMiden/rust-sdk/pull/1939)).
* [BREAKING] Updated miden-node crates to v0.14.3 and adapted to upstream package-related changes: `MastArtifact` and `PackageKind` removed, `Package::mast` is now `Arc<Library>`, `PackageManifest::new()` returns `Result`, `assemble_library()` returns `Arc<Library>`([#1972](https://github.com/0xMiden/rust-sdk/pull/1972)).

### Features

* [FEATURE][web] New `MidenClient` class with resource-based API (`client.accounts`, `client.transactions`, `client.notes`, `client.tags`, `client.settings`). Provides high-level transaction helpers (`send`, `mint`, `consume`, `swap`, `consumeAll`), transaction dry-runs via `preview()`, confirmation polling via `waitFor()`, and flexible account/note references that accept hex strings, bech32 strings, or WASM objects interchangeably (`AccountRef`, `NoteInput` types). Factory methods: `MidenClient.create()`, `MidenClient.createTestnet()`, `MidenClient.createMock()`. ([#1762](https://github.com/0xMiden/rust-sdk/pull/1762))
* [FEATURE][web] Added `TransactionId.fromHex()` static constructor for creating transaction IDs from hex strings. ([#1762](https://github.com/0xMiden/rust-sdk/pull/1762))
* [FEATURE][web] Added standalone tree-shakeable note utilities (`createP2IDNote`, `createP2IDENote`, `buildSwapTag`) usable without a client instance. ([#1762](https://github.com/0xMiden/rust-sdk/pull/1762))
* [FEATURE][web] SDK ergonomics: `accounts.getOrImport(ref)` convenience method, `accounts.import()` accepts full `AccountRef`, `transactions.send()` return type changed to `SendResult` with optional `returnNote`, notes API simplified (`listAvailable` returns `InputNoteRecord[]`, `consume` accepts `Note` objects), `MidenClient.create()` accepts rpcUrl/proverUrl shorthands.
* [BREAKING][FEATURE][web] Custom contract support: `accounts.create()` with `ImmutableContract`/`MutableContract` types, new `client.compile` resource (`compile.component()`, `compile.txScript()` with `"dynamic"`/`"static"` linking), and `transactions.execute({ account, script, foreignAccounts? })` for custom script execution with FPI. `transactions.send()` return type changed. ([#1828](https://github.com/0xMiden/rust-sdk/pull/1828))
* [FEATURE][web] Account import improvements: `accounts.getOrImport(ref)` convenience method, and `accounts.import()` now accepts full `AccountRef` (string, `AccountId`, `Account`, `AccountHeader`) in addition to `{ file }` and `{ seed }` forms. ([#1828](https://github.com/0xMiden/rust-sdk/pull/1828))
* [FEATURE][web] Added `AccountId.fromPrefixSuffix(prefix, suffix)` constructor for building an `AccountId` from its two felt components, useful when prefix/suffix are stored separately in storage maps. ([#1889](https://github.com/0xMiden/rust-sdk/pull/1889))
* [FEATURE][web] Added `TransactionRequestBuilder.withExpirationDelta()` for expiring manual transaction requests ([#1904](https://github.com/0xMiden/rust-sdk/pull/1904))
* [FEATURE][web] Added `accounts.insert({ account, overwrite? })` to `MidenClient` for inserting pre-built `Account` objects into the local store. Enables external signer integrations that build accounts via `AccountBuilder` with custom auth commitments ([#1922](https://github.com/0xMiden/rust-sdk/pull/1922)).
* [FEATURE][web] Exposed `executeProgram` (view call) to the JS side, allowing local execution of a transaction script against an account and inspection of the 16-element stack output without submitting to the network. Added `AdviceInputs` constructor and reverse `From` conversions. ([#1859](https://github.com/0xMiden/rust-sdk/issues/1859))
* [FEATURE][web] Added `client.keystore` sub-object API for managing secret keys. Methods: `insert(accountId, secretKey)`, `get(pubKeyCommitment)`, `remove(pubKeyCommitment)`, `getCommitments(accountId)`, `getAccountId(pubKeyCommitment)`. Also available on `MidenClient` as a resource (`client.keystore`). ([#1947](https://github.com/0xMiden/rust-sdk/pull/1947))

### Fixes

* [FIX][web] Replaced `.unwrap()` panics with proper `Result` returns in `MerklePath.computeRoot()`, `NoteExecutionHint.fromParts()`, `NoteExecutionHint.canBeConsumed()`, `NoteStorage` constructor, and `TransactionStatus.discarded()` WASM bindings ([#1870](https://github.com/0xMiden/rust-sdk/pull/1870)).
* [FIX][rust] Fixed `get_vault_asset_witnesses` failing with `MerkleError::RootNotInStore` when the vault root is missing from the `AccountSmtForest`. The error is now caught and falls back to loading the full vault from the store ([#1890](https://github.com/0xMiden/rust-sdk/pull/1890)).
* [FIX][web] Fixed the error `TypeError: parameter 1 is not of type 'ArrayBuffer'` when re-initializing a client with an imported database. `Uint8Array` fields (e.g. the client version setting) were exported as plain arrays and not restored to `Uint8Array` on import, causing `TextDecoder.decode()` to fail. Export now tags `Uint8Array` values for correct round-trip. ([#1952](https://github.com/0xMiden/rust-sdk/pull/1952))
* [FIX][rust] Replaced `.expect()` panics on RPC response data with proper error propagation ([#1833](https://github.com/0xMiden/rust-sdk/pull/1833)).

## 0.13.4 (2026-03-23)

* [FIX][rust,web] Fixed storage map slots with duplicate roots losing their entries after a store round-trip, which corrupted the storage commitment ([#1915](https://github.com/0xMiden/rust-sdk/pull/1915)).
* [FIX][all] Fixed private notes delivered via NTL getting stuck as `Expected` when syncing at high frequency (e.g. every 3s). The on-chain commitment could be processed before the NTL delivered the note data, causing the note to never transition to `Committed`. The note import flow now scans back up to 20 blocks from the current sync height when checking for committed notes, so notes committed just before the client synced past them are found during import.

## 0.13.3 (2026-03-16)

* [FIX][rust,web] Fixed `sync_state()` invoking the external signer (e.g. wallet extension) during note consumability checks, causing repeated confirmation popups on every sync cycle. `NoteScreener` no longer attaches the `TransactionAuthenticator` when trial-executing consume transactions; accounts requiring auth now return `ConsumableWithAuthorization` instead ([#1905](https://github.com/0xMiden/rust-sdk/pull/1905)).
* [FIX][rust] Fixed redundant `/GetAccount` RPC calls during `sync_state()` — a public account active across N sync steps now triggers exactly 1 fetch instead of N ([#1876](https://github.com/0xMiden/rust-sdk/pull/1876)).
* [FIX] Deduplicated storage map entries returned by the `SyncAccountStorageMaps` RPC endpoint, keeping only the latest value per key. Previously, accounts with storage map keys updated across multiple blocks would fail to load ([#1902](https://github.com/0xMiden/rust-sdk/pull/1902)).
* [FIX][web] Fixed `PrematureCommitError` crash during `syncState()` by moving all IndexedDB writes into a single Dexie transaction instead of spawning competing inner transactions ([#1876](https://github.com/0xMiden/rust-sdk/pull/1876)).
* [FEATURE][web] Exposed `getAccountProof` in the `RpcClient`, accepting optional `AccountStorageRequirements` and block number parameters to fetch specific storage maps without full account reconstruction ([#1917](https://github.com/0xMiden/rust-sdk/pull/1917)).
* [FEATURE][web] Exposed `syncStorageMaps` in the `RpcClient` for paginated retrieval of large storage maps ([#1917](https://github.com/0xMiden/rust-sdk/pull/1917)).
* [FEATURE][rust] Added `storage_details()` and `find_map_details()` accessors to `AccountProof` for direct access to storage map data ([#1917](https://github.com/0xMiden/rust-sdk/pull/1917)).

## 0.13.2 (2026-02-26)

* Updated to `miden-crypto` v0.19.5 ([#1813](https://github.com/0xMiden/rust-sdk/pull/1813)).
* [FIX] Stopped including unnecessary storage map data when loading existing accounts for transaction execution. New accounts (nonce == 0) still get full storage maps as needed for kernel validation ([#1832](https://github.com/0xMiden/rust-sdk/pull/1832)).
* [FIX][web] Added missing `attachment()` getter to `NoteMetadata` WASM binding ([#1810](https://github.com/0xMiden/rust-sdk/pull/1810)).
* [FIX][web] Fixed transaction execution failures after reopening a browser extension by always persisting MMR authentication nodes during sync, even for blocks with no relevant notes. Previously, closing and reopening the extension lost in-memory MMR state and the store was missing nodes needed for Merkle authentication paths. Also surfaces a distinct `PartialBlockchainNodeNotFound` error instead of a confusing deserialization crash when nodes are missing ([#1789](https://github.com/0xMiden/rust-sdk/pull/1789)).

## 0.13.1 (2026-02-13)

* Added the `@miden-sdk/react` hooks library (see [its own changelog](packages/react-sdk/CHANGELOG.md)) ([#1711](https://github.com/0xMiden/rust-sdk/pull/1711)).
* Fixed WASM bindings consuming JS objects: `RpcClient` and `WebClient` methods now take references (`&AccountId`, `&Word`) instead of owned values, so callers can reuse objects after passing them ([#1765](https://github.com/0xMiden/rust-sdk/pull/1765)).
* Fixed `AccountSmtForest` pruning shared SMT roots between old and new account states, which caused `MerkleError::RootNotInStore` during note screening after `sync_state()` ([#1771](https://github.com/0xMiden/rust-sdk/pull/1771)).
* [FEATURE][web] Added `setupLogging(level)` and `logLevel` parameter on `createClient` to route Rust tracing output to the browser console with configurable verbosity ([#1669](https://github.com/0xMiden/rust-sdk/pull/1669)).
* [FEATURE][web] Added 3-layer concurrency safety for WASM access: in-tab async lock, cross-tab IndexedDB lock, and auto-sync on cross-tab state changes ([#1784](https://github.com/0xMiden/rust-sdk/pull/1784)).

## 0.13.0 (2026-01-28)

* [BREAKING] Removed `getRpoFalcon512PublicKeyAsWord` and `getEcdsaK256KeccakPublicKeyAsWord` in `AuthSecretKey`
* Improved auth scheme handling across the Rust and web clients (typed `build_wallet_id`, unified transaction tests, new shared `getPublicKeyAsWord` binding, and refreshed typedoc output) ([#1556](https://github.com/0xMiden/rust-sdk/pull/1556)).
* [BREAKING] Typed the `auth_scheme` plumbing across the Rust WebClient ID-building helpers and aligned the WebClient bindings with the native enum to avoid passing raw identifiers ([#1546](https://github.com/0xMiden/rust-sdk/pull/1546)).
* [BREAKING] WebClient `AccountComponent.createAuthComponentFromCommitment` now takes `AuthScheme` (enum) instead of a numeric scheme id. The old `AccountComponent.createAuthComponent` method was removed; use `createAuthComponentFromSecretKey` instead ([#1578](https://github.com/0xMiden/rust-sdk/issues/1578)).
* Changed `blockNum` type from `string` to `number` in WebClient transaction interfaces for better type safety and consistency ([#1528](https://github.com/0xMiden/rust-sdk/pull/1528)).
* Consolidated `FetchedNote` fields into `NoteHeader` ([#1536](https://github.com/0xMiden/rust-sdk/pull/1536)).
* Tied the web client's IndexedDB schema to the running package version, automatically recreating or wiping stale stores and applying the same guard to `forceImportStore` ([#1576](https://github.com/0xMiden/rust-sdk/pull/1576)).
* Added the `--remote-prover-timeout` configuration to the CLI ([#1551](https://github.com/0xMiden/rust-sdk/pull/1551)).
* Surface WASM worker errors to the JS wrapper with their original stacks for clearer diagnostics ([#1565](https://github.com/0xMiden/rust-sdk/issues/1565)).
* Added doc_cfg as top level cfg_attr to turn on feature annotations in docs.rs and added make targets to serve the docs ([#1543](https://github.com/0xMiden/rust-sdk/pull/1543)).
* Updated `DataStore` implementation to prevent retrieving whole `vault` and `storage` ([#1419](https://github.com/0xMiden/rust-sdk/pull/1419))
* Added RPC limit handling for `sync_nullifiers` endpoint ([#1590](https://github.com/0xMiden/rust-sdk/pull/1590)).
* Added pagination handling for `sync_storage_maps` and `sync_account_vault` RPC endpoints.
* Added a convenience function `fromBech32` to turn a bech32 string into an AccountId ([#1607](https://github.com/0xMiden/rust-sdk/pull/1607)).
* [BREAKING] Refactored the fields in retrieved notes in the WebClient: now the inclusion proof has been factored out and is always accessible ([#1606](https://github.com/0xMiden/rust-sdk/pull/1606)).
* [BREAKING] Renamed `NodeRpcClient::get_account_proofs` to `NodeRpcClient::get_account_proof` & added `account_state` parameter (block at which we want to retrieve the proof) ([#1616](https://github.com/0xMiden/rust-sdk/pull/1616)).
* [BREAKING] Refactored `NetworkId` to allow custom networks ([#1612](https://github.com/0xMiden/rust-sdk/pull/1612)).
* [BREAKING] Removed `toBech32Custom` and implemented custom id conversion for wasm derived class `NetworkId` ([#1612](https://github.com/0xMiden/rust-sdk/pull/1612)).
* [BREAKING] Remove `SecretKey` model and consolidated functionality into `AuthSecretKey` ([#1592](https://github.com/0xMiden/rust-sdk/issues/1380))
* Incremented the limits for various RPC calls to accommodate larger data sets ([#1621](https://github.com/0xMiden/rust-sdk/pull/1621)).
* [BREAKING] Introduced named storage slots, changed `FilesystemKeystore` to not be generic over RNG ([#1626](https://github.com/0xMiden/rust-sdk/pull/1626)).
* Added `submit_new_transaction_with_prover` to the Rust client and `submitNewTransactionWithProver` to the WebClient([#1622](https://github.com/0xMiden/rust-sdk/pull/1622)).
* Fixed MMR reconstruction code and fixed how block authentication paths are adjusted ([#1633](https://github.com/0xMiden/rust-sdk/pull/1633)).
* Added WebClient bindings and RPC helpers for additional account, note, and validation workflows ([#1638](https://github.com/0xMiden/rust-sdk/pull/1638)).
* [BREAKING] Modified JS binding for `AccountComponent::compile` which now takes an `AccountComponentCode` built with the newly added binding `CodeBuilder::compile_account_component_code` ([#1627](https://github.com/0xMiden/rust-sdk/pull/1627)).
* Expanded the `GrpcClient` API with methods to fetch account proofs and rebuild the slots for an account ([#1591](https://github.com/0xMiden/rust-sdk/pull/1591)).
* [BREAKING] `WebClient.addAccountSecretKeyToWebStore` now takes an additional parameter: an account ID. This will link the ID with the secret key in the WebStore. Added `WebClient.getPublicKeyCommitmentsOfAccount` method that will return a list of related public key commitments for the given account ID ([#1608](https://github.com/0xMiden/rust-sdk/pull/1608)).
* [BREAKING] Added naming to `IndexedDB` store to allow multiple WebClient instances to run in the same browser; `WebClient.createClient` now takes an optional DB name (otherwise defaults to name based on the endpoint/network) ([#1645](https://github.com/0xMiden/rust-sdk/pull/1645)).
* [BREAKING] Simplified the `NoteScreener` API, removing `NoteRelevance` in favor of `NoteConsumptionStatus`; exposed JS bindings for consumption check results ([#1630](https://github.com/0xMiden/rust-sdk/pull/1630)).
* [BREAKING] Replaced `TransactionRequestBuilder::unauthenticated_input_notes` & `TransactionRequestBuilder::authenticated_input_notes` for `TransactionRequestBuilder::input_notes`, now the user passes a list of notes which the `Client` itself determines the authentication status of ([#1624](https://github.com/0xMiden/rust-sdk/issues/1624)).
* Updated `SqliteStore`: replaced `MerkleStore` with `SmtForest` and introduced `AccountSmtForest`; simplified queries ([#1526](https://github.com/0xMiden/rust-sdk/pull/1526), [#1663](https://github.com/0xMiden/rust-sdk/pull/1663)).
* Added filter to store query to improve how the MMR is built ([#1681](https://github.com/0xMiden/rust-sdk/pull/1681)).
* [BREAKING] Required the client RNG to be `Send + Sync` (via the `ClientFeltRng` marker and `ClientRngBox` alias) so `Client` can be `Send + Sync` ([#1677](https://github.com/0xMiden/rust-sdk/issues/1677)).
* [BREAKING] Refactored `FilesystemKeyStore` to implement the new `Keystore` trait, enabling custom keystore implementations ([#1726](https://github.com/0xMiden/rust-sdk/pull/1726)).
* Fixed a race condition in `pruneIrrelevantBlocks` that could delete the current block header when multiple tabs share IndexedDB, causing sync to panic ([#1650](https://github.com/0xMiden/rust-sdk/pull/1650)).
* Fixed a race condition where concurrent sync operations could cause sync height to go backwards, leading to block header deletion and subsequent panics ([#1650](https://github.com/0xMiden/rust-sdk/pull/1650)).
* Changed `get_current_partial_mmr` to return a `StoreError::BlockHeaderNotFound` error instead of panicking when the block header is missing ([#1650](https://github.com/0xMiden/rust-sdk/pull/1650)).
* Added `CliClient` wrapper and `CliConfig::from_system()` to allow creating a CLI-configured client programmatically ([#1642](https://github.com/0xMiden/rust-sdk/pull/1642)).
* [BREAKING] Updated `BlockNumber` IndexedDB type: changed from `string` to `number` ([#1684](https://github.com/0xMiden/rust-sdk/pull/1684)).
* [BREAKING] Upgraded to protocol 0.13: exposed and aligned note-related structs to WebClient; `NoteTag` and `NoteAttachment` APIs updated renamed `NoteTag.fromAccountId` to `withAccountTarget`, added `withCustomAccountTarget`; added `NoteAttachmentScheme` wrapper and content accessors (`asWord`, `asArray`) to `NoteAttachment`; removed `NoteExecutionMode` ([#1685](https://github.com/0xMiden/rust-sdk/pull/1685)).
* Added sync lock to coordinate concurrent `syncState()` calls in the WebClient using the Web Locks API, with coalescing behavior where concurrent callers share results from an in-progress sync ([#1690](https://github.com/0xMiden/rust-sdk/pull/1690)).
* [BREAKING] Removed the `payback_note_type` field from the swap command ([#1700](https://github.com/0xMiden/rust-sdk/pull/1700)).
* Added `miden-bench` tool to benchmark client operations ([#1721](https://github.com/0xMiden/rust-sdk/pull/1721)).

## 0.12.6 (2026-01-08)

* Enabled Workers with `createClientWithExternalKeystore` via callbacks ([#1569](https://github.com/0xMiden/rust-sdk/pull/1569)).
* Added `executeForSummary` method to WebClient that executes a transaction and returns a `TransactionSummary`, handling both authorized and unauthorized transactions ([#1620](https://github.com/0xMiden/rust-sdk/pull/1620)).
* Added WebClient bindings for the RPO Falcon512 multisig auth component ([#1620](https://github.com/0xMiden/rust-sdk/pull/1620)).
* Added seed to `AccountStatus::Locked` variant in `AccountRecord` to track private accounts that are locked due to a mismatch in the account commitment ([#1665](https://github.com/0xMiden/rust-sdk/pull/1665)).

## 0.12.5 (2025-12-01)

* Removed the top-level await from the web-client JS entry point by lazily loading the WASM module, allowing `@miden-sdk/miden-sdk` to be imported normally (including in Next.js SSR builds), and updated the worker bootstrap to match.
* Changed the default note transport endpoint from `localhost` to `https://transport.miden.io` ([#1574](https://github.com/0xMiden/rust-sdk/pull/1574)).
* Fixed a bug where insertions in the `Addresses` table in the IndexedDB Store resulted in the `id` and `address` fields being inverted with each other ([#1532](https://github.com/0xMiden/rust-sdk/pull/1532)).
* Changed the note script pre-loading step to include all expected scripts based on specified recipients ([#1539](https://github.com/0xMiden/rust-sdk/pull/1539)).
* Added methods to `Package` exposing inner `Program`/`Library`. Also implemented `fromPackage` methods for `NoteScript` & `TransactionScript` ([#1550](https://github.com/0xMiden/rust-sdk/pull/1550)).
* Added RPC limit handling for `check_nullifiers` and `get_notes_by_id` ([#1558](https://github.com/0xMiden/rust-sdk/pull/1558)).
* Fixed account rollback bug by not loading already discarded transaction on sync state ([#1567](https://github.com/0xMiden/rust-sdk/pull/1567)).
* Added `--version` flag to client CLI ([#1586](https://github.com/0xMiden/rust-sdk/pull/1586)).
* Refactored note fetching from the transport layer, calling now `import_note()` on retrieved notes ([#1579](https://github.com/0xMiden/rust-sdk/pull/1579)).

## Miden Client CLI - 0.12.4 (2025-11-17)

* Fixed CLI install process to statically include account component package files ([#1530](https://github.com/0xMiden/rust-sdk/pull/1530)).

## 0.12.3 (2025-11-16)

* Added `recoverFrom()` function to WASM `PublicKey` and added back `TransactionSummary` back to `index.d.ts` ([#1513](https://github.com/0xMiden/rust-sdk/pull/1513)).
* Added `hasProcedure` to `AccountCode` and `getProcedures` to `AccountComponent` in the WebClient ([#1517](https://github.com/0xMiden/rust-sdk/pull/1517)).
* Retrieve inclusion proofs for fetched notes from the Note Transport layer ([#1495](https://github.com/0xMiden/rust-sdk/pull/1495)).
* Added ECDSA auth component to the rust-client & web-client ([#1527](https://github.com/0xMiden/rust-sdk/pull/1527))

## 0.12.2 (2025-11-12)

* Added `prover()` setter to `ClientBuilder` to allow configuring custom transaction provers ([#1499](https://github.com/0xMiden/rust-sdk/pull/1499)).
* Added `AccountStorageMode` getters for `Account` and `AccountId`. [(#1509)](https://github.com/0xMiden/rust-sdk/pull/1509).
* Allowed `new-account` command to create accounts with non-Falcon auth components ([#1443](https://github.com/0xMiden/rust-sdk/pull/1443)).
* Added new `.miden` directory for configuration files at the client CLI ([#1464](https://github.com/0xMiden/rust-sdk/pull/1464)).
* Added bindings for the new ECDSA auth scheme [(#1478)](https://github.com/0xMiden/rust-sdk/pull/1478).
* Exposed all auth packages from `miden-base`: `no-auth`, `multisig-auth`, and `acl-auth` components are now available in the CLI under `packages/auth/` subdirectory ([#1132](https://github.com/0xMiden/rust-sdk/issues/1132)).

## 0.12.0 (2025-11-10)

### Features

* Added support for getting specific vault and storage elements from `Store` along with their proofs ([#1164](https://github.com/0xMiden/rust-sdk/pull/1164)).
* Implemented functions for lazy loading on webstore [(#1184)](https://github.com/0xMiden/rust-sdk/pull/1184).
* Separated `migrations` and `settings` tables [(#1287)](https://github.com/0xMiden/rust-sdk/pull/1287).
* Added single default address on account creation ([#1308](https://github.com/0xMiden/rust-sdk/pull/1308)).
* Added a `GetNoteScriptByRoot` call to the `RpcClient` ([#1311](https://github.com/0xMiden/rust-sdk/pull/1311)).
* Implemented account lazy loading with more granular account data getters ([#1321](https://github.com/0xMiden/rust-sdk/pull/1321)).
* Added `NoAuth` component to the web client ([#1330](https://github.com/0xMiden/rust-sdk/pull/1330)).
* Implemented shared source manager for better error reporting ([#1275](https://github.com/0xMiden/rust-sdk/pull/1275)).
* Added `getMapEntries` method to `AccountStorage` in web client for iterating storage map entries ([#1323](https://github.com/0xMiden/rust-sdk/pull/1323)).
* Added `Address` addition and removal for accounts ([#1367](https://github.com/0xMiden/rust-sdk/pull/1367)).
* Refactored code into their own files and added `ProvenTransaction` and `TransactionStoreUpdate` bindings for the WebClient ([#1408](https://github.com/0xMiden/rust-sdk/pull/1408)).
* Added `NoteFile` type, used for exporting and importing `Notes`([#1378](https://github.com/0xMiden/rust-sdk/pull/1383)).
* Build `IndexedDB` code from a `build.rs` instead of pushing artifacts to the repo ([#1409](https://github.com/0xMiden/rust-sdk/pull/1409)).
* Implemented missing RPC endpoints: `/SyncStorageMaps`, `/SyncAccountVault` & `/SyncTransactions` ([#1362](https://github.com/0xMiden/rust-sdk/pull/1362)).
* Updated `submit_proven_transaction()` to include `TransactionInputs` for validator ([#1421](https://github.com/0xMiden/rust-sdk/pull/1421)).
* [BREAKING] Replaced `AccountComponentTemplates` for `Packages` for account creation ([#1313](https://github.com/0xMiden/rust-sdk/pull/1313)).
* Added support for silently initializing the client CLI ([#1424](https://github.com/0xMiden/rust-sdk/pull/1424)).
* Started allowing for note ID prefixes in CLI `notes --send` ([#1433](https://github.com/0xMiden/rust-sdk/pull/1433)).
* Refactored note scripts to be pre-loaded into the store instead of providing them through advice inputs ([#1426](https://github.com/0xMiden/rust-sdk/pull/1426)).
* [BREAKING] Refactored client transaction APIs and the new `TransactionResult` type ([#1407](https://github.com/0xMiden/rust-sdk/pull/1407)).
* Introduce an account and note tag limit to be tracked by the client. ([#1476](https://github.com/0xMiden/rust-sdk/pull/1476)).
* Added ability to create `AccountComponent` from a `Package` and `StorageSlot` array in the Web Client ([#1469](https://github.com/0xMiden/rust-sdk/pull/1469)).
* Added new global default .miden directory in HOME path at the client CLI ([#1465](https://github.com/0xMiden/rust-sdk/pull/1465))

### Changes

* [BREAKING] Incremented MSRV to 1.90.
* Added typed arrays for each public web-client model/struct ([#1292](https://github.com/0xMiden/rust-sdk/pull/1292))
* [BREAKING] Unified chain tip and block number types to use `BlockNumber` instead of `u32` ([#1415](https://github.com/0xMiden/rust-sdk/pull/1415)).
* Modified the RPC client to avoid reconnection when setting commitment header ([#1166](https://github.com/0xMiden/rust-sdk/pull/1166)).
* [BREAKING] Moved `SqliteStore` and `WebStore` into their own separate crates ([#1253](https://github.com/0xMiden/rust-sdk/pull/1253)).
* [BREAKING] Added `block_to` parameter to `NodeRpcClient::sync_nullifiers` for better pagination control ([#1309](https://github.com/0xMiden/rust-sdk/pull/1309)).
* [BREAKING] Removed `web-tonic` feature ([#1268](https://github.com/0xMiden/rust-sdk/pull/1268)).
* [BREAKING] Updated Web Client account store functions from insert to upsert ([#1274](https://github.com/0xMiden/rust-sdk/pull/1274)).
* [BREAKING] Added connectivity to the Transport Layer, adding a new `Client` field and `Store` methods ([#1296](https://github.com/0xMiden/rust-sdk/pull/1296)).
* Removed `miden-lib` and `miden-objects` dependencies from web client & cli ([#1333](https://github.com/0xMiden/rust-sdk/pull/1333)).
* Add more context to errors when deserializing objects ([#1336](https://github.com/0xMiden/rust-sdk/pull/1336))
* [BREAKING] Renamed `TonicRpcClient` to `GrpcClient` and `tonic_rpc_client()` method to `grpc_client()` ([#1360](https://github.com/0xMiden/rust-sdk/pull/1360)).
* [BREAKING] Removed WebClient's `compileNoteScript` method and both `TransactionScript` and `NoteScript` compile methods; the new `ScriptBuilder` should be used instead ([#1331](https://github.com/0xMiden/rust-sdk/pull/1274)).
* [BREAKING] Implemented `AccountFile` in the WebClient ([#1258](https://github.com/0xMiden/rust-sdk/pull/1258)).
* [BREAKING] Added remote key storage and signature requesting to the `WebKeyStore` ([#1371](https://github.com/0xMiden/rust-sdk/pull/1371)).
* Added `sqlite_store` under `ClientBuilderSqliteExt` method to the `ClientBuilder` ([#1416](https://github.com/0xMiden/rust-sdk/pull/1416)).
* [BREAKING] Updated the Web Client to integrate Note Transport ([#1374](https://github.com/0xMiden/rust-sdk/pull/1374)).
* [BREAKING] Refactored transaction APIs to support more granular updates in the transaction lifecycle ([#1407](https://github.com/0xMiden/rust-sdk/pull/1407)).
* Updated Dexie indexes and SQL schema; fixed sync-related transaction state bug ([#1452](https://github.com/0xMiden/rust-sdk/pull/1452)).
* Started syncing output note nullifiers by default, to track when they are consumed ([#1452](https://github.com/0xMiden/rust-sdk/pull/1452)).
* Expanded some `ClientError` variants to contain explanations and hints about the errors ([#1462](https://github.com/0xMiden/rust-sdk/pull/1462)).
* [BREAKING] Removed debug mode from the client, migrated to VM 0.20 ([#1629](https://github.com/0xMiden/rust-sdk/pull/1629)).

## 0.11.11 (2025-10-16)

* Added missing details to `SigningInputs` object to fetch underlying data type ([#1389](https://github.com/0xMiden/rust-sdk/pull/1389)).

## 0.11.10 (2025-10-15)

* Optimized sync-related lookups and RPC requests ([#1387](https://github.com/0xMiden/rust-sdk/pull/1387)).

## 0.11.9 (2025-10-08)

* Fixed a bug where StateSync failed when called multiple times while using Safari ([#1377](https://github.com/0xMiden/rust-sdk/pull/1377)).
* Implemented new note compatibility checker [(#1376)](https://github.com/0xMiden/rust-sdk/pull/1376).
* Added indexes to improve sync process performance [(#1363)](https://github.com/0xMiden/rust-sdk/pull/1363).

## 0.11.8 (2025-09-29)

* Added `serialize` and `deserialize` methods for `NoteScript` [(#1117)](https://github.com/0xMiden/rust-sdk/pull/1117).

## 0.11.7 (2025-09-26)

* Fixed an issue where `AccountId` was being left as null-pointer ([#1340](https://github.com/0xMiden/rust-sdk/pull/1340)).

## 0.11.6 (2025-09-18)

* Added a way to retrieve a secret key in the client given a pub key ([#1293](https://github.com/0xMiden/rust-sdk/pull/1293)).
* Reexported all authentication components from `miden-lib` ([#1297](https://github.com/0xMiden/rust-sdk/pull/1297)).
* Added `Signature` to the list of exported types in `index.d.ts`([#1303](https://github.com/0xMiden/rust-sdk/pull/1303)).
* Patched `miden-base` dependencies to 0.11.4 ([#1314](https://github.com/0xMiden/rust-sdk/pull/1314)).

## 0.11.4 (2025-09-11)

* Added a mutable getter for `TransactionRequest`'s advice map ([#1254](https://github.com/0xMiden/rust-sdk/pull/1254)).
* Added a way to retrieve map items in web client ([#1282](https://github.com/0xMiden/rust-sdk/pull/1282)).
* Defined `AccountInterface.Unspecified` in web client ([#1286](https://github.com/0xMiden/rust-sdk/pull/1286)).
* Removed `AccountId.fromBech32` ([#1288](https://github.com/0xMiden/rust-sdk/pull/1288)).

## 0.11.3 (2025-09-08)

* Refreshed dependencies ([#1269](https://github.com/0xMiden/rust-sdk/pull/1269)).

## 0.11.2 (2025-09-02)

* Added WASM bindings for the `Address` type from the miden_objects crate ([#1244](https://github.com/0xMiden/rust-sdk/pull/1244)).
* Updated index.d.ts file to reflect recent address changes + updates to `NetworkId` enum ([#1249](https://github.com/0xMiden/rust-sdk/pull/1249))

## 0.11.1 (2025-08-31)

### Fixes

* Added JS files generated from TypeScript ([#1218](https://github.com/0xMiden/rust-sdk/pull/1218)).
* Changed method for automatically picking up tests for integraion tests binary ([#1219](https://github.com/0xMiden/rust-sdk/pull/1219)).

## 0.11.0 (2025-08-30)

### Features

* Added ability to convert `Word` to `U64` array and `Felt` array in Web Client ([#1041](https://github.com/0xMiden/rust-sdk/pull/1041)).
* [BREAKING] Added genesis commitment header to `TonicRpcClient` requests ([#1045](https://github.com/0xMiden/rust-sdk/pull/1045)).
* Added `TokenSymbol` type to Web Client ([#1046](https://github.com/0xMiden/rust-sdk/pull/1046)).
* Implemented missing endpoints for the `MockRpcApi` ([#1074](https://github.com/0xMiden/rust-sdk/pull/1074)).
* Added bindings for retrieving storage `AccountDelta` in the web client ([#1098](https://github.com/0xMiden/rust-sdk/pull/1098)).
* Added `TransactionSummary`, `AccountDelta`, and `BasicFungibleFaucet` types to Web Client ([#1115](https://github.com/0xMiden/rust-sdk/pull/1115)).
* Added authentication arguments support to `TransactionRequest` ([#1121](https://github.com/0xMiden/rust-sdk/pull/1121)).
* Added `multicall` support for the CLI ([#1141](https://github.com/0xMiden/rust-sdk/pull/1141)).
* Added `SigningInputs` to Web Client ([#1160](https://github.com/0xMiden/rust-sdk/pull/1160)).
* Added an `RpcClient` to the Web Client, with a `getNotesById` call ([#1191](https://github.com/0xMiden/rust-sdk/pull/1191)).

### Changes

* [BREAKING] Incremented MSRV to 1.88.
* Introduced enums instead of booleans for public APIs ([#1042](https://github.com/0xMiden/rust-sdk/pull/1042)).
* [BREAKING] Updated `toBech32` AccountID method: it now expects a parameter to specify the NetworkID ([#1043](https://github.com/0xMiden/rust-sdk/pull/1043)).
* [BREAKING] Updated `applyStateSync` to receive a single object and then write the changes in a single transaction ([#1050](https://github.com/0xMiden/rust-sdk/pull/1050)).
* [BREAKING] Refactored `OnNoteReceived` callback to return enum with update action ([#1051](https://github.com/0xMiden/rust-sdk/pull/1051)).
* [BREAKING] Made authenticator optional for `ClientBuilder` and `Client::new`. The authenticator parameter is now optional, allowing clients to be created without authentication capabilities ([#1056](https://github.com/0xMiden/rust-sdk/pull/1056)).
* [BREAKING] `insertAccountRecord` changed the order of some parameters [(#1068)](https://github.com/0xMiden/rust-sdk/pull/1068).
* The rust-client has now a simple TypeScript setup for its JS code [(#1068)](https://github.com/0xMiden/rust-sdk/pull/1068).
* Added the `miden-client-integration-tests` binary for running integration tests against a remote node ([#1075](https://github.com/0xMiden/rust-sdk/pull/1075)).
* [BREAKING] Changed `OnNoteReceived` from closure to trait object ([#1080](https://github.com/0xMiden/rust-sdk/pull/1080)).
* `NoteScript` now has a `toString` method that prints its own MAST source [(#1082)](https://github.com/0xMiden/rust-sdk/pull/1082).
* Added support for `MockRpcApi` to web client ([#1096](https://github.com/0xMiden/rust-sdk/pull/1096)).
* [BREAKING] Implemented asynchronous execution hosts and removed web key store workarounds [(#1104)](https://github.com/0xMiden/rust-sdk/pull/1104).
* Exposed signatures and serialization for public keys and secret keys [(#1107)](https://github.com/0xMiden/rust-sdk/pull/1107).
* Added a `exportAccount` method in Web Client ([#1111](https://github.com/0xMiden/rust-sdk/pull/1111)).
* Exposed additional `TransactionFilter` filters in Web Client ([#1114](https://github.com/0xMiden/rust-sdk/pull/1114)).
* Refactored internal structure of account vault and storage Sqlite tables ([#1128](https://github.com/0xMiden/rust-sdk/pull/1128)).
* Added a `NoteScript` getter for the Web Client `Note` model ([#1135](https://github.com/0xMiden/rust-sdk/pull/1135/)).
* Account related records are now directly stored as Uint8Arrays instead of using Blobs, this fixes a bug with Webkit-based browsers [(#1137)](https://github.com/0xMiden/rust-sdk/pull/1137).
* [BREAKING] Fixed `createP2IDNote` and `createP2IDENote` convenience functions in the Web Client ([#1142](https://github.com/0xMiden/rust-sdk/pull/1142)).
* Store changes after transaction execution no longer require fetching the whole account state ([#1147](https://github.com/0xMiden/rust-sdk/pull/1147)).
* [BREAKING] Use typescript for web_store files: transactions.js & sync.js; add some utils to avoid error-related boilerplate [(#1151)](https://github.com/0xMiden/rust-sdk/pull/1151). Breaking change: `upsertTransactionRecord` has changed the order of its parameters.
* [BREAKING] Renamed `export/importNote` to `export/importNoteFile`, expose serialization functions for `Note` in Web Client ([#1159](https://github.com/0xMiden/rust-sdk/pull/1159)).
* Reexported utils to parse token amounts as base units ([#1161](https://github.com/0xMiden/rust-sdk/pull/1161)).
* Every JS file under `rust-client's` `web store` is now using Typescript ([#1171](https://github.com/0xMiden/rust-sdk/pull/1171)).
* [BREAKING] The WASM import has been changed into an async function to avoid issues with top-level awaits and some vite projects. ([#1172])(<https://github.com/0xMiden/rust-sdk/pull/1172>).
* Tracked creation and committed timestamps for `TransactionRecord` ([#1173](https://github.com/0xMiden/rust-sdk/pull/1173)).
* [BREAKING] Removed `AccountId` to bech32 conversions and the `get_account_state_delta` RPC endpoint  ([#1177](https://github.com/0xMiden/rust-sdk/pull/1177)).
* [BREAKING] Changed `exportNoteFile` to fail fast on invalid export type ([#1198](https://github.com/0xMiden/rust-sdk/pull/1198)).
* [BREAKING] Refactored RPC errors ([#1202](https://github.com/0xMiden/rust-sdk/pull/1202)).
* Accounts are now retrieved partially when reading transaction inputs ([#1438](https://github.com/0xMiden/rust-sdk/pull/1438)).

## 0.10.2 (2025-08-04)

### Fixes

* Added `AuthScheme::NoAuth` support to `Client` (#1123).

## 0.10.1 (2025-07-26)

* Avoid passing unneeded nodes to `PartialMmr::from_parts` (#1081).

## 0.10.0 (2025-07-12)

### Features

* Added support for FPI in Web Client (#958).
* Exposed `bech32` account IDs in Web Client (#978).
* Added transaction script argument support to `TransactionRequest` (#1017).
* [BREAKING] Added support for timelock P2IDE notes (#1020).

### Changes

* Replaced deprecated #[clap(...)] with #[command(...)] and #[arg(...)] (#897).
* [BREAKING] Renamed `miden-cli` crate to `miden-client-cli`, and the `miden` executable to `miden-client` (#960).
* [BREAKING] Merged `concurrent` feature with `std` (#974).
* [BREAKING] Changed `TransactionRequest` to use expected output recipients instead of output notes (#976).
* [BREAKING] Removed `TransactionExecutor` from `Client` and `NoteScreener` (#998).
* Enforced input note order in `TransactionRequest` (#1001).
* Added check for duplicate input notes in `TransactionRequest` (#1001).
* [BREAKING] Renamed P2IDR to P2IDE (#1016).
* [BREAKING] Removed `with_` prefix from builder functions (#1018).
* Added a way to instantiate a `ScriptBuilder` from `Client` (#1022).
* [BREAKING] Removed `relevant_notes` from `TransactionResult` (#1030).
* Changed sync to store notes regardless of consumption checks if it matched a tracked tag (#1031).

### Fixes

* Fixed Intermittent Block Header Error During Sync in Web Client (#997).
* Fixed Swap Transaction Request in Web Client (#1002)

## v0.9.4 (2025-07-02)

* Support Operations From Counter Contract FPI Example in Web Client (#958).

## v0.9.3 (2025-06-28)

* Fixed a bug where some partial MMR nodes were missing and causing problems with note consumption (#995).

## 0.9.2 (2025-06-11)

* Refresh dependencies (#972).

### Features

* Added necessary methods to support network transactions in the Web Client (#955).

### Changes

* Fixed wasm-opt options to improve performance of generated wasm (#961).

### Fixes

* Fixed bug where network accounts were not being updated correctly in the client (#955).

## 0.9.0 (2025-05-30)

### Features

* Added support for `bech32` account IDs in the CLI (#840).
* Added support for MASM account component libraries in Web Client (#900).
* Added support for RPC client/server version matching through HTTP ACCEPT header (#912).
* Added a way to ignore invalid input notes when consuming them in a transaction (#898).
* Added `NoteUpdate` type to the note update tracker to distinguish between different types of updates (#821).
* Updated `TonicRpcClient` and `Store` traits to be subtraits of `Send` and `Sync` (#926).
* Updated `TonicRpcClient` and `Store` trait functions to return futures which are `Send` (#926).

### Changes

* Updated Web Client README and Documentation (#808).
* [BREAKING] Removed `script_roots` mod in favor of `WellKnownNote` (#834).
* Made non-default options lowercase when prompting for transaction confirmation (#843)
* [BREAKING] Updated keystore to accept arbitrarily large public keys (#833).
* Added Examples to Mdbook for Web Client (#850).
* Added account code to `miden account --show` command (#835).
* Changed exec's input file format to TOML instead of JSON (#870).
* [BREAKING] Client's methods renamed after `PartialMmr` change to `PartialBlockchain` (#894).
* [BREAKING] Made the maximum number of blocks the client can be behind the network customizable (#895).
* Improved Web Client Publishing Flow on Next Branch (#906).
* [BREAKING] Refactored `TransactionRequestBuilder` preset builders (#901).
* Improved the consumability check of the `NoteScreener` (#898).
* Exposed new test utilities in the `testing` feature (#882).
* [BREAKING] Added `tx_graceful_blocks` to `Client` constructor and refactored `TransactionRecord` (#848).
* [BREAKING] Updated the client so that only relevant block headers are stored (#828).
* [BREAKING] Added `DiscardCause` for transactions (#853).
* Chained pending transactions get discarded when one of the transactions in the chain is discarded (#889).
* [BREAKING] Renamed `NetworkNote` and `AccountDetails` to `FetchedNote` and `FetchedAccount` respectively (#931).
* Fixed wasm-opt options to improve performance of generated wasm. wasm-opt settings were broken before.

## 0.8.2 (TBD)

* Converted Web Client `NoteType` class to `enum` (#831)
* Exported `import_account_by_id` function to Web Client (#858)
* Fixed duplicate key bug in `import_account` (#899)

## 0.8.1 (2025-03-28)

### Features

* Added wallet generation from seed & import from seed on web SDK (#710).
* [BREAKING] Generalized `miden new-account` CLI command (#728).
* Added support to import public accounts to `Client` (#733).
* Added import/export for web client db (#740).
* Added `ClientBuilder` for client initialization (#741).
* [BREAKING] Merged `TonicRpcClient` with `WebTonicRpcClient` and added missing endpoints (#744).
* Added support for script execution in the `Client` and CLI (#777).
* Added note code to `miden notes --show` command (#790).
* Added Delegated Proving Support to All Transaction Types in Web Client (#792).

### Changes

* Added check for empty pay to ID notes (#714).
* [BREAKING] Refactored authentication out of the `Client` and added new separate authenticators (#718).
* Added `ClientBuilder` for client initialization (#741).
* [BREAKING] Removed `KeyStore` trait and added ability to provide signatures to `FilesystemKeyStore` and `WebKeyStore` (#744).
* Moved error handling to the `TransactionRequestBuilder::build()` (#750).
* Re-exported `RemoteTransactionProver` in `rust-client` (#752).
* [BREAKING] Added starting block number parameter to `CheckNullifiersByPrefix` and removed nullifiers from `SyncState` (#758).
* Added recency validations for the client (#776).
* [BREAKING] Updated client to Rust 2024 edition (#778).
* [BREAKING] Removed the `TransactionScriptBuilder` and associated errors from the `rust-client` (#781).
* [BREAKING] Renamed "hash" with "commitment" for block headers, note scripts and accounts (#788, #789).
* [BREAKING] Removed `Rng` generic from `Client` and added support for different keystores and RNGs in `ClientBuilder`  (#782).
* Web client: Exposed `assets` iterator for `AssetVault` (#783)
* Updated protobuf bindings generation to use `miden-node-proto-build` crate (#807).

### Fixes

* [BREAKING] Changed Snake Case Variables to Camel Case in JS/TS Files (#767).
* Fixed Web Keystore (#779).
* Fixed case where the `CheckNullifiersByPrefix` response contained nullifiers after the client's sync height (#784).

## 0.7.2 (2025-03-05) -  `miden-client-web` and `miden-client` crates

### Changes

* [BREAKING] Added initial Web Workers implementation to web client (#720, #743).
* Web client: Exposed `InputNotes` iterator and `assets` getter (#757).
* Web client: Exported `TransactionResult` in typings (#768).
* Implemented serialization and deserialization for `SyncSummary` (#725).

### Fixes

* Web client: Fixed submit transaction; Typescript types now match underlying Client call (#760).

## 0.7.0 (2025-01-28)

### Features

* [BREAKING] Implemented support for overwriting of accounts when importing (#612).
* [BREAKING] Added `AccountRecord` with information about the account's status (#600).
* [BREAKING] Added `TransactionRequestBuilder` for building `TransactionRequest` (#605).
* Added caching for foreign account code (#597).
* Added support for unauthenticated notes consumption in the CLI (#609).
* [BREAKING] Added foreign procedure invocation support for private accounts (#619).
* [BREAKING] Added support for specifying map storage slots for FPI (#645)
* Limited the number of decimals that an asset can have (#666).
* [BREAKING] Removed the `testing` feature from the CLI (#670).
* Added per transaction prover support to the web client (#674).
* [BREAKING] Added `BlockNumber` structure (#677).
* Created functions for creating standard notes and note scripts easily on the web client (#686).
* [BREAKING] Renamed plural modules to singular (#687).
* [BREAKING] Made `idxdb` only usable on WASM targets (#685).
* Added fixed seed option for web client generation (#688).
* [BREAKING] Updated `init` command in the CLI to receive a `--network` flag (#690).
* Improved CLI error messages (#682).
* [BREAKING] Renamed APIs for retrieving account information to use the `try_get_*` naming convention, and added/improved module documentation (#683).
* Enabled TLS on tonic client (#697).
* Added account creation from component templates (#680).
* Added serialization for `TransactionResult` (#704).

### Fixes

* Print MASM debug logs when executing transactions (#661).
* Web Store Minor Logging and Error Handling Improvements (#656).
* Web Store InsertChainMmrNodes Duplicate Ids Causes Error (#627).
* Fixed client bugs where some note metadata was not being updated (#625).
* Added Sync Loop to Integration Tests for Small Speedup (#590).
* Added Serial Num Parameter to Note Recipient Constructor in the Web Client (#671).

### Changes

* [BREAKING] Refactored the sync process to use a new `SyncState` component (#650).
* [BREAKING] Return `None` instead of `Err` when an entity is not found (#632).
* Add support for notes without assets in transaction requests (#654).
* Refactored RPC functions and structs to improve code quality (#616).
* [BREAKING] Added support for new two `Felt` account ID (#639).
* [BREAKING] Removed unnecessary methods from `Client` (#631).
* [BREAKING] Use `thiserror` 2.0 to derive errors (#623).
* [BREAKING] Moved structs from `miden-client::rpc` to `miden-client::rpc::domain::*` and changed prost-generated code location (#608, #610, #615).
* Refactored `Client::import_note` to return an error when the note is already being processed (#602).
* [BREAKING] Added per transaction prover support to the client (#599).
* [BREAKING] Removed unused dependencies (#584).

## 0.6.0 (2024-11-08)

### Features

* Added FPI (Foreign Procedure Invocation) support for `TransactionRequest` (#560).
* [BREAKING] Added transaction prover component to `Client` (#550).
* Added WASM consumable notes API + improved note models (#561).
* Added remote prover support to the web client with CI tests (#562).
* Added delegated proving for web client + improved note models (#566).
* Enabled setting expiration delta for `TransactionRequest` (#553).
* Implemented `GetAccountProof` endpoint (#556).
* [BREAKING] Added support for committed and discarded transactions (#531).
* [BREAKING] Added note tags for future notes in `TransactionRequest` (#538).
* Added support for multiple input note inserts at once (#538).
* Added support for custom transactions in web client (#519).
* Added support for remote proving in the CLI (#552).
* Added Transaction Integration Tests for Web Client (#569).
* Added WASM Input note tests + updated input note models (#554)
* Added Account Integration Tests for Web Client (#532).

### Fixes

* Fixed WASM + added additional WASM models (#548).
* [BREAKING] Added IDs to `SyncSummary` fields (#513).
* Added better error handling for WASM sync state (#558).
* Fixed Broken WASM (#519).
* [BREAKING] Refactored Client struct to use trait objects for inner struct fields (#539).
* Fixed panic on export command without type (#537).

### Changes

* Moved note update logic outside of the `Store` (#559).
* [BREAKING] Refactored the `Store` structure and interface for input notes (#520).
* [BREAKING] Replaced `maybe_await` from `Client` and `Store` with `async`, removed `async` feature (#565, #570).
* [BREAKING] Refactored `OutputNoteRecord` to use states and transitions for updates (#551).
* Rebuilt WASM with latest dependencies (#575).
* [BREAKING] Removed serde's de/serialization from `NoteRecordDetails` and `NoteStatus` (#514).
* Added new variants for the `NoteFilter` struct (#538).
* [BREAKING] Re-exported `TransactionRequest` from submodule, renamed `AccountDetails::Offchain` to `AccountDetails::Private`, renamed `NoteDetails::OffChain` to `NoteDetails::Private` (#508).
* Expose full SyncSummary from WASM (#555).
* [BREAKING] Changed `PaymentTransactionData` and `TransactionRequest` to allow for multiple assets per note (#525).
* Added dedicated separate table for tracked tags (#535).
* [BREAKING] Renamed `off-chain` and `on-chain` to `private` and `public` respectively for the account storage modes (#516).

## v0.5.0 (2024-08-27)

### Features

* Added support for decimal values in the CLI (#454).
* Added serialization for `TransactionRequest` (#471).
* Added support for importing committed notes from older blocks than current (#472).
* Added support for account export in the CLI (#479).
* Added the Web Client Crate (#437)
* Added testing suite for the Web Client Crate (#498)
* Fixed typing for the Web Client Crate (#521)
* [BREAKING] Refactored `TransactionRequest` to represent a generalized transaction (#438).

### Enhancements

* Added conversions for `NoteRecordDetails` (#392).
* Ignored stale updates received during sync process (#412).
* Changed `TransactionRequest` to use `AdviceInputs` instead of `AdviceMap` (#436).
* Tracked token symbols with config file (#441).
* Added validations in transaction requests (#447).
* [BREAKING] Track expected block height for notes (#448).
* Added validation for consumed notes when importing (#449).
* [BREAKING] Removed `TransactionTemplate` and `account_id` from `TransactionRequest` (#478).

### Changes

* Refactor `TransactionRequest` constructor (#434).
* [BREAKING] Refactored `Client` to merge submit_transaction and prove_transaction (#445).
* Change schema and code to to reflect changes to `NoteOrigin` (#463).
* [BREAKING] Updated Rust Client to use the new version of `miden-base` (#492).

### Fixes

* Fixed flaky integration tests (#410).
* Fixed `get_consumable_notes` to consider block header information for consumability (#432).

## v0.4.1 (2024-07-08) - `miden-client` crete only

* Fixed the build script to avoid updating generated files in docs.rs environment (#433).

## v0.4.0 (2024-07-05)

### Features

* [BREAKING] Separated `prove_transaction` from `submit_transaction` in `Client`. (#339)
* Note importing in client now uses the `NoteFile` type (#375).
* Added `wasm` and `async` feature to make the code compatible with WASM-32 target (#378).
* Added WebStore to the miden-client to support WASM-compatible store mechanisms (#401).
* Added WebTonicClient to the miden-client to support WASM-compatible RPC calls (#409).
* [BREAKING] Added unauthenticated notes to `TransactionRequest` and necessary changes to consume unauthenticated notes with the client (#417).
* Added advice map to `TransactionRequest` and updated integration test with example using the advice map to provide more than a single `Word` as `NoteArgs` for a note (#422).
* Made the client `no_std` compatible (#428).

### Enhancements

* Fixed the error message when trying to consume a pending note (now it shows that the transaction is not yet ready to be consumed).
* Added created and consumed note info when printing the transaction summary on the CLI. (#348).
* [BREAKING] Updated CLI commands so assets are now passed as `<AMOUNT>::<FAUCET_ACCOUNT_ID>` (#349).
* Changed `consume-notes` to pick up the default account ID if none is provided, and to consume all notes that are consumable by the ID if no notes are provided to the list. (#350).
* Added integration tests using the CLI (#353).
* Simplified and separated the `notes --list` table (#356).
* Fixed bug when exporting a note into a file (#368).
* Added a new check on account creation / import on the CLI to set the account as the default one if none is set (#372).
* Changed `cargo-make` usage for `make` and `Makefile.toml` for a regular `Makefile` (#359).
* [BREAKING] Library API reorganization (#367).
* New note status added to reflect more possible states (#355).
* Renamed "pending" notes to "expected" notes (#373).
* Implemented retrieval of executed transaction info (id, commit height, account_id) from sync state RPC endpoint (#387).
* Added build script to import Miden node protobuf files to generate types for `tonic_client` and removed `miden-node-proto` dependency (#395).
* [BREAKING] Split cli and client into workspace (#407).
* Moved CLI tests to the `miden-cli` crate (#413).
* Restructured the client crate module organization (#417).

## v0.3.1 (2024-05-22)

* No changes; re-publishing to crates.io to re-build documentation on docs.rs.

## v0.3.0 (2024-05-17)

* Added swap transactions and example flows on integration tests.
* Flatten the CLI subcommand tree.
* Added a mechanism to retrieve MMR data whenever a note created on a past block is imported.
* Changed the way notes are added to the database based on `ExecutedTransaction`.
* Added more feedback information to commands `info`, `notes list`, `notes show`, `account new`, `notes import`, `tx new` and `sync`.
* Add `consumer_account_id` to `InputNoteRecord` with an implementation for sqlite store.
* Renamed the CLI `input-notes` command to `notes`. Now we only export notes that were created on this client as the result of a transaction.
* Added validation using the `NoteScreener` to see if a block has relevant notes.
* Added flags to `init` command for non-interactive environments
* Added an option to verify note existence in the chain before importing.
* Add new store note filter to fetch multiple notes by their id in a single query.
* [BREAKING] `Client::new()` now does not need a `data_store_store` parameter, and `SqliteStore`'s implements interior mutability.
* [BREAKING] The store's `get_input_note` was replaced by `get_input_notes` and a `NoteFilter::Unique` was added.
* Refactored `get_account` to create the account from a single query.
* Added support for using an account as the default for the CLI
* Replace instead of ignore note scripts with when inserting input/output notes with a previously-existing note script root to support adding debug statements.
* Added RPC timeout configuration field
* Add off-chain account support for the tonic client method `get_account_update`.
* Refactored `get_account` to create the account from a single query.
* Admit partial account IDs for the commands that need them.
* Added nextest to be used as test runner.
* Added config file to run integration tests against a remote node.
* Added `CONTRIBUTING.MD` file.
* Renamed `format` command from `Makefile.toml` to `check-format` and added a new `format` command that applies the formatting.
* Added methods to get output notes from client.
* Added a `input-notes list-consumable` command to the CLI.

## 0.2.1 (2024-04-24)

* Added ability to start the client in debug mode (#283).

## 0.2.0 (2024-04-14)

* Added an `init` command to the CLI.
* Added support for on-chain accounts.
* Added support for public notes.
* Added `NoteScreener` struct capable of detecting notes consumable by a client (via heuristics), for storing only relevant notes.
* Added `TransactionRequest` for defining transactions with arbitrary scripts, inputs and outputs and changed the client API to use this definition.
* Added `ClientRng` trait for randomness component within `Client`.
* Refactored integration tests to be run as regular rust tests.
* Normalized note script fields for input note and output note tables in SQLite implementation.
* Added support for P2IDR (pay-to-id with recall) transactions on both the CLI and the lib.
* Removed the `mock-data` command from the CLI.

## 0.1.0 (2024-03-15)

* Initial release.
