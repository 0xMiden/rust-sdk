#!/usr/bin/env bash

set -euo pipefail

EVENT_NAME=${1:?event name is required}
BASE_SHA=${2-}
HEAD_SHA=${3-}
GITHUB_OUTPUT=${GITHUB_OUTPUT:?GITHUB_OUTPUT is required}
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

if [[ "$EVENT_NAME" != "pull_request" ]]; then
  {
    echo "agglayer=true"
    echo "integration=true"
    echo "miden-bench=true"
    echo "test-node=true"
  } >> "$GITHUB_OUTPUT"
  exit 0
fi

if [[ -z "$BASE_SHA" || -z "$HEAD_SHA" ]]; then
  echo "pull request base and head revisions are required" >&2
  exit 1
fi

changed_files=$(mktemp)
trap 'rm -f "$changed_files"' EXIT
if ! git diff --no-renames --name-only "$BASE_SHA...$HEAD_SHA" > "$changed_files"; then
  echo "failed to compare $BASE_SHA with $HEAD_SHA" >&2
  exit 1
fi

"$SCRIPT_DIR/select-system-tests.sh" < "$changed_files" >> "$GITHUB_OUTPUT"
