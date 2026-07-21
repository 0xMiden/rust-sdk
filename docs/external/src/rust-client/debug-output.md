---
title: MASM Debug Output
sidebar_position: 8
---

# MASM Debug Output

To print VM state to standard output while a script runs, call the `miden::core::debug` procedures
from your Miden Assembly. The client's transaction executor prints their output by default, so there
is nothing to enable. Debugging is opt-in per script: output is produced only where a script calls a
print procedure.

## Procedures

| Procedure | Prints |
| --------- | ------ |
| `print_stack` | the entire operand stack |
| `print_mem` | memory over a `[start, end)` range (consumes the two range arguments, at most 1024 addresses) |
| `print_mem_addr` | the memory cell at a single address |
| `print_mem_all` | every initialized memory cell of the current context |

The operand-stack and memory printers are enabled by default. The advice-stack and advice-map
printers are not, because they can expose witness data.

## Example

Compile and execute a script that prints the operand stack:

```rust
let tx_script = client.code_builder().compile_tx_script(
    "
    use miden::core::debug
    use miden::core::sys

    @transaction_script
    pub proc main
        push.1.2.3
        exec.debug::print_stack
        drop drop drop
        exec.sys::truncate_stack
    end
    ",
)?;

client
    .execute_program(account_id, tx_script, AdviceInputs::default(), BTreeMap::new())
    .await?;
```

Executing it prints the operand stack to standard output (the step count includes the transaction
prologue that runs before the script):

```text
Stack state before step 2506:
├──  0: 3
├──  1: 2
├──  2: 1
└── (rest of the stack)
```

:::note
Under tests, pass `--no-capture` (`cargo nextest`, used by `make test`) or `--nocapture`
(`cargo test`) to see the output.
:::
