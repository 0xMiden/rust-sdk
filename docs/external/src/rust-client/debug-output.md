---
title: MASM Debug Output
sidebar_position: 8
---

# MASM Debug Output

The core library's `miden::core::debug` module (`print_stack`, `print_mem`, `print_mem_addr`,
`print_mem_all`, `print_adv_stack`, `print_adv_stack_all`, `print_adv_map_all`,
`print_adv_map_item`) prints VM state to the client's standard output while a script runs. This is a
lightweight alternative to [interactive DAP debugging](./debugging.md).

These are ordinary `emit` events handled by the host, so they carry no MAST or decorator cost and
they print whenever invoked. There is no debug-mode toggle to enable. Output goes to the client's
standard output, not to `tracing`/`RUST_LOG` and not to the node logs.

The advice-stack and advice-map printers are **not** registered by default, because the data they
print can include witness material. Programs calling them execute normally but produce no output.

## Example

Compile and execute a script that prints the operand stack:

```rust
let tx_script = client.code_builder().compile_tx_script(
    "
    use.miden::core::debug

    begin
        push.1.2.3
        exec.debug::print_stack
        drop drop drop
    end
    ",
)?;

client
    .execute_program(account_id, tx_script, AdviceInputs::default(), BTreeMap::new())
    .await?;
```

Executing it prints the operand stack to the client's standard output (the step count includes the
transaction prologue that runs before the script):

```text
Stack state before step 2419:
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
default-constructed per execution.

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

This is what `@miden-sdk/miden-sdk` uses to surface debug output in the browser console.

:::warning
Unlike the default host, the routed sink also receives the advice-stack and advice-map printers.
Use it only where that witness data is already the caller's own.
:::
