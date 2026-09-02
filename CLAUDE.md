# Claude Code Guidelines for miden-client

## Code Comments

**Never reference the previous state of the code in comments.** No "previous version did X", "used to do Y", "changed from Z", "the old pattern was", etc. Comments must describe the CURRENT code's rationale — the invariant, constraint, or subtle correctness reason that isn't obvious from the code itself. If context about a change matters, put it in the PR description or commit message, not in the source. The source gets read long after the diff stops being relevant, and stale "previously" comments become actively misleading.

## Important: Always Format Code

**After making any code changes, always run `make format` before committing.** This ensures consistent formatting across Rust, TypeScript, and other files.

## Repository Overview

Miden Client is the official client library for interacting with the Miden rollup. It provides APIs for managing accounts, notes, and transactions on the Miden network.

**Key crates:**
- `rust-client` - Core client library (no_std compatible)
- `sqlite-store` - SQLite persistence backend
- `miden-cli` - Command-line interface

The browser-focused SDK (WASM/JavaScript wrapper, React hooks library, vite plugin, IndexedDB store) lives in a dedicated repository at [0xMiden/web-sdk](https://github.com/0xMiden/web-sdk).

## Build System

The project uses **Cargo** with a **Makefile** wrapper. Always prefer Make targets over raw cargo commands.

### Essential Commands

```bash
# Before submitting PRs - runs all lints
make lint

# Run unit tests
make test

# Run integration tests (requires test node)
make start-node-background
make integration-test
make stop-node

# Build everything
make build

# Install development tools (nextest, taplo, etc.)
make install-tools
```

## Testing

- **Unit tests:** `make test` (uses cargo-nextest)
- **Doc tests:** `make test-docs`
- **Integration tests:** `make integration-test` (requires running test node)

## Code Style

### Rust
- Edition 2024, MSRV 1.96
- Use section headers for major code sections:
  ```rust
  // SECTION NAME
  // ================================================================================================
  ```
- Rustdoc with 100-char line limit
- All public APIs must be documented
- Clippy warnings are errors in CI

## Architecture

### Storage Trait Pattern
The `Store` trait abstracts persistence. Implementation:
- `SqliteStore` - Native applications

### Client Structure
```rust
pub struct Client<AUTH> {
    // AUTH implements TransactionAuthenticator
    // Uses Arc-wrapped Store and RpcClient
}
```

### Key Directories
```
crates/
├── rust-client/src/
│   ├── account/      # Account management
│   ├── note/         # Note handling
│   ├── transaction/  # TX building/execution
│   ├── rpc/          # Node communication
│   ├── store/        # Persistence trait
│   └── sync/         # State synchronization
└── sqlite-store/src/ # SQLite implementation
```

## Git Workflow

- **Main branch:** `next`
- **Commit format:** `type(scope): description`
  - Types: `feat`, `fix`, `refactor`, `chore`, `docs`, `test`
  - Example: `fix(rust-client): handle store race condition`
- **Always update CHANGELOG.md** for user-visible changes
- **Rebase before PR** (avoid merge commits)

## Common Patterns

### Error Handling
```rust
// Use ClientError with proper context
return Err(ClientError::StoreError(err));

// ErrorHint provides user-friendly help messages
```

### Async Operations
```rust
// Most store operations are async
let header = self.store.get_block_header_by_num(block_num).await?;
```

## CI Checks

PRs must pass:
1. `make lint` - Format, clippy, typos, TOML
2. `make test` - Unit tests
3. `make test-docs` - Documentation tests
4. Integration tests (run automatically in CI)

## Feature Flags

- `std` - Standard library support
- `tonic` - gRPC client
- `testing` - Test utilities and mocks

## Debugging

### Test Node
```bash
make start-node-background  # Start
make stop-node              # Stop
```

## Dependencies

Key external dependencies:
- `miden-protocol`, `miden-standards`, `miden-tx` - Miden protocol crates
- `tokio` - Async runtime
- `rusqlite` - SQLite bindings

## Versioning

- All workspace crates share version set in the Cargo.toml
- Protocol version must match node version
