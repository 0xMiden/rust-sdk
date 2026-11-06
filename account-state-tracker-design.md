# AccountStateTracker: account transitions and incomplete local state

Status: the first account-state scope is implemented locally and ready for review. The PR boundaries
below separate that implementation from the transaction-history follow-up. Sections about the
broader session registry and durable history describe later work.

## Decision

Use AccountStateTracker to reconcile the state of each tracked account across local execution,
chain observations, imports, recovery, and rollback.

Keep AccountUpdates as a separate finalized write batch. AccountStateTracker is the mutable working
state for one operation. Consuming it produces AccountUpdates, which becomes the account part of
StateSyncUpdate. The store receives the updates and returns the changes that actually committed.

The tracker must distinguish three facts:

1. The account commitment confirmed by the chain.
2. The account data available to this client and the commitment to which that data belongs.
3. The optimistic account state produced by pending local transactions.

The complete design also persists every discovered transaction, including transactions executed
elsewhere. It retains at least the full TransactionHeader and its inclusion context, together with
missing history ranges. Transaction persistence and header backfill belong to a follow-up PR.

AccountStateTracker names the reconciliation component. AccountUpdates names its output.
This follows the responsibility split between NoteUpdateTracker and NoteUpdates.

## Simple state-machine boundary

Keep the tracker small. It owns the current working state of each account and applies explicit
events to that state. The client supplies verified observations, recovered data, and resolved
transaction outcomes. The tracker does not fetch data or persist it.

    let mut tracker = AccountStateTracker::from_snapshot(snapshot, &account_ids)?;
    tracker.apply(account_id, event)?;
    let updates = tracker.into_updates(&transactions)?;
    store.apply_account_updates(updates, transactions).await?;

Here, transactions contains the resolved transaction outcomes and the captured pending-record
basis. Sync and transaction submission include the same account plans in their larger atomic write.

Each changed account has one output entry. It contains the expected basis, the resulting confirmed,
available, and optimistic positions, the resulting status, and ordered data effects. An effect can
apply a patch, replace data, or restore a specified historical target. An empty effect list keeps
the existing data. A revision is an expected basis. Persistence advances it when the state changes.

A local batch can apply several patches to one account. Keep those data writes in order and emit
one final state for that account. Do not make the store derive intermediate decisions from the
transaction records.

The tracker chooses the complete result. For example, a new private confirmation can retain body A,
advance confirmation to C, and keep execution locked. A rollback specifies the restored header and
positions. The store does not infer either decision from transaction records or stored headers.

The store checks the expected revision, checkpoint, pending-record basis, proofs, and content roots.
It then performs the supplied data effects and writes the supplied positions and status in one
transaction. It can reject an invalid plan. It cannot replace that plan with a different transition.

Include the historical headers and rollback metadata required for a decision in the coherent
snapshot. Keep SQL history restoration as a data operation against the selected target. Keep
transaction commit and discard decisions in TransactionUpdateTracker. Local execution and import
must produce the same account plan before persistence.

## Scope and terminology

This proposal covers account registration, deployment, local execution, external execution,
transaction reconciliation, state recovery, locking, import, history, and rollback.

Native and Watched are existing client tracking modes:

| Mode | Local execution | Derived note subscriptions for account addresses |
| --- | --- | --- |
| Native | Enabled, subject to authentication and required state | Enabled |
| Watched | Disabled | Disabled |

Both modes follow public account changes. The mode is separate from public/private account
visibility. It does not grant protocol authority or prove that keys are available. A native account
can execute transactions through several clients.

This change preserves the existing restriction on switching an account's tracking mode. It does
not introduce an account permission system.

## PR boundaries

### First PR: account-state reconciliation

Include the tracker/update split and the separate account positions, together with the checks
needed to apply those positions safely.

| Area | First PR |
| --- | --- |
| Mutable AccountStateTracker and finalized AccountUpdates | Include |
| Durable confirmed, available, and optimistic account positions | Include |
| Block provenance for confirmed positions and recovery snapshots | Include |
| Public recovery and private locking/import behavior | Include |
| Continued confirmed-position updates while a private account is locked | Include |
| Atomic account revision, starting-basis, pending-dependency, and checkpoint checks | Include |
| Schema migration and account-state regression tests | Include |
| Existing transaction outcome integration | Retain; consume it to reconcile account state |
| Header-only transaction representation and external transaction persistence | Follow-up PR |
| Durable transaction-history gaps, coverage, and header backfill | Follow-up PR |
| Full transition journal and comprehensive history query APIs | Follow-up work |
| Wallet-wide revision framework and a shared session registry | Not prerequisites for this PR |

The required part of progress tracking in the first PR is the account's verified block context and
its durable availability. For example, the store must retain that the chain confirmed C while the
available private body is still A. This requirement does not need transaction-header persistence.

Read account positions, relevant pending records, and the checkpoint from a coherent snapshot.
Check the account revision and pending-record bases again inside the write transaction. Account
data, confirmation, lock, or availability changes must invalidate that basis even when the account
commitment does not change. Pending membership, status, and metadata used for reconciliation must
also be checked. Recheck the starting checkpoint in the same transaction.

These account-scoped checks are required for correctness. A new wallet-wide revision framework is
not required to implement them.

Use the existing note ownership pattern for the first PR. AccountStateTracker can own the working
account records and move changed records into AccountUpdates when consumed. A shared SyncRecords
registry can be added separately without changing the tracker/update contract.

The first PR does not require a new transaction record model, TransactionUpdates type, or
transaction query API. It uses the existing transaction tracker and its outcomes. Any changes at
that boundary must support account reconciliation rather than expand transaction-history scope.

The first PR is complete when the account positions survive restart, every account writer follows
the transition rules, rollback preserves valid state, and concurrent changes reject stale updates.
It does not claim complete transaction history.

### Follow-up PR: transaction persistence and history recovery

Add the header-backed transaction representation, insertion of externally observed transactions,
record enrichment, and transaction history queries. Add durable history gaps, range coverage,
and resumable header backfill with that work.

The first PR provides the account positions that this follow-up uses to identify missing history.
It must not mark unprocessed transaction ranges as complete or fabricate records to fill them.

The remaining sections describe the complete target design. Features assigned to the follow-up
are not requirements for merging the first PR.

## State model

Keep registration metadata separate from changing account state:

| Information | Meaning |
| --- | --- |
| Registration | Account ID, native/watched mode, addresses, and required creation metadata |
| Confirmed position | Chain commitment and the block at which it was verified |
| Available data | Header, code, storage, vault, and witnesses, each tied to an account commitment |
| Optimistic position | Valid pending transactions applied after an available baseline |
| History coverage | Processed ranges, requested backfill ranges, and unresolved gaps |
| Execution availability | Whether local execution has a valid basis and the required data |

A confirmed private commitment does not imply that its header or body is available. A stored body
does not imply that it is the current confirmed state.

For example:

    Confirmed commitment: C, verified at block H
    Available body:       A
    Pending work:         none
    Execution:            locked until private data for C is imported

Record C durably. Keep A as available historical data. Do not label A as the current chain state.

Partial data must identify the commitment it belongs to. Map entries and witnesses for A cannot
be used as entries or witnesses for C without validation. Local execution can use partial data
when the transaction has all required authenticated inputs. It does not require a complete body
for every account.

Keep deployment status, confirmation, data completeness, locking, and tracking mode separate.
The existing New, Tracked, and Locked labels can remain API projections of this information.

## Ownership and responsibilities

Use the single-owner record model described in [state-sync-design.md](./state-sync-design.md).
Full account data and transaction records live in one session registry. Trackers hold identifiers,
indexes, and transition decisions.

This registry describes the broader session design. In the first PR, the account tracker can own
its working records and consume them into AccountUpdates, as specified in the PR boundaries.

This proposal refines that draft in two places. Confirmed private state can consist of a commitment
without its header or body. A local discard can also be superseded by later inclusion evidence.
The related sync proposal must use these same rules.

| Component | Responsibility |
| --- | --- |
| RPC validation | Validate identities, response ranges, chain context, and available evidence |
| AccountStateTracker | Reconcile confirmed, available, and optimistic account positions |
| TransactionUpdateTracker | Own transaction commit/discard decisions and their reasons |
| Note reconciliation | Resolve note availability, consumption, and pending note dependencies |
| AccountUpdates | Carry finalized account changes, payloads, and expected bases to the store |
| Store | Check the plan's basis and apply all related changes atomically |

Use a fixed reconciliation sequence. AccountStateTracker identifies account conflicts.
TransactionUpdateTracker resolves transaction outcomes and dependencies. AccountStateTracker then
finalizes account data changes and recovery requirements. Do not give two trackers independent
authority to discard the same transaction.

All account mutation paths must use the same transition validation rules. Import and recovery must
not bypass reconciliation through a direct full-state replacement.

## Tracker and updates split

Use three distinct stages:

    AccountStateTracker
        -- reconcile and consume --> AccountUpdates
        -- apply atomically -------> committed result

| Property | AccountStateTracker | AccountUpdates |
| --- | --- | --- |
| Lifetime | One preparation and reconciliation operation | From finalization to store application |
| Contents | Starting bases, account IDs, indexes, decisions, and outstanding recovery work | Changed account records, write payloads, history entries, and preconditions |
| Receives new information | Yes | No |
| Contains unchanged tracked accounts | Yes, through references to the session records | No |
| Contains unresolved required recovery | Yes, during preparation | No |
| Runs reconciliation rules | Yes | No |
| Is passed to the store | No | Yes, within the operation's atomic update |

AccountUpdates is a proposed write batch. It is not a report of committed changes. The store still
checks its basis, validates the persisted result, and can reject the whole operation.

### Working state and ownership

AccountStateTracker loads the starting account positions and expected bases. It observes validated
transaction IDs, identifies competing branches and gaps, accepts recovered account data, and
records which accounts need writes. It coordinates with transaction and note reconciliation before
finalization.

With the session registry in this proposal, full account and transaction records stay in SyncRecords
during reconciliation. The tracker holds IDs and small decision metadata. At finalization, changed
records and payloads move from the registry into AccountUpdates. Unchanged records are not emitted.

This is the same lifecycle split as the note types. The current NoteUpdateTracker owns its working
note records and consumes them through into_note_updates. The registry is an ownership choice for
the proposed session. Neither arrangement requires two copies of a full record.

### Final write batch

The following types show the proposed boundary. The exact field types remain implementation details.
The first PR needs the account positions, data operations, witness changes, and expected bases.
Durable journal and transaction-history coverage fields belong to the later work.

    struct AccountStateTracker {
        accounts: BTreeMap<AccountId, TrackedAccountState>,
    }

    struct AccountUpdates {
        accounts: Vec<AccountStateUpdate>,
    }

    struct AccountStateUpdate {
        account_id: AccountId,
        expected: AccountStateBasis,
        state: PersistedAccountState,
        data: AccountDataUpdate,
        witness: AccountWitnessUpdate,
        history: Vec<AccountTransition>,
    }

Each AccountStateUpdate contains:

- The expected absence or starting account positions.
- The resulting confirmed, available, and optimistic positions, availability, and history coverage.
- The required data operation: preserve data, replace with validated full data, apply a patch,
  restore historical data, or attach authenticated partial data at a named commitment.
- Any witness change, tied to its account state and block context.
- Durable account transition entries and their transaction references.

Represent preserve, replace, and remove operations explicitly. An absent optional payload must not
mean both "leave unchanged" and "delete".

Emit at most one finalized entry per account. That entry includes all required account effects and
retains the intermediate history decisions. For example, a branch replacement must retain its
rollback and recovery decisions even though it has one final account position.

Fold repeated observations while the tracker is mutable. Finalized account batches must not use
unconditional last-write-wins merging. Two batches for the same account can have different bases
or incompatible branches.

A commitment-only private update is a valid entry:

    confirmed:   advance from B to C
    available:   retain body A
    execution:   remain locked
    data write:  preserve existing body
    history:     record the new observation

This update is not empty merely because it contains no replacement Account.

### Transaction headers have one owner

Full transaction headers and optional execution data belong in TransactionUpdates. AccountUpdates
contains the resulting account positions and account events, with references to transaction IDs.

This transaction payload is part of the follow-up PR. The first account PR continues to use the
existing transaction update representation.

The combined StateSyncUpdate owns AccountUpdates, TransactionUpdates, and NoteUpdates. All three
are applied in one store transaction. An external transaction that preserves account state can
still require a transaction write even when it needs no account data write.

Transaction outcomes identify invalidated transaction IDs. AccountStateTracker translates them
into explicit account rollback effects in AccountUpdates. The store must not independently infer
additional account rollback from TransactionUpdates.

Do not put a second copy of each transaction header in AccountUpdates. The transaction batch owns
the header that the store persists. Account history queries join account events to transaction
records through their IDs.

### Finalization and application

Use a consuming conversion like into_note_updates:

    let account_updates = accounts.into_updates(&mut records)?;
    let transaction_updates = transactions.into_updates(&mut records)?;
    let note_updates = notes.into_updates(&mut records)?;

    let update = StateSyncUpdate::new(
        origin,
        account_updates,
        transaction_updates,
        note_updates,
        chain_updates,
    );

    let report = store.apply_state_sync(update).await?;

The example describes the proposed session API. Complete all reconciliation before moving records
out of the registry. Validate cross-domain references before the final update reaches the store.

For the first PR, the account conversion can be into_updates(self), with the tracker moving its own
changed records into the payload. The registry-based conversions and a new TransactionUpdates type
are not dependencies of that split.

Finalization rejects unresolved required recovery, inconsistent bases, and incompatible decisions.
An explicit locked private position and a permitted historical backfill gap are resolved decisions.
They can be persisted without a full account body.

Complete public recovery before finalization when its need is known. If store validation discovers
that a patch cannot reproduce its target, roll back the operation. Prepare a new tracker, add the
verified recovery data, and produce a new AccountUpdates. Keep a finalized batch immutable.

Apply the same account payload and validation rules within local transaction and import operations.
Those operations must retain their own atomic relationship with transaction and note writes.

Place AccountUpdates and its data types in the shared store-facing crate. Keep AccountStateTracker
with client reconciliation. The store must not depend on the tracker, RPC calls, or session lookup
indexes. Keep result types separate from both the tracker and the proposed writes.

## Durable transaction records

This section belongs to the transaction follow-up PR.

Every transaction discovered for a tracked account becomes a durable record. Missing local
execution data is a supported record state.

TransactionUpdates carries the record to the store. The record retains:

- The full TransactionHeader, including its account ID, commitments, and note commitments.
- The transaction ID, checked against the header.
- Inclusion block number and block hash when available.
- Observation source and verification evidence.
- Local execution or submission data when available.
- References to any additional transaction data stored locally.

Separate data availability from transaction status. A transaction can be committed while only its
header is available. A local transaction can have execution data while it is still pending.

Do not fill unknown submission times, execution blocks, expiration heights, or scripts with defaults.
The current TransactionRecord representation assumes local execution details. It must support
header-only observations, or use normalized optional execution data behind one transaction query
interface.

Required merge rules:

1. Identify records by transaction ID within the store's network.
2. Insert a new observed header without requiring a local transaction record.
3. Enrich an existing record when more data arrives.
4. Preserve full local data when a header-only observation arrives.
5. Reject conflicting headers or incompatible inclusion contexts for the same transaction.
6. Make repeated observations idempotent.

Use the evidence the RPC actually supplies. Receiving a header from an RPC endpoint does not
create an independent inclusion proof. Retain its provenance and do not report a stronger
verification level than the client established.

Query transaction history by account, block range, and transaction ID. Return local and external
transactions together, with their data availability and provenance.

## Transition rules

| Event | Validation | Result |
| --- | --- | --- |
| Register a new account | Validate its identity and required creation seed | Preserve a new baseline; confirmed deployment is absent |
| Register an existing account | Establish an authenticated baseline at the wallet checkpoint | Activate that baseline and record earlier history coverage |
| Execute locally | Require native mode, an unlocked valid basis, required inputs, and a valid executed transaction | Add pending work and advance only the optimistic position |
| Include a local transaction | Match the exact transaction ID and validate the chain context | Mark it committed and preserve any valid pending suffix |
| Observe an external transaction | Validate its header and available chain evidence | Persist the record and reconcile its account transition |
| Confirm a known ancestor | Establish that pending work still starts from the confirmed state | Advance confirmation without removing valid optimistic work |
| Observe a competing branch | Establish that a pending transition can no longer execute from its initial state | Discard incompatible work and resolve its dependencies |
| Observe unknown public state | Authenticate the target account commitment | Recover matching data before installing a replacement |
| Observe unknown private state | Authenticate the target commitment | Record that commitment, preserve available data, and lock execution |
| Observe a later private state while locked | Authenticate the new observation | Advance the confirmed position and history without reporting a new lock |
| Recover public state | Validate the recovered contents against the target header | Make the matching data available and reconcile remaining pending work |
| Import private recovery data | Match the authenticated target commitment and validate account identity and creation data | Replace the available body and unlock when execution has a valid basis |
| Discard pending work | Identify its invalid account and note effects | Remove those effects while preserving confirmed state |
| Roll back a pending deployment | Restore the saved new baseline | Preserve registration and the original creation seed |
| Repeat an applied operation | Match its identity and expected outcome | Return an idempotent result without duplicate history |

A nonce is one validation input. It does not establish block provenance or prove that two states
belong to the same branch.

A verified replacement can have a lower nonce than an abandoned optimistic tip. Reconcile and
remove the invalid pending branch before validating the replacement against the confirmed baseline.
Do not implement recovery as a global rule that the stored nonce can never decrease.

## Transaction reconciliation

Represent transaction relationships through their commitments:

    A -- transaction X --> B -- transaction Y --> C

Process all observed inclusions before expiry, timeout, nullifier, or supersession decisions.
Confirmed inclusion takes precedence over a previous local discard.

Required cases:

- Included A to A does not invalidate pending A to B.
- Discarding A to A does not make state A invalid.
- Pending A to A can become invalid after the chain advances from A to B without including it.
- An external A to C transition invalidates an incompatible pending A to B branch.
- Different transaction IDs remain different transactions even when both produce state B.
- If the chain confirms B, preserve B when discarding a different local transaction that also
  produced B.
- Preserve pending work from B only when its account basis and note dependencies remain valid.
- A child that requires an output note available only from a losing transaction must also be
  reconciled. Matching account commitments alone do not establish that note's availability.
- Late inclusion updates a previously discarded transaction to committed. Recover account data if
  its optimistic body was already removed. Do not automatically restore its discarded descendants.

Order state-changing transactions through their initial and final commitments. Preserve every
record when ordering them. Reject forks, cycles, duplicate inclusion across blocks, and gaps in
responses that claim complete coverage.

State-preserving transactions belong at their account state on that chain. Their relative order
may be unknown. A stable transaction-ID display order must not be reported as execution order.

## Missing history and recovery

The motivating case is:

    Available locally: A
    Missed elsewhere:  A --> B
    Newly observed:    B --> C

Persist the header for B to C. Record the missing connection from A to B. Fetch missing transaction
headers over the corresponding block range and store them even if their full data is unavailable.
Do not apply the B to C patch directly to A.

For a public account:

1. Pin recovery to an authenticated target block.
2. Fetch a matching account snapshot, or obtain enough updates to reconstruct the target.
3. For oversized accounts, use the available history endpoints to reconstruct storage and vault.
4. Validate code, storage, map, and vault commitments against the authenticated target.
5. Install the replacement with the transaction reconciliation in one store transaction.

Recovering the current body does not fill missing transaction history. Keep header backfill and
body recovery as separate requirements. Record any remaining history gaps explicitly.

For a private account, persist the observed commitment and transaction headers, preserve available
private data, and lock execution. Public commitments cannot reconstruct missing private state.
An authenticated import can supply the missing body.

A repeated observation while locked must still update the stored confirmed position. Locking must
not stop account history tracking.

## Snapshot provenance and coverage

Associate every confirmed baseline and recovered snapshot with its verified block reference.
The block where a commitment was verified is not necessarily the block where the state was created.
Block provenance and baseline establishment are part of the first PR. Durable history coverage and
header backfill belong to the transaction follow-up.

A snapshot verified at block 200 cannot become an active confirmed baseline at checkpoint 100
without evidence at the earlier checkpoint. A higher nonce does not provide that evidence.

An existing account added at checkpoint H must establish a baseline at H before incremental sync
can rely on it. This must work when the wallet is already at the chain tip.

Track live history coverage and historical backfill separately:

- Live coverage records the account ranges required by normal wallet sync.
- Backfill records earlier ranges requested for account history.
- Gaps retain their known block bounds and commitment endpoints.
- Recovery resumes from stored progress after restart, including when no new block arrives.
- The wallet checkpoint must not advance past required live work that was skipped.
- A custom partial sync must not advance the shared wallet checkpoint without coverage for the
  omitted required work.

For a newly added account, H can be the live baseline once it is verified. Queue earlier header
backfill from an earlier authenticated account anchor when one is known. Otherwise expose the
pre-baseline history as unknown and support an explicit request from genesis.

Retaining an older body is permitted when its commitment and availability are explicit. A locked
private account can advance confirmed position and live chain coverage while its body is unavailable.
For the first implementation, failure to validate a required public recovery aborts the sync
application. It must not install an unverified replacement.

## Atomic application and concurrency

Prepare the account context, pending transactions, and checkpoint from one coherent store snapshot.
Bind the plan to the originating store, network, checkpoint, expected account revisions, and
pending-record bases.

Account commitments are insufficient concurrency guards. A transaction can change pending records
and processing notes without changing the account commitment. An import can also change available
data while retaining that commitment.

Every relevant account mutation must change its revision. Validate pending records against the
same snapshot, including the fields used for reconciliation. The existing forest revision is a
tree mutation counter. It is not a revision for this complete account context.

The broader session design can generalize these checks to a monotonic store revision that covers
notes and other sync state. That wallet-wide mechanism is separate from the first PR.

Inside one write transaction:

1. Check the origin, revision, checkpoint, and entity-specific expected bases.
2. Persist new transaction headers and merge richer records.
3. Apply transaction outcomes from TransactionUpdates and rollback effects from AccountUpdates.
4. Apply account data, confirmed positions, availability, and witnesses.
5. Apply the related note changes and durable transition records.
6. Update history coverage, backfill progress, and the wallet checkpoint.
7. Commit and return the result derived from committed changes.

A stale basis rejects the plan. Rebuild it from the current store snapshot.

Validate restored contents against the restored header before rollback commits. Reimporting the
same body must preserve the history needed to undo pending work. Account state rollback must not
delete account registration.

Update the in-memory MMR after commit. Report a post-commit maintenance failure separately from a
rejected sync transaction. A lost response must not hide committed progress or cause duplicate
history when the caller retries.

## Persistent history and result API

The first PR returns account transition outcomes and persists current account positions.
The durable journal and history query APIs below are later work.

Persist compact account transition records containing:

- Account ID and transaction ID when the change has one.
- Previous and resulting confirmed, available, and optimistic positions.
- Transition kind and reason.
- Relevant block context and evidence provenance.
- The store revision or another durable event identity.

Registration, import, locking, recovery, and rollback are account events even when no transaction
header describes the local decision. Keep these events distinguishable from chain transactions.

Return committed decisions through the sync result. Also provide account history queries so an
application does not need to retain every SyncSummary to avoid losing the history.

Results must distinguish:

- Confirmed state advanced.
- Available account data changed.
- Pending work was committed, preserved, or discarded.
- Execution became available or unavailable.
- History remains incomplete and backfill is outstanding.

Keep headers and gaps needed to explain active account state and pending dependencies. Make history
pruning explicit. Full historical bodies can use a separate retention policy. Pruning must not
invalidate the confirmed baseline or remove information required for recovery.

## Persistence and migration

The schema needs durable storage for transaction headers, optional execution data, account state
positions, coverage and gaps, transition records, and the store revision.

Use one logical transaction record for local and external activity. Separate tables can normalize
optional payloads, but they must not create competing records for the same transaction ID.

Existing local records can retain their execution details. Recover missing legacy headers from
retained data or RPC responses where possible. Mark unresolved legacy data explicitly. Do not
fabricate unknown header fields or claim historical coverage that the old store did not retain.

Prepare a migration or an explicit database-version transition before changing the store contract.
Document query API and serialized result changes.

## Acceptance tests

| Scenario | Required result |
| --- | --- |
| Another client executes a transaction | Its full header is stored and appears in account history |
| An external transaction has no local execution data | Store a committed header-only record without invented fields |
| A header-only record later gains execution data | Enrich one record and preserve its inclusion context |
| A full local record receives a header-only observation | Preserve the full local data |
| The same observation is received twice | No duplicate transaction or account event |
| The same transaction ID has conflicting data | Reject the inconsistent update |
| The client has A and observes B to C | Store the observation, backfill the gap, and recover the correct body |
| Public body recovery succeeds before header backfill | Report current body availability and incomplete history separately |
| Private data for the new commitment is unavailable | Persist the new position and headers; preserve old data and lock execution |
| A locked private account advances again | Persist the later commitment without reporting another lock |
| A new account is added while the wallet is at the tip | Establish its baseline without waiting for a new block |
| An imported snapshot is verified only after the checkpoint | Keep it as a candidate until the required block context is established |
| Local and external branches compete | Remove incompatible pending effects and install the verified branch atomically |
| Different transactions produce the same state | Preserve transaction identity and confirmed state |
| A surviving account branch depends on an invalid output note | Reconcile the note dependency before preserving the child transaction |
| An included or discarded transaction preserves account state | Do not invalidate valid work solely through that unchanged state |
| A previously discarded transaction is included | Persist commitment and recover or mark unavailable data correctly |
| A pending deployment is undone | Restore the original new baseline, registration, and seed |
| Rollback history cannot reproduce its header | Reject the rollback without partial writes |
| A concurrent transaction preserves the account commitment | Detect the changed account revision or pending-record basis and rebuild the plan |
| Sync is retried after a lost post-commit response | Return consistent progress without duplicate records |
| The process restarts during backfill | Resume from durable gaps and coverage |
| A required header range is unavailable or pruned | Report the unresolved range and preserve its recovery requirement |
| A custom run omits required account work | Do not advance the shared checkpoint past that work |
| An unchanged account is loaded and no new information arrives | Finalization emits no account entry |
| A locked account receives a new commitment without a body | Emit and persist a commitment-only AccountUpdates entry |
| An external transaction preserves account state | Emit its transaction header even when no account data write is needed |
| Finalization still needs required public recovery | Reject finalization until the recovery decision is resolved |
| A finalized patch fails store validation | Roll back, rebuild the tracker, and produce a new immutable batch |
| Several decisions affect one account | Emit one final entry and preserve its required effects and history |
| Finalization drains session records | Move changed payloads once without emitting unchanged records |

## Local implementation status

The implementation is on the uncommitted branch `codex/complete-account-update-tracker` in
`/private/tmp/miden-account-tracker-complete`. The tracker consumes a coherent snapshot, receives
account events, and emits one final plan per changed account. SQLite checks the bases and evidence,
applies the selected data effects, and writes the supplied state. Registration, imports, sync, and
local transaction batches use that same boundary. No pull request has been opened for this work.

| Area | Status |
| --- | --- |
| Separate AccountStateTracker and finalized AccountUpdates | Implemented as an event-driven state machine and immutable plans |
| Lifecycle events and typed validation | Implemented |
| Separate durable confirmed, available, and optimistic positions | Implemented with a new SQLite migration |
| Confirmed block context and account-witness validation | Implemented for sync and verified imports |
| Coherent account, pending-record, and checkpoint snapshot | Implemented |
| Atomic account revision, starting-state, pending-record, and checkpoint checks | Implemented |
| Public recovery at an existing checkpoint | Implemented for full and oversized accounts |
| Continued private confirmations while locked | Implemented; available data remains separate |
| Verified imports and execution-basis checks | Implemented |
| Pending branch reconciliation and state-preserving transactions | Implemented; the account tracker consumes resolved transaction outcomes |
| Rollback boundaries and registration preservation | Implemented with explicit restore targets and ordered data effects |
| Confirmed-position changes in SyncSummary | Implemented, including changes without a body update |
| External transaction observations in SyncSummary | Reduced summaries for the current call |
| Durable full transaction headers for external activity | Follow-up PR |
| Durable gaps, coverage, and header backfill | Follow-up PR |
| Wallet-wide revision and shared session registry | Follow-up work |
| Durable account transition history and history queries | Follow-up work |

An existing account added from local data starts without a verified execution basis. Sync must
authenticate that baseline before local execution. A network import retains its proof at the
current wallet checkpoint and can establish the execution basis immediately. A new account can
execute its deployment from its saved creation baseline.

The migration does not treat legacy account bodies as confirmed. It preserves an existing pending
tip when the retained pending record matches the available body. Sync establishes the missing
confirmed position from a verified account witness.

Observed transaction summaries do not establish complete transaction history. The account witness
authenticates the account commitment at its block. Persisting external transaction headers and
recovering missing history remain separate work.

### Validation

- Core, client, and SQLite library tests: 352 passed and one ignored.
- Mocked client tests: 247 passed in the full optimized run. The remaining foreign-account test
  passed after its RPC assertion stopped retaining a failure for a later sync. All 248 test cases
  are covered by those runs.
- `make format` and `make lint`: passed, including all workspace targets.
- Client build for `wasm32-unknown-unknown` without default features: passed.
- Core and client documentation with warnings denied and the `testing,tonic` features: passed.
- Migration append-only guard and `git diff --check`: passed.

The Store API now accepts finalized account plans. The bare `insert_account` and `update_account`
methods are replaced by `apply_account_updates`. Transaction and batch writes receive the same
plans with their captured pending-record basis. These are breaking interface changes.
