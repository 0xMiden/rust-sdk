#!/usr/bin/env bash
#
# Installs the Note Transport service with `cargo install`, at the transport release pinned in
# Cargo.lock.
#
# Modes:
#   (no args)     install the service unless the pinned release is already installed
#   --print-rev   print the pinned release tag (CI cache key) and exit

set -euo pipefail

MODE="install"
case "${1:-}" in
    --print-rev) MODE="print-rev" ;;
    "")          ;;
    *) echo "error: unknown argument '$1'" >&2; exit 2 ;;
esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_URL=${REPO_URL:-https://github.com/0xMiden/miden-note-transport}
BINARY_NAME=miden-note-transport-node-bin

# Resolve the transport release from the `miden-note-transport-proto-build` version locked for the
# client, so the service speaks the protocol the client was built against. Tracking the transport
# default branch instead would pull in releases built for a newer protocol and toolchain.
VERSION="$(awk -F'"' '/^name = "miden-note-transport-proto-build"$/ { getline; print $2; exit }' "$ROOT/Cargo.lock")"
[ -n "$VERSION" ] || {
    echo "error: no miden-note-transport-proto-build version in Cargo.lock" >&2
    exit 1
}
TAG="v$VERSION"

if [ "$MODE" = "print-rev" ]; then
    echo "$TAG"
    exit 0
fi

# `.crates.toml` records each install as `"<bin> <version> (<source>)"`. Matching the tag inside the
# source keeps a binary left over from another release from being reused.
METADATA="${CARGO_HOME:-$HOME/.cargo}/.crates.toml"
if [ -f "$METADATA" ] && grep -F "\"$BINARY_NAME " "$METADATA" | grep -Fq "?tag=$TAG#"; then
    echo "==> using installed note transport service ($TAG)"
    exit 0
fi

echo "==> installing note transport service ($TAG)"
cargo install --git "$REPO_URL" --tag "$TAG" --locked
