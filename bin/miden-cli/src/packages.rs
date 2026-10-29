use std::fs;
use std::path::{Path, PathBuf};

use miden_client::account::component::MIDEN_PACKAGE_EXTENSION;
use miden_client::vm::Package;
use miden_protocol::utils::serde::Deserializable;

use crate::config::CliConfig;
use crate::errors::CliError;
use crate::package_registry::{PackageSpec, resolve_registry_artifact};

// PACKAGE LOADING
// ================================================================================================

/// Reads [[`miden_core::vm::Package`]]s from the given package arguments.
///
/// A bare name resolves to a package in the configured package directory. The CLI writes those
/// packages itself, so they are read as trusted. A path with the `.masp` extension is used as is. A
/// `name@version` reference resolves to a package file in the local package registry. The CLI reads
/// `.masp` paths and registry packages as untrusted, so it validates their MAST forest.
pub(crate) fn load_packages(
    cli_config: &CliConfig,
    package_paths: &[PathBuf],
) -> Result<Vec<Package>, CliError> {
    let mut packages = Vec::with_capacity(package_paths.len());

    let packages_dir = &cli_config.package_directory;
    for path in package_paths {
        let mut expected_name = None;
        let (path, trusted) = match PackageSpec::parse(path)? {
            PackageSpec::File(path) => (path, false),
            PackageSpec::Directory(path) => {
                (packages_dir.join(path.with_extension(MIDEN_PACKAGE_EXTENSION)), true)
            },
            PackageSpec::Registry { name, version } => {
                let path = resolve_registry_artifact(&name, version.as_deref())?;
                expected_name = Some(name);
                (path, false)
            },
        };

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

        // The registry index and its artifact files can go out of sync. A name check makes sure
        // that the file contains the package that the user asked for.
        if let Some(expected_name) = expected_name
            && AsRef::<str>::as_ref(&package.name) != expected_name
        {
            return Err(CliError::PackageRegistry(format!(
                "the registry entry for `{expected_name}` points to {}, which contains the \
                 package `{}`",
                path.display(),
                package.name
            )));
        }

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
