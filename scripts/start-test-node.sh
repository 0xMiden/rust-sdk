#!/usr/bin/env bash
#
# Starts a self-contained testing node (validator, sequencer, ntx-builder, and tx prover) from
# the standalone node executables, installed with `cargo install` at the node source pinned in
# Cargo.lock.
#
# Modes:
#   (no args)        start the node and stream its logs; Ctrl+C stops it
#   --background     return once the node's RPC is ready, leaving it running (used by CI)
#   --install-only   install the node binaries and exit (used by the CI build job)
#   --print-rev      print the pinned node rev or version (CI cache key) and exit

set -euo pipefail

MODE="foreground"
case "${1:-}" in
    --background)   MODE="background" ;;
    --install-only) MODE="install-only" ;;
    --print-rev)    MODE="print-rev" ;;
    "")             ;;
    *) echo "error: unknown argument '$1'" >&2; exit 2 ;;
esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CACHE="$ROOT/target/test-node"
BIN="$CACHE/install/bin"
BUILD="$CACHE/build"
GEN_GENESIS="${CARGO_TARGET_DIR:-$ROOT/target}/release/gen-genesis"
DATA="$CACHE/data"
LOG_DIR="$DATA/logs"
PID_FILE="$CACHE/pids"

RPC="127.0.0.1:57291"   # matches the client default (`MIDEN_NODE_PORT`)
VALIDATOR="127.0.0.1:50101"
NTX="127.0.0.1:50301"
PROVER_PORT=50051
PROVER="127.0.0.1:$PROVER_PORT"
# Shared secret authorizing the ntx-builder to submit network transactions; the sequencer rejects
# them unless both sides agree on it.
NETWORK_TX_AUTH="${MIDEN_NETWORK_TX_AUTH:-miden-client-testing-ntx-secret}"

NODE_BINS=(miden-validator miden-node miden-ntx-builder miden-remote-prover)

# Resolve the pinned node source from Cargo.lock: a git pin takes precedence, otherwise use the
# crates.io version locked for `miden-node-proto-build`.
SRC_LINE="$(grep -m1 'source = "git+https://github.com/0xMiden/node' "$ROOT/Cargo.lock" || true)"
if [ -n "$SRC_LINE" ]; then
    NODE_SOURCE="git"
    SRC="${SRC_LINE#*\"git+}"; SRC="${SRC%\"}"
    NODE_REV="${SRC##*#}"
    NODE_URL="${SRC%%#*}"; NODE_URL="${NODE_URL%%\?*}"
    NODE_DESC="$NODE_URL @ $NODE_REV"
else
    NODE_SOURCE="registry"
    NODE_VERSION="$(awk -F'"' '/^name = "miden-node-proto-build"$/ { getline; print $2; exit }' "$ROOT/Cargo.lock")"
    [ -n "$NODE_VERSION" ] || {
        echo "error: no 0xMiden/node git source and no miden-node-proto-build version in Cargo.lock" >&2
        exit 1
    }
    NODE_REV="v$NODE_VERSION"
    NODE_DESC="crates.io @ $NODE_VERSION"
fi

if [ "$MODE" = "print-rev" ]; then
    echo "$NODE_REV"
    exit 0
fi

node_binaries_installed() {
    local metadata="$CACHE/install/.crates.toml"
    [ -f "$metadata" ] || return 1

    # `.crates.toml` records each install as `"<bin> <version> (<source>)"`.
    for bin in "${NODE_BINS[@]}"; do
        [ -x "$BIN/$bin" ] || return 1
        if [ "$NODE_SOURCE" = "git" ]; then
            grep -F "\"$bin " "$metadata" | grep -Fq "#$NODE_REV)" || return 1
        else
            grep -Fq "\"$bin $NODE_VERSION (registry+" "$metadata" || return 1
        fi
    done
}

if node_binaries_installed; then
    echo "==> using cached node binaries ($NODE_DESC)"
else
    echo "==> installing node binaries ($NODE_DESC)"
    INSTALL_SPECS=("${NODE_BINS[@]}")
    if [ "$NODE_SOURCE" = "git" ]; then
        INSTALL_FLAGS=(--git "$NODE_URL" --rev "$NODE_REV")
    else
        INSTALL_FLAGS=()
        INSTALL_SPECS=()
        for bin in "${NODE_BINS[@]}"; do INSTALL_SPECS+=("$bin@$NODE_VERSION"); done
    fi
    # Override the profile to drop debug info and strip symbols to reduce the size
    CARGO_PROFILE_RELEASE_DEBUG=false \
    CARGO_PROFILE_RELEASE_STRIP=symbols \
        cargo install --locked --root "$CACHE/install" --target-dir "$BUILD" \
        ${INSTALL_FLAGS[@]+"${INSTALL_FLAGS[@]}"} \
        "${INSTALL_SPECS[@]}"
fi

if [ "$MODE" = "install-only" ]; then
    echo "==> install-only: node binaries ready in $BIN"
    exit 0
fi

if (exec 3<>"/dev/tcp/${RPC%:*}/${RPC##*:}") 2>/dev/null; then
    exec 3>&- 3<&-
    echo "error: something is already listening on $RPC; run stop-test-node.sh first" >&2
    exit 1
fi

echo "==> building gen-genesis"
cargo build --release -p test-node-genesis --bin gen-genesis

echo "==> generating genesis + bootstrapping"
rm -rf "$DATA"
# Each component opens its SQLite DB directly under its data dir and does not create it.
mkdir -p "$LOG_DIR" "$DATA/validator" "$DATA/node" "$DATA/ntx-builder"
"$GEN_GENESIS" "$DATA/genesis-config"
mkdir -p "$ROOT/data"
cp "$DATA/genesis-config/tst_faucet.mac" "$ROOT/data/account.mac"
# With AGGLAYER_GENESIS set, gen-genesis also emits the agglayer account files; expose them under
# ./data so tests can load them via AGGLAYER_ACCOUNTS_DIR=./data.
for mac in bridge_admin.mac ger_manager.mac bridge.mac agglayer_faucet.mac; do
    if [ -f "$DATA/genesis-config/$mac" ]; then
        cp "$DATA/genesis-config/$mac" "$ROOT/data/$mac"
    fi
done

{
    "$BIN/miden-validator" bootstrap --data-directory "$DATA/validator" \
        --genesis-block-directory "$DATA/genesis" --accounts-directory "$DATA/accounts" \
        --genesis-config-file "$DATA/genesis-config/genesis.toml"
    "$BIN/miden-node" bootstrap --data-directory "$DATA/node" --file "$DATA/genesis/genesis.dat"
    "$BIN/miden-ntx-builder" bootstrap --data-directory "$DATA/ntx-builder" --file "$DATA/genesis/genesis.dat"
} >"$LOG_DIR/bootstrap.log" 2>&1

echo "==> starting components"
: > "$PID_FILE"
start() {
    local name="$1"; shift
    # As async children the components would inherit an ignored SIGINT and survive Ctrl+C, so
    # reset the disposition to default before exec'ing them; the terminal's Ctrl+C then kills
    # them directly, without relying on this script's (racy) signal trap.
    RUST_LOG="${RUST_LOG:-info}" nohup perl -e '$SIG{INT} = "DEFAULT"; exec @ARGV' "$@" \
        >"$LOG_DIR/$name.log" 2>&1 &
    echo "$!" >> "$PID_FILE"
}
cleanup() {
    trap - INT TERM
    if [ -n "${TAIL_PID:-}" ]; then kill "$TAIL_PID" 2>/dev/null || true; fi
    "$ROOT/scripts/stop-test-node.sh"
}
# Best-effort teardown for SIGTERM and for interrupts the components' own SIGINT death doesn't
# cover (e.g. `kill <script>`); Ctrl+C teardown does not depend on this trap firing.
trap 'echo; cleanup; exit 0' INT TERM
start validator   "$BIN/miden-validator" start --listen "$VALIDATOR" --data-directory "$DATA/validator"
# Let the validator bind before the sequencer starts producing blocks against it.
sleep 2
start sequencer   "$BIN/miden-node" sequencer --rpc.listen "$RPC" --data-directory "$DATA/node" \
    --validator.url "http://$VALIDATOR" --ntx-builder.url "http://$NTX" \
    --rpc.network-tx-auth-header-value "$NETWORK_TX_AUTH" \
    --block.interval 3s --batch.interval 1s \
    --rpc.rate-limit.burst-size 10000 --rpc.rate-limit.replenish-per-second 10000
start prover      "$BIN/miden-remote-prover" --kind=transaction --port="$PROVER_PORT"
# Let the sequencer bind its RPC before the ntx-builder dials it.
sleep 2
start ntx-builder "$BIN/miden-ntx-builder" start --listen "$NTX" --rpc.url "http://$RPC" \
    --rpc.auth-header-value "$NETWORK_TX_AUTH" --tx-prover.url "http://$PROVER" \
    --max-cycles "$((1 << 18))" \
    --data-directory "$DATA/ntx-builder"

# Returns non-zero (with a message) if any started component is no longer running.
check_components_alive() {
    while read -r pid; do
        [ -n "$pid" ] || continue
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "error: a node service exited; see $LOG_DIR" >&2
            return 1
        fi
    done < "$PID_FILE"
}

echo "==> waiting for RPC on $RPC"
READY=""
for _ in $(seq 1 60); do
    if (exec 3<>"/dev/tcp/${RPC%:*}/${RPC##*:}") 2>/dev/null; then
        exec 3>&- 3<&-
        READY=1
        break
    fi
    check_components_alive || exit 1
    sleep 1
done
if [ -z "$READY" ]; then
    echo "error: RPC did not become ready within 60s; see $LOG_DIR" >&2
    exit 1
fi
echo "==> node is up (RPC on http://$RPC); logs in $LOG_DIR"

if [ "$MODE" = "background" ]; then
    exit 0
fi

# Foreground: stream logs until Ctrl+C (which stops the node) or a component dies. The tail gets
# the same default-SIGINT treatment as the components so Ctrl+C kills it too.
echo "==> streaming logs (Ctrl+C stops the node)"
perl -e '$SIG{INT} = "DEFAULT"; exec @ARGV' tail -n +1 -F "$LOG_DIR"/*.log &
TAIL_PID=$!
while check_components_alive; do
    sleep 1
done
cleanup
exit 1
