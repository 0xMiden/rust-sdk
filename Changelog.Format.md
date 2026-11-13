# Changelog Entry Format

Each entry is one line of a file in `.changelog/unreleased/`, without a bullet and without a PR link. The release
adds the PR link. See [`.changelog/README.md`](.changelog/README.md).

### Breaking Changes

```markdown
[BREAKING][category][scope] Description. Context if non-obvious.
```

**Categories:** `rename` | `removal` | `param` | `type` | `behavior` | `arch`

**Scopes:** `lib` | `cli` | `store`

- `lib`: the public APIs of the client library crates.
- `cli`: the `miden-client` command-line interface.
- `store`: the `Store` trait and the SQLite store. Use it for changes that a `Store` implementation must follow.

An entry can have more than one scope, e.g. `[lib,store]`.

**Examples:**
```markdown
[BREAKING][rename][lib] `TonicRpcClient` → `GrpcClient`.
[BREAKING][param][lib] `sync_state()` now requires `AccountStateAt` parameter. Use `Synced` for previous default behavior.
[BREAKING][removal][lib] Removed `compile_note_script()`. Use `ScriptBuilder` instead.
[BREAKING][type][lib] `BlockNumber` unified to `u32` across all APIs.
[BREAKING][arch][lib,store] `SqliteStore` moved to separate `miden-client-sqlite-store` crate.
[BREAKING][behavior][lib] `get_account()` now returns `None` instead of error for unknown accounts.
```

### Features

```markdown
[FEATURE][scope] Description.
```

**Examples:**
```markdown
[FEATURE][lib] Added `TransactionSummary` API for previewing transactions before execution.
[FEATURE][lib,store] New `AccountFilter` for querying accounts by type.
```

### Fixes

```markdown
[FIX][scope] Description.
```

**Examples:**
```markdown
[FIX][lib] Fixed "Current block should be in the store" panic on concurrent syncs.
[FIX][lib] Corrected balance calculation for fungible assets with decimals.
```

---

## Category Reference

| Category | What It Means | LLM Migration Strategy |
|----------|---------------|------------------------|
| `rename` | Type/method/crate name changed | Find old name usage, show find-replace |
| `removal` | API/feature removed | Find replacement API, show alternative |
| `param` | Parameter added/removed/reordered | Show old vs new signature, explain new params |
| `type` | Type signature changed | Show type conversion, import changes |
| `behavior` | Same API, different behavior | Explain old vs new behavior, when to adapt |
| `arch` | Crate structure, imports, feature flags | Show import path changes, Cargo.toml updates |

---

## Example Entry File

A PR adds a file such as `.changelog/unreleased/grpc-client-rename.md`. Each line is one entry, without a bullet
and without a PR link:

```markdown
[BREAKING][rename][lib] `TonicRpcClient` → `GrpcClient`.
[FEATURE][cli] `--dry-run` flag for transaction commands.
[FIX][lib] `get_notes_by_filter` now correctly handles empty tag arrays.
```

The release moves the entries to `.changelog/<tag>/<PR>.md` and adds the PR links (see
[`.changelog/README.md`](.changelog/README.md)):

```markdown
- [BREAKING][rename][lib] `TonicRpcClient` → `GrpcClient` ([#1360](https://github.com/0xMiden/rust-sdk/pull/1360)).
- [FEATURE][cli] `--dry-run` flag for transaction commands ([#1360](https://github.com/0xMiden/rust-sdk/pull/1360)).
- [FIX][lib] `get_notes_by_filter` now correctly handles empty tag arrays ([#1360](https://github.com/0xMiden/rust-sdk/pull/1360)).
```
