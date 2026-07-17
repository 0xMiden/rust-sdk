---
title: MASM Debug Output
sidebar_position: 8
---

# MASM Debug Output

Miden Assembly's `debug.*` instructions (`debug.stack`, `debug.stack.<n>`, `debug.mem`,
`debug.local.<a>.<b>`, `debug.adv_stack.<n>` — see the
[assembly reference](https://github.com/0xMiden/miden-vm/blob/next/docs/src/user_docs/assembly/debugging.md))
print VM state to the client's standard output while a script runs. This is a lightweight
alternative to [interactive DAP debugging](./debugging.md).

They only print when the client that **executes** the script is in debug mode — enable it with
`.in_debug_mode(DebugMode::Enabled)` on the builder, or run the CLI with `--debug` (or
`MIDEN_DEBUG=true`). Compilation is unaffected; the decorators are always retained in freshly
compiled scripts. Output goes to the client's standard output (not `tracing`/`RUST_LOG`, not the
node logs).

## Example

Compile and execute a script containing `debug.stack` with a debug-mode client:

```rust
// Client built with `.in_debug_mode(DebugMode::Enabled)`.
let tx_script = client.code_builder().compile_tx_script(
    "
    begin
        push.1.2.3
        debug.stack.3
        drop drop drop
    end
    ",
)?;

client
    .execute_program(account_id, tx_script, AdviceInputs::default(), BTreeMap::new())
    .await?;
```

Executing it prints the top three stack elements to the client's standard output (the step count
includes the transaction prologue that runs before the script):

```text
Stack state in interval [0, 2] before step 2419:
├── 0: 3
├── 1: 2
├── 2: 1
└── (16 more items)
```

:::note
Under tests, pass `--no-capture` (`cargo nextest`, used by `make test`) or `--nocapture`
(`cargo test`) to see the output.
:::

## Routing debug output to a custom sink

Standard output is a no-op on some targets (notably `wasm32-unknown-unknown`, which has no stdout).
Enable the `debug-output` feature and run execution through `Client::execute_program_with_debugger`
(or `execute_transaction_with_debugger`), parameterized by your own `fmt::Write` sink `W`. `W` is
default-constructed per execution; output is still only produced when the client is in debug mode.

```rust
// `ConsoleWriter: fmt::Write + Default` — here it forwards to the browser console.
client
    .execute_program_with_debugger::<ConsoleWriter>(
        account_id,
        tx_script,
        AdviceInputs::default(),
        BTreeMap::new(),
    )
    .await?;
```

This is what `@miden-sdk/miden-sdk` uses to surface `debug.*` output in the browser console.
