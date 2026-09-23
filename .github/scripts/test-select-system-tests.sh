#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SELECTOR="$SCRIPT_DIR/select-system-tests.sh"

assert_selection() {
  local name=$1
  local expected=$2
  local path=$3

  if ! diff -u <(printf '%s\n' "$expected") <(printf '%s\n' "$path" | "$SELECTOR"); then
    echo "selection failed for $name" >&2
    exit 1
  fi
}

all_systems=$(printf '%s\n' \
  "agglayer=true" \
  "integration=true" \
  "miden-bench=true" \
  "test-node=true")
integration_only=$(printf '%s\n' \
  "agglayer=false" \
  "integration=true" \
  "miden-bench=false" \
  "test-node=true")
no_systems=$(printf '%s\n' \
  "agglayer=false" \
  "integration=false" \
  "miden-bench=false" \
  "test-node=false")

assert_selection "AggLayer build script" "$all_systems" "bin/integration-tests/build.rs"
assert_selection "Nextest configuration" "$all_systems" ".config/nextest.toml"
assert_selection "validator fixture" "$all_systems" \
  "scripts/testdata/insecure-golden-storage-key/secret-share.wire"
assert_selection "selector" "$all_systems" ".github/scripts/select-system-tests.sh"
assert_selection "other integration code" "$integration_only" "bin/miden-cli/src/main.rs"
assert_selection "unrelated workflow" "$no_systems" ".github/workflows/lint.yml"
