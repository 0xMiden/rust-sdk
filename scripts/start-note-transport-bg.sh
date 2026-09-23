#!/usr/bin/env bash
#
# Starts the note transport service in the background and returns once it accepts connections
# (used by CI). stop-note-transport.sh stops it.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG_DIR="$ROOT/target/test-node/data/logs"   # collected by CI together with the node logs
PID_FILE="$ROOT/target/test-node/note-transport.pid"
LISTEN="127.0.0.1:57292"                     # must match start-note-transport.sh

mkdir -p "$LOG_DIR"
nohup "$ROOT/scripts/start-note-transport.sh" >"$LOG_DIR/note-transport.log" 2>&1 &
PID=$!
echo "$PID" > "$PID_FILE"

for _ in $(seq 1 30); do
    if (exec 3<>"/dev/tcp/${LISTEN%:*}/${LISTEN##*:}") 2>/dev/null; then
        exec 3>&- 3<&-
        echo "==> note transport is up (pid $PID, listening on $LISTEN); log in $LOG_DIR"
        exit 0
    fi
    kill -0 "$PID" 2>/dev/null || break
    sleep 1
done

echo "error: note transport did not come up on $LISTEN; see $LOG_DIR/note-transport.log" >&2
"$ROOT/scripts/stop-note-transport.sh"
exit 1
