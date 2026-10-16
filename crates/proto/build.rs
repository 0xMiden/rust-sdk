use protox::file::{
    ChainFileResolver,
    DescriptorSetFileResolver,
    GoogleFileResolver,
    IncludeFileResolver,
};

/// The schemas of the miden-client types. Each file has its own `client.*` package, so it generates
/// one Rust module.
const PROTO_FILES: &[&str] = &[
    "proto/client/input_note.proto",
    "proto/client/note_transport.proto",
    "proto/client/output_note.proto",
    "proto/client/protocol.proto",
    "proto/client/pswap.proto",
    "proto/client/rpc.proto",
    "proto/client/transaction.proto",
];

/// Generates the Rust protobuf bindings for the miden-client types.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo::rerun-if-changed=proto");

    // An `import` in the schema resolves against this crate's `proto` directory, then the object
    // schemas that `miden-objects` embeds, then the well-known Google types.
    let mut resolver = ChainFileResolver::new();
    resolver.add(IncludeFileResolver::new("proto".into()));
    resolver.add(DescriptorSetFileResolver::decode(miden_objects::FILE_DESCRIPTOR_SET)?);
    resolver.add(GoogleFileResolver::new());

    let mut compiler = protox::Compiler::with_file_resolver(resolver);
    compiler.include_imports(true).open_files(PROTO_FILES)?;

    // The object schemas resolve to the types that `miden-objects` defines, so its conversions
    // apply to them.
    let mut config = prost_build::Config::new();
    for &(proto_path, rust_path) in miden_objects::EXTERN_PATHS {
        config.extern_path(proto_path, rust_path);
    }
    config.compile_fds(compiler.file_descriptor_set())?;

    Ok(())
}
