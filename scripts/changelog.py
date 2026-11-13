"""Validate unreleased changelog entries."""

import re
import sys
from pathlib import Path

SRC = Path(".changelog/unreleased")
SCOPES = r"(?:lib|cli|store)(?:,(?:lib|cli|store))*"
ENTRY = re.compile(
    rf"(?:\[BREAKING\]\[(?:rename|removal|param|type|behavior|arch)\]|\[(?:FEATURE|FIX)\])"
    rf"\[{SCOPES}\] \S.*"
)


def read_entries(path: Path) -> list[str]:
    if path.parent != SRC:
        raise ValueError(f"{path}: Entries must be directly in {SRC}/")
    lines = path.read_text().splitlines()
    if not lines:
        raise ValueError(f"{path}: Entry files must not be empty")
    for number, line in enumerate(lines, 1):
        if not ENTRY.fullmatch(line):
            raise ValueError(f"{path}:{number}: Invalid entry. See Changelog.Format.md")
    return lines


def main() -> None:
    try:
        for path in sorted(SRC.rglob("*.md")):
            read_entries(path)
    except (OSError, ValueError) as error:
        sys.exit(str(error))
    print("Changelog entries are valid.")


if __name__ == "__main__":
    main()
