# SQLite Store

SQLite-backed `Store` implementation for the Miden client. This crate provides a production‑ready
persistence layer for std environments using SQLite (via `rusqlite`) with a small in‑memory
`AccountSmtForest` cache for fast proof queries.

- Persists accounts, notes, transactions, block headers, and MMR nodes
- Atomic updates on transaction and state sync paths
- Connection pooling (Deadpool) and bundled SQLite for reproducible builds

## Quick Start

Add to `Cargo.toml`:

```toml
miden-client              = { version = "0.16.0-alpha.1" }
miden-client-sqlite-store = { version = "0.16.0-alpha.1" }
```

## License
This project is licensed under the MIT License. See the [LICENSE](../../LICENSE) file for details.
