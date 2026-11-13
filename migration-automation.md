# Changelog Format for LLM-Generated Migration Guides

This document describes the changelog entry format and LLM prompt templates for automatically generating migration guides for the Miden Client SDK.

## Design Philosophy

The LLM has access to:
- **PR diffs** (via GitHub API or PR number lookup)
- **Full codebase** (can grep, read files, trace types)
- **Git history** (can see what changed between versions)

Therefore, the changelog only needs to provide:
1. **What changed** (precise identifier - type/method/feature name)
2. **Scope** (which crates/languages are affected)
3. **Category** (what kind of change - guides migration strategy)
4. **Brief context** (why it changed, any non-obvious gotchas)

The LLM will then explore the codebase to generate before/after examples, migration steps, and troubleshooting guidance.

---

## Changelog Entry Format

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

---

## LLM Prompt Template

Use this prompt to generate migration guides. The LLM will automatically read the changelog entries.

### Full Migration Guide Prompt

```
Generate a migration guide for the Miden Client SDK.

## Instructions

1. Read the entries of version {VERSION} from `.changelog/v{VERSION}/*.md` in the repository root
   - A final release also includes the entries of its pre-releases (`.changelog/v{VERSION}-*/`)
   - If version is "latest", use the release directory with the highest version
   - Releases up to 0.18.0 are sections of CHANGELOG.md instead
   - Parse all `[BREAKING]` entries from that version
3. For each breaking change, generate a migration section
4. If a breaking change is missing category/scope tags (e.g. only `[BREAKING]`), infer the scope by
   searching the library, CLI and store code. When in doubt, treat scope as `lib`.

## Target Version
{VERSION}  (e.g., "0.13.0", "0.12.0", or "latest")

## Previous Version (for "before" examples)
{PREVIOUS_VERSION}  (e.g., "0.12.0", or "auto" to use the version before target)

## Migration Guide Structure

For each `[BREAKING]` entry, generate:

### [Breaking Change Title]

**PR:** #NUMBER

#### Summary
1-2 sentence explanation of what changed and why.

#### Affected Code

**Rust:**
```rust
// Before ({PREVIOUS_VERSION})
old_code_example();

// After ({VERSION})
new_code_example();
```

#### Migration Steps
1. Step-by-step instructions
2. ...

#### Common Errors
| Error Message | Cause | Solution |
|---------------|-------|----------|
| `error[E0...]` | ... | ... |

---

## Guidelines

1. **Read the changelog entries first** to get the list of breaking changes
2. **Explore the codebase** to find real usage patterns for before/after examples
3. **Use PR numbers** to fetch diffs via `gh pr view #NUMBER` for context
4. **Apply migration strategy based on category tag:**
   - `rename`: Show find-replace, update imports
   - `removal`: Show replacement API with equivalent functionality
   - `param`: Show old vs new function signatures, explain new parameters
   - `type`: Show type conversions and import changes
   - `behavior`: Explain the behavioral difference and when code needs adaptation
   - `arch`: Show Cargo.toml changes, new import paths
5. **Scope inference for untagged entries:** if `[scope]` is absent or ambiguous, search for the
   identifiers in `crates/rust-client`, `bin/miden-cli` and `crates/sqlite-store`.
6. **Cover every scope of an entry:** show the CLI change for `cli` and the `Store` implementation
   change for `store`.
7. **Group related changes** that affect the same workflow
8. **Identify compile errors** users will encounter and how to fix them

## Output

Generate a professionally formatted markdown document suitable for `Migrations.MD`.

### Content Requirements
- One section per breaking change (or grouped related changes)
- Real, runnable code examples (not pseudocode)
- Practical migration steps
- Troubleshooting table for common errors
- Do NOT include non-breaking features or fixes

### Formatting & Style (Industry Best Practices)

**Document Structure:**
- Start with a brief version summary (1-2 sentences about the release theme)
- Use a table of contents for guides with 5+ breaking changes
- Order sections by impact: most disruptive changes first
- End with a "Need Help?" section linking to Discord/GitHub issues

**Headings & Organization:**
- Use consistent heading hierarchy (##, ###, ####)
- Include anchor-friendly slugs for deep linking
- Group related changes under a single section when they share a workflow

**Code Blocks:**
- Always specify language for syntax highlighting (```rust, ```bash)
- Use diff syntax (```diff) when showing inline changes: `- old` / `+ new`
- Include necessary imports in examples
- Keep examples minimal but complete (compilable/runnable)
- Add comments only when the code isn't self-explanatory

**Tables:**
- Use tables for structured comparisons (before/after, error/solution)
- Align columns consistently
- Keep cell content concise

**Callouts & Emphasis:**
- Use blockquotes (>) for important warnings or tips
- Use **bold** for key terms on first mention
- Use `inline code` for types, methods, and file names
- Avoid excessive formatting - let content speak

**Tone:**
- Direct and actionable ("Update your imports" not "You may want to consider updating")
- Empathetic but not apologetic about breaking changes
- Focus on the "what to do" not the "why we broke it" (brief context is fine)

**Accessibility:**
- Write alt-text style descriptions if including diagrams
- Ensure code examples are copy-paste friendly
- Use descriptive link text (not "click here")
```

---

### Example Usage

**Generate migration guide for latest release:**
```
Generate a migration guide for the Miden Client SDK.
Target version: latest
Previous version: auto
```

**Generate migration guide for specific version:**
```
Generate a migration guide for the Miden Client SDK.
Target version: 0.13.0
Previous version: 0.12.0
```

---

### Quick Migration Checklist Prompt

For a condensed version (e.g., release notes):

```
Generate a quick migration checklist for Miden Client SDK.

1. Read `.changelog/v{VERSION}/*.md` (CHANGELOG.md for releases up to 0.18.0) and extract `[BREAKING]` entries
2. Format as a scannable bullet list:
   - [ ] Change X: do Y
   - [ ] Change Z: do W

Target version: {VERSION}  (or "latest")

One line per breaking change with the key action required.
```

---

## Why This Format Works

**For Authors (minimal effort):**
- Single line per change
- No code examples required
- Just describe what changed + category/scope tags

**For LLM (rich enough to generate guides):**
- **Category** tells it what migration pattern to apply
- **Scope** tells it which languages to cover
- **PR number** links to full context (the release adds it to each entry)
- **Brief description** gives semantic understanding
- Then LLM explores codebase to find concrete examples

---

## Optional: Enhanced Format for Complex Changes

For particularly complex breaking changes, authors can optionally add a note:

```markdown
[BREAKING][behavior][lib] Transaction validation now occurs at submission time instead of build time. This may surface errors later in the workflow. See PR for migration examples.
```

The "See PR for migration examples" hint tells the LLM to pay special attention to the PR description/comments for author-provided guidance.
