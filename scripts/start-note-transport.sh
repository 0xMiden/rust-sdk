#!/usr/bin/env bash
#
# Starts the note transport service in the foreground from the node binaries installed by
# start-test-node.sh. Ctrl+C stops it. Every start begins from an empty database, like the node
# components.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/test-node/install/bin/miden-note-transport"
DATA="$ROOT/target/test-node/data/note-transport"

RPC="http://127.0.0.1:57291"   # the test node's RPC (see start-test-node.sh)
LISTEN="127.0.0.1:57292"       # matches the client default (`TEST_MIDEN_NOTE_TRANSPORT_URL`)
MAX_STORAGE_BYTES=$((1 << 30))

[ -x "$BIN" ] || "$ROOT/scripts/start-test-node.sh" --install-only

rm -rf "$DATA"
"$BIN" bootstrap --data-directory "$DATA"

RUST_LOG="${RUST_LOG:-info}" exec "$BIN" start --data-directory "$DATA" --rpc-url "$RPC" \
    --listen "$LISTEN" --max-storage-bytes "$MAX_STORAGE_BYTES"
