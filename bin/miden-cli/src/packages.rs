use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use miden_client::account::component::MIDEN_PACKAGE_EXTENSION;
use miden_client::utils::Deserializable;
use miden_client::vm::Package;

use crate::config::CliConfig;
use crate::errors::CliError;

// PACKAGE LOADING
// ================================================================================================

/// Reads [[`miden_core::vm::Package`]]s from the given file paths.
///
/// A bare name resolves to a package in the configured package directory. The CLI writes those
/// packages itself, so they are read as trusted. A path with the `.masp` extension is used as is
/// and is read as untrusted, so its MAST forest is validated.
pub(crate) fn load_packages(
    cli_config: &CliConfig,
    package_paths: &[PathBuf],
) -> Result<Vec<Package>, CliError> {
    let mut packages = Vec::with_capacity(package_paths.len());

    let packages_dir = &cli_config.package_directory;
    for path in package_paths {
        // If a user passes in a file with the `.masp` file extension, then we leave the path as is;
        // since it probably is a full path (this is the case with cargo-miden for instance).
        let (path, trusted) = match path.extension() {
            None => {
                let path = path.with_extension(MIDEN_PACKAGE_EXTENSION);
                Ok((packages_dir.join(path), true))
            },
            Some(extension) => {
                if extension == OsStr::new(MIDEN_PACKAGE_EXTENSION) {
                    Ok((path.clone(), false))
                } else {
                    let error = std::io::Error::new(
                        std::io::ErrorKind::InvalidFilename,
                        format!(
                            "{} has an invalid file extension: '{}'. \
                            Expected: {MIDEN_PACKAGE_EXTENSION}",
                            path.display(),
                            extension.display()
                        ),
                    );
                    Err(CliError::AccountComponentError(
                        Box::new(error),
                        format!("refuesed to read {}", path.display()),
                    ))
                }
            },
        }?;

        let bytes = fs::read(&path).map_err(|e| {
            CliError::AccountComponentError(
                Box::new(e),
                format!("failed to read Package file from {}", path.display()),
            )
        })?;

        let package = if trusted {
            Package::read_from_bytes_trusted(&bytes)
        } else {
            Package::read_from_bytes(&bytes)
        }
        .map_err(|e| {
            CliError::AccountComponentError(
                Box::new(e),
                format!("failed to deserialize Package in {}", path.display()),
            )
        })?;

        packages.push(package);
    }

    Ok(packages)
}

/// Reads every `.masp` package found recursively under `dir`. A missing directory yields no
/// packages rather than an error, so inspection still falls back to bare MAST roots.
pub(crate) fn load_packages_from_directory(dir: &Path) -> Result<Vec<Package>, CliError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut package_paths = Vec::new();
    collect_masp_files(dir, &mut package_paths)?;

    // Paths already carry the `.masp` extension, so `load_packages` uses them as-is and never
    // consults the config's packages directory.
    load_packages(&CliConfig::default(), &package_paths)
}

/// Recursively collects the paths of all `.masp` files under `dir` into `paths`.
fn collect_masp_files(dir: &Path, paths: &mut Vec<PathBuf>) -> Result<(), CliError> {
    let entries = std::fs::read_dir(dir).map_err(|err| {
        CliError::Config(
            Box::new(err),
            format!("failed to read packages directory {}", dir.display()),
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|err| {
            CliError::Config(
                Box::new(err),
                format!("failed to read entry in packages directory {}", dir.display()),
            )
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_masp_files(&path, paths)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some(MIDEN_PACKAGE_EXTENSION) {
            paths.push(path);
        }
    }

    Ok(())
}
