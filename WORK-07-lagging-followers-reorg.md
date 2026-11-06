# Handle lagging follower nodes on sync/reconnect; groundwork for reorg safety

With multiple follower nodes behind the RPC endpoint, a client can reconnect — or be load-balanced mid-sync — to a node whose chain tip is *behind* the client's own sync height. Today this case has no explicit handling: `sync_state` discovers the tip via `sync_chain_mmr` and early-returns only on `chain_tip == current_block_num`; a tip *below* the client's height falls through into request paths built for forward ranges, surfacing as confusing downstream errors (missing headers, failed range validation) rather than a diagnosis. The decomposed sync protocol aggravates this: `sync_chain_mmr`, `sync_notes`, and `sync_transactions` are separate requests that may each hit a different follower, so a sync cycle can discover a fresh tip on one node and then request that range from a node that doesn't have it.

This also aggravates [rust-sdk#2105](https://github.com/0xMiden/rust-sdk/issues/2105): from the client's perspective, bouncing between followers at different heights is observationally similar to a shallow reorg, and we currently have neither detection nor a typed vocabulary for it.

What already defends us (verified): the MMR/header/inclusion-proof spine is cryptographically verified against local peaks, `validate_note_blocks_range` pins sub-requests to `[current, chain_tip]`, and the RPC layer pins a genesis commitment — so a *divergent* follower mostly can't corrupt state. The unhandled case is the *lagging* follower.

## Motivation

- Multiple followers behind a load balancer make reconnection to a node that's behind you an *expected operational event*, not a corner case — and the decomposed sync protocol means it can happen mid-cycle even without a reconnect.
- Today the failure surfaces as arbitrary downstream errors (missing headers, range-validation failures); an operator cannot distinguish "the node is behind, retry in a minute" from "my store is corrupt" — the difference between a shrug and an incident.
- The typed vocabulary this work introduces (`NodeBehind` vs. divergent-chain) is also the detection layer that any future reorg handling (rust-sdk#2105) has to build on; skipping it now means building reorg recovery on undiagnosable errors later.

## Plan

- **Behind-tip detection:** when the discovered `chain_tip` is below the local sync height, return a typed `NodeBehind { local_height, node_tip }` error (transient by nature — callers can retry/backoff) and touch nothing. Never issue sync sub-requests for inverted ranges.
- **Monotonicity audit:** verify no sync-application path can persist data anchored below the current sync height. The blockchain checkpoint already guards with `WHERE block_num < ?`; extend the audit to note-state, transaction-commit, and account-patch writes.
- **Mid-cycle follower drift:** define behavior when a pinned-range sub-request fails because its follower lacks the range: the sync cycle must abort atomically (no partial application) and surface the same typed error; a retry re-discovers the tip from scratch.
- **Consistency spot-check on reconnect:** fetch the node's header at the client's current sync height and compare against the stored header. Match + lower tip → lagging follower (retryable). Mismatch → divergent chain (wrong network, or an actual reorg) → distinct typed error, no state mutation. This check is also the reorg *detector* that #2105 needs.
- **Scope boundary with #2105:** this delivers detection and refusal-to-corrupt with a typed error surface; rollback/recovery from a genuine reorg is the follow-up, building on the historical state the store already keeps (account history tables, note state machine).
- **Optional (from #2105's original ask):** expose `SyncTarget` on `Client::sync_state` — clients targeting `ProvenChainTip` or a fixed `BlockNumber` are inherently more tolerant of follower lag.
- **Node-side note (separate conversation):** followers exposing their tip in `Status`, and/or LB affinity for sync sessions, would reduce how often clients hit this — but the client must be defensive regardless.

Cross-links: WORK-04 (companion: WORK-04 guarantees what we store converges to the chain; this guarantees we only talk to a chain that's ours and ahead of us), WORK-09 (shared discipline), rust-sdk#2105.
