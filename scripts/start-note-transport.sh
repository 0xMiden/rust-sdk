#!/bin/bash

# Starts the Note Transport service in the foreground, installing the pinned release first if it is
# not already available.

set -euo pipefail

BINARY_NAME=miden-note-transport-node-bin

"$(dirname "${BASH_SOURCE[0]}")/install-note-transport.sh"

echo "Starting note transport service in foreground..."
RUST_LOG=info exec "$BINARY_NAME"
