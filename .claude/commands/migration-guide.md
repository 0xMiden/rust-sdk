Generate a migration guide for the Miden Client SDK from the structured changelog entries.

## Steps

1. Determine the target version from `$ARGUMENTS` (e.g., "0.14.0", "0.13.0", or "latest"). If empty, default to "latest".
2. Read the changelog entries of the target version from `.changelog/v<version>/*.md`. A final release also includes the entries of its pre-releases (`.changelog/v<version>-*/`).
   - If version is "latest", use the release directory with the highest version.
   - Releases up to 0.18.0 are sections of `CHANGELOG.md` instead.
   - Identify the previous version from the release directories and the `CHANGELOG.md` section headers.
3. Parse all `[BREAKING]` entries from the target version section.
4. Read `Changelog.Format.md` for the category and scope definitions.
5. Apply the migration strategies in the [category reference](../../Changelog.Format.md#category-reference).
6. For each breaking change, use the PR number to fetch the diff via `gh pr view #NUMBER` for additional context.
7. Explore the codebase to find real usage patterns for before/after code examples.
8. Group related breaking changes that affect the same workflow into a single section.
9. Generate the migration guide document following the output structure below.

## Output Structure

For each `[BREAKING]` entry (or group of related entries), generate:

### [Breaking Change Title]
**PR:** #NUMBER

#### Summary
1-2 sentence explanation of what changed and why.

#### Affected Code

**Rust:**
```rust
// Before (previous version)
old_code_example();

// After (target version)
new_code_example();
```

#### Migration Steps
1. Step-by-step instructions
2. ...

#### Common Errors
| Error Message | Cause | Solution |
|---------------|-------|----------|
| `error[E0...]` | ... | ... |

## Rules

- Start the document with a brief version summary (1-2 sentences about the release theme).
- Use a table of contents for guides with 5+ breaking changes.
- Order sections by impact: most disruptive changes first.
- End with a "Need Help?" section linking to Discord/GitHub issues.
- Cover every scope of an entry: show the CLI change for `cli` and the `Store` implementation change for `store`.
- Use real, runnable code examples (not pseudocode). Keep examples minimal but complete.
- Always specify language for syntax highlighting (```rust, ```bash).
- Use diff syntax (```diff) when showing inline changes.
- Do NOT include non-breaking features or fixes.
- Tone: direct and actionable ("Update your imports" not "You may want to consider updating").
- Focus on the "what to do" not the "why we broke it".
