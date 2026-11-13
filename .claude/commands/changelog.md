Generate changelog entries for the current branch following the project's Changelog.Format.md conventions.

## Steps

1. Determine the base branch (default: `next`, or use `$ARGUMENTS` if provided).
2. Run `git log --oneline origin/<base>...HEAD` to get all commits on this branch.
3. Run `git diff --stat origin/<base>...HEAD` to understand the scope of changes.
4. Read `Changelog.Format.md` for the exact format specification and `.changelog/README.md` for where entries go.
5. Read a few files in `.changelog/unreleased/` to match the existing style.
6. Categorize the changes with the tags defined in `Changelog.Format.md`.
7. Write the generated entries to `.changelog/unreleased/<name>.md`, one entry per line, without a bullet and without a PR link. `<name>` is the last segment of the branch name. If the branch already added an entry file, add the new lines to that file. Do not duplicate entries that are already present.
8. Show the user the entries that were added.

## Rules

- Use the categories and scopes defined in [Changelog.Format.md](../../Changelog.Format.md).
- Do not add a PR number or link. The release adds it.
- Do not edit `CHANGELOG.md`. It is the archive of the releases up to 0.18.0.
- Group related commits into single entries (don't produce one entry per commit).
- Focus on user-visible changes; skip internal refactors unless they affect the public API.
- Match the tone and detail level of the existing entries.
