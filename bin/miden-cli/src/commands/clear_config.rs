use std::path::PathBuf;
use std::{fs, io};

use clap::Parser;

use crate::config::{MIDEN_DIR, get_global_miden_dir, get_local_miden_dir};
use crate::errors::CliError;

// CLEAR COMMAND
// ================================================================================================

#[derive(Debug, Clone, Parser)]
#[command(
    about = "Clear miden client configuration. By default removes local config if present, otherwise removes global config. \
Use --global to specifically target global config."
)]
pub struct ClearConfigCmd {
    /// Force removal of global configuration, even if local config exists
    #[clap(long)]
    global: bool,
    /// Do not prompt for confirmation before deleting configuration directories
    #[clap(long)]
    force: bool,
}

impl ClearConfigCmd {
    pub fn execute(&self) -> Result<(), CliError> {
        if self.global {
            // Clear global config specifically
            self.clear_global_config()
        } else {
            // Priority logic: local first, then global
            self.try_clear_local_config()
        }
    }

    /// Try to clear the local config if it exists, and if not, try to clear the global config. This
    /// function will first try to clear the local config if it exists, and if not, it will clear
    /// the global config. For both cases, it will prompt the user for confirmation to clear the
    /// config.
    fn try_clear_local_config(&self) -> Result<(), CliError> {
        // Try local config first
        let local_miden_dir = get_local_miden_dir()?;
        if local_miden_dir.exists() {
            self.remove_directory(&local_miden_dir, "local")?;
            return Ok(());
        }

        // Clear global config if no local config exists
        println!("No local configuration found. Attempting to clear global configuration.");
        self.clear_global_config()
    }

    fn clear_global_config(&self) -> Result<(), CliError> {
        let global_miden_dir = get_global_miden_dir().map_err(|e| {
            CliError::Config(Box::new(e), "Failed to determine home directory".to_string())
        })?;

        if global_miden_dir.exists() {
            self.remove_directory(&global_miden_dir, "global")?;
        } else {
            println!("No global Miden configuration found to clear.");
        }

        Ok(())
    }

    fn remove_directory(&self, dir_path: &PathBuf, config_type: &str) -> Result<(), CliError> {
        if !dir_path.exists() {
            println!("No {config_type} Miden configuration found to clear.");
            return Ok(());
        }

        println!("Found {config_type} Miden configuration at: {}", dir_path.display());

        if !self.force {
            println!("Are you sure you want to remove it? (y/N)");
            let mut proceed_str: String = String::new();
            io::stdin().read_line(&mut proceed_str)?;
            if proceed_str.trim().to_lowercase() != "y" {
                println!("Operation cancelled.");
                return Ok(());
            }
        }

        println!("Removing {config_type} Miden configuration...");

        fs::remove_dir_all(dir_path).map_err(|err| {
            CliError::Config(
                Box::new(err),
                format!("Failed to remove {config_type} {MIDEN_DIR} directory"),
            )
        })?;

        println!("Successfully removed {config_type} miden configuration.");
        Ok(())
    }
}
