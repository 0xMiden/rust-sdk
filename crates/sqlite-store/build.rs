use protox::file::{
    ChainFileResolver,
    DescriptorSetFileResolver,
    GoogleFileResolver,
    IncludeFileResolver,
};

/// The schemas of the values this store keeps. All of them are in the `miden.client.store` package,
/// so they generate one Rust module.
const STORE_PROTO_FILES: &[&str] = &[
    "proto/store/input_note.proto",
    "proto/store/output_note.proto",
    "proto/store/protocol.proto",
    "proto/store/settings.proto",
    "proto/store/transaction.proto",
];

/// Generates the Rust protobuf bindings for the values this store keeps.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo::rerun-if-changed=proto");

    // An `import` in the schema resolves against this crate's `proto` directory, then the object
    // schemas that `miden-objects` embeds, then the well-known Google types.
    let mut resolver = ChainFileResolver::new();
    resolver.add(IncludeFileResolver::new("proto".into()));
    resolver.add(DescriptorSetFileResolver::decode(miden_objects::FILE_DESCRIPTOR_SET)?);
    resolver.add(GoogleFileResolver::new());

    let mut compiler = protox::Compiler::with_file_resolver(resolver);
    compiler.include_imports(true).open_files(STORE_PROTO_FILES)?;

    // The object schemas resolve to the types that `miden-objects` defines, so its conversions
    // apply to them.
    let mut config = prost_build::Config::new();
    for &(proto_path, rust_path) in miden_objects::EXTERN_PATHS {
        config.extern_path(proto_path, rust_path);
    }
    config.compile_fds(compiler.file_descriptor_set())?;

    Ok(())
}
