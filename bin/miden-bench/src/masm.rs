use std::fmt::Write;

use miden_client::keystore::FilesystemKeyStore;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Felt, Word};

use crate::benchmarks::transaction::{ReadOp, StorageSlotInfo};
use crate::generators::{SlotDescriptor, generate_reader_component_code};

// READ SCRIPT GENERATION
// ================================================================================================

/// Maximum read ops per code block to stay within the Miden parser's `u16::MAX` instruction
/// limit. Map entries generate 6 ops each (push key + call + 4 dropw). Using 7000 as the
/// conservative limit.
const MAX_OPS_PER_BLOCK: usize = 7_000;

/// Writes the MASM instructions for a single map entry read (push key, call reader, drop result).
fn write_read_op_instructions(script: &mut String, op: &ReadOp) {
    // Push key (4 felts)
    writeln!(script, "    push.{}", op.key.as_word().to_hex())
        .expect("write to string should not fail");

    // Call the account's reader procedure for this storage map slot.
    // Stack input: [KEY]
    // Stack output via call frame: [VALUE, pad(12)] = 16 elements
    writeln!(script, "    call.storage_reader::get_map_item_slot_{}", op.slot_idx)
        .expect("write to string should not fail");

    // Drop the result (we just want to measure read time)
    script.push_str("    dropw dropw dropw dropw\n");
}

/// Generates a MASM script that reads storage entries from the active account.
///
/// Uses `call` to invoke account reader procedures rather than directly `exec`-ing
/// kernel syscalls. The kernel's `authenticate_account_origin` requires the caller
/// to be an account procedure.
///
/// When the total number of read ops exceeds [`MAX_OPS_PER_BLOCK`], the script is
/// split into `repeat.1 ... end` blocks to stay within the Miden parser's per-block
/// instruction limit.
fn generate_storage_read_script(read_ops: &[ReadOp]) -> String {
    let mut script = String::from(
        "use bench_reader::storage_reader

@transaction_script
pub proc main
",
    );

    if read_ops.len() <= MAX_OPS_PER_BLOCK {
        for op in read_ops {
            write_read_op_instructions(&mut script, op);
        }
    } else {
        // Split into repeat.1 blocks to create new block scopes, each with its own
        // independent instruction limit. repeat.1 compiles to a single pass (no overhead).
        for chunk in read_ops.chunks(MAX_OPS_PER_BLOCK) {
            script.push_str("    repeat.1\n");
            for op in chunk {
                write_read_op_instructions(&mut script, op);
            }
            script.push_str("    end\n");
        }
    }

    script.push_str("end\n");
    script
}

/// Compiles and builds a transaction request for a chunk of read operations.
pub fn build_chunk_tx_request(
    client: &miden_client::Client<FilesystemKeyStore>,
    chunk: &[ReadOp],
    slot_infos: &[StorageSlotInfo],
) -> anyhow::Result<miden_client::transaction::TransactionRequest> {
    let script_code = generate_storage_read_script(chunk);

    let descriptors: Vec<SlotDescriptor> = slot_infos
        .iter()
        .map(|info| SlotDescriptor { name: info.name.clone(), is_map: true })
        .collect();
    let reader_code = generate_reader_component_code(&descriptors);

    let tx_script = client
        .code_builder()
        .with_linked_module("bench_reader::storage_reader", reader_code.as_str())?
        .compile_tx_script(&script_code)?;
    Ok(TransactionRequestBuilder::new().custom_script(tx_script).build()?)
}

// EXPANSION SCRIPT GENERATION
// ================================================================================================

/// Generates MASM code for an account component that can set items in multiple storage maps.
/// Creates a procedure `set_item_slot_N` for each slot that receives key/value from the stack.
pub fn generate_expansion_component_code(num_slots: usize) -> String {
    let mut code = String::new();

    for i in 0..num_slots {
        let slot_name = format!("miden::bench::map_slot_{i}");
        write!(
            code,
            r#"const MAP_SLOT_{i} = word("{slot_name}")

# Sets an item in storage slot {i}.
# Stack input:  [KEY, VALUE, ...]
# Stack output: [...]
@account_procedure
pub proc set_item_slot_{i}
    push.MAP_SLOT_{i}[0..2]
    # Stack: [slot_suffix, slot_prefix, KEY, VALUE, ...]

    exec.::miden::protocol::native_account::set_map_item
    # Stack: [OLD_VALUE, ...]

    dropw
end

"#
        )
        .expect("writing to String should not fail");
    }

    code
}

/// Generates MASM transaction script code that writes entries into a single storage map slot.
pub fn generate_expansion_tx_script(slot_idx: usize, entries: &[([Felt; 4], [Felt; 4])]) -> String {
    let mut script =
        String::from("use expander::storage_expander\n\n@transaction_script\npub proc main\n");
    let procedure_name = format!("set_item_slot_{slot_idx}");

    for (key, value) in entries {
        write!(
            script,
            "    push.{}\n    push.{}\n    call.storage_expander::{procedure_name}\n    dropw dropw dropw dropw\n\n",
            Word::from(*value).to_hex(),
            Word::from(*key).to_hex(),
        )
        .expect("write to string should not fail");
    }

    script.push_str("end\n");
    script
}
