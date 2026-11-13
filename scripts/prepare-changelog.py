#!/usr/bin/env python3
"""Moves the `.changelog/unreleased/` entries into `.changelog/<tag>/<PR>.md`, adding their PR links. The PR of
an entry is the `(#N)` that GitHub appends to the subject of the squash commit that added the file. A file
holding just the name of another entry adds its PR to the links of that entry instead. A line that already ends
with a PR link is kept as it is.
"""

import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

SRC = Path(".changelog/unreleased")
PR_URL = "https://github.com/0xMiden/rust-sdk/pull"
# The links at the end of a line that already has them, e.g. `([#1](url), [#2](url)).`.
LINKED = re.compile(r"\(\[#(\d+)\]\([^()]*\)(?:, \[#\d+\]\([^()]*\))*\)\.$")


def pr_of(path: Path) -> int:
    log = ["git", "log", "--no-renames", "--diff-filter=A", "--format=%s", "--", str(path)]
    subject = subprocess.run(log, capture_output=True, text=True, check=True).stdout.partition("\n")[0]
    match = re.search(r"\(#(\d+)\)$", subject)
    if not match:
        sys.exit(
            f'No PR number in "{subject}", the commit that added {path}. '
            "Move its lines to .changelog/<tag>/<PR>.md by hand, with the PR link."
        )
    return int(match[1])


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(f"Usage: {sys.argv[0]} <tag>")
    nested = [str(p) for p in SRC.rglob("*.md") if p.parent != SRC]
    if nested:
        sys.exit(f"Entries must be directly in {SRC}/, move or remove: {', '.join(nested)}")
    files = sorted(SRC.glob("*.md"))
    if not files:
        sys.exit(f"No changelog entries found in {SRC}/")
    stems = {f.stem for f in files}

    # Find every PR before any file is written, so that a failure leaves the entries in place.
    links = defaultdict(set)  # The PRs of the files that reference an entry.
    entries = {}  # The lines and the PR of every entry.
    for f in files:
        target = f.read_text().strip()
        if target in stems and target != f.stem:
            links[target].add(pr_of(f))
            continue
        lines = list(filter(None, f.read_text().splitlines()))
        linked = [LINKED.search(line) for line in lines]
        # An entry whose lines all have links is named after the PR of its first link.
        pr = int(linked[0][1]) if lines and all(linked) else pr_of(f)
        entries[f.stem] = (lines, pr)

    for target in links:
        if target not in entries:
            sys.exit(f"{target}.md is referenced by another file but only references an entry itself")
        if all(LINKED.search(line) for line in entries[target][0]):
            sys.exit(f"{target}.md is referenced by another file but its lines already have PR links")

    dst = Path(".changelog") / sys.argv[1]
    dst.mkdir(exist_ok=True)
    for stem, (lines, pr) in entries.items():
        refs = ", ".join(f"[#{n}]({PR_URL}/{n})" for n in sorted(links[stem] | {pr}))
        with open(dst / f"{pr}.md", "a") as out:
            for line in lines:
                if not LINKED.search(line):
                    line = f"{line.removesuffix('.')} ({refs})."
                out.write(f"- {line}\n")

    subprocess.run(["git", "rm", "-q", *map(str, files)], check=True)
    subprocess.run(["git", "add", str(dst)], check=True)


if __name__ == "__main__":
    main()
