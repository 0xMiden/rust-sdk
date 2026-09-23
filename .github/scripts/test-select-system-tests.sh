#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ENTRYPOINT="$SCRIPT_DIR/select-system-tests-from-git.sh"
SELECTOR="$SCRIPT_DIR/select-system-tests.sh"

assert_selection() {
  local name=$1
  local expected=$2
  shift 2

  if ! diff -u <(printf '%s\n' "$expected") <(printf '%s\0' "$@" | "$SELECTOR"); then
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
benchmark_only=$(printf '%s\n' \
  "agglayer=false" \
  "integration=false" \
  "miden-bench=true" \
  "test-node=true")
benchmark_and_integration=$(printf '%s\n' \
  "agglayer=false" \
  "integration=true" \
  "miden-bench=true" \
  "test-node=true")
no_systems=$(printf '%s\n' \
  "agglayer=false" \
  "integration=false" \
  "miden-bench=false" \
  "test-node=false")

assert_selection "AggLayer build script" "$all_systems" "bin/integration-tests/build.rs"
assert_selection "AggLayer Foundry source" "$all_systems" \
  "bin/integration-tests/foundry-vectors/src/DepositContractTestHelpers.sol"
assert_selection "AggLayer Foundry configuration" "$all_systems" \
  "bin/integration-tests/foundry-vectors/foundry.toml"
assert_selection "Nextest configuration" "$all_systems" ".config/nextest.toml"
assert_selection "validator fixture" "$all_systems" \
  "scripts/testdata/insecure-golden-storage-key/secret-share.wire"
assert_selection "selector" "$all_systems" ".github/scripts/select-system-tests.sh"
assert_selection "other integration code" "$integration_only" "bin/miden-cli/src/main.rs"
assert_selection "benchmark script" "$benchmark_only" "scripts/test-miden-bench-smoke.sh"
assert_selection \
  "benchmark then integration" \
  "$benchmark_and_integration" \
  "scripts/test-miden-bench-smoke.sh" \
  "bin/miden-cli/src/main.rs" \
  ".github/workflows/lint.yml"
assert_selection \
  "integration then benchmark" \
  "$benchmark_and_integration" \
  "bin/miden-cli/src/main.rs" \
  "scripts/test-miden-bench-smoke.sh" \
  ".github/workflows/lint.yml"
assert_selection "unrelated workflow" "$no_systems" ".github/workflows/lint.yml"

assert_comparison_failure() (
  local name=$1
  local base_sha=$2
  local head_sha=$3
  local expected_error=$4
  local output_file
  local error_file=""

  output_file=$(mktemp)
  trap 'rm -f "$output_file" "$error_file"' EXIT
  error_file=$(mktemp)
  if GITHUB_OUTPUT="$output_file" \
    "$ENTRYPOINT" pull_request "$base_sha" "$head_sha" 2> "$error_file"; then
    echo "$name did not fail" >&2
    exit 1
  fi
  if [[ -s "$output_file" ]]; then
    echo "$name wrote selection outputs" >&2
    exit 1
  fi
  if ! grep -Fq "$expected_error" "$error_file"; then
    echo "$name returned the wrong error" >&2
    exit 1
  fi
)

assert_comparison_failure \
  "invalid revisions" \
  "missing-base-revision" \
  "missing-head-revision" \
  "failed to compare missing-base-revision with missing-head-revision"
assert_comparison_failure \
  "missing base revision" \
  "" \
  "missing-head-revision" \
  "pull request base and head revisions are required"
assert_comparison_failure \
  "missing head revision" \
  "missing-base-revision" \
  "" \
  "pull request base and head revisions are required"

assert_fixture_move_selects_systems() (
  local test_repo
  local output_file
  local base_sha
  local head_sha

  test_repo=$(mktemp -d)
  trap 'rm -rf "$test_repo"' EXIT
  git -C "$test_repo" init -q
  git -C "$test_repo" config user.email "ci@example.com"
  git -C "$test_repo" config user.name "CI"
  mkdir -p "$test_repo/scripts/testdata/insecure-golden-storage-key"
  echo "fixture" > "$test_repo/scripts/testdata/insecure-golden-storage-key/secret-share.wire"
  git -C "$test_repo" add scripts/testdata/insecure-golden-storage-key/secret-share.wire
  git -C "$test_repo" commit -qm "Add fixture"
  base_sha=$(git -C "$test_repo" rev-parse HEAD)

  mkdir -p "$test_repo/moved"
  git -C "$test_repo" mv \
    scripts/testdata/insecure-golden-storage-key/secret-share.wire \
    moved/secret-share.wire
  git -C "$test_repo" commit -qm "Move fixture"
  head_sha=$(git -C "$test_repo" rev-parse HEAD)

  output_file=$(mktemp)
  trap 'rm -rf "$test_repo"; rm -f "$output_file"' EXIT
  (
    cd "$test_repo"
    GITHUB_OUTPUT="$output_file" "$ENTRYPOINT" pull_request "$base_sha" "$head_sha"
  )
  if ! diff -u <(printf '%s\n' "$all_systems") "$output_file"; then
    echo "moving a validator fixture did not select all systems" >&2
    exit 1
  fi
)

assert_fixture_move_selects_systems

assert_quoted_filename_selects_systems() (
  local test_repo
  local output_file
  local base_sha
  local head_sha
  local quoted_path

  test_repo=$(mktemp -d)
  trap 'rm -rf "$test_repo"' EXIT
  git -C "$test_repo" init -q
  git -C "$test_repo" config user.email "ci@example.com"
  git -C "$test_repo" config user.name "CI"
  echo "base" > "$test_repo/README.md"
  git -C "$test_repo" add README.md
  git -C "$test_repo" commit -qm "Add base"
  base_sha=$(git -C "$test_repo" rev-parse HEAD)

  quoted_path=$'crates/rust-client/src/quoted\tname.rs'
  mkdir -p "$test_repo/crates/rust-client/src"
  echo "changed" > "$test_repo/$quoted_path"
  git -C "$test_repo" add -- "$quoted_path"
  git -C "$test_repo" commit -qm "Add quoted filename"
  head_sha=$(git -C "$test_repo" rev-parse HEAD)

  output_file=$(mktemp)
  trap 'rm -rf "$test_repo"; rm -f "$output_file"' EXIT
  (
    cd "$test_repo"
    GITHUB_OUTPUT="$output_file" "$ENTRYPOINT" pull_request "$base_sha" "$head_sha"
  )
  if ! diff -u <(printf '%s\n' "$all_systems") "$output_file"; then
    echo "a quoted filename under shared client code did not select all systems" >&2
    exit 1
  fi
)

assert_quoted_filename_selects_systems

assert_push_selects_all_systems() (
  local test_dir
  local output_file

  test_dir=$(mktemp -d)
  trap 'rm -rf "$test_dir"' EXIT
  output_file=$(mktemp)
  trap 'rm -rf "$test_dir"; rm -f "$output_file"' EXIT
  (
    cd "$test_dir"
    GITHUB_OUTPUT="$output_file" "$ENTRYPOINT" push
  )
  if ! diff -u <(printf '%s\n' "$all_systems") "$output_file"; then
    echo "push did not select all systems" >&2
    exit 1
  fi
)

assert_push_selects_all_systems
