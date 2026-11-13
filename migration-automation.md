# Migration guide generation

[Changelog.Format.md](Changelog.Format.md) defines the entry syntax, scopes, categories, and migration
strategies. [The changelog README](.changelog/README.md) describes entry files and release preparation.

Use [the migration guide command](.claude/commands/migration-guide.md) to generate a full guide. It defines
the source selection, output structure, and review rules.

## Full guide

Ask the agent to follow the migration guide command and supply a target version:

```text
Follow .claude/commands/migration-guide.md.
Target version: 0.19.0
Previous version: auto
```

Use `latest` to select the most recent release. The agent uses repository code, Git history, and PR diffs
to produce examples and migration steps. Check that the examples use the APIs of the selected versions.

## Quick checklist

```text
Follow the source selection rules in .claude/commands/migration-guide.md for version {VERSION}.
Read Changelog.Format.md and extract the breaking changes.
Write one checklist item per change, with the action that users must take.
Include the entries of pre-releases when the target is a final release.
```
