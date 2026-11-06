# Future-proof the public API before the stable release

Nothing in the workspace uses `#[non_exhaustive]` today, so almost any change — a new enum variant, a new struct field, a new `Store` method — is a breaking change for downstream users. Before we commit to a stable release we should harden the API surface so it can grow without major version bumps.

## Motivation

- After the stable release, every enum variant, struct field, or trait method added to this surface is a semver-major event for every downstream (wallets, web-sdk, integrators). The protocol still evolves fast — new note states, discard causes, and RPC fields arrive every minor — so an API that can't absorb additive change forces a choice between constant major bumps (ecosystem churn, version fragmentation) and freezing development.
- Glob re-exports mean upstream crates can widen our public API without any review on our side.
- The asymmetry is the argument: the work is near-free now (annotations and constructors), while retrofitting later is itself a breaking change — the fix for "we can't evolve without breaking" would break everyone one more time.

Concretely:

- **Enums:** mark the ~60 public enums `#[non_exhaustive]` (errors, `TransactionStatus`, `DiscardCause`, `OutputNoteState`, `NoteFilter`, etc.), except where exhaustive matching is an intentional contract.
- **Structs:** 45 public structs expose public fields. They split into:
  - ~19 RPC domain DTOs under `rpc/domain/` that mirror protobuf messages — since proto messages are explicitly designed to accrete fields, these should all become `#[non_exhaustive]` with public fields retained for read ergonomics.
  - The 10 input-note state types under `store/note_record/` — same treatment.
  - High-traffic user-facing records — `SyncSummary` (8 fields, and it just grew one for NTL support), `TransactionDetails` (9 fields), `TransactionRecord`, `PswapLineageRecord`, `NoteTagRecord`, `FaucetMetadata` — same treatment, plus a case-by-case call on whether any field's type is likely to change (those get accessors instead).
  - Input/config structs users construct (`GetAccountRequest`, `NoteTransportConfig`, `StateSyncInput`) — these must first gain constructors or builders, since `#[non_exhaustive]` disables downstream struct-literal construction and functional-update syntax entirely.
- **Accidental leaks:** privatize `ClientBuilder.store` (`builder.rs:115`) and `FilesystemKeyStore.keys_directory` (`fs_keystore.rs:155`). Also `rpc::generated` (`pub` under `testing`, `rpc/mod.rs:102`) and `note_transport::generated` (`pub` unconditionally, `note_transport/mod.rs:2`): nothing outside the service modules uses them, so make both `pub(crate)` now, before node#2699/#2708 rename every proto message and service (see WORK-03, RPC layer prerequisites).
- **Traits:** `Store` (~59 methods) is implemented outside this repo (web-sdk IndexedDB store), so any new method breaks external stores — add default impls, split it, or document it as semver-exempt. Seal the traits not meant for external implementation (`TransactionProver`, `NoteObserver`, `TransactionObserver`, `OnNoteReceived`, `StoreFactory`, `Keystore` — decide each). Resolved by WORK-03 if the crate split proceeds. `NodeRpcClient` (32 methods) has four implementors including external mocks, so new endpoints must arrive as provided methods with an unsupported-endpoint default, never as required methods.
- **Re-exports:** replace glob re-exports (`pub use errors::*`, `pub use miden_agglayer::*`) with explicit lists, and curate the ~40 protocol-crate re-export blocks, since they currently fuse our semver surface to `miden-protocol`/`miden-standards`/`miden-tx`. Also absorbed by WORK-03's core-crate design if it proceeds — do it once, there.
- **CI:** add `cargo-semver-checks` so future breakage is caught mechanically.

## Sequencing

- Per-type annotations can start immediately; they survive any crate move.
- The trait-stability and re-export items fold into WORK-03's `miden-client-core` design if that split happens — avoid doing them twice.
- All breaking changes here ship in the same major release as WORK-03.

Cross-links: WORK-03 (crate restructuring), WORK-05 (FPI needs `RpcLimits` to be `#[non_exhaustive]` before gaining a field). The comparative-review artifact's `stab-4`/`stab-5` plans cover the same ground and add: exact-pinning the protocol crates, a deprecation policy keyed to the changelog taxonomy, and tokio-style stability tiers — coordinate rather than duplicate ([review artifact](https://claude.ai/code/artifact/c77c33ec-ae95-416a-9d21-91aa94be03fe)).
