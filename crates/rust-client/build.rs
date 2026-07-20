use std::fs;
use std::path::{Path, PathBuf};

use miden_node_proto_build::{remote_prover_api_descriptor, rpc_api_descriptor};
use miden_note_transport_proto_build::mnt_api_descriptor;
use miette::IntoDiagnostic;

const RPC_STD_DIR: &str = "rpc/std";
const RPC_NOSTD_DIR: &str = "rpc/nostd";
const NOTE_TRANSPORT_STD_DIR: &str = "note_transport/std";
const NOTE_TRANSPORT_NOSTD_DIR: &str = "note_transport/nostd";
const REMOTE_PROVER_STD_DIR: &str = "remote_prover/std";
const REMOTE_PROVER_NOSTD_DIR: &str = "remote_prover/nostd";

const RPC_STD_WRAPPER: &str = "rpc_std.rs";
const RPC_NOSTD_WRAPPER: &str = "rpc_nostd.rs";
const NOTE_TRANSPORT_STD_WRAPPER: &str = "note_transport_std.rs";
const NOTE_TRANSPORT_NOSTD_WRAPPER: &str = "note_transport_nostd.rs";
const REMOTE_PROVER_STD_WRAPPER: &str = "remote_prover_std.rs";
const REMOTE_PROVER_NOSTD_WRAPPER: &str = "remote_prover_nostd.rs";

fn main() -> miette::Result<()> {
    // Proto definitions come from build-dependency crates. Cargo automatically re-runs this
    // script when those crates change. This directive opts out of the default behavior of
    // re-running on every source file change.
    // https://doc.rust-lang.org/cargo/reference/build-scripts.html#rerun-if-changed
    println!("cargo::rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").into_diagnostic()?);

    compile_tonic_client_proto(&out_dir)?;
    compile_tonic_note_transport_proto(&out_dir)?;
    compile_tonic_remote_prover_proto(&out_dir)?;

    replace_no_std_types_in_dir(&out_dir.join(RPC_NOSTD_DIR))?;
    replace_no_std_types_in_dir(&out_dir.join(NOTE_TRANSPORT_NOSTD_DIR))?;
    replace_no_std_types_in_dir(&out_dir.join(REMOTE_PROVER_NOSTD_DIR))?;

    generate_wrapper(&out_dir, RPC_STD_DIR, RPC_STD_WRAPPER)?;
    generate_wrapper(&out_dir, RPC_NOSTD_DIR, RPC_NOSTD_WRAPPER)?;
    generate_wrapper(&out_dir, NOTE_TRANSPORT_STD_DIR, NOTE_TRANSPORT_STD_WRAPPER)?;
    generate_wrapper(&out_dir, NOTE_TRANSPORT_NOSTD_DIR, NOTE_TRANSPORT_NOSTD_WRAPPER)?;
    generate_wrapper(&out_dir, REMOTE_PROVER_STD_DIR, REMOTE_PROVER_STD_WRAPPER)?;
    generate_wrapper(&out_dir, REMOTE_PROVER_NOSTD_DIR, REMOTE_PROVER_NOSTD_WRAPPER)?;

    Ok(())
}

// REMOTE PROVER CLIENT PROTO CODEGEN
// ===============================================================================================

/// Generates the Rust protobuf bindings for the remote prover client.
fn compile_tonic_remote_prover_proto(out_dir: &Path) -> miette::Result<()> {
    let file_descriptors = remote_prover_api_descriptor();

    let std_out = out_dir.join(REMOTE_PROVER_STD_DIR);
    let nostd_out = out_dir.join(REMOTE_PROVER_NOSTD_DIR);
    fs::create_dir_all(&std_out).into_diagnostic()?;
    fs::create_dir_all(&nostd_out).into_diagnostic()?;

    // The `nostd` bindings target `wasm32`, where the transport is provided by
    // `tonic-web-wasm-client`, so tonic's own transport codegen must be disabled.
    tonic_prost_build::configure()
        .build_transport(false)
        .build_server(false)
        .out_dir(&nostd_out)
        .compile_fds_with_config(file_descriptors.clone(), tonic_prost_build::Config::new())
        .into_diagnostic()?;

    tonic_prost_build::configure()
        .build_server(false)
        .out_dir(&std_out)
        .compile_fds_with_config(file_descriptors, tonic_prost_build::Config::new())
        .into_diagnostic()?;

    Ok(())
}

// NOTE TRANSPORT CLIENT PROTO CODEGEN
// ===============================================================================================

/// Generates the Rust protobuf bindings for the Note Transport client.
fn compile_tonic_note_transport_proto(out_dir: &Path) -> miette::Result<()> {
    let file_descriptors = mnt_api_descriptor();

    let std_out = out_dir.join(NOTE_TRANSPORT_STD_DIR);
    let nostd_out = out_dir.join(NOTE_TRANSPORT_NOSTD_DIR);
    fs::create_dir_all(&std_out).into_diagnostic()?;
    fs::create_dir_all(&nostd_out).into_diagnostic()?;

    let mut prost_config = tonic_prost_build::Config::new();
    prost_config.skip_debug(["AccountId", "Digest"]);

    let mut web_tonic_prost_config = tonic_prost_build::Config::new();
    web_tonic_prost_config.skip_debug(["AccountId", "Digest"]);
    // Use BTreeMap so the no_std bindings don't depend on std::collections::HashMap.
    web_tonic_prost_config.btree_map(["."]);

    // Generate the header of the user facing server from its proto file
    tonic_prost_build::configure()
        .build_transport(false)
        .build_server(false)
        .out_dir(&nostd_out)
        .compile_fds_with_config(file_descriptors.clone(), web_tonic_prost_config)
        .into_diagnostic()?;

    tonic_prost_build::configure()
        .build_server(false)
        .out_dir(&std_out)
        .compile_fds_with_config(file_descriptors, prost_config)
        .into_diagnostic()?;

    Ok(())
}

// NODE RPC CLIENT PROTO CODEGEN
// ===============================================================================================

/// Generates the Rust protobuf bindings for the RPC client.
fn compile_tonic_client_proto(out_dir: &Path) -> miette::Result<()> {
    let file_descriptors = rpc_api_descriptor();

    let std_out = out_dir.join(RPC_STD_DIR);
    let nostd_out = out_dir.join(RPC_NOSTD_DIR);
    fs::create_dir_all(&std_out).into_diagnostic()?;
    fs::create_dir_all(&nostd_out).into_diagnostic()?;

    let mut prost_config = tonic_prost_build::Config::new();
    prost_config.skip_debug(["AccountId", "Digest"]);

    let mut web_tonic_prost_config = tonic_prost_build::Config::new();
    web_tonic_prost_config.skip_debug(["AccountId", "Digest"]);

    // Use BTreeMap so the no_std bindings don't depend on std::collections::HashMap
    web_tonic_prost_config.btree_map(["."]);

    // Generate the header of the user facing server from its proto file
    tonic_prost_build::configure()
        .build_transport(false)
        .build_server(false)
        .out_dir(&nostd_out)
        .compile_fds_with_config(file_descriptors.clone(), web_tonic_prost_config)
        .into_diagnostic()?;

    tonic_prost_build::configure()
        .build_server(false)
        .out_dir(&std_out)
        .compile_fds_with_config(file_descriptors, prost_config)
        .into_diagnostic()?;

    Ok(())
}

// WRAPPER GENERATION
// ===============================================================================================

/// Scans `out_dir/subdir/` for generated `.rs` files and produces a single wrapper file at
/// `out_dir/wrapper_name` that re-exports each file as a module via `include!`.
///
/// The wrapper converts each file into a module declaration:
///
/// ```ignore
/// #[allow(clippy::doc_markdown, ...)]
/// pub mod foo { include!(concat!(env!("OUT_DIR"), "/subdir/foo.rs")); }
/// ```
fn generate_wrapper(out_dir: &Path, subdir: &str, wrapper_name: &str) -> miette::Result<()> {
    let dir = out_dir.join(subdir);

    // Discover all generated .rs files in the output directory
    let mut mod_names: Vec<String> = fs::read_dir(&dir)
        .into_diagnostic()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            name.strip_suffix(".rs").map(str::to_owned)
        })
        .collect();
    mod_names.sort();

    let allow_attr = "#[allow(clippy::doc_markdown, clippy::struct_field_names, \
                      clippy::trivially_copy_pass_by_ref, clippy::large_enum_variant)]";

    let mut wrapper = String::new();
    for mod_name in &mod_names {
        let mod_declaration = format!(
            "{allow_attr}\n\
             pub mod {mod_name} {{ include!(concat!(env!(\"OUT_DIR\"), \"/{subdir}/{mod_name}.rs\")); }}\n"
        );
        wrapper.push_str(&mod_declaration);
    }

    fs::write(out_dir.join(wrapper_name), wrapper).into_diagnostic()?;

    Ok(())
}

// NO_STD REPLACEMENTS
// ===============================================================================================

/// Applies `no_std` type replacements to all `.rs` files in the given directory.
///
/// This is needed because `tonic_build` doesn't generate `no_std` compatible files and we need
/// to build WASM without `std`.
fn replace_no_std_types_in_dir(dir: &Path) -> miette::Result<()> {
    for entry in fs::read_dir(dir).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            let content = fs::read_to_string(&path).into_diagnostic()?;
            let replaced = content
                .replace("std::result", "core::result")
                .replace("std::marker", "core::marker")
                .replace("format!", "alloc::format!");
            fs::write(&path, replaced).into_diagnostic()?;
        }
    }
    Ok(())
}
