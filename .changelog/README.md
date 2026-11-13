# Changelog entries

Every PR that changes something a downstream user notices adds a file to `unreleased/` named after the change
(e.g. the branch name), with one line per entry:

```markdown
[BREAKING][rename][lib] Renamed `Foo` to `Bar`.
```

[`Changelog.Format.md`](../Changelog.Format.md) describes the tags. The PR link is added at release, so the file
can be written before the PR exists. A follow-up PR can update an existing entry or add its own entry file.
Each file must contain complete entries.

PRs that do not need an entry get the `no changelog` label. CI validates entry files even with this label.
Run `python3 scripts/changelog.py` to validate the entries locally.

## Release

Before creating the GitHub release, run the following and commit the result. The release directory must not
already exist:

```sh
./scripts/prepare-changelog.py <tag>
```

It moves the entries into `.changelog/<tag>/<PR>.md` and adds their PR links, taking each PR number from the
`(#N)` suffix of the commit that added the file. A line that already ends with a PR link is kept as it is. The
publish workflow replaces the release page's notes with these entries, and a final release also includes the
entries of its pre-releases.

`unreleased/.gitkeep` keeps the directory alive: without it, git treats the release as a rename of `unreleased/`
and moves entries added on `next` in the meantime into the release directory when the release branch is merged
back.

After merging `main` or a release branch back into `next`, delete any `unreleased/` entry that already shipped in
a release directory (e.g. a cherry-picked fix).

Entries of earlier releases are in [`CHANGELOG.md`](../CHANGELOG.md).
