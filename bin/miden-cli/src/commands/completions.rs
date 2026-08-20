use std::io;

use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::{Cli, Parser, client_binary_name};

// COMPLETIONS COMMAND
// ================================================================================================

#[derive(Debug, Clone, Parser)]
#[command(about = "Print a shell completion script for the client CLI to stdout")]
pub struct CompletionsCmd {
    /// Shell to generate the completion script for
    #[arg(value_enum)]
    shell: Shell,
}

impl CompletionsCmd {
    pub fn execute(&self) {
        let bin_name = client_binary_name().to_string_lossy().into_owned();
        generate(self.shell, &mut Cli::command(), bin_name, &mut io::stdout());
    }
}

#[cfg(test)]
mod tests {
    use clap::ValueEnum;
    use clap_complete::Shell;

    use super::*;

    #[test]
    fn generation_succeeds_and_is_non_empty_for_every_shell() {
        for shell in Shell::value_variants() {
            let mut buf = Vec::new();
            generate(*shell, &mut Cli::command(), "miden-client", &mut buf);
            assert!(!buf.is_empty(), "completion script for {shell} was empty");
        }
    }
}
