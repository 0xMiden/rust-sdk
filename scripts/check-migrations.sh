#!/bin/bash
# Fails closed: any error resolving the base branch or running git aborts the script rather than
# reporting success. This is the only automated guard against an edited migration, so a silent
# pass would be worse than a false alarm.
set -euo pipefail

MIGRATIONS_DIR="${1:-crates/sqlite-store/src/migrations}"
BASE="origin/${BASE_REF:?must be set to the base branch of the pull request}"

if ! git rev-parse --verify --quiet "${BASE}^{commit}" > /dev/null; then
    >&2 echo "Cannot resolve \"${BASE}\". Fetch the base branch before running this check."
    exit 1
fi

# Compared against the merge base rather than the tip of the base branch, so a migration added on
# the base branch after this one forked is not attributed to this pull request.
CHANGED=$(git diff --name-only --diff-filter=MDR --merge-base "${BASE}" -- "${MIGRATIONS_DIR}")

if [ -z "${CHANGED}" ]; then
    echo "No released migration was modified."
    exit 0
fi

>&2 echo "The following merged migrations were modified, renamed or deleted:"
>&2 echo "${CHANGED}"
>&2 echo ""
>&2 echo "Migrations are append-only. Add a new file under \"${MIGRATIONS_DIR}\" with the next
version prefix instead, register it in CLIENT_MIGRATIONS and append its schema hash to
PINNED_SCHEMA_HASHES, both in crates/sqlite-store/src/db_management/migration.rs, rather than editing
the existing entries."
exit 1
