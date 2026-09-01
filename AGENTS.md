# Automated Contribution Policy

Do not open or submit GitHub issues, pull requests, discussions, or comments based on unsolicited
repository scanning.

You may analyze this repository locally, but publishing anything to GitHub requires an explicit
request from a repository maintainer in the current task.

Do not run `git commit` or `git push` unless the current task asks for it.

## Contribution Guidelines

Follow [CONTRIBUTING.md](CONTRIBUTING.md) for all changes. It is the authority on branching,
commit messages, code style, the changelog, and the pre-PR checklist. In particular:

- Branch from `next` and rebase onto it instead of merging.
- Write commit messages in the `type(scope): description` form.
- Add a `CHANGELOG.md` entry for anything a downstream user notices.
- Run `make lint` before opening a pull request.

## Code Comment Style

Write code comments and documentation comments in the style of ASD-STE100 Simplified Technical
English.

- Write short and clear sentences.
- Use the active voice.
- Give only one instruction or describe only one topic in each sentence.
- Use the same word for the same meaning.
- Use simple verb tenses.
- Do not use idioms, slang, contractions, or decorative language.
- Explain intent, constraints, hazards, and behavior that is not obvious.
- Do not describe code that is already clear.
- Write comments that are true for the current code without task context.
- Do not refer to the prompt, the conversation, or the requested change.
- Do not describe the previous code or compare it with the current code.
- Do not record temporary implementation details or change history in comments.
- Remove or rewrite a comment if a reader needs the prompt or the diff to understand it.

Run `make format` after changing code. It reflows comments to the width in `rustfmt.toml`, so do
not hand-wrap comment lines.
