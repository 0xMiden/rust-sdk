#!/usr/bin/env python3
"""Move unreleased entries into a release directory and add their PR links."""

import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

from changelog import SRC, read_entries

PR_URL = "https://github.com/0xMiden/rust-sdk/pull"
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
    tag = sys.argv[1]
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?", tag):
        sys.exit("Release tags must look like v1.2.3 or v1.2.3-rc.1")
    dst = Path(".changelog") / tag
    if dst.exists():
        sys.exit(f"{dst} already exists. Refusing to append release entries.")
    files = sorted(SRC.rglob("*.md"))
    if not files:
        sys.exit(f"No changelog entries found in {SRC}/")

    # Validate all entries and find their PRs before writing release files.
    entries = defaultdict(list)
    try:
        for path in files:
            lines = read_entries(path)
            linked = [LINKED.search(line) for line in lines]
            pr = int(linked[0][1]) if all(linked) else pr_of(path)
            for line, link in zip(lines, linked):
                if not link:
                    line = f"{line.removesuffix('.')} ([#{pr}]({PR_URL}/{pr}))."
                entries[pr].append(f"- {line}\n")
    except (OSError, ValueError) as error:
        sys.exit(str(error))

    dst.mkdir()
    for pr, lines in entries.items():
        (dst / f"{pr}.md").write_text("".join(lines))

    subprocess.run(["git", "rm", "-q", *map(str, files)], check=True)
    subprocess.run(["git", "add", str(dst)], check=True)


if __name__ == "__main__":
    main()
