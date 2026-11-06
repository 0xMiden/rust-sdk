# Validate account code completeness on add/import (client mirror of node#2295)

[node#2295](https://github.com/0xMiden/node/issues/2295) will reject new account submissions whose code doesn't contain full executable MAST (roots-only code breaks the planned protocol code registry, [protocol#2982](https://github.com/0xMiden/protocol/discussions/2982)). The client has the same exposure, arguably worse: incomplete code doesn't just break a future registry migration — it breaks local execution.

## Motivation

- Diagnostic distance: a roots-only account imports without complaint today and fails only deep inside the VM at transaction time — the worst possible place for the user to learn their account file was malformed.
- Timing: once node#2295 lands, deployments of such accounts get rejected at submission with a node error the client can't explain. A mirrored client-side check must exist *before* that, and drift between the two definitions of "complete" would be worse than no check — which is why the validator must live in `miden-protocol`, shared by both.

## Current state

- `AccountCode::from_parts(mast, procedures)` is public and builds an `AccountCode` from a MAST forest + procedure roots with no check that the forest contains executable bodies for those roots.
- `add_account`/`add_account_inner` checks seed presence, nonce ordering, and watched-mismatch — nothing about code. Account files imported from other parties go straight into the `account_code` table.

## Failure modes

1. User imports/constructs an account with roots-only code; the first transaction against it dies inside the VM with a cryptic unresolved-MAST error, far from the actual mistake.
2. Once node#2295 lands, deploying such an account gets rejected at submission time with a node error the client can't explain.
3. Our `account_code` table accumulates blobs that couldn't serve the registry reconstruction the node cares about.

## Plan

- **Push the check down, not sideways:** the definition of "complete code" must be identical in the node's rejection and the client's validation, or we get client-says-valid/node-rejects drift. Propose `AccountCode::validate_completeness()` in `miden-protocol` (every declared procedure root resolves to executable MAST in the forest, no dangling/external-only roots); node#2295's implementation and the client both call it.
- Client calls the check in `add_account_inner` (covers `add_account`, file import, and watch-import in one place) and before submitting account-creating transactions.
- Reject with a typed `ClientError` + `ErrorHint` ("account code is incomplete — the account file may have been generated with roots-only code") instead of letting it surface as a VM or node error.
- Optionally: a one-time check when reading code out of the store, to flag pre-existing bad imports after upgrade.

## Note scripts (sibling concern, different completeness definition)

The same fail-late class applies to imported notes: a `NoteScript` whose MAST forest lacks the body of a reachable procedure imports fine (note files, NTL) and fails only when the consuming transaction executes. Two differences from account code: external MAST references are *legitimate* in note scripts (stdlib/miden-lib calls resolve at execution), so completeness means "entrypoint present and every reachable non-external node in the forest" — not "no external refs"; and there's no registry/node driver, so this is purely a client-side import guard. Propose the protocol validator with a parameterized notion covering both (`AccountCode` strict, `NoteScript` externals-allowed), and hook the note check into the import boundaries (`import_notes`, NTL fetch — where WORK-08's screening already executes scripts and would surface deep breakage for transport notes).

Small line of work — one PR once the protocol helper exists. Cross-link node#2295 and protocol#2982 when filing.
