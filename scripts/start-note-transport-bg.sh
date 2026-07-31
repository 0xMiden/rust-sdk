#!/bin/bash

# Starts the Note Transport service in the background, installing the pinned release first if it is
# not already available.

set -euo pipefail

BINARY_NAME=miden-note-transport-node-bin
PID_FILE=.note-transport.pid

"$(dirname "${BASH_SOURCE[0]}")/install-note-transport.sh"

echo "Starting note transport service in background..."
RUST_LOG=info "$BINARY_NAME" & echo $! > "$PID_FILE"

sleep 4

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

if ! ps -p "$PID" > /dev/null 2>&1; then
  echo "Failed to start note transport service"
  rm -f "$PID_FILE"
  exit 1
fi

echo "Note transport service started (pid $PID)"
