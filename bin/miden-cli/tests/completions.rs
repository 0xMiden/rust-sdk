use std::env::temp_dir;
use std::path::PathBuf;
use std::{fs, process};

use assert_cmd::Command;

/// A binary name different from the default `miden-client`, so the test invokes the client as a
/// differently named executable. The process id suffix keeps parallel test runs from colliding on
/// the same file in the temp directory.
const RENAMED_BINARY_NAME: &str = "miden-client-renamed";

/// Runs the client under a distinct name and checks that the generated completion script is named
/// after it.
///
/// The test invokes the real binary, so it exercises `client_binary_name()` end to end instead of
/// bypassing it with a hard-coded name. The binary is copied rather than symlinked because
/// `std::env::current_exe()` resolves symlinks on some platforms (on Linux it reads
/// `/proc/self/exe`), which would report the target's file name instead of the invoked one.
#[test]
fn completions_use_the_name_the_client_is_invoked_with() {
    let client_binary = PathBuf::from(env!("CARGO_BIN_EXE_miden-client"));

    let mut renamed_binary = temp_dir().join(format!("{RENAMED_BINARY_NAME}-{}", process::id()));
    if cfg!(windows) {
        renamed_binary.set_extension("exe");
    }

    fs::copy(&client_binary, &renamed_binary)
        .expect("should be able to copy the client binary into the temp directory");

    let output = Command::new(&renamed_binary)
        .args(["completions", "bash"])
        .output()
        .expect("renamed client binary should be runnable");

    let _ = fs::remove_file(&renamed_binary);

    assert!(
        output.status.success(),
        "the completions command should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let script = String::from_utf8(output.stdout).expect("completion script should be valid UTF-8");
    assert!(
        script.contains(RENAMED_BINARY_NAME),
        "script should be named after the binary the client is invoked with"
    );
}
