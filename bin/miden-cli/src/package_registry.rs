//! Resolves the package arguments of the CLI commands to package files.
//!
//! A package argument is a path to a `.masp` file, a name in the configured package directory, or a
//! `name@version` reference to the local package registry. The CLI resolves a registry reference
//! with the `miden registry` command of the active Miden toolchain.

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use miden_client::account::component::MIDEN_PACKAGE_EXTENSION;
use serde::Deserialize;

use crate::errors::CliError;

/// The command that runs the tools of the active Miden toolchain.
///
/// The CLI calls `miden registry` and not `miden-registry` directly. Only the `miden` wrapper sets
/// `MIDEN_SYSROOT`, and the registry uses that variable to find the toolchain it belongs to.
const MIDEN_COMMAND: &str = "miden";

/// The version that selects the highest semantic version in the registry.
const LATEST_VERSION: &str = "latest";

// PACKAGE SPEC
// ================================================================================================

/// The source of a package, parsed from a package argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PackageSpec {
    /// A `.masp` file at the given path.
    File(PathBuf),
    /// A package with the given name in the configured package directory.
    Directory(PathBuf),
    /// A package in the local package registry.
    Registry {
        name: String,
        /// The version to resolve. `None` selects the highest semantic version.
        version: Option<String>,
    },
}

impl PackageSpec {
    /// Parses a package argument.
    ///
    /// A path with the `.masp` extension is a file, also when it contains `@`. Any other argument
    /// that contains `@` is a registry reference. An empty version or `latest` selects the highest
    /// semantic version. An argument without an extension is a name in the package directory.
    ///
    /// The `@` check comes before the extension check, because `name@0.1.0` has the extension `0`.
    pub(crate) fn parse(path: &Path) -> Result<Self, CliError> {
        if path.extension() == Some(OsStr::new(MIDEN_PACKAGE_EXTENSION)) {
            return Ok(Self::File(path.to_path_buf()));
        }

        let text = path.to_string_lossy();
        if let Some((name, version)) = text.split_once('@') {
            // A name that starts with `-` would reach `miden registry` as an option.
            if name.is_empty() || name.starts_with('-') || name.contains(['/', '\\']) {
                return Err(CliError::InvalidArgument(format!(
                    "'{text}' is not a valid package reference. Expected `<NAME>@<VERSION>`."
                )));
            }

            let version = match version {
                "" | LATEST_VERSION => None,
                version => Some(version.to_string()),
            };
            return Ok(Self::Registry { name: name.to_string(), version });
        }

        match path.extension() {
            None => Ok(Self::Directory(path.to_path_buf())),
            Some(extension) => Err(CliError::InvalidArgument(format!(
                "{} has an invalid file extension: '{}'. Expected: {MIDEN_PACKAGE_EXTENSION}.",
                path.display(),
                extension.display()
            ))),
        }
    }
}

// REGISTRY LOOKUP
// ================================================================================================

/// The fields of the `miden registry show --json` output that the CLI uses.
#[derive(Debug, Deserialize)]
struct RegistryPackageSummary {
    name: String,
    version: String,
    artifact_path: PathBuf,
}

/// Returns the path of the package file that the local registry stores for `name` at `version`.
///
/// When `version` is `None`, the registry selects the highest semantic version.
pub(crate) fn resolve_registry_artifact(
    name: &str,
    version: Option<&str>,
) -> Result<PathBuf, CliError> {
    let mut command = Command::new(MIDEN_COMMAND);
    command.args(["registry", "show", name, "--json"]);
    if let Some(version) = version {
        command.args(["--version", version]);
    }

    let output = command.output().map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            CliError::PackageRegistry(format!(
                "the `{MIDEN_COMMAND}` command was not found, so `{}` cannot be resolved. Install \
                 the Miden toolchain with midenup: https://github.com/0xMiden/midenup",
                package_reference(name, version)
            ))
        } else {
            CliError::PackageRegistry(format!("failed to run `{MIDEN_COMMAND} registry`: {err}"))
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CliError::PackageRegistry(format!(
            "failed to resolve `{}`: {}. Run `{MIDEN_COMMAND} registry list` to see the packages \
             in the local registry",
            package_reference(name, version),
            registry_error_message(&stderr)
        )));
    }

    let summary = parse_registry_summary(&output.stdout)?;
    tracing::debug!(
        "Resolved package {name} to {}@{} at {}",
        summary.name,
        summary.version,
        summary.artifact_path.display()
    );

    Ok(summary.artifact_path)
}

/// Parses the JSON that `miden registry show --json` writes to stdout.
fn parse_registry_summary(stdout: &[u8]) -> Result<RegistryPackageSummary, CliError> {
    serde_json::from_slice(stdout).map_err(|err| {
        CliError::PackageRegistry(format!(
            "failed to parse the output of `{MIDEN_COMMAND} registry show --json`: {err}"
        ))
    })
}

/// Returns the error message of the registry from its stderr.
///
/// The `miden` wrapper writes `info:` lines about the active toolchain to stderr. These lines do
/// not describe the error, so this function removes them.
fn registry_error_message(stderr: &str) -> String {
    let message = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("info:"))
        .collect::<Vec<_>>()
        .join(" ");

    if message.is_empty() {
        "the registry returned an error without a message".to_string()
    } else {
        message
    }
}

/// Formats a registry reference as the user writes it.
fn package_reference(name: &str, version: Option<&str>) -> String {
    format!("{name}@{}", version.unwrap_or(LATEST_VERSION))
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{PackageSpec, parse_registry_summary, registry_error_message};

    fn parse(input: &str) -> PackageSpec {
        PackageSpec::parse(Path::new(input)).unwrap()
    }

    fn registry(name: &str, version: Option<&str>) -> PackageSpec {
        PackageSpec::Registry {
            name: name.to_string(),
            version: version.map(str::to_string),
        }
    }

    #[test]
    fn parses_masp_paths_as_files() {
        assert_eq!(parse("pkg.masp"), PackageSpec::File(PathBuf::from("pkg.masp")));
        assert_eq!(
            parse("target/miden/x@1.masp"),
            PackageSpec::File(PathBuf::from("target/miden/x@1.masp"))
        );
    }

    #[test]
    fn parses_bare_names_as_package_directory_entries() {
        assert_eq!(parse("basic-wallet"), PackageSpec::Directory(PathBuf::from("basic-wallet")));
        assert_eq!(parse("auth/no-auth"), PackageSpec::Directory(PathBuf::from("auth/no-auth")));
    }

    #[test]
    fn parses_registry_references() {
        assert_eq!(parse("counter-contract@0.1.0"), registry("counter-contract", Some("0.1.0")));
        assert_eq!(
            parse("counter-contract@0.1.0#0xabc"),
            registry("counter-contract", Some("0.1.0#0xabc"))
        );
    }

    #[test]
    fn parses_empty_or_latest_version_as_highest_version() {
        assert_eq!(parse("counter-contract@"), registry("counter-contract", None));
        assert_eq!(parse("counter-contract@latest"), registry("counter-contract", None));
    }

    #[test]
    fn rejects_invalid_arguments() {
        assert!(PackageSpec::parse(Path::new("pkg.txt")).is_err());
        assert!(PackageSpec::parse(Path::new("@0.1.0")).is_err());
        assert!(PackageSpec::parse(Path::new("dir/pkg@0.1.0")).is_err());
        assert!(PackageSpec::parse(Path::new("--help@0.1.0")).is_err());
    }

    #[test]
    fn parses_registry_show_output() {
        let stdout = br#"{
            "name": "counter-contract",
            "version": "0.1.0#0xd4ae",
            "description": null,
            "dependencies": { "miden-core": "0.35.0#0xdd25" },
            "artifact_path": "/toolchains/0.17.0/lib/counter-contract.masp"
        }"#;

        let summary = parse_registry_summary(stdout).unwrap();
        assert_eq!(summary.name, "counter-contract");
        assert_eq!(
            summary.artifact_path,
            PathBuf::from("/toolchains/0.17.0/lib/counter-contract.masp")
        );
    }

    #[test]
    fn rejects_registry_output_without_artifact_path() {
        assert!(parse_registry_summary(br#"{ "name": "x", "version": "1.0.0" }"#).is_err());
        assert!(parse_registry_summary(b"not json").is_err());
    }

    #[test]
    fn removes_toolchain_info_lines_from_registry_errors() {
        let stderr = "info: current toolchain is devnet and is installed\n\
                      Version '9.9.9' does not exist for package 'counter-contract'\n";
        assert_eq!(
            registry_error_message(stderr),
            "Version '9.9.9' does not exist for package 'counter-contract'"
        );
        assert_eq!(
            registry_error_message("info: current toolchain is devnet\n"),
            "the registry returned an error without a message"
        );
    }
}
