---
title: DAP Debugging
sidebar_position: 7
---

# DAP Debugging

The Miden client supports interactive debugging via the [Debug Adapter Protocol (DAP)](https://microsoft.github.io/debug-adapter-protocol/). You can debug both raw Miden Assembly scripts and Rust programs compiled to Miden via `midenc`. This lets you step through execution, set breakpoints, and inspect stack/memory state using any DAP-compatible client (e.g. VS Code, the `miden-debug` TUI).

:::warning
Interactive DAP debugging is **temporarily unavailable in this release.** The `miden-debug` 0.9
executor drives execution from a compiled package, but the VM's `ProgramExecutor` interface the
client integrates against provides only a bare program, and there is no lossless conversion between
the two. Starting a DAP session (`--start-debug-adapter`) therefore fails at execution time with an
explicit "DAP debugging is not supported with the current miden-debug backend" error. The `dap`
feature and CLI flag remain so the surface is preserved; support will return once the upstream
executor accepts a program directly.
:::

## Feature flags

Two feature flags control debugging support:

| Feature | Crate | What it enables |
| ------- | ----- | --------------- |
| `dap` | `miden-client`, `miden-client-cli` | Compiles in DAP support (`execute_program_with_dap`, `--start-debug-adapter` CLI flag). |
| `testing` | `miden-client-cli` | Enables test-only CLI helpers such as offline account creation. Not available in production builds. |

### Building with features

```bash
# Build the CLI with DAP support
cargo build -p miden-client-cli --features dap

# Build with DAP and test-only offline helpers
cargo build -p miden-client-cli --features dap,testing

# Build without DAP
cargo build -p miden-client-cli
```

Include the `dap` feature to use `--start-debug-adapter`.

## Quick Start

### 1. Create an account

With a running node:

```bash
miden-client init
miden-client new-wallet
miden-client sync
```

Or without a node (requires `testing` feature):

```bash
miden-client init
miden-client new-wallet --offline
```

### 2. Write a test script

Create a file `test_debug.masm`:

```
begin
  push.1.2
  add
  push.3
  mul
end
```

### 3. Start the DAP server

```bash
miden-client exec \
  --script-path test_debug.masm \
  --start-debug-adapter 127.0.0.1:4711
```

The client will compile the script, start a debug adapter server, and wait for a DAP client to
connect before executing.

### 4. Connect a debugger

In a separate terminal, connect the `miden-debug` TUI:

```bash
miden-debug --dap-connect 127.0.0.1:4711
```

You can now step through execution, inspect the stack, and set breakpoints.


## How it works

When `--start-debug-adapter` is passed:

1. The client compiles the transaction script from its filesystem path so source locations point at
   the real file.
2. The transaction executor runs with the DAP program executor, which binds a TCP listener on the
   specified address and waits for a DAP client connection.
3. Once connected, the DAP client controls execution: continue, step, breakpoints, and state
   inspection.
4. If the DAP client requests a restart, the client refreshes the cached source file, recompiles the
   script from disk, and starts a new debug session.
