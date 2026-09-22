#!/bin/bash

# Starts the Note Transport service from the node revision in Cargo.lock.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="$ROOT/target/test-node/install/bin/miden-note-transport"
DATA="${MIDEN_NOTE_TRANSPORT_DATA_DIRECTORY:-$ROOT/target/test-node/data/note-transport}"
RPC_URL="${MIDEN_NOTE_TRANSPORT_RPC_URL:-http://127.0.0.1:57291}"
LISTEN="${MIDEN_NOTE_TRANSPORT_LISTEN:-127.0.0.1:57292}"
MAX_STORAGE_BYTES="${MIDEN_NOTE_TRANSPORT_MAX_STORAGE_BYTES:-1073741824}"

if [ ! -x "$BINARY" ]; then
  "$ROOT/scripts/start-test-node.sh" --install-only
fi

if [ ! -f "$DATA/notes.sqlite3" ]; then
  "$BINARY" bootstrap --data-directory "$DATA"
else
  "$BINARY" migrate --data-directory "$DATA"
fi

echo "Starting note transport service in foreground"
RUST_LOG=info exec "$BINARY" start --data-directory "$DATA" --rpc-url "$RPC_URL" \
  --listen "$LISTEN" --max-storage-bytes "$MAX_STORAGE_BYTES"
