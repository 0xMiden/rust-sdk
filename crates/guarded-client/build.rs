use miette::IntoDiagnostic;

const GUARDIAN_PROTO: &str = "proto/guardian.proto";

fn main() -> miette::Result<()> {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=proto");

    let file_descriptors = protox::compile([GUARDIAN_PROTO], ["proto"]).into_diagnostic()?;

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(false)
        .compile_fds(file_descriptors)
        .into_diagnostic()
}
