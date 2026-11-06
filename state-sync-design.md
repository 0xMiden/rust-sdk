# State sync: one session, one record owner, and application plugins

## Decision

Use one call for normal wallet sync:

```rust
let report = client.sync_state().await?;
```

Register application plugins when constructing the client. Use PSWAP as the first plugin:

```rust
let mut client = Client::builder()
    .with_sync_plugin(PswapPlugin::new())
    .build()
    .await?;

let report = client.sync_state().await?;
```

The builder example omits the store, RPC, and authenticator configuration. The high-level method
prepares, runs, and applies one single-use `StateSync` session. Advanced callers can inspect the
staged update before application:

```rust
let sync = client.prepare_state_sync().await?;
let update = sync.run().await?;
let report = client.apply_state_sync(update).await?;
```

The client prepares one coherent seed. `StateSync::run(self)` fetches and validates network data,
reconciles the seed, and returns one staged update. Applying the update performs one atomic store
transaction and then refreshes the client's in-memory chain state.

The result answers three questions: through which authenticated block is the persisted chain state
consistent, which records changed, and which sources or plugins still need work. A staged update
describes proposed writes. A `SyncReport` describes the writes that committed and reports component
status. It does not claim that every data source succeeded because chain sync succeeded.

Use one owner for full domain objects. The session owns a `SyncRecords` registry containing account,
note, transaction, and witness values. Trackers contain only stable identifiers, indexes, and small
transition decisions. They never contain a second copy of a full record.

Do not give `StateSync` a general store interface. Load the active context once before the run and
provide a narrow `SyncRead` adapter for batched identity lookups discovered during the run. Records
returned by the adapter move into the session registry exactly once. The final store transaction
merges candidates with current persisted state and rejects an update if concurrent local changes
made its seed incompatible.

This design deliberately chooses a simpler first implementation:

- One complete chain sync for the wallet can advance the wallet checkpoint.
- A custom partial run is observational until durable per-scope coverage exists.
- The engine runs validation and core reconciliation in a fixed sequence.
- Application plugins declare interests and return contributions through a small public contract.
- Required plugins must succeed before the wallet checkpoint advances. Optional plugins need an
  independent replay mechanism before they can fail without blocking that checkpoint.
- There is no public phase API, generic tracker trait, event bus, resolver registry, or persistence
  hook.
- `StateSyncUpdate` remains the only persistence boundary.

The examples below describe the intended replacement API. They are not compatibility requirements
for the current API.

## 1. Ownership model

### Full objects live in one registry

```rust
struct SyncRecords {
    accounts: BTreeMap<AccountId, AccountSyncRecord>,
    input_notes: BTreeMap<NoteDetailsCommitment, InputNoteRecord>,
    output_notes: BTreeMap<NoteId, OutputNoteRecord>,
    transactions: BTreeMap<TransactionId, TransactionRecord>,
    public_account_updates: BTreeMap<AccountId, PublicAccountUpdate>,
    private_account_mismatches: BTreeMap<AccountId, Word>,
    account_witnesses: BTreeMap<AccountId, AccountWitness>,
}
```

The exact field names are illustrative. The important rule is that a full object has one owner
during the run. RPC buffers can own raw responses until validation. After validation, the session
moves accepted values into `SyncRecords`.

### Trackers contain identifiers and transition state

```rust
struct NoteUpdateTracker {
    input_updates: BTreeMap<NoteDetailsCommitment, NoteUpdateType>,
    output_updates: BTreeMap<NoteId, NoteUpdateType>,
    input_by_id: BTreeMap<NoteId, NoteDetailsCommitment>,
    input_by_nullifier: BTreeMap<Nullifier, NoteDetailsCommitment>,
    output_by_nullifier: BTreeMap<Nullifier, NoteId>,
}

struct TransactionUpdateTracker {
    tracked: BTreeSet<TransactionId>,
    outcomes: BTreeMap<TransactionId, TransactionOutcome>,
    transaction_by_initial_state: BTreeMap<(AccountId, Word), TransactionId>,
    child_by_initial_state: BTreeMap<Word, TransactionId>,
    external_nullifier_accounts: BTreeMap<Nullifier, AccountId>,
}

struct AccountUpdateTracker {
    tracked: BTreeSet<AccountId>,
    updated_public: BTreeSet<AccountId>,
    mismatched_private: BTreeSet<AccountId>,
    refreshed_witnesses: BTreeSet<AccountId>,
    pending_transactions: BTreeMap<AccountId, Vec<TransactionId>>,
}
```

Commitments and nullifiers are identifiers in these indexes. Status enums and update kinds are
tracker state, not duplicated domain records.

Tracker methods receive the registry explicitly:

```rust
transactions.observe_inclusion(transaction_id, &mut records)?;
notes.observe_nullifier(nullifier, &mut records)?;
accounts.reconcile(account_id, &transactions, &mut records)?;
```

This makes ownership visible. A tracker cannot return a stale copy of an object because it never
owns one.

### Stable identifiers

| Entity | Session key |
| --- | --- |
| Account, account update, account witness | `AccountId` |
| Input note | `NoteDetailsCommitment` |
| Metadata-bearing input-note lookup | `NoteId` → `NoteDetailsCommitment` |
| Output note | `NoteId` |
| Transaction | `TransactionId` |
| Account-state dependency | `(AccountId, initial commitment)` |

An input note cannot always use `NoteId`: an expected note can lack metadata. Its details commitment
is available before metadata and remains stable when the note later receives an ID and nullifier.

## 2. Session structure

```rust
struct StateSync {
    rpc: Arc<dyn NodeRpcClient>,
    transport: Option<Arc<dyn NoteTransportClient>>,
    read: Arc<dyn SyncRead>,
    origin: SyncOrigin,
    records: SyncRecords,
    notes: NoteUpdateTracker,
    transactions: TransactionUpdateTracker,
    accounts: AccountUpdateTracker,
    chain: ChainTracker,
    interests: SyncInterests,
    transport_progress: Option<TransportProgress>,
    plugins: Vec<PreparedSyncPlugin>,
    plugin_state_updates: Vec<PluginStateUpdate>,
    component_status: SyncComponentStatus,
    on_note: Option<NoteCallback>,
}

impl StateSync {
    async fn run(self) -> Result<StateSyncUpdate, SyncError>;
}
```

`Client::prepare_state_sync` reads a coherent seed and creates a revision-bound `SyncRead` adapter.
The seed contains:

- The chain checkpoint and partial MMR.
- Confirmed account baselines and their provenance.
- Active input and output notes that can still transition.
- Pending transactions and their account-state dependencies.
- Note tags, note-transport progress, and plugin requirements.
- Registered plugin state, including active PSWAP lineages and their original-note identifiers.

The preparation step creates the registry and gives each tracker the identifiers it needs. It does
not copy records into trackers.

Read the seed and plugin state from one store snapshot. The revision belongs to that same snapshot.
Every local mutation that affects sync context must change the revision, including changes to
pending transactions, tags, transport cursors, and plugin state. `SyncOrigin` binds the seed to the
store identity, network identity, starting checkpoint, and revision.

Client registration holds plugin factories and configuration. Preparation creates a fresh plugin
session for each run and retry. Plugin sessions own their application state and domain identifiers.
Full account, note, and transaction records remain in `SyncRecords`.

The read adapter is intentionally small:

```rust
trait SyncRead {
    async fn input_notes_by_id(
        &self,
        ids: &[NoteId],
    ) -> Result<Versioned<Vec<InputNoteRecord>>, SyncError>;

    async fn input_notes_by_details(
        &self,
        commitments: &[NoteDetailsCommitment],
    ) -> Result<Versioned<Vec<InputNoteRecord>>, SyncError>;

    async fn output_notes_by_id(
        &self,
        ids: &[NoteId],
    ) -> Result<Versioned<Vec<OutputNoteRecord>>, SyncError>;
}
```

Each result reports the store revision it observed. The session accepts it only when it matches
`origin.revision`; otherwise the run returns `StaleSeed` and the client prepares a new session.
Missing records are omitted. Read failures are errors, not evidence that a record does not exist.

The adapter does not select work, expose settings, fetch accounts, mutate the store, or retain
records. The session batches missing identities, moves returned records into `SyncRecords`, and gives
their identifiers to the relevant tracker without marking them modified.

Tests and advanced callers can construct a `SyncSeed` and use a direct constructor:

```rust
let sync = StateSync::from_seed(rpc, seed)?
    .with_note_transport(transport)
    .on_note(callback);

let update = sync.run().await?;
```

Use a separate builder only if construction has enough fallible validation to justify one. Do not
keep a builder, a second seed object, and a runnable object that all contain the same data.

## 3. One fixed execution flow

```rust
async fn run(mut self) -> Result<StateSyncUpdate, SyncError> {
    self.pin_target().await?;

    let chain_batch = self.fetch_chain_batch().await?;
    let transport_batch = self.fetch_transport_batch().await;

    self.validate_and_index_chain(chain_batch)?;
    self.merge_transport_candidates(transport_batch)?;
    self.resolve_existing_discoveries().await?;
    self.reconcile_transactions()?;
    self.reconcile_accounts().await?;
    self.reconcile_notes().await?;
    self.reconcile_nullifiers().await?;
    let contributions = self.collect_plugin_contributions()?;
    self.merge_plugin_candidates(contributions)?;
    self.resolve_existing_discoveries().await?;
    self.reconcile_plugin_notes().await?;
    self.validate_plugin_state_updates()?;

    self.finish()
}
```

The implementation can fetch independent batches concurrently. It folds validated results into the
session in deterministic order. RPC completion timing must not decide which state wins.

Important ordering rules:

1. Pin and authenticate the target before target-dependent queries.
2. Resolve observed transaction inclusions before any terminal transaction discard.
3. Preserve `Discarded` as terminal. Prevent incorrect discard precedence instead of reviving a
   discarded transaction.
4. Reconcile account-state conflicts before rolling back dependent optimistic state.
5. Resolve missing discovered identities through `SyncRead` before changing their lifecycle state.
6. Add newly discovered note identifiers before building nullifier queries.
7. Give plugins authenticated observations and reconciled core state. Collect their contributions
   against the same immutable view before merging any contribution.
8. Resolve, authenticate, and check consumption for plugin-discovered notes in one explicit
   follow-up. The follow-up cannot discard an already included transaction or revive a terminal
   record.
9. Finalize only after every required chain concern and application plugin reached the pinned target.

Do not implement this as a generic fixed-point engine. The first plugin contract permits one batch
of discoveries followed by core reconciliation. Plugins cannot request recursive execution or
depend on another plugin's contribution. Reject registrations that require those capabilities.

## 4. Account-state consistency

The motivating case in [issue #640](https://github.com/0xMiden/rust-sdk/issues/640#issuecomment-2556434059)
is a client at block 100 that imports an account snapshot obtained at block 200, then syncs only
through block 150. The account can have a different state at 150. A monotonic nonce check does not
solve this temporal inconsistency.

For a wallet at checkpoint H, each active confirmed account baseline must be established through H.
Local pending execution is optimistic state derived from that baseline. It does not claim chain
confirmation.

The registry holds the full account data and provenance:

```rust
struct AccountSyncRecord {
    confirmed: Option<ConfirmedAccountState>,
    pending_transactions: Vec<TransactionId>,
}

struct ConfirmedAccountState {
    header: AccountHeader,
    verified_at: BlockRef,
    covered_through: BlockRef,
}
```

The account tracker stores the `AccountId` and related `TransactionId` values only. Transaction
records hold the initial and final commitments. `TransactionUpdateTracker` remains the only owner of
commit/discard decisions.

Required rules:

- A snapshot known only at block 200 cannot become the confirmed baseline at checkpoint 100 or 150.
- Import by ID requests and verifies the account at the client's current checkpoint.
- Import from a file activates an existing account only when its commitment matches a witness at the
  checkpoint.
- A lower network nonce does not replace valid local optimistic execution.
- A competing proven transition discards the losing pending transaction and its dependent chain.
- A public patch names its expected base commitment and target context.
- A private mismatch preserves the available body and marks the account unavailable under the
  existing lock policy.
- Store application checks the expected base and checkpoint in the same transaction as the update.

The proof block is an observation anchor, not necessarily the block where the account state was
created. A later proof cannot be relabeled as evidence at an earlier checkpoint.

## 5. Transaction transitions

`TransactionUpdateTracker` tracks transaction IDs and outcomes. The full `TransactionRecord` stays
in `SyncRecords`.

An inclusion is matched in this order:

1. Exact `TransactionId`.
2. For unauthenticated input-note processing, the same account ID, initial state, and final state.
3. Otherwise, an external transaction by a tracked account.

A different on-chain transition from the same initial account state proves that the local branch
lost. Discard the local transaction as superseded and cascade `DiscardedInitialState` through its
dependent transactions. Process all observed inclusions before stale, expiry, superseded-state, or
nullifier-based discard decisions because discard is terminal.

The tracker records only changed transaction IDs and outcomes. During finalization, the session
updates the corresponding records in the registry and moves those records into the final update.

## 6. Notes and discovery

`NoteUpdateTracker` stores note keys, lookup indexes, and update kinds. Full input and output records
remain in the registry.

The initial seed includes active notes whose lifecycle can still change. For discoveries absent from
the registry, collect their IDs and details commitments, query `SyncRead` in batches, and move any
returned records into the registry before applying new facts. A discovery still absent afterward
becomes a new candidate.

Application merges candidates with current persisted records:

- A consumed note never regresses to expected or committed.
- Known local consumption attribution is preserved.
- Consistent metadata and attachments can enrich an existing record.
- Immutable facts with the same identity must agree.
- A processing note requires compatible pending-transaction context.

This avoids loading lifetime note history into every seed and prevents a redelivered terminal note
from being treated as new. It does not create a second owner: the adapter returns owned records, the
session moves them into the registry, and trackers retain only their identifiers.

### Note transport

Merge inbound note-transport deliveries into the same registry and note tracker:

```text
NTL delivery → validate candidate → add record once → track its identifier
             → chain reconciliation → final update
```

Remove the current write-read detour:

```text
NTL delivery → import store write → store read → chain tracker → second store write
```

Transport progress advances only when every accepted delivery is represented in the applied update
or deliberately rejected. Successful complete pages can advance their cursors in the same
transaction as their note records. A failed or unprocessed page keeps its previous cursor. The
report records the failure even when chain sync succeeds.

Transport cursors are independent of the wallet's chain checkpoint. Reconcile each late delivery
against the pinned target, including a note committed or consumed before the starting checkpoint.
A successful transport fetch does not prove that the relay has delivered every private note through
the chain target. Relay-outbox flushing remains a separate client operation because it has external
side effects.

### Output discovery limitation

A public output body can produce a complete `OutputNoteRecord`. A private output header learned from
RPC cannot produce the current record type because assets and recipient details are unavailable.
Do not fabricate empty details.

The first implementation can support public output discovery. Supporting every watched output needs
a header-level observation with optional details and a later promotion path. That storage-model
change is separate from the core session simplification.

## 7. Selection and progress

Keep the first durable contract narrow: one complete chain sync for the wallet advances one wallet
checkpoint. The client prepares all required account, note, transaction, tag, and application plugin
context for that operation. Transport has separate progress and status.

Each registered plugin is required by default. A required plugin must process its interests through
the pinned target before the wallet checkpoint can advance. If the store contains active plugin
state but the required plugin is unavailable, preparation fails. It must not silently skip that
state.

Keep a durable manifest of active plugin IDs, state versions, and activation checkpoints. Create
and update its entries atomically with the corresponding plugin state. Preparation checks the
manifest even when a plugin factory is missing. Registration alone does not establish historical
coverage. Activating a plugin at the current wallet checkpoint requires a compatible baseline with
no earlier outstanding work; otherwise it needs explicit replay before activation.

An optional plugin can fail independently only when a durable cursor or retained observations permit
replay without using the advanced wallet checkpoint as its starting point. Durable optional plugins
are deferred until that mechanism exists. The first application plugin contract does not permit
best-effort persistence of incomplete wallet state.

Custom selection has two safe modes before per-scope coverage exists:

- Run against an isolated store and checkpoint.
- Return observations without advancing the normal wallet checkpoint.

The observational API returns `SyncObservations`. These observations cannot be applied to the
wallet store. Store application accepts only a complete wallet update bound to its originating
store and checkpoint.

Do not let a partial run update account A through block 120 and move the shared checkpoint past
account B's unprocessed activity at block 110.

Durable partial sync can be added later with explicit coverage per scope. Coverage is not part of
the core tracker design, and the first implementation must not introduce a generic subscription
engine in anticipation of it.

Keep these concepts distinct:

1. Seeded state whose lifecycle can change.
2. Discovery interests that can find new records.
3. Durable coverage proving how far an interest was processed.

Supplying an output-note record follows that record. It does not request every output produced by
its account. Supplying an account baseline does not implicitly enable every account discovery mode.

## 8. Application plugins and observation

Register plugin factories on the client. Each preparation creates a fresh plugin run from the same
store snapshot used by the core seed. A factory keeps configuration. A plugin run keeps application
state and identifiers for one sync attempt:

```rust
trait SyncPlugin {
    fn id(&self) -> PluginId;

    fn start(&self, state: PluginState) -> Result<Box<dyn SyncPluginRun>, PluginError>;
}

trait SyncPluginRun {
    fn requirements(&self) -> PluginRequirements;

    fn reconcile(
        self: Box<Self>,
        view: SyncView<'_>,
    ) -> Result<SyncContribution, PluginError>;
}
```

The engine assigns each plugin ID a unique settings namespace. Preparation loads that namespace
and moves its state into `start`. The plugin then declares local record identifiers and discovery
interests through `PluginRequirements`. Preparation resolves the local identifiers in the same
snapshot and adds the records to `SyncRecords`. The engine combines discovery interests before it
fetches network data. Duplicate plugin IDs are configuration errors.

`SyncView` exposes borrowed registry records, authenticated chain observations, and reconciled core
outcomes. It includes relevant observations that ordinary wallet note screening would exclude. It
does not expose the store, mutable trackers, unverified RPC responses, or unrestricted RPC calls.

A contribution contains:

- New note candidates with the evidence needed to authenticate their identity and inclusion.
- Proposed tag changes, with source attribution for the contributing plugin.
- Plugin state mutations within the registered settings namespace.

Each state mutation includes its expected base and can carry a typed application change descriptor.
The engine adds the corresponding manifest changes. A plugin cannot edit another plugin's
namespace or the engine's manifest directly.

Plugins do not choose core lifecycle states, account patches, transaction outcomes, or checkpoint
positions. The engine consumes contributions, moves candidates into the registry, resolves existing
records, and checks the candidates against the pinned target. No plugin sees another plugin's
contribution in the same run. Consistent discoveries of the same note merge. Conflicting immutable
facts or state mutations fail the run. Registration order does not select a winning value.

Plugin state mutations become eligible for persistence only after all candidate checks succeed.
Store application rechecks their expected state in the same transaction as the corresponding notes,
tags, and checkpoint. A plugin failure aborts a required run without advancing plugin state. A
retry creates a new plugin run. Plugin factories and runs must not perform external side effects.

The first contract supports one contribution and one core follow-up per plugin. It does not support
plugins that need to rerun after inspecting their candidates' final lifecycle states. Such plugins
need a separate extension to the contract.

### PSWAP as the first plugin

`PswapPlugin` demonstrates the complete lifecycle:

1. Preparation loads active lineage records and tip indexes from the existing `pswap/order/` and
   `pswap/tip/` settings keys. Plugin requirements identify the original output notes and the
   relevant PSWAP discovery interests.
2. The engine loads each original output note once into the registry. The plugin later borrows its
   body for reconstruction.
3. Reconciliation joins authenticated PSWAP attachment observations with core consumption facts.
   It walks the rounds available at the pinned target and checks each reconstructed note ID against
   the observed ID.
4. The contribution returns reconstructed payback and final remainder candidates, lineage and tip
   index mutations, and tag changes. It does not write any of them.
5. The engine resolves existing records and verifies inclusion and consumption for every candidate.
   A note reconstructed after its consumption is persisted in its final consumed state.
6. Store application commits the notes, lineage, tip indexes, tags, and wallet checkpoint together.
   The report includes the PSWAP orders that changed.

Keep node and note transport access behind their existing client interfaces. Replacing a data
source does not require the application plugin contract to expose networking or persistence hooks.

### Observation callbacks

Use one optional note callback:

```rust
async fn on_note(note: &SyncedNote, block: &BlockHeader);
```

Call it after authenticating the note and its block. The callback observes; it does not choose
membership, retain blocks, order phases, or persist. A retry can invoke it again.

Replace the observer persistence lifecycle with the plugin contribution contract. Keep plugin IDs
for registration, settings isolation, errors, and report status. The optional note callback is an
observation facility; application state changes belong in plugin contributions.

## 9. Finalization and persistence

Trackers do not become the persistence payload. `finish()` resolves their changed identifiers
against the registry and constructs a concrete `StateSyncUpdate`:

```rust
struct StateSyncUpdate {
    origin: SyncOrigin,
    target: BlockRef,
    chain_update: PartialBlockchainUpdate,
    input_note_updates: Vec<InputNoteUpdate>,
    output_note_updates: Vec<OutputNoteUpdate>,
    transaction_updates: Vec<TransactionRecord>,
    public_account_updates: Vec<PublicAccountUpdate>,
    private_account_mismatches: Vec<(AccountId, Word)>,
    account_witnesses: Vec<(AccountId, AccountWitness)>,
    tag_updates: Vec<TagUpdate>,
    plugin_state_updates: Vec<PluginStateUpdate>,
    transport_progress: Option<TransportProgress>,
    component_status: SyncComponentStatus,
}
```

This is the only point where full objects leave the session. Each changed object is moved, not
cloned, from its registry entry.

The store applies the update atomically:

1. Check the originating store, network, seed revision, and checkpoint.
2. Check entity-specific preconditions such as account patch bases, processing transactions, and
   plugin state bases.
3. Apply terminal transaction outcomes and their explicit account rollback effects.
4. Apply confirmed account, note, witness, tag, plugin state, and progress changes in their required
   domain order.
5. Advance the checkpoint only when all required chain work and application plugins succeeded.
6. Compute `SyncReport` from the rows actually inserted or changed and the component statuses.

Update the client's MMR cache only after the transaction commits. If the store revision changed,
discard the staged update and prepare a new session. Do not try to merge incompatible local
execution after the network run.

The final update contains full payloads because new notes, account patches, witnesses, and updated
transaction records cannot be reconstructed from identifiers alone. Identifier-only ownership is a
tracker rule, not a persistence rule.

### Result returned to the caller

```rust
struct SyncReport {
    from: BlockRef,
    to: BlockRef,
    changes: SyncChanges,
    transport: TransportSyncStatus,
    plugins: Vec<PluginSyncReport>,
    warnings: Vec<SyncWarning>,
}
```

`from` and `to` are the persisted chain checkpoints before and after application. Reaching `to`
means reaching the authenticated target pinned at the start of this run. It does not assert that
the node's tip stayed unchanged during the run. An unchanged checkpoint can still accompany new
transport deliveries or record enrichment.

`SyncChanges` contains effective record changes, not every intermediate observation:

| Entity | Reported change |
| --- | --- |
| Input or output note | Stable key, available note ID, discovery source, and previous and final lifecycle state |
| Account | Account ID and the confirmed update or lock outcome |
| Transaction | Transaction ID and the committed or discarded outcome, including the discard cause |
| Plugin state | Plugin ID and application changes accepted by the store; PSWAP reports order ID, depth, and state changes |

A note discovered and consumed in one run has one change with its final consumed state. An input
note without metadata uses its details commitment as its stable key. Repeated delivery of an
unchanged record does not appear as a new change. The report contains identifiers and outcomes;
callers query the store for full records.

Transport status distinguishes disabled, complete, and incomplete fetches. An incomplete fetch
includes the error and reports the cursors actually committed. Plugin status identifies the
registered plugin and its processed target. Future optional plugins must also report their
independent progress and failures. A
required plugin failure returns an error before application; there is no successful report for
that failed run.

Return an application change descriptor only when the corresponding state mutation commits and
changes its expected base. A PSWAP descriptor names the order and its depth and state transition.
The generic store confirms the mutation outcome without parsing PSWAP state. Existing summary
counts can be projections of `SyncChanges` rather than a separate result assembled before
persistence.

Return the report only after the store transaction commits. Treat pruning and other maintenance
after the commit separately from sync failure. If refreshing the in-memory cache fails, invalidate
it and report that condition without claiming the committed update was rolled back.

## 10. What this removes

| Current item | Replacement |
| --- | --- |
| `StateSyncInput` plus later tracker construction | One prepared `SyncSeed` consumed into the session |
| Public `ChainSyncData` and manually ordered phase calls | Private fields and one `run(self)` method |
| `FetchedSyncData` / `ChainAdvance` ownership ladder | Short-lived private RPC batches |
| Full records stored inside trackers | One `SyncRecords` registry plus identifier-only trackers |
| General store access during sync | A revision-bound `SyncRead` with three batched identity lookups |
| NTL import/write/read detour | Candidate insertion into the registry |
| Observer lifecycle and persistence hooks | Plugin factories, single-use plugin runs, and staged contributions |
| Private PSWAP step in the engine | The first required application plugin |
| Summary assembled from a proposed update | A committed `SyncReport` with effective changes and component status |
| Account update payload container named as a tracker | Account IDs in the tracker; payloads in the registry |
| Tracker objects inside `StateSyncUpdate` | Materialized concrete update vectors |
| Durable partial-scope framework in the first release | Complete-wallet checkpoint contract |

Keep existing note state machines, terminal transaction semantics, validation routines, account
patch construction, and the atomic SQLite boundary. Simplification changes ownership and
orchestration, not protocol rules.

## 11. Implementation order

1. Add `SyncRecords` and change `AccountUpdateTracker` to store account IDs only. Keep full account
   updates, mismatches, and witnesses in the registry.
2. Change `TransactionUpdateTracker` to store transaction IDs, outcomes, and dependency indexes.
   Preserve inclusion-before-discard precedence and discard cascades.
3. Change `NoteUpdateTracker` to store note keys and update kinds. Move note records into the
   registry without changing note state-machine behavior.
4. Add the revision-bound `SyncRead` adapter and move returned note records into the registry before
   applying discoveries.
5. Make finalization materialize the existing store payload from changed identifiers. Keep store
   application behavior stable during this step.
6. Replace `StateSyncInput`, `FetchedSyncData`, and public `ChainSyncData` phase coordination with a
   prepared single-use session.
7. Move inbound note transport into the session and remove its intermediate store write.
8. Add coherent snapshots, store revision and origin checks, plugin state mutations, and
   `SyncReport` to the atomic store contract.
9. Implement plugin factories and single-use plugin runs. Move PSWAP into the first required plugin
   and replace observer writes with contributions and the explicit candidate follow-up.
10. Keep one high-level `Client::sync_state` call and client-level plugin registration. Remove phase
    coordination and compatibility scaffolding from ordinary caller flows.
11. Add durable optional plugins and custom scopes only after their replay and coverage models are
    implemented and tested.

Use PSWAP to validate the small plugin contract before adding more application plugins. Keep a
general dependency scheduler, recursive plugin execution, and output-note storage migration outside
this implementation. They have separate correctness and persistence requirements.

## 12. Required tests

| Scenario | Required result |
| --- | --- |
| Included local transaction also appears superseded by the target account snapshot | Inclusion wins; the transaction remains committed |
| External transition starts from a pending transaction's initial account state | Losing transaction and dependent chain are discarded |
| Discarded transaction is observed again | It remains terminal; the run cannot revive it |
| Account snapshot is known only after the wallet checkpoint | It cannot become the active baseline |
| Valid local optimistic state is ahead of an older network snapshot | Pending execution is preserved |
| Account patch has the wrong base | Application rejects the update |
| NTL delivery commits and is consumed in the same window | One final consumed record is written |
| Terminal note is redelivered but absent from the seed | Application preserves terminal state |
| `SyncRead` returns a record from another store revision | The run returns `StaleSeed` |
| `SyncRead` returns an existing unmodified record | The registry owns it and the tracker does not emit it |
| Node, transport, or persistence fails | Checkpoint and corresponding progress follow the defined atomicity rules |
| Same-height local mutation occurs after preparation | Revision check rejects the staged update |
| RPC requests complete in different orders | Final update is identical |
| PSWAP reconstructs a note consumed in the window | The registry contains one record with its final state |
| Partial custom run uses the wallet store | The normal wallet checkpoint does not advance |
| Plugin preparation resolves local records while another local write occurs | The seed remains a coherent snapshot; incompatible application returns `StaleSeed` |
| The store contains active state for an unavailable required plugin | Preparation fails before the wallet checkpoint advances |
| A plugin is registered with outstanding activity before its activation checkpoint | Activation requires explicit replay; registration does not imply historical coverage |
| A required plugin fails after other contributions are staged | No staged records, plugin state, or progress are written |
| A plugin requests optional durability without an independent replay mechanism | Registration rejects that configuration |
| Plugin registration order changes | The effective update is identical |
| Two plugins return conflicting immutable facts for one note | The run fails; registration order does not choose a winner |
| A plugin candidate fails authentication or state validation | Its application state and tags do not advance |
| PSWAP contributes notes, lineage, tip indexes, and tags | All changes commit together or none commit |
| A PSWAP observation is excluded by ordinary wallet screening | The authenticated observation remains available to the plugin |
| An observation has an invalid inclusion proof | It is rejected before it reaches a plugin or callback |
| Transport delivers a note committed and consumed before the starting checkpoint | The final consumed state is persisted at the pinned target |
| Transport completes some pages and then fails | Successful page records and their cursors commit together; the report includes the failure |
| Store merge leaves a redelivered note unchanged | The report does not count it as a new discovery |
| A transaction is discarded during sync | The report includes its ID and discard cause |
| A staged update is applied to another store or network with the same numeric revision | Application rejects the origin mismatch |
| Pruning fails after the sync transaction commits | The committed checkpoint is reported; maintenance failure does not imply rollback |

## Summary

The simplification is one ownership rule and one execution rule:

- The session owns every full object exactly once; trackers refer to those objects by identifiers.
- The client prepares once, the session runs once, and the store applies one concrete update once.

Application plugins fit those rules by borrowing records and returning contributions for engine
validation. They do not add another persistence boundary. PSWAP is the first plugin that exercises
discovery, reconstruction, application state, and atomic persistence.

The caller receives one committed report containing chain progress, effective changes, transport
status, and plugin outcomes. The ordinary API remains one sync call.

`SyncRead` fits this model because it only transfers missing records into the session's single
registry. It is a lookup boundary, not another orchestration or persistence layer.

Everything else should justify itself against those rules.

[state-sync]: crates/rust-client/src/sync/state_sync.rs
[state-update]: crates/rust-client/src/sync/state_sync_update.rs
[note-tracker]: crates/rust-client/src/note/note_update_tracker.rs
[transaction-record]: crates/rust-client/src/transaction/record.rs
[rpc-transactions]: crates/rust-client/src/rpc/domain/transaction.rs
[output-record]: crates/rust-client/src/store/note_record/output_note_record/mod.rs
[sqlite-sync]: crates/sqlite-store/src/sync.rs
