#!/bin/bash

# Starts the Note Transport service from the node revision in Cargo.lock.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="$ROOT/target/test-node/install/bin/miden-note-transport"
DATA="${MIDEN_NOTE_TRANSPORT_DATA_DIRECTORY:-$ROOT/target/test-node/data/note-transport}"
RPC_URL="${MIDEN_NOTE_TRANSPORT_RPC_URL:-http://127.0.0.1:57291}"
LISTEN="${MIDEN_NOTE_TRANSPORT_LISTEN:-127.0.0.1:57292}"
MAX_STORAGE_BYTES="${MIDEN_NOTE_TRANSPORT_MAX_STORAGE_BYTES:-1073741824}"
PID_FILE="$ROOT/.note-transport.pid"
LOG_FILE="$DATA/note-transport.log"

if [ ! -x "$BINARY" ]; then
  "$ROOT/scripts/start-test-node.sh" --install-only
fi

if [ ! -f "$DATA/notes.sqlite3" ]; then
  "$BINARY" bootstrap --data-directory "$DATA"
else
  "$BINARY" migrate --data-directory "$DATA"
fi

echo "Starting note transport service in background"
RUST_LOG=info nohup "$BINARY" start --data-directory "$DATA" --rpc-url "$RPC_URL" \
  --listen "$LISTEN" --max-storage-bytes "$MAX_STORAGE_BYTES" >"$LOG_FILE" 2>&1 &
echo $! > "$PID_FILE"

if [ ! -s "$PID_FILE" ]; then
  echo "Failed to start note transport service: PID file missing or empty"
  rm -f "$PID_FILE"
  exit 1
fi

PID=$(cat "$PID_FILE")
if ! [[ "$PID" =~ ^[0-9]+$ ]]; then
  echo "Failed to start note transport service: PID file invalid"
  rm -f "$PID_FILE"
  exit 1
fi

for _ in $(seq 1 30); do
  if ! ps -p "$PID" > /dev/null 2>&1; then
    echo "Failed to start note transport service; see $LOG_FILE"
    rm -f "$PID_FILE"
    exit 1
  fi
  if (exec 3<>"/dev/tcp/${LISTEN%:*}/${LISTEN##*:}") 2>/dev/null; then
    exec 3>&- 3<&-
    echo "Note transport service started (pid $PID); log at $LOG_FILE"
    exit 0
  fi
  sleep 1
done

echo "Note transport service did not listen on $LISTEN; see $LOG_FILE"
kill "$PID" 2>/dev/null || true
rm -f "$PID_FILE"
exit 1
