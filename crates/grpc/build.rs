use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use miden_node_proto_build::{
    note_transport_api_descriptor,
    remote_prover_api_descriptor,
    rpc_api_descriptor,
};
use miette::IntoDiagnostic;
use tonic_prost_build::FileDescriptorSet;

/// Messages of the node RPC API whose bindings do not implement `Debug`.
const RPC_SKIP_DEBUG: &[&str] = &["RegisterAccountRequest", "IsInvitationCodeValidRequest"];

fn main() -> miette::Result<()> {
    // Proto definitions come from build-dependency crates. Cargo automatically re-runs this
    // script when those crates change. This directive opts out of the default behavior of
    // re-running on every source file change.
    // https://doc.rust-lang.org/cargo/reference/build-scripts.html#rerun-if-changed
    println!("cargo::rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").into_diagnostic()?);

    compile_client_proto(&out_dir, "rpc", rpc_api_descriptor(), RPC_SKIP_DEBUG)?;
    compile_client_proto(&out_dir, "note_transport", note_transport_api_descriptor(), &[])?;
    compile_client_proto(&out_dir, "remote_prover", remote_prover_api_descriptor(), &[])?;

    Ok(())
}

// PROST CONFIGURATION
// ===============================================================================================

/// Builds a prost config that resolves the canonical object schemas to `miden-objects` types.
///
/// The node's descriptors embed the object schemas they import. These paths make prost reference
/// the types `miden-objects` already defines, so the conversions it ships apply to them.
fn canonical_object_config() -> tonic_prost_build::Config {
    let mut config = tonic_prost_build::Config::new();
    for (proto_path, rust_path) in miden_objects::EXTERN_PATHS {
        config.extern_path(*proto_path, *rust_path);
    }
    config
}

// CLIENT PROTO CODEGEN
// ===============================================================================================

/// Generates the Rust client bindings of one gRPC API.
///
/// The bindings for native targets go to `out_dir/<api>/std`. The bindings for `wasm32` go to
/// `out_dir/<api>/nostd`. The wrapper files `out_dir/<api>_std.rs` and `out_dir/<api>_nostd.rs`
/// include the bindings of one directory as modules.
///
/// The bindings of the messages in `skip_debug` do not implement `Debug`.
fn compile_client_proto(
    out_dir: &Path,
    api: &str,
    file_descriptors: FileDescriptorSet,
    skip_debug: &[&str],
) -> miette::Result<()> {
    let std_dir = format!("{api}/std");
    let nostd_dir = format!("{api}/nostd");
    fs::create_dir_all(out_dir.join(&std_dir)).into_diagnostic()?;
    fs::create_dir_all(out_dir.join(&nostd_dir)).into_diagnostic()?;

    let mut std_config = canonical_object_config();
    std_config.skip_debug(skip_debug);

    let mut nostd_config = canonical_object_config();
    nostd_config.skip_debug(skip_debug);
    // Use BTreeMap so the `wasm32` bindings do not depend on std::collections::HashMap.
    nostd_config.btree_map(["."]);

    // On `wasm32`, `tonic-web-wasm-client` provides the transport. The bindings must not use the
    // transport of `tonic`.
    tonic_prost_build::configure()
        .build_client(true)
        .build_transport(false)
        .build_server(false)
        .out_dir(out_dir.join(&nostd_dir))
        .compile_fds_with_config(file_descriptors.clone(), nostd_config)
        .into_diagnostic()?;

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(false)
        .out_dir(out_dir.join(&std_dir))
        .compile_fds_with_config(file_descriptors, std_config)
        .into_diagnostic()?;

    generate_wrapper(out_dir, &std_dir, &format!("{api}_std.rs"))?;
    generate_wrapper(out_dir, &nostd_dir, &format!("{api}_nostd.rs"))?;

    Ok(())
}

// WRAPPER GENERATION
// ===============================================================================================

/// Scans `out_dir/subdir/` for generated `.rs` files and produces a single wrapper file at
/// `out_dir/wrapper_name` that re-exports each file as a module via `include!`.
///
/// The wrapper converts the file names into nested module declarations. Files that share a prefix
/// share the parent modules:
///
/// ```ignore
/// #[allow(clippy::doc_markdown, ...)]
/// pub mod foo {
///     pub mod bar { include!(concat!(env!("OUT_DIR"), "/subdir/foo.bar.rs")); }
///     pub mod baz { include!(concat!(env!("OUT_DIR"), "/subdir/foo.baz.rs")); }
/// }
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

    let mut tree = ModuleTree::default();
    for mod_name in &mod_names {
        tree.insert(mod_name);
    }

    let allow_attr = "#[allow(clippy::doc_markdown, clippy::struct_field_names, \
                      clippy::trivially_copy_pass_by_ref, clippy::large_enum_variant)]";

    let mut wrapper = String::new();
    for (name, module) in &tree.children {
        writeln!(wrapper, "{allow_attr}\npub mod {name} {{ {} }}", module.render(subdir))
            .into_diagnostic()?;
    }

    fs::write(out_dir.join(wrapper_name), wrapper).into_diagnostic()?;

    Ok(())
}

/// Module tree built from the generated file names. Packages that share a prefix, such as
/// `miden.node.v1` and `miden.node.v2`, share their parent modules.
#[derive(Default)]
struct ModuleTree {
    file: Option<String>,
    children: BTreeMap<String, ModuleTree>,
}

impl ModuleTree {
    fn insert(&mut self, mod_name: &str) {
        let module = mod_name
            .split('.')
            .fold(self, |module, name| module.children.entry(name.to_owned()).or_default());
        module.file = Some(mod_name.to_owned());
    }

    fn render(&self, subdir: &str) -> String {
        let include = self
            .file
            .iter()
            .map(|file| format!("include!(concat!(env!(\"OUT_DIR\"), \"/{subdir}/{file}.rs\"));"));
        let children = self
            .children
            .iter()
            .map(|(name, child)| format!("pub mod {name} {{ {} }}", child.render(subdir)));
        include.chain(children).collect::<Vec<_>>().join(" ")
    }
}
