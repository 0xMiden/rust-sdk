# Keystore at-rest encryption — invariants for the persistence layer

Status: `keystore/vault.rs` (envelope primitives) landed and green. This document
is the contract the *next* layer — `fs_keystore.rs` and the CLI — must satisfy.

## Threat statement (goes in the PR description verbatim)

> This design protects secret key material in an offline copy of the keystore.
> It does not protect against an attacker who can execute code as the user or
> inspect the client process. The primary safety requirement is recoverability:
> encryption must never make a previously usable key unrecoverable. Therefore,
> migrations are explicit, atomic, restartable, and verified before plaintext is
> removed.

Two things this rules out saying:

- "The OS keyring protects your keys." A silently-unlocking keyring is silently
  available to any process running as that user, which can call the same API.
  The defensible claim is only that an offline copy without keyring state is
  useless. Anything stronger is platform-specific (macOS binds ACLs to a signed
  binary; libsecret generally does not) and must be stated per-platform or not
  at all.
- "Encrypted therefore safer." Not until every invariant below holds. Until
  then, encryption is a net *regression* against the plaintext status quo,
  because plaintext keys cannot be lost to a botched unlock.

## Recoverability: the tension this design exists to resolve

"The keyring holds the DEK" and "the directory is a self-contained backup unit"
cannot both be true. They were both asserted in an earlier draft; that was a
defect, not a trade-off.

- If a copied directory plus a passphrase recovers the DEK, it is portable.
- If recovery needs the original keyring, restoring elsewhere **permanently
  loses every account**.

Resolved with LUKS-style key slots: one DEK, several `KeySlot`s, each wrapping it
under a different protector. A machine-bound keyring slot and a portable recovery
passphrase coexist. `KeyEnvelope::is_portable()` exists so callers can enforce
that at least one portable slot is present.

**The persistence layer must refuse to create or leave an envelope with no
portable slot**, unless the user passes an explicit override. `is_portable()`
reports; it does not enforce.

## Safety invariants

| # | Invariant | Enforced where |
|---|---|---|
| 1 | Migration never destroys the last verified decryptable copy | **TODO** — `fs_keystore` |
| 2 | Writes: temp file → fsync → atomic rename → **fsync the directory** | **TODO** — see gap G3 |
| 3 | Migration verifies every newly sealed key before removing plaintext | **TODO** — `fs_keystore` |
| 4 | Interrupted migration is restartable and leaves one unambiguous state | **TODO** — per-file magic prefix makes each file's state self-describing |
| 5 | Unknown envelope versions fail closed and trigger no rewrites | **DONE** — `ensure_supported`, called by `unlock`/`add_slot`/`remove_slot`/`replace_slot`; test `a_newer_envelope_version_fails_closed` asserts all four |
| 6 | Concurrent writers cannot rotate or overwrite the DEK independently | **TODO** — needs a lockfile; `dek_fingerprint` detects the damage but does not prevent it |
| 7 | Backup/restore tested across machines, keyring loss, partial writes, upgrades | **TODO** — test plan below |

Invariant 5 is worth spelling out: an older binary must never "helpfully" rewrite
an envelope it cannot fully parse, because that silently drops slots it did not
understand — destroying recovery paths the user believes they still have.

## Lifecycle operations

Specified now because they determine the format.

| Operation | API | Status |
|---|---|---|
| Add a recovery wrapper | `add_slot(&vault, protector, id)` | done — requires an unlocked vault, so only someone who can already open the store can add a path |
| Remove a wrapper | `remove_slot(id)` | done — refuses the last slot; **caller must also refuse the last portable one** |
| Change a passphrase | `replace_slot(&vault, protector, id)` | done — O(1), reseals no secret |
| Export / import for recovery | copy the directory; open via a portable slot | relies on invariant: at least one portable slot |
| Downgrade an old binary onto a new store | `ensure_supported` | done — fails closed |
| Keyring deleted / app uninstalled | keyring slot dies; portable slot is the survivor | relies on the same invariant |
| Corruption detection | per-slot AAD binds version ‖ fingerprint ‖ id ‖ kind | partial — see gap G2 |
| **DEK rotation after suspected compromise** | — | **gap G1** |

## What stays plaintext, deliberately

The envelope is stored in the clear and reveals: format version, DEK fingerprint,
and per slot its id, kind, and KDF cost parameters. `key_index.json` additionally
reveals account ids, public key commitments, and key counts.

Sealing the index was considered and rejected: it would force an unlock just to
*list* accounts, and the index is where the envelope wants to live so that the
directory is one atomic backup unit.

Not revealed: the DEK, any secret key, any passphrase.

Slot ids are visible to anyone holding a copy — do not put anything sensitive in
them.

## The test backend must be conspicuous

`SlotKind::Plaintext` is deliberately not named `none`. It uses an all-zero
wrapping key so that the envelope shape is identical to a real one — which makes
upgrading a throwaway keystore to a real protector an O(1) slot swap rather than
a re-encryption (test: `upgrading_a_test_keystore_to_a_passphrase_reseals_nothing`).

The CLI must:

- persist the backend selection rather than inferring it per-invocation;
- label it in `info` / diagnostics output, not only at creation time;
- refuse it on non-devnet networks without an explicit override flag.

Otherwise the easiest backend becomes the permanent backend.

## Open gaps

**G1 — DEK rotation is not implemented.** After a suspected compromise, replacing
a slot is not enough: the DEK itself must change, which means re-sealing every
secret. That is inherently O(n) and belongs at the persistence layer, where it
must be atomic and restartable like a migration. Until it exists, the honest
answer to "my passphrase leaked" is `replace_slot`; the honest answer to "my DEK
leaked" is "create a new keystore and move the accounts".

**G2 — the envelope as a whole is not authenticated.** Each slot is individually
bound by AAD, so a slot cannot be forged, relabelled, or lifted from another
envelope (tests cover all three). But nothing stops an attacker with write access
from *deleting* a slot — silently removing a recovery path the user still
believes they have. Fix: MAC the slot set under a key derived from the DEK and
verify on unlock. Detects, does not prevent; cheap, and worth doing before the
format ships.

**G3 — `KeyIndex::write_to_file` does not fsync the directory.** It does
`NamedTempFile` → `sync_all()` on the file → `persist()`, which is the right
shape, but without a directory fsync the rename itself can be lost on power
failure. Pre-existing, but invariant 2 makes it load-bearing now.

## Test plan for invariant 7

- restore a keystore on a machine with no keyring entry → opens via the portable slot
- delete the keyring entry, then unlock → falls back with a clear message
- kill the process mid-migration at each step → restartable, no key lost
- open a v2 envelope with a v1 binary → fails closed, file unmodified after
- create keyring-only, attempt to remove the last portable slot → refused
- copy only one key file (without the envelope) → fails with a message that says
  the whole directory is required, not a decode error
