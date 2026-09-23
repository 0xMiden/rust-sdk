use std::io::{Write, stdout};

use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};

use crate::errors::CliError;
use crate::{Cli, client_binary_name};

// COMPLETIONS COMMAND
// ================================================================================================

/// Generate a completion script for the client.
///
/// The command prints the completion script of the requested shell on stdout. Source the script in
/// the shell configuration to enable completions for the client commands and flags.
#[derive(Debug, Clone, Parser)]
#[command(about = "Generate a completion script for the client and print it on stdout")]
pub struct CompletionsCmd {
    /// Shell to generate the completion script for
    #[arg(value_enum)]
    shell: Shell,
}

impl CompletionsCmd {
    pub fn execute(&self) -> Result<(), CliError> {
        let bin_name = client_binary_name().to_string_lossy().into_owned();

        let mut buffer = Vec::new();
        self.write_completions(&bin_name, &mut buffer);
        stdout().write_all(&buffer)?;

        Ok(())
    }

    /// Write the completion script of the client to `buffer`.
    ///
    /// The script uses `bin_name` as the command name, so the completions match the name the user
    /// invokes the client with.
    fn write_completions(&self, bin_name: &str, buffer: &mut dyn Write) {
        let mut command = Cli::command();

        generate(self.shell, &mut command, bin_name, buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_script_lists_subcommands() {
        let cmd = CompletionsCmd { shell: Shell::Bash };

        let mut buffer = Vec::new();
        cmd.write_completions("miden-client", &mut buffer);

        let script = String::from_utf8(buffer).expect("completion script should be valid UTF-8");
        assert!(script.contains("account"), "script should list the account command");
        assert!(script.contains("completions"), "script should list the completions command");
    }
}
