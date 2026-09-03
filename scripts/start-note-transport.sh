#!/bin/bash

# Starts the Note Transport service in the foreground.
# Installs it via cargo install if not already available.

set -euo pipefail

NOTE_TRANSPORT_VERSION=${NOTE_TRANSPORT_VERSION:-0.5.0-rc.2}
BINARY_NAME=miden-note-transport-node

if ! command -v "$BINARY_NAME" &>/dev/null; then
  echo "Installing note transport service..."
  cargo install --locked "miden-note-transport-node-bin@$NOTE_TRANSPORT_VERSION"
fi

echo "Starting note transport service in foreground..."
RUST_LOG=info exec "$BINARY_NAME"
