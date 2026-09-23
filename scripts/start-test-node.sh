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
#   --print-rev      print the cache key naming the node binaries this script installs, and exit
#
# Env vars:
#   MIDEN_VERIFICATION_BASE_FEE  genesis `verification_base_fee` (default 500; 0 disables fees)
#   MIDEN_NUM_FUNDER_WALLETS     number of funder wallets a fee-charging genesis declares
#   MIDEN_BATCH_BUILDER_WALLET   account that receives the batch builder's fees
#   MIDEN_ACCOUNT_ALLOWLIST      1 enforces the account allowlist, binds the administration
#                                API the tests create invitation codes through, and starts the
#                                funding service that pays each account which registers.
#                                0 (default) allows unrestricted account creation

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
# Private administration API of the sequencer, bound only when allowlist enforcement is on. It is
# the only way to add an invitation code to the account allowlist, because no genesis option and
# no bootstrap subcommand writes one. The allowlist tests create their codes through it.
ADMIN="127.0.0.1:50100"
PROVER_PORT=50051
PROVER="127.0.0.1:$PROVER_PORT"
# HTTP API of the funding service, started only when allowlist enforcement is on. The sequencer
# calls it for every account that registers. Matches the port of the node's own compose file.
FUNDING="127.0.0.1:50401"
# Native asset base units the sequencer asks the funding service to pay each registered account.
# Matches `FUNDING_AMOUNT` in `bin/integration-tests/src/fee_funding.rs`, which covers the fees of
# every transaction a test runs against the account.
FUNDING_AMOUNT=10000000
# How long a single network transaction proof may take. The prover enforces it server-side and the
# ntx-builder waits that long for the response. Shared so the two cannot drift apart: if the
# ntx-builder waited less, it would abandon a request the prover is still working on, re-queue the
# same proof behind it, and repeat until the note is dropped.
PROVER_TIMEOUT=300s
# Shared secret authorizing the ntx-builder to submit network transactions; the sequencer rejects
# them unless both sides agree on it.
NETWORK_TX_AUTH="${MIDEN_NETWORK_TX_AUTH:-miden-client-testing-ntx-secret}"
# Genesis `verification_base_fee`. Every transaction pays out of its own account's vault, as on a
# real chain. At 0 fees are never charged.
VERIFICATION_BASE_FEE="${MIDEN_VERIFICATION_BASE_FEE:-500}"
# Account that receives the batch builder's fees in a P2ID note. The sequencer requires the value
# but never reads the account, so this is the same placeholder id the node repo uses for local
# runs. No test consumes the fee notes.
BATCH_BUILDER_WALLET="${MIDEN_BATCH_BUILDER_WALLET:-0xcc0000000000dd010000ee000000ff}"
# Account allowlist enforcement. The node enforces it by default, which rejects every account
# creation the integration tests do, so the default here is off and callers opt in.
ACCOUNT_ALLOWLIST="${MIDEN_ACCOUNT_ALLOWLIST:-0}"

NODE_BINS=(miden-validator miden-node miden-ntx-builder miden-remote-prover miden-funding-service)

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

# CI keys its cache of the installed binaries on this. It covers the set of binaries as well as the
# source. A binary added to NODE_BINS with an unchanged source otherwise hits a cache entry that
# does not contain it, and CI never refreshes that entry because it saves only on a miss.
NODE_CACHE_KEY="$NODE_REV-bins.$(printf '%s\n' "${NODE_BINS[@]}" | cksum | cut -d' ' -f1)"

if [ "$MODE" = "print-rev" ]; then
    echo "$NODE_CACHE_KEY"
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

# The allowlist tests deploy each registered account with the note the funding service paid it. A
# fee-free genesis declares no wallet for the service to pay from, so the two cannot be combined.
if [ "$ACCOUNT_ALLOWLIST" = "1" ] && [ "$VERIFICATION_BASE_FEE" = "0" ]; then
    echo "error: MIDEN_ACCOUNT_ALLOWLIST=1 needs a fee-charging chain for the funding service." \
        "Unset MIDEN_VERIFICATION_BASE_FEE or set it above 0" >&2
    exit 1
fi

echo "==> building gen-genesis"
cargo build --release -p test-node-genesis --bin gen-genesis

echo "==> generating genesis + bootstrapping (verification_base_fee = $VERIFICATION_BASE_FEE)"
rm -rf "$DATA"
# Each component opens its SQLite DB directly under its data dir and does not create it.
mkdir -p "$LOG_DIR" "$DATA/validator" "$DATA/node" "$DATA/ntx-builder"
MIDEN_VERIFICATION_BASE_FEE="$VERIFICATION_BASE_FEE" "$GEN_GENESIS" "$DATA/genesis-config"
# Cleared up front so a fee-free run cannot leave a previous run's funders behind, and re-exposed
# below once `miden-validator genesis` has generated them.
rm -rf "$ROOT/data/funders"
mkdir -p "$ROOT/data"
cp "$DATA/genesis-config/tst_faucet.mac" "$ROOT/data/account.mac"
# Expose the agglayer accounts under ./data, where the tests read them via AGGLAYER_ACCOUNTS_DIR.
for mac in bridge_admin.mac ger_manager.mac bridge.mac agglayer_faucet.mac \
           native_faucet.mac faucet_operator.mac; do
    cp "$DATA/genesis-config/$mac" "$ROOT/data/$mac"
done

# The validator's signing key and the set's shared transaction encryption key are passed on the
# command line. The genesis header commits to the signing key's public half, so the keys have to
# exist before the genesis block is built. These are hardcoded INSECURE test-only fixtures (one
# `miden-validator keygen` output, so the signing and validator keys pair up), like the
# storage-key material below. A fixed key is safe here because `$DATA` is wiped above, so no
# earlier chain state depends on it. If a node bump changes the key format, regenerate all three
# with `miden-validator keygen`.
SIGNING_KEY="9cbcf0fc18b2a4afeff56ef43ad96af92e804fae64615c9802cff2a182e9cae2"
VALIDATOR_PUBLIC_KEY="020c06515b355a62133ae98e53e4b5d3e6ee9ff60ce620a436780e4e308a3ff3e9"
ENCRYPTION_KEY="9964dbb2590adeb415d3291b64a0a9991fbcac5adacb05ee17efee5296d081d7"

{
    # Genesis generation is separate from bootstrap: `genesis` builds the block once, then every
    # component seeds its database from the resulting file. The native faucet and the funding
    # account are required inputs with their own flags; the fee and the timestamp are genesis
    # parameters rather than accounts, so they are passed here instead of through the fixtures.
    "$BIN/miden-validator" genesis --genesis-block-directory "$DATA/genesis" \
        --accounts-directory "$DATA/accounts" \
        --accounts-config "$DATA/genesis-config/accounts.toml" \
        --native-faucet "$DATA/genesis-config/native_faucet.mac" \
        --funding-account "$DATA/genesis-config/funding_account.mac" \
        --verification-base-fee "$VERIFICATION_BASE_FEE" \
        --timestamp "$(date +%s)" \
        --validator.key "$VALIDATOR_PUBLIC_KEY"
    "$BIN/miden-validator" bootstrap --data-directory "$DATA/validator" \
        --genesis "$DATA/genesis/genesis.dat"
    "$BIN/miden-node" bootstrap --data-directory "$DATA/node" --genesis "$DATA/genesis/genesis.dat"
    "$BIN/miden-ntx-builder" bootstrap --data-directory "$DATA/ntx-builder" \
        --genesis "$DATA/genesis/genesis.dat"
} >"$LOG_DIR/bootstrap.log" 2>&1
NATIVE_FAUCET_ID="$(sed -n 's/^Native faucet account id: //p' "$LOG_DIR/bootstrap.log")"
echo "==> native faucet $NATIVE_FAUCET_ID, operator wallet in $ROOT/data/faucet_operator.mac"

# Expose the wallets the node generated from the genesis `[[wallet]]` entries under ./data/funders,
# where the tests read them via MIDEN_FUNDER_ACCOUNTS_DIR. A fee-free genesis declares none.
if compgen -G "$DATA/accounts/wallet_*.mac" >/dev/null; then
    mkdir -p "$ROOT/data/funders"
    cp "$DATA"/accounts/wallet_*.mac "$ROOT/data/funders/"
    echo "==> exposed $(ls "$ROOT/data/funders" | wc -l | tr -d ' ') funder wallets in $ROOT/data/funders"
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

# The fee collector deployment and the sequencer both need the validator.
echo "==> waiting for validator on $VALIDATOR"
VALIDATOR_READY=""
for _ in $(seq 1 30); do
    if (exec 3<>"/dev/tcp/${VALIDATOR%:*}/${VALIDATOR##*:}") 2>/dev/null; then
        exec 3>&- 3<&-
        VALIDATOR_READY=1
        break
    fi
    sleep 1
done
if [ -z "$VALIDATOR_READY" ]; then
    echo "error: validator did not become ready within 30s; see $LOG_DIR" >&2
    exit 1
fi

# The sequencer does not start until the batch builder's fee collector account exists in its data
# directory and is deployed on chain. The deployment proves one block locally and pays no fee.
echo "==> creating and deploying the fee collector account"
if ! {
    "$BIN/miden-node" fee-collector create --data-directory "$DATA/node" &&
    "$BIN/miden-node" fee-collector deploy --data-directory "$DATA/node" \
        --validator.url "http://$VALIDATOR"
} >"$LOG_DIR/fee-collector.log" 2>&1; then
    echo "error: fee collector deployment failed; see $LOG_DIR/fee-collector.log" >&2
    tail -n 20 "$LOG_DIR/fee-collector.log" >&2
    exit 1
fi
# The node enforces the account allowlist unless told otherwise, and enforcement rejects every
# account-creating submission from an unregistered account. Only the allowlist tests want that.
# The admin API is bound only alongside enforcement, because seeding the invitation codes is the
# one thing it is needed for, and binding it otherwise would only add a port that can clash.
#
# With enforcement on, the sequencer also asks the funding service to pay every account that
# registers, and answers the registration only once that note is committed. The service starts
# after the sequencer, because it reads its account from the RPC. That order is safe, because the
# sequencer calls the service only when an account registers.
# A registration waits for the funding note to commit, which takes longer than the 10s default
# request timeout of the RPC server.
SEQUENCER_ALLOWLIST_ARGS=()
if [ "$ACCOUNT_ALLOWLIST" = "1" ]; then
    SEQUENCER_ALLOWLIST_ARGS+=(--admin.listen "$ADMIN")
    SEQUENCER_ALLOWLIST_ARGS+=(--funding-service.url "http://$FUNDING")
    SEQUENCER_ALLOWLIST_ARGS+=(--funding-service.amount "$FUNDING_AMOUNT")
    SEQUENCER_ALLOWLIST_ARGS+=(--rpc.grpc.timeout 60s)
else
    SEQUENCER_ALLOWLIST_ARGS=(--disable-account-allowlist)
fi

start sequencer   "$BIN/miden-node" sequencer --rpc.listen "$RPC" --data-directory "$DATA/node" \
    --validator.url "http://$VALIDATOR" --ntx-builder.url "http://$NTX" \
    --rpc.network-tx-auth-header-value "$NETWORK_TX_AUTH" \
    --batch.builder.wallet-account-id "$BATCH_BUILDER_WALLET" \
    ${SEQUENCER_ALLOWLIST_ARGS[@]+"${SEQUENCER_ALLOWLIST_ARGS[@]}"} \
    --block.interval 3s --batch.interval 1s
# A network transaction's proof runs well past the prover's 60s default on a shared CI runner, and
# the default capacity of 1 rejects the ntx-builder's retry outright, so it never converges.
start prover      "$BIN/miden-remote-prover" --kind=transaction --port="$PROVER_PORT" \
    --timeout "$PROVER_TIMEOUT" --capacity 8
# Let the sequencer bind its RPC before the ntx-builder dials it.
sleep 2
# The ntx-builder's own default of 10s is shorter than the heaviest proofs take on CI, so it is
# given the prover's full budget (see PROVER_TIMEOUT).
start ntx-builder "$BIN/miden-ntx-builder" start --listen "$NTX" --rpc.url "http://$RPC" \
    --rpc.auth-header-value "$NETWORK_TX_AUTH" --tx-prover.url "http://$PROVER" \
    --tx-prover.timeout "$PROVER_TIMEOUT" \
    --max-cycles "$((1 << 18))" \
    --data-directory "$DATA/ntx-builder"
# The funding service pays out of the `funding_service` wallet that `miden-validator genesis` wrote,
# and trusts the validator key the validator was started with. A request blocks until the note is
# committed, so its HTTP timeout covers a proof plus the expiration window.
if [ "$ACCOUNT_ALLOWLIST" = "1" ]; then
    start funding-service "$BIN/miden-funding-service" start --listen "$FUNDING" \
        --rpc.url "http://$RPC" \
        --tx-prover.url "http://$PROVER" \
        --tx-prover.timeout "$PROVER_TIMEOUT" \
        --account-file "$DATA/accounts/funding_service.mac" \
        --validator-signing-public-key "$VALIDATOR_PUBLIC_KEY" \
        --http.timeout "$PROVER_TIMEOUT" \
        --poll-interval 250ms
fi

# Waits for the sequencer administration API to accept connections. The allowlist tests create
# their invitation codes through it, so `--background` must not return before it is up. The API is
# served by its own task, which may bind slightly after the RPC does.
wait_for_admin_api() {
    for _ in $(seq 1 30); do
        if (exec 3<>"/dev/tcp/${ADMIN%:*}/${ADMIN##*:}") 2>/dev/null; then
            exec 3>&- 3<&-
            return 0
        fi
        sleep 1
    done

    echo "error: admin API did not become ready on $ADMIN within 30s; see $LOG_DIR" >&2
    return 1
}

# Waits for the funding service to serve HTTP. A registration fails when the service is not up, so
# `--background` must not return before it is. `GET /status` answers while the service still
# synchronizes. Asking for the route rather than the socket also catches a process that bound the
# port but cannot serve.
wait_for_funding_service() {
    for _ in $(seq 1 60); do
        if curl -sfo /dev/null "http://$FUNDING/status"; then
            return 0
        fi
        check_components_alive || return 1
        sleep 1
    done

    echo "error: funding service did not become ready on $FUNDING within 60s; see $LOG_DIR" >&2
    return 1
}

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

if [ "$ACCOUNT_ALLOWLIST" = "1" ]; then
    wait_for_admin_api
    wait_for_funding_service
    echo "==> account allowlist enforcement is ON (admin API on http://$ADMIN," \
        "registrations funded by http://$FUNDING)"
fi

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
