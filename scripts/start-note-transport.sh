#!/bin/bash

# Starts the Note Transport service in the foreground.
# Installs it via cargo install if not already available.

set -euo pipefail

REPO_URL=${REPO_URL:-https://github.com/0xMiden/miden-note-transport}
BINARY_NAME=miden-note-transport-node

if ! command -v "$BINARY_NAME" &>/dev/null; then
  echo "Installing note transport service..."
  cargo install --git "$REPO_URL" --locked
fi

echo "Starting note transport service in foreground..."
RUST_LOG=info exec "$BINARY_NAME"
