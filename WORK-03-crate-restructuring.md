# Crate restructuring: `miden-client-core`, standalone `GrpcClient`, facade

Tracks the client side of [rust-sdk#1423](https://github.com/0xMiden/rust-sdk/issues/1423): a lean `miden-client-core` crate defining the `Store` trait, proto-agnostic RPC traits, the note-transport trait, and the structs in their signatures, plus the curated protocol re-exports; backend crates (`sqlite-store`, web-sdk's IndexedDB store, `GrpcClient`) depend on core instead of the whole client; `miden-client` implements `Client` and re-exports core as a facade so existing downstream code keeps compiling.

## Motivation

- The timing is the forcing function: moving types across crates is breaking even with facades, so the stable release is the one cheap moment to do this. Deferring means either living with the monolith through 1.x or paying a second major bump later.
- External store implementors (web-sdk IndexedDB) today depend on the entire client to implement one trait — the standard motivation for `-core` crates (sqlx-core, rand_core, tower-service).
- RPC-only consumers exist and currently must carry stores/proving/CLI weight.
- The `rpc/domain/` *type definitions* are proto-agnostic, but the files are not: 9 of the 10 domain files import `generated`, because the `TryFrom<proto::…>` impls and helpers (`note_id_from_proto`, `get_param`, …) sit next to the types (`account`, `account_vault`, `limits`, `note`, `nullifier`, `status`, `storage_map`, `sync`, `transaction`; only `mod.rs` is clean). Extraction becomes a file move only after those conversions move into the gRPC backend as per-endpoint codecs (see "RPC layer prerequisites" below). That is legal across crates: the backend owns the proto types, so `impl TryFrom<proto::X> for core::Y` passes the orphan rule.
- The `Store` layer's `miden-tx` coupling is one error variant (`StoreError::DataStoreError`) plus redirectable serde re-exports — core can plausibly avoid `miden-tx` entirely.

## Design rules (from ecosystem precedent)

- **Sizing (rand_core/tower-service lesson):** core's value is inversely proportional to its size. Core gets types that appear in trait signatures + their errors + curated re-exports. Sync orchestration, note screener, `TransactionRequestBuilder` stay in `miden-client`.
- **Keep `miden-tx` out of core** (or feature-gate authenticator concerns) — otherwise the lean-RPC story collapses.
- **One release train (alloy lesson, not ethers):** workspace stays version-locked, facade re-exports consistent versions; never let core drift independently on crates.io.
- **Stability promise is "stable modulo protocol bumps"** — protocol re-exports tie core's semver to protocol releases; state this explicitly.

## Store trait collapse (absorbed from the API-hardening discussion)

Reduce ~57 required methods to ~45 with **zero performance loss** — collapse required-ness, not implementations:

- `apply_transaction` → default over `apply_transaction_batch(vec![u])` (docs already define batch as semantically equivalent).
- `set_setting`/`remove_setting` → defaults over `apply_settings_mutations` (one-element batches).
- Account-header family (4 methods) → one method taking an `AccountFilter { All, Ids, Commitment }`, others become default projections.
- Block-header family (4 → 1) via `BlockFilter { Numbers, Tracked, All }`; also fix `get_tracked_block_header_numbers` returning `BTreeSet<usize>` instead of `BlockNumber`.
- `get_input_note_by_offset` (5 params) → `NoteQuery` struct or default impl over `get_input_notes`.
- Projection conveniences (`get_account_ids`, `get_tracked_block_header_numbers`, offset queries) stay as **overridable provided methods** — the `Iterator::nth`/`Read::read_to_end` pattern: correct default body, backends keep their optimized SQL as overrides. Neither backend deletes a single query.
- `get_current_timestamp` doesn't belong on the trait (clock concern, only sync method) — move to `Client` or a clock abstraction.
- Do NOT split into capability traits (two real implementors, `Arc<dyn Store>` everywhere — would re-aggregate via supertrait anyway). Small required kernel + provided defaults IS the extension mechanism: new capabilities arrive as default methods = non-breaking.
- Run existing integration tests with timing (or a quick criterion bench on `get_input_notes`/sync paths) once to prove the refactor is motion-free.

## RPC layer prerequisites (node #2699 / #2708)

Node PRs [#2699](https://github.com/0xMiden/node/pull/2699) (a dedicated request/response message per gRPC method) and [#2708](https://github.com/0xMiden/node/pull/2708) (packages renamed to `miden.<service>.v1`, services renamed `NodeService` / `ProverService` / `NoteTransportService`) are stacked and will land in one `miden-node-proto-build` bump (client pins `0.17.0-rc.3`). Migration measured against the stacked branch on 2026-09-29: 12 files, +102/−124, green on std+tonic+testing with tests, clippy, and wasm32 with and without `tonic`. What the bump forces, and what to do while touching every conversion site anyway:

- **`build.rs`:** prost names output files after the full package, so `generate_wrapper` emits `pub mod miden.node.v1 { … }` and the build fails. Replace it with `tonic_prost_build::configure().include_file("mod.rs")`, which emits the nested module tree itself (net −52 lines). Any future package or version then needs no build-script change.
- **One version pin per service:** each `generated.rs` re-exports the chosen version under a stable alias (`pub use gen::miden::node::v1 as rpc;`), so call sites keep `proto::rpc::X` paths and a move to v2 is a one-line flip. Same for `remote_prover` and `note_transport`. One version compiled per release; the node's `miden.*.v1` scheme carries the overlap window.
- **Target layout:** `rpc/{mod.rs (trait), domain/, errors/, endpoint.rs, verifying/}` stay proto-free and move to core. `rpc/wire/{mod.rs (version alias), transport.rs (channel setup for std+wasm, accept/bearer interceptors, retry), v1/{generated.rs, codec/, client.rs}}` is the `GrpcClient` crate. `codec/` holds one struct per endpoint with `validate` / `encode` / `decode` and associated `Request` / `Response` / `Output` types; `client.rs` is one line per trait method; `transport.rs` runs encode → retry → send → decode once, generically. A v2 is a copy of `wire/v1` with the differing codecs edited. Remote prover and note transport get the same shape.
- **`generated` must be `pub(crate)`:** `rpc::generated` is `pub` under `testing` (`rpc/mod.rs:102`) and `note_transport::generated` is `pub` unconditionally (`note_transport/mod.rs:2`). Nothing outside the three service modules uses either, so this is free and it removes proto names from the semver surface before the split.
- **Compat facts for rollout:** #2699 changes wire bytes on only three messages (submit tx/batch requests, `GetNetworkNoteStatus` request, `GetTransactionEncryptionKey` response); #2708 changes every route, so old client ↔ new node fails with gRPC `Unimplemented` on every call. Map `Unimplemented` on a `NodeService` method to an explicit incompatible-version error. The review artifact's stab-6 (capability negotiation, N/N-1 support) still needs the node-side asks: structured mismatch errors (node#1129) and feature enumeration on `Status`, which is itself versioned after #2708.
- **Endpoint registry drift guard:** `RpcEndpoint::proto_name` returns `SubmitProvenTransaction` and `SubmitProvenBatch` for methods the proto calls `SubmitProvenTx` and `SubmitProvenTxBatch` (`rpc/mod.rs:766-767`). Add a unit test that checks every `proto_name` against the methods in `rpc_api_descriptor()`, and derive the `is_idempotent` / requires-genesis tables from the same descriptor instead of hand-written matches.
- **The `rpc-method-trait` worktree experiment:** keep its per-endpoint `RpcMethod` structs (`validate` / `encode` / `decode`); drop the `RpcRequest` / `RpcResponse` enums and the public `send` method. Those enums carry `proto::rpc::AccountRequest` and friends through the public trait, the opposite of a proto-agnostic core trait, and would make every node rename a breaking change for all four implementors (`GrpcClient`, `VerifyingRpcClient`, `MockRpcApi`, `CannedTransport`) plus web-sdk. Codecs are the private, generic form of the same idea.
- **New trait methods get default bodies** returning an unsupported-endpoint error, so adding an endpoint never breaks external `NodeRpcClient` implementors (32 methods, 4 implementors today).
- **CI:** a non-blocking job building the client against node `next`'s `proto/` crate would have flagged both PRs weeks before the bump.

## Sequencing

- Must precede or accompany the stable release — moving types across crates later is breaking even with facades.
- The conversion move (`rpc/domain/` → `rpc/wire/v1/codec/`) is non-breaking and precedes the split. Do it on the proto-build bump that carries node#2699/#2708, since that bump already touches every conversion site; the split then really is a file move.
- Absorbs WORK-01's re-export curation and trait-stability items (do them once, here).
- The trait surgery (method collapse) rides the same breaking release as the move into core — one migration guide for downstream instead of two.
- `web-objects` (shared WASM bindings) is a separate call for the web-sdk repo; it should not gate the Rust-side split.

Cross-links: WORK-01 (absorbs its trait/re-export items), WORK-08 (NTL trait placement decided here), rust-sdk#1423, node#2699 / node#2708 (gRPC message and service renames; see RPC layer prerequisites), the review artifact's stab-6.
