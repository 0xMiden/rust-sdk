#!/usr/bin/env bash

set -euo pipefail

agglayer=false
integration=false
miden_bench=false

while IFS= read -r -d '' path; do
  case "$path" in
    Cargo.toml|Cargo.lock|Makefile|rust-toolchain.toml|.cargo/*|.config/nextest.toml|.github/workflows/test.yml|.github/actions/cleanup-runner/*|.github/scripts/select-system-tests*.sh|scripts/testdata/insecure-golden-storage-key/*)
      agglayer=true
      integration=true
      miden_bench=true
      ;;
  esac

  case "$path" in
    crates/*|bin/*|data/*|scripts/start-ci-service.sh|scripts/start-note-transport*.sh|scripts/stop-note-transport.sh|scripts/start-test-node.sh|scripts/stop-test-node.sh)
      integration=true
      ;;
  esac

  case "$path" in
    bin/miden-bench/*|bin/integration-tests/*|crates/rust-client/*|crates/sqlite-store/*|crates/testing/test-node-genesis/*|data/*|scripts/start-test-node.sh|scripts/stop-test-node.sh|scripts/test-miden-bench-smoke.sh)
      miden_bench=true
      ;;
  esac

  case "$path" in
    bin/integration-tests/Cargo.toml|bin/integration-tests/build.rs|bin/integration-tests/foundry-vectors/*|bin/integration-tests/src/config.rs|bin/integration-tests/src/fee_funding.rs|bin/integration-tests/src/lib.rs|bin/integration-tests/src/tests/mod.rs|bin/integration-tests/src/tests/agglayer/*|bin/integration-tests/tests/integration.rs|crates/rust-client/*|crates/sqlite-store/*|crates/testing/test-node-genesis/*|data/*|scripts/start-test-node.sh|scripts/stop-test-node.sh)
      agglayer=true
      ;;
  esac
done

printf 'agglayer=%s\n' "$agglayer"
printf 'integration=%s\n' "$integration"
printf 'miden-bench=%s\n' "$miden_bench"
if [[ "$agglayer" == "true" || "$integration" == "true" || "$miden_bench" == "true" ]]; then
  echo "test-node=true"
else
  echo "test-node=false"
fi
