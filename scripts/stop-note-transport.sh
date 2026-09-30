#!/usr/bin/env bash
#
# Stops the note transport service started by start-note-transport-bg.sh.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PID_FILE="$ROOT/target/test-node/note-transport.pid"

if [ -f "$PID_FILE" ]; then
    kill "$(cat "$PID_FILE")" 2>/dev/null || true
    rm -f "$PID_FILE"
fi

# Fallback in case the pid file is stale.
pkill -f "$ROOT/target/test-node/install/bin/miden-note-transport" 2>/dev/null || true

sleep 1
echo "Stopped note transport."
