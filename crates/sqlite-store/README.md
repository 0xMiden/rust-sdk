# SQLite Store

SQLite-backed `Store` implementation for the Miden client. This crate provides a production‑ready
persistence layer for std environments using SQLite (via `rusqlite`).

- Persists accounts, notes, transactions, block headers, MMR nodes, and the account SMT forest
- Atomic updates on transaction and state sync paths
- WAL journaling and bundled SQLite for reproducible builds

## Quick Start

Add to `Cargo.toml`:

```toml
miden-client              = { version = "0.16.0-alpha.1" }
miden-client-sqlite-store = { version = "0.16.0-alpha.1" }
```

## License
This project is licensed under the MIT License. See the [LICENSE](../../LICENSE) file for details.
