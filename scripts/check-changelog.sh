#!/bin/bash
set -euo pipefail

CHANGELOG_DIR=".changelog/unreleased"

python3 scripts/changelog.py

if [ "${NO_CHANGELOG_LABEL:-false}" = "true" ]; then
    echo '"no changelog" label has been set'
    exit 0
fi

changed=$(git diff --name-only --no-renames --diff-filter=AM "origin/${BASE_REF}...HEAD" -- ":(glob)${CHANGELOG_DIR}/*.md")
if [ -z "$changed" ]; then
    >&2 echo "Add or update an entry in ${CHANGELOG_DIR}/. See .changelog/README.md.
Use the \"no changelog\" label for changes that do not need an entry."
    exit 1
fi

echo "The ${CHANGELOG_DIR} directory has been updated."
