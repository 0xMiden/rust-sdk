#!/bin/bash

# Starts the Note Transport service in the background.
# Installs it via cargo install if not already available.

set -euo pipefail

REPO_URL=${REPO_URL:-https://github.com/0xMiden/miden-note-transport}
BINARY_NAME=miden-note-transport-node
PID_FILE=.note-transport.pid

if ! command -v "$BINARY_NAME" &>/dev/null; then
  echo "Installing note transport service..."
  cargo install --git "$REPO_URL" --locked
fi

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
