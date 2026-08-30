#!/usr/bin/env bash
#
# Starts a self-contained testing node (validator, sequencer, ntx-builder, and tx prover) from
# the standalone node executables, installed with `cargo install` at the node source pinned in
# Cargo.lock -- unless MIDEN_TEST_NODE_USE_INSTALLED says otherwise, see Environment below.
#
# Modes:
#   (no args)        start the node and stream its logs; Ctrl+C stops it
#   --background     return once the node's RPC is ready, leaving it running (used by CI)
#   --install-only   install the node binaries and exit (used by the CI build job)
#   --print-rev      print the pinned node rev or version (CI cache key) and exit
#
# Environment:
#   MIDEN_TEST_NODE_VERIFICATION_BASE_FEE  a non-zero u32 makes the chain charge that fee and puts
#                                          funder wallets in genesis; unset or 0 leaves fees off,
#                                          and anything else aborts before the previous chain is
#                                          wiped. See
#                                          crates/testing/test-node-genesis/README.md
#   MIDEN_TEST_NODE_USE_INSTALLED          any non-empty value (0 included) runs whatever is
#                                          already installed instead of the pinned node
#   AGGLAYER_GENESIS                       any value, empty included, adds the agglayer accounts
#                                          to genesis and exports them to ./data
#   MIDEN_NETWORK_TX_AUTH                  shared ntx-builder/sequencer secret; defaults below
#   RUST_LOG                               log level for the components; defaults to info
#   CARGO_TARGET_DIR                       where gen-genesis is built and looked up. The node
#                                          install ignores it, always using target/test-node/build

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

# `-f` as well as `-x`, because a directory is executable and would otherwise pass for the binary it
# is named after.
have_binary() { [ -f "$BIN/$1" ] && [ -x "$BIN/$1" ]; }

node_binaries_installed() {
    local metadata="$CACHE/install/.crates.toml"
    [ -f "$metadata" ] || return 1

    # `.crates.toml` records each install as `"<bin> <version> (<source>)"`.
    for bin in "${NODE_BINS[@]}"; do
        have_binary "$bin" || return 1
        if [ "$NODE_SOURCE" = "git" ]; then
            grep -F "\"$bin " "$metadata" | grep -Fq "#$NODE_REV)" || return 1
        else
            grep -Fq "\"$bin $NODE_VERSION (registry+" "$metadata" || return 1
        fi
    done
}

# Escape hatch for running against a node built from source. Testing an unreleased protocol change
# means the node has to carry it too: a guarded account assembled from a patched miden-standards
# calls procedure roots a published node cannot resolve, and it rejects the transaction at
# `submit_proven_transaction` with "procedure with root digest ... could not be found".
# `[patch.crates-io]` cannot fix that here, because a registry or git `cargo install` ignores the
# patch section entirely -- the binaries have to be `cargo install --path`-ed from a patched
# checkout. Those record a `path+file://` source, which the pin check above (correctly) rejects, so
# without this the next run silently reinstalls the published node over them.
#
# A binary this cannot find is fatal rather than a fall-through to the install below: reinstalling
# the pin is never what this variable asked for, and doing it would overwrite the very binaries the
# variable exists to protect.
use_installed_binaries() {
    local missing=""
    for bin in "${NODE_BINS[@]}"; do
        have_binary "$bin" || missing="$missing $bin"
    done
    if [ -n "$missing" ]; then
        echo "error: MIDEN_TEST_NODE_USE_INSTALLED is set, but these are not executable in" \
            "$BIN:$missing" >&2
        echo "       build them from a node checkout first, e.g." >&2
        echo "       cargo install --path <node-checkout>/bin/validator --root $CACHE/install" >&2
        echo "       or unset the variable to install the $NODE_DESC pin" >&2
        exit 1
    fi

    echo "==> MIDEN_TEST_NODE_USE_INSTALLED: using $BIN as-is, not checking it against $NODE_DESC"
    # Cargo's inventory of the packages it tracks in this install root. It is bookkeeping, not
    # provenance: it omits a binary placed here by other means, and it still describes one that was
    # installed and later overwritten. So it is labelled as what it is rather than presented as the
    # provenance of what is about to run, and the header is skipped when it lists nothing.
    local listing=""
    if [ -f "$CACHE/install/.crates.toml" ]; then
        listing="$(sed -n 's/^"\([A-Za-z0-9_-]*\) \([^ ]*\) (\([a-z-]*\)+.*/        \1 \2 (\3)/p' \
            "$CACHE/install/.crates.toml")"
    fi
    if [ -n "$listing" ]; then
        echo "    cargo last installed here:"
        printf '%s\n' "$listing"
    fi
}

if [ -n "${MIDEN_TEST_NODE_USE_INSTALLED:-}" ]; then
    use_installed_binaries
elif node_binaries_installed; then
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
    if [ -n "${MIDEN_TEST_NODE_USE_INSTALLED:-}" ]; then
        echo "==> install-only: using the binaries already in $BIN; nothing was installed"
    else
        echo "==> install-only: node binaries ready in $BIN"
    fi
    exit 0
fi

if (exec 3<>"/dev/tcp/${RPC%:*}/${RPC##*:}") 2>/dev/null; then
    exec 3>&- 3<&-
    echo "error: something is already listening on $RPC; run stop-test-node.sh first" >&2
    exit 1
fi

echo "==> building gen-genesis"
cargo build --release -p test-node-genesis --bin gen-genesis

# Ask gen-genesis to check the environment before the cleanup below destroys the previous chain. It
# applies the same rules when it runs for real, so this only moves the diagnosis earlier: a typo in
# MIDEN_TEST_NODE_VERIFICATION_BASE_FEE costs nothing instead of costing a chain that was working.
"$GEN_GENESIS" --check-env

echo "==> generating genesis + bootstrapping"
AGGLAYER_MACS=(bridge_admin.mac ger_manager.mac bridge.mac agglayer_faucet.mac)
# ./data outlives a run, unlike $DATA, so everything this script exports there describes the chain
# about to be wiped. Drop all of it, and drop it before $DATA: a removal that fails then stops the
# run with the old chain still whole, rather than with $DATA already gone. The export block after
# bootstrap explains why nothing is written back until the new chain exists.
mkdir -p "$ROOT/data"
rm -f "$ROOT/data/account.mac"
rm -f "$ROOT/data"/wallet_*.mac
for mac in "${AGGLAYER_MACS[@]}"; do
    rm -f "$ROOT/data/$mac"
done
rm -rf "$DATA"
# Each component opens its SQLite DB directly under its data dir and does not create it.
mkdir -p "$LOG_DIR" "$DATA/validator" "$DATA/node" "$DATA/ntx-builder"
"$GEN_GENESIS" "$DATA/genesis-config"

# The validator's signing key and the set's shared transaction encryption key are passed on the
# command line. The genesis header commits to the signing key's public half, so the key-pair has to
# exist before the genesis block is built. A fresh pair per run is fine because `$DATA` is wiped
# above, so no earlier chain state depends on the previous one.
VALIDATOR_KEYS="$("$BIN/miden-validator" keygen)"
validator_key() {
    printf '%s\n' "$VALIDATOR_KEYS" | awk -v field="$1:" '$1 == field { print $2; exit }'
}
SIGNING_KEY="$(validator_key signing-key)"
VALIDATOR_PUBLIC_KEY="$(validator_key validator-key)"
ENCRYPTION_KEY="$(validator_key encryption-key)"
for key in SIGNING_KEY VALIDATOR_PUBLIC_KEY ENCRYPTION_KEY; do
    [ -n "${!key}" ] || {
        echo "error: miden-validator keygen did not report a $key" >&2
        exit 1
    }
done

# Chained on `&&` rather than run as plain statements because the group sits on the left of an
# `||`, which suspends `set -e` inside it; without the chaining a failed genesis would go on to
# bootstrap components against a block that was never built. The whole group's output goes to a
# file, so the failure branch is the only thing that can tell an operator where to look.
{
    # Genesis generation is separate from bootstrap: `genesis` builds the block once, then every
    # component seeds its database from the resulting file.
    "$BIN/miden-validator" genesis --genesis-block-directory "$DATA/genesis" \
        --accounts-directory "$DATA/accounts" --config "$DATA/genesis-config/genesis.toml" \
        --validator.key "$VALIDATOR_PUBLIC_KEY" &&
    "$BIN/miden-validator" bootstrap --data-directory "$DATA/validator" \
        --genesis "$DATA/genesis/genesis.dat" &&
    "$BIN/miden-node" bootstrap --data-directory "$DATA/node" \
        --genesis "$DATA/genesis/genesis.dat" &&
    "$BIN/miden-ntx-builder" bootstrap --data-directory "$DATA/ntx-builder" \
        --genesis "$DATA/genesis/genesis.dat"
} >"$LOG_DIR/bootstrap.log" 2>&1 || {
    STATUS=$?
    echo "error: node genesis/bootstrap failed (exit $STATUS); see $LOG_DIR/bootstrap.log" >&2
    # `set -e` is live on this side of the `||`, so nothing here may fail: a log the redirection
    # never managed to create is skipped by the `-s` test, and a tail that fails anyway (an
    # unreadable log) must not kill the script before it reports the status it came here to report.
    if [ -s "$LOG_DIR/bootstrap.log" ]; then
        echo "       last 40 lines:" >&2
        tail -n 40 "$LOG_DIR/bootstrap.log" >&2 || true
    fi
    exit "$STATUS"
}

# With a non-zero MIDEN_TEST_NODE_VERIFICATION_BASE_FEE, genesis carries MIDEN-funded funder
# wallets. The node generates them, so only it knows their ids and keys; it writes them to the
# accounts directory above.
#
# Counting the manifest's wallet entries pins the expectation to what the node was actually asked
# for, so a rename of the files it writes is caught here; a fee-charging chain whose funders never
# reached ./data would otherwise surface as an unexplained failure once a test went looking.
# grep exits 1 on no matches, which is a legitimate count of zero; any other status is a failure.
EXPECTED_FUNDERS="$(grep -c '^\[\[wallet\]\]' "$DATA/genesis-config/genesis.toml" || [ $? -eq 1 ])"
FUNDERS=0
for mac in "$DATA/accounts"/wallet_*.mac; do
    if [ -f "$mac" ]; then
        FUNDERS=$((FUNDERS + 1))
    fi
done
if [ "$FUNDERS" -ne "$EXPECTED_FUNDERS" ]; then
    echo "error: genesis.toml declares $EXPECTED_FUNDERS funder wallet(s) but $FUNDERS matched" \
        "wallet_*.mac in $DATA/accounts" >&2
    # The glob encodes the pinned node's naming, so a node that names them otherwise lands here with
    # a count of zero. Show what it did write rather than only what failed to match.
    echo "       that directory holds:" >&2
    ls -1 "$DATA/accounts" >&2 || true
    exit 1
fi

# Every export into ./data happens here, past everything that can fail on the way to a chain: it is
# built, and the funder set is the one genesis asked for. So a run that fails between the ./data
# clear above and this point leaves ./data exactly as that clear left it -- empty of accounts for a
# chain that was never built -- rather than holding a subset written before the failure. A `cp`
# failing inside this block does leave a partial set; staging and publishing atomically is not
# worth it for a disk that filled at the last step of an otherwise successful bootstrap.
cp "$DATA/genesis-config/tst_faucet.mac" "$ROOT/data/account.mac"
# With AGGLAYER_GENESIS set, gen-genesis also emits the agglayer account files; expose them under
# ./data so tests can load them via AGGLAYER_ACCOUNTS_DIR=./data.
for mac in "${AGGLAYER_MACS[@]}"; do
    if [ -f "$DATA/genesis-config/$mac" ]; then
        cp "$DATA/genesis-config/$mac" "$ROOT/data/$mac"
    fi
done
for mac in "$DATA/accounts"/wallet_*.mac; do
    if [ -f "$mac" ]; then
        cp "$mac" "$ROOT/data/"
    fi
done
if [ "$FUNDERS" -gt 0 ]; then
    echo "==> exported $FUNDERS funder wallet(s) to ./data"
fi

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
# The storage-key files are the node repo's checked-in insecure development fixtures
# (scripts/testdata/insecure-golden-storage-key), vendored here because the validator requires
# threshold storage-key material to start and ships no generator for it.
STORAGE_KEY_DIR="$ROOT/scripts/testdata/insecure-golden-storage-key"
start validator   "$BIN/miden-validator" start --listen "$VALIDATOR" --data-directory "$DATA/validator" \
    --signing-key.hex "$SIGNING_KEY" \
    --encryption-key.hex "$ENCRYPTION_KEY" \
    --storage-key.epoch "0909090909090909090909090909090909090909090909090909090909090909" \
    --storage-key.setup-context "$STORAGE_KEY_DIR/setup-context.wire" \
    --storage-key.public-key-set "$STORAGE_KEY_DIR/public-key-set.wire" \
    --storage-key.secret-share "$STORAGE_KEY_DIR/secret-share.wire"
# Let the validator bind before the sequencer starts producing blocks against it.
sleep 2
start sequencer   "$BIN/miden-node" sequencer --rpc.listen "$RPC" --data-directory "$DATA/node" \
    --validator.url "http://$VALIDATOR" --ntx-builder.url "http://$NTX" \
    --rpc.network-tx-auth-header-value "$NETWORK_TX_AUTH" \
    --block.interval 3s --batch.interval 1s
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
